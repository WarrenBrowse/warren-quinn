use std::collections::VecDeque;

use bytes::Bytes;
use thiserror::Error;
use tracing::{debug, trace};

use super::Connection;
use crate::config::DatagramAqmConfig;
use crate::connection::stats::DatagramTxStats;
use crate::{
    Duration, Instant, TransportError,
    frame::{Datagram, FrameStruct},
};

/// API to control datagram traffic
pub struct Datagrams<'a> {
    pub(super) conn: &'a mut Connection,
}

impl Datagrams<'_> {
    /// Queue an unreliable, unordered datagram for immediate transmission
    ///
    /// If `drop` is true, previously queued datagrams which are still unsent may be discarded to
    /// make space for this datagram, in order of oldest to newest. If `drop` is false, and there
    /// isn't enough space due to previously queued datagrams, this function will return
    /// `SendDatagramError::Blocked`. `Event::DatagramsUnblocked` will be emitted once datagrams
    /// have been sent.
    ///
    /// Returns `Err` iff a `len`-byte datagram cannot currently be sent.
    pub fn send(&mut self, data: Bytes, drop: bool, now: Instant) -> Result<(), SendDatagramError> {
        if self.conn.config.datagram_receive_buffer_size.is_none() {
            return Err(SendDatagramError::Disabled);
        }
        let max = self
            .max_size()
            .ok_or(SendDatagramError::UnsupportedByPeer)?;
        if data.len() > max {
            return Err(SendDatagramError::TooLarge);
        }
        if drop {
            while self.conn.datagrams.outgoing_total > self.conn.config.datagram_send_buffer_size {
                let prev = self
                    .conn
                    .datagrams
                    .outgoing
                    .pop_front()
                    .expect("datagrams.outgoing_total desynchronized");
                trace!(len = prev.datagram.data.len(), "dropping outgoing datagram");
                self.conn.datagrams.outgoing_total -= prev.datagram.data.len();
                self.conn.stats.datagram_tx.dropped_overflow += 1;
            }
        } else if self.conn.datagrams.outgoing_total + data.len()
            > self.conn.config.datagram_send_buffer_size
        {
            self.conn.datagrams.send_blocked = true;
            return Err(SendDatagramError::Blocked(data));
        }
        self.conn.datagrams.outgoing_total += data.len();
        self.conn.datagrams.outgoing.push_back(QueuedDatagram {
            queued_at: now,
            datagram: Datagram { data },
        });
        Ok(())
    }

    /// Compute the maximum size of datagrams that may passed to `send_datagram`
    ///
    /// Returns `None` if datagrams are unsupported by the peer or disabled locally.
    ///
    /// This may change over the lifetime of a connection according to variation in the path MTU
    /// estimate. The peer can also enforce an arbitrarily small fixed limit, but if the peer's
    /// limit is large this is guaranteed to be a little over a kilobyte at minimum.
    ///
    /// Not necessarily the maximum size of received datagrams.
    pub fn max_size(&self) -> Option<usize> {
        // We use the conservative overhead bound for any packet number, reducing the budget by at
        // most 3 bytes, so that PN size fluctuations don't cause users sending maximum-size
        // datagrams to suffer avoidable packet loss.
        let max_size = self.conn.path.current_mtu() as usize
            - self.conn.predict_1rtt_overhead(None)
            - Datagram::SIZE_BOUND;
        let limit = self
            .conn
            .peer_params
            .max_datagram_frame_size?
            .into_inner()
            .saturating_sub(Datagram::SIZE_BOUND as u64);
        Some(limit.min(max_size as u64) as usize)
    }

    /// Receive an unreliable, unordered datagram
    pub fn recv(&mut self) -> Option<Bytes> {
        self.conn.datagrams.recv()
    }

    /// Bytes available in the outgoing datagram buffer
    ///
    /// When greater than zero, [`send`](Self::send)ing a datagram of at most this size is
    /// guaranteed not to cause older datagrams to be dropped.
    pub fn send_buffer_space(&self) -> usize {
        self.conn
            .config
            .datagram_send_buffer_size
            .saturating_sub(self.conn.datagrams.outgoing_total)
    }
}

/// An outgoing datagram together with the time it entered the send buffer
///
/// The timestamp is what the AQM measures: queue latency is the sojourn time
/// of the head-of-line datagram, not the buffer's byte occupancy.
pub(super) struct QueuedDatagram {
    queued_at: Instant,
    datagram: Datagram,
}

impl QueuedDatagram {
    /// Wire size of the queued datagram's DATAGRAM frame
    pub(super) fn frame_size(&self, length_prefix: bool) -> usize {
        self.datagram.size(length_prefix)
    }
}

/// Head-drop CoDel controller state (RFC 8289) for the outgoing datagram queue
///
/// Distilled to the dequeue-time decision: the caller pops the head, asks
/// [`Self::should_drop`], and either transmits or discards it. Dropping starts
/// only after the sojourn time has stayed above target for a whole interval,
/// then escalates as `interval / sqrt(count)` until the queue drains below
/// target, which bounds queue latency at any link speed without rate tuning.
#[derive(Default)]
struct CodelState {
    /// When the sojourn time first rose above target, plus one interval
    first_above_time: Option<Instant>,
    /// Scheduled time of the next drop while in the dropping state
    drop_next: Option<Instant>,
    /// Drops in the current dropping episode (sets the escalation rate)
    count: u64,
    dropping: bool,
}

/// Backlog at or below which the AQM never drops (about two full datagrams)
///
/// RFC 8289's "one MTU" floor: a nearly-drained queue is proof the link is
/// keeping up, and dropping the last packets in flight would only starve it.
const CODEL_MIN_BACKLOG: usize = 3000;

impl CodelState {
    /// Dequeue-time decision for the queue head: `true` = drop it
    ///
    /// `sojourn` is how long the head waited in the buffer and `backlog` the
    /// queue's byte size before popping it.
    fn should_drop(
        &mut self,
        sojourn: Duration,
        backlog: usize,
        now: Instant,
        config: &DatagramAqmConfig,
    ) -> bool {
        if sojourn < config.target || backlog <= CODEL_MIN_BACKLOG {
            // Queue latency is under control: leave the dropping state.
            self.first_above_time = None;
            self.dropping = false;
            return false;
        }
        let first_above = *self.first_above_time.get_or_insert(now + config.interval);
        if self.dropping {
            match self.drop_next {
                Some(drop_next) if now >= drop_next => {
                    self.count += 1;
                    self.drop_next = Some(drop_next + Self::backoff(config.interval, self.count));
                    true
                }
                _ => false,
            }
        } else if now >= first_above {
            // Sojourn stayed above target for a full interval: start dropping.
            // Re-entering shortly after an episode resumes the previous drop
            // rate instead of restarting the slow ramp (RFC 8289 section 5.4:
            // a standing queue that comes right back is the same overload).
            self.count = match self.drop_next {
                Some(drop_next)
                    if now.saturating_duration_since(drop_next) < config.interval * 16 =>
                {
                    self.count.saturating_sub(2).max(1)
                }
                _ => 1,
            };
            self.dropping = true;
            self.drop_next = Some(now + Self::backoff(config.interval, self.count));
            true
        } else {
            false
        }
    }

    /// The drop-rate control law: drops accelerate as the square root of the
    /// episode's drop count, per RFC 8289.
    fn backoff(interval: Duration, count: u64) -> Duration {
        Duration::from_secs_f64(interval.as_secs_f64() / (count as f64).sqrt())
    }
}

#[derive(Default)]
pub(super) struct DatagramState {
    /// Number of bytes of datagrams that have been received by the local transport but not
    /// delivered to the application
    pub(super) recv_buffered: usize,
    pub(super) incoming: VecDeque<Datagram>,
    pub(super) outgoing: VecDeque<QueuedDatagram>,
    pub(super) outgoing_total: usize,
    pub(super) send_blocked: bool,
    codel: CodelState,
}

impl DatagramState {
    pub(super) fn received(
        &mut self,
        datagram: Datagram,
        window: &Option<usize>,
    ) -> Result<bool, TransportError> {
        let window = match window {
            None => {
                return Err(TransportError::PROTOCOL_VIOLATION(
                    "unexpected DATAGRAM frame",
                ));
            }
            Some(x) => *x,
        };

        if datagram.data.len() > window {
            return Err(TransportError::PROTOCOL_VIOLATION("oversized datagram"));
        }

        let was_empty = self.recv_buffered == 0;
        while datagram.data.len() + self.recv_buffered > window {
            debug!("dropping stale datagram");
            self.recv();
        }

        self.recv_buffered += datagram.data.len();
        self.incoming.push_back(datagram);
        Ok(was_empty)
    }

    /// Discard outgoing datagrams with a payload larger than `max_payload` bytes
    ///
    /// Used to ensure that reductions in MTU don't get us stuck in a state where we have a datagram
    /// queued but can't send it.
    pub(super) fn drop_oversized(&mut self, max_payload: usize) {
        let outgoing_total = &mut self.outgoing_total;
        self.outgoing.retain(|queued| {
            let result = queued.datagram.data.len() < max_payload;
            if !result {
                trace!(
                    "dropping {} byte datagram violating {} byte limit",
                    queued.datagram.data.len(),
                    max_payload
                );
                *outgoing_total -= queued.datagram.data.len();
            }
            result
        });
    }

    /// Attempt to write a datagram frame into `buf`, consuming it from `self.outgoing`
    ///
    /// Returns whether a frame was written. At most `max_size` bytes will be written, including
    /// framing.
    ///
    /// When `aqm` is configured, queue heads whose sojourn time keeps the
    /// queue above the latency target are dropped (and counted in `stats`)
    /// instead of transmitted, oldest first, per the CoDel control law.
    pub(super) fn write(
        &mut self,
        buf: &mut Vec<u8>,
        max_size: usize,
        now: Instant,
        aqm: Option<&DatagramAqmConfig>,
        stats: &mut DatagramTxStats,
    ) -> bool {
        loop {
            let queued = match self.outgoing.pop_front() {
                Some(x) => x,
                None => {
                    // An empty queue is by definition below target.
                    self.codel.first_above_time = None;
                    self.codel.dropping = false;
                    return false;
                }
            };

            if let Some(config) = aqm {
                let sojourn = now.saturating_duration_since(queued.queued_at);
                if self
                    .codel
                    .should_drop(sojourn, self.outgoing_total, now, config)
                {
                    trace!(
                        len = queued.datagram.data.len(),
                        sojourn_ms = sojourn.as_millis() as u64,
                        "AQM dropping outgoing datagram"
                    );
                    self.outgoing_total -= queued.datagram.data.len();
                    stats.dropped_aqm += 1;
                    continue;
                }
            }

            let datagram = queued.datagram;
            if buf.len() + datagram.size(true) > max_size {
                // Future work: we could be more clever about cramming small datagrams into
                // mostly-full packets when a larger one is queued first
                self.outgoing.push_front(QueuedDatagram {
                    queued_at: queued.queued_at,
                    datagram,
                });
                return false;
            }

            trace!(len = datagram.data.len(), "DATAGRAM");

            self.outgoing_total -= datagram.data.len();
            datagram.encode(true, buf);
            return true;
        }
    }

    pub(super) fn recv(&mut self) -> Option<Bytes> {
        let x = self.incoming.pop_front()?.data;
        self.recv_buffered -= x.len();
        Some(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aqm() -> DatagramAqmConfig {
        DatagramAqmConfig::default()
    }

    /// A backlog comfortably above the CoDel floor.
    const BACKLOG: usize = 64 * 1024;

    #[test]
    fn codel_never_drops_below_target() {
        let mut codel = CodelState::default();
        let config = aqm();
        let mut now = Instant::now();
        for _ in 0..1000 {
            assert!(!codel.should_drop(config.target / 2, BACKLOG, now, &config));
            now += Duration::from_millis(1);
        }
        assert!(!codel.dropping);
    }

    #[test]
    fn codel_tolerates_bursts_shorter_than_interval() {
        let mut codel = CodelState::default();
        let config = aqm();
        let mut now = Instant::now();
        // Sojourn above target, but only for half an interval, then recovery.
        for _ in 0..5 {
            for _ in 0..10 {
                assert!(!codel.should_drop(config.target * 2, BACKLOG, now, &config));
                now += config.interval / 20;
            }
            assert!(!codel.should_drop(config.target / 2, BACKLOG, now, &config));
        }
    }

    #[test]
    fn codel_drops_after_sustained_standing_queue_then_escalates() {
        let mut codel = CodelState::default();
        let config = aqm();
        let mut now = Instant::now();
        let mut drops = 0u32;
        let mut first_drop_at = None;
        let start = now;
        // A persistent standing queue: sojourn pinned above target.
        for _ in 0..400 {
            if codel.should_drop(config.target * 4, BACKLOG, now, &config) {
                drops += 1;
                first_drop_at.get_or_insert(now);
            }
            now += Duration::from_millis(5);
        }
        let first = first_drop_at.expect("a standing queue must eventually drop");
        assert!(
            first.saturating_duration_since(start) >= config.interval,
            "the first drop must wait out a full interval"
        );
        assert!(
            drops > 5,
            "the drop rate must escalate while the queue stands, got {drops}"
        );
        assert!(codel.dropping);
    }

    #[test]
    fn codel_stops_dropping_once_queue_drains() {
        let mut codel = CodelState::default();
        let config = aqm();
        let mut now = Instant::now();
        for _ in 0..400 {
            codel.should_drop(config.target * 4, BACKLOG, now, &config);
            now += Duration::from_millis(5);
        }
        assert!(codel.dropping);
        assert!(!codel.should_drop(config.target / 4, BACKLOG, now, &config));
        assert!(!codel.dropping, "sojourn below target must end the episode");
    }

    #[test]
    fn codel_spares_a_nearly_empty_queue() {
        let mut codel = CodelState::default();
        let config = aqm();
        let mut now = Instant::now();
        // Sojourn far above target but almost nothing queued: the link is
        // keeping up, dropping would only starve it.
        for _ in 0..400 {
            assert!(!codel.should_drop(config.target * 10, CODEL_MIN_BACKLOG, now, &config));
            now += Duration::from_millis(5);
        }
    }

    #[test]
    fn write_head_drops_stale_datagrams_and_counts_them() {
        let mut state = DatagramState::default();
        let config = aqm();
        let mut stats = DatagramTxStats::default();
        let start = Instant::now();
        // A queue whose head has been waiting far beyond target for well over
        // an interval, deep enough to stay above the backlog floor.
        let payload = Bytes::from_static(&[0u8; 1000]);
        for _ in 0..64 {
            state.outgoing.push_back(QueuedDatagram {
                queued_at: start,
                datagram: Datagram {
                    data: payload.clone(),
                },
            });
            state.outgoing_total += payload.len();
        }
        let now = start + Duration::from_millis(500);
        // Prime the controller past first_above_time, as a live queue would
        // have done on earlier dequeues.
        let mut buf = Vec::new();
        assert!(state.write(&mut buf, usize::MAX, now, Some(&config), &mut stats));
        let later = now + Duration::from_millis(200);
        let mut buf = Vec::new();
        assert!(state.write(&mut buf, usize::MAX, later, Some(&config), &mut stats));
        assert!(
            stats.dropped_aqm > 0,
            "stale heads must be AQM-dropped before delivery"
        );
        let queued_bytes: usize = state.outgoing.iter().map(|d| d.datagram.data.len()).sum();
        assert_eq!(
            state.outgoing_total, queued_bytes,
            "outgoing_total must stay in sync through AQM drops"
        );
    }

    #[test]
    fn write_without_aqm_never_drops() {
        let mut state = DatagramState::default();
        let mut stats = DatagramTxStats::default();
        let start = Instant::now();
        let payload = Bytes::from_static(&[0u8; 1000]);
        for _ in 0..64 {
            state.outgoing.push_back(QueuedDatagram {
                queued_at: start,
                datagram: Datagram {
                    data: payload.clone(),
                },
            });
            state.outgoing_total += payload.len();
        }
        let now = start + Duration::from_secs(10);
        let mut delivered = 0;
        let mut buf = Vec::new();
        while state.write(&mut buf, usize::MAX, now, None, &mut stats) {
            delivered += 1;
        }
        assert_eq!(delivered, 64);
        assert_eq!(stats.dropped_aqm, 0);
    }
}

/// Errors that can arise when sending a datagram
#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SendDatagramError {
    /// The peer does not support receiving datagram frames
    #[error("datagrams not supported by peer")]
    UnsupportedByPeer,
    /// Datagram support is disabled locally
    #[error("datagram support disabled")]
    Disabled,
    /// The datagram is larger than the connection can currently accommodate
    ///
    /// Indicates that the path MTU minus overhead or the limit advertised by the peer has been
    /// exceeded.
    #[error("datagram too large")]
    TooLarge,
    /// Send would block
    #[error("datagram send blocked")]
    Blocked(Bytes),
}
