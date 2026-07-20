use std::collections::VecDeque;

use bytes::Bytes;
use rustc_hash::FxHashMap;
use thiserror::Error;
use tracing::{debug, trace};

use super::Connection;
use crate::config::{DatagramAqmConfig, DatagramBdpBufferConfig};
use crate::connection::stats::DatagramTxStats;
use crate::{
    Duration, Instant, TransportError,
    frame::{Datagram, FrameStruct},
};

/// ECN codepoint carried by a tunnelled inner IP packet
///
/// Distinct from [`crate::EcnCodepoint`], which marks the OUTER UDP socket:
/// this classifies the traffic a tunnel is about to seal into datagrams, so
/// the Not-ECT case is first-class (it is the distribution's denominator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramEcn {
    /// Not ECN-capable transport (`0b00`)
    NotEct,
    /// ECN-capable transport, ECT(0) (`0b10`)
    Ect0,
    /// ECN-capable transport, ECT(1) (`0b01`)
    Ect1,
    /// Congestion experienced (`0b11`)
    Ce,
}

impl DatagramEcn {
    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b01 => Self::Ect1,
            0b10 => Self::Ect0,
            0b11 => Self::Ce,
            _ => Self::NotEct,
        }
    }
}

/// Caller-supplied classification of an application datagram
///
/// A tunnel encrypts its payload before handing it to QUIC, so the inner IP
/// header is unreadable at this layer; the caller classifies while it still
/// holds the plaintext (see [`Self::of_inner_ip_packet`]) and passes the
/// result to `send_datagram_classified`. `Default` is fully unclassified:
/// no ECN accounting and the shared catch-all flow bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatagramClass {
    /// Stable flow key (e.g. a 5-tuple hash) for per-flow queueing
    pub flow: Option<u64>,
    /// ECN codepoint of the inner packet's IP header
    pub ecn: Option<DatagramEcn>,
}

impl DatagramClass {
    /// Classify a plaintext IP packet: the ECN codepoint from the IPv4 TOS /
    /// IPv6 traffic-class field, and a flow key from an FNV-1a hash of the
    /// TCP/UDP 5-tuple.
    ///
    /// The ECN codepoint is read for any well-formed IPv4/IPv6 header; the
    /// flow key only for first-fragment TCP/UDP packets with readable ports
    /// (other protocols and IPv6 extension headers fall back to `None`, the
    /// shared bucket). Non-IP payloads yield the unclassified default.
    #[must_use]
    pub fn of_inner_ip_packet(pkt: &[u8]) -> Self {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x100_0000_01b3;
        let fnv = |chunks: &[&[u8]]| {
            let mut h = FNV_OFFSET;
            for chunk in chunks {
                for &b in *chunk {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(FNV_PRIME);
                }
            }
            h
        };
        let Some(&first) = pkt.first() else {
            return Self::default();
        };
        match first >> 4 {
            4 if pkt.len() >= 20 => {
                let ecn = Some(DatagramEcn::from_bits(pkt[1]));
                let ihl = usize::from(first & 0x0f) * 4;
                let proto = pkt[9];
                // Ports only exist in the first fragment (offset 0).
                let frag_offset = (u16::from(pkt[6] & 0x1f) << 8) | u16::from(pkt[7]);
                let flow = ((proto == 6 || proto == 17)
                    && frag_offset == 0
                    && ihl >= 20
                    && pkt.len() >= ihl + 4)
                    .then(|| fnv(&[&[proto], &pkt[12..20], &pkt[ihl..ihl + 4]]));
                Self { flow, ecn }
            }
            6 if pkt.len() >= 40 => {
                let tc = ((pkt[0] & 0x0f) << 4) | (pkt[1] >> 4);
                let ecn = Some(DatagramEcn::from_bits(tc));
                let next = pkt[6];
                let flow = ((next == 6 || next == 17) && pkt.len() >= 44)
                    .then(|| fnv(&[&[next], &pkt[8..40], &pkt[40..44]]));
                Self { flow, ecn }
            }
            _ => Self::default(),
        }
    }
}

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
        self.send_classified(data, drop, now, DatagramClass::default())
    }

    /// [`Self::send`] with a caller-supplied [`DatagramClass`]
    ///
    /// The classification feeds the inner-ECN distribution counters in
    /// `ConnectionStats::datagram_tx` and keys per-flow queueing when the
    /// AQM runs with more than one flow queue.
    pub fn send_classified(
        &mut self,
        data: Bytes,
        drop: bool,
        now: Instant,
        class: DatagramClass,
    ) -> Result<(), SendDatagramError> {
        if self.conn.config.datagram_receive_buffer_size.is_none() {
            return Err(SendDatagramError::Disabled);
        }
        let max = self
            .max_size()
            .ok_or(SendDatagramError::UnsupportedByPeer)?;
        if data.len() > max {
            return Err(SendDatagramError::TooLarge);
        }
        let limit = self.effective_send_buffer_size();
        if drop {
            while self.conn.datagrams.outgoing_total > limit {
                let len = self
                    .conn
                    .datagrams
                    .evict_from_fattest_flow()
                    .expect("datagrams.outgoing_total desynchronized");
                trace!(len, "dropping outgoing datagram");
                self.conn.stats.datagram_tx.dropped_overflow += 1;
            }
        } else if self.conn.datagrams.outgoing_total + data.len() > limit {
            self.conn.datagrams.send_blocked = true;
            return Err(SendDatagramError::Blocked(data));
        }
        self.conn.stats.datagram_tx.record_ecn(class.ecn);
        let bucket = bucket_for(class.flow, self.conn.config.datagram_send_aqm.as_ref());
        self.conn.datagrams.enqueue(
            bucket,
            QueuedDatagram {
                queued_at: now,
                datagram: Datagram { data },
            },
        );
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
        let limit = match (
            &self.conn.config.datagram_send_buffer_bdp,
            self.conn.datagrams.bdp_ewma,
        ) {
            (Some(config), Some(bdp)) => {
                adaptive_send_buffer_limit(self.conn.config.datagram_send_buffer_size, config, bdp)
            }
            _ => self.conn.config.datagram_send_buffer_size,
        };
        limit.saturating_sub(self.conn.datagrams.outgoing_total)
    }

    /// Effective send-buffer byte limit for this enqueue: the fixed
    /// configured size, shrunk toward the path's measured BDP when adaptive
    /// sizing is on and the congestion controller has an estimate
    fn effective_send_buffer_size(&mut self) -> usize {
        let configured = self.conn.config.datagram_send_buffer_size;
        let Some(config) = &self.conn.config.datagram_send_buffer_bdp else {
            return configured;
        };
        let Some(sample) = self.conn.path.congestion.bdp_estimate() else {
            return configured;
        };
        let bdp = self.conn.datagrams.update_bdp_ewma(sample);
        adaptive_send_buffer_limit(configured, config, bdp)
    }
}

/// `clamp(multiple x bdp, floor, configured)`, with the configured cap
/// winning over a larger floor (adaptation may only ever SHRINK the buffer)
fn adaptive_send_buffer_limit(
    configured: usize,
    config: &DatagramBdpBufferConfig,
    bdp: u64,
) -> usize {
    let target = (bdp as f64 * config.multiple) as usize;
    target.clamp(config.floor.min(configured), configured)
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

/// The catch-all bucket: unclassified datagrams, and every datagram when
/// per-flow queueing is off (no AQM, or `flow_queues <= 1`).
const SHARED_BUCKET: u32 = 0;

/// DRR byte credit per scheduling round (RFC 8290 section 4.2 quantum)
///
/// Deliberately ~12 full-size datagrams rather than the classic one-MTU
/// quantum: a per-packet round-robin shreds the per-flow packet trains that
/// GSO batching and receiver-side GRO coalescing depend on, which measurably
/// costs clean-path throughput at high rates (13-20% in the fork.11 A/B).
/// Packet-train turns keep that batching; the sparse-flow latency cost is
/// bounded by quantum/line_rate (3 ms at 40 Mbit, microseconds at 1 Gbps)
/// and fresh sparse flows still preempt via the new-flow priority list.
const FQ_QUANTUM: i64 = 15_000;

/// Maps a caller-supplied flow key onto a queue bucket. Classified flows
/// spread over `1..=flow_queues`; [`SHARED_BUCKET`] stays reserved so cover
/// traffic and unclassifiable packets never collide with a hashed flow.
fn bucket_for(flow: Option<u64>, aqm: Option<&DatagramAqmConfig>) -> u32 {
    match (flow, aqm) {
        (Some(f), Some(config)) if config.flow_queues > 1 => {
            1 + (f % config.flow_queues as u64) as u32
        }
        _ => SHARED_BUCKET,
    }
}

/// One flow's FIFO plus its scheduler and AQM state
///
/// Lives only while the flow has datagrams queued (or is finishing its DRR
/// round): state is proportional to ACTIVE flows, not to the bucket space.
struct FlowQueue {
    queue: VecDeque<QueuedDatagram>,
    /// Queued payload bytes (this queue's share of `outgoing_total`)
    bytes: usize,
    /// DRR byte credit; a flow only dequeues while positive
    deficit: i64,
    codel: CodelState,
}

impl FlowQueue {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            deficit: FQ_QUANTUM,
            codel: CodelState::default(),
        }
    }
}

/// Which DRR list the scheduler is currently serving from
#[derive(Clone, Copy, PartialEq, Eq)]
enum FlowList {
    New,
    Old,
}

#[derive(Default)]
pub(super) struct DatagramState {
    /// Number of bytes of datagrams that have been received by the local transport but not
    /// delivered to the application
    pub(super) recv_buffered: usize,
    pub(super) incoming: VecDeque<Datagram>,
    pub(super) outgoing_total: usize,
    pub(super) send_blocked: bool,
    /// Active flow queues by bucket. With classification off this holds at
    /// most [`SHARED_BUCKET`], which reduces to the historic single FIFO.
    flows: FxHashMap<u32, FlowQueue>,
    /// Flows that became active since last served: they get scheduling
    /// priority (RFC 8290), which is what shields a sparse flow from a
    /// standing bulk queue.
    new_flows: VecDeque<u32>,
    /// Flows being served round-robin
    old_flows: VecDeque<u32>,
    /// Smoothed BDP estimate driving adaptive send-buffer sizing
    bdp_ewma: Option<u64>,
}

impl DatagramState {
    /// Fold a fresh BDP sample into the smoothed estimate (EWMA, alpha 1/8)
    /// and return it
    ///
    /// The controller's bandwidth/rtt filters are already windowed, so this
    /// only damps filter-rotation steps; per-enqueue updates converge within
    /// a few packets of any sustained change.
    pub(super) fn update_bdp_ewma(&mut self, sample: u64) -> u64 {
        let next = match self.bdp_ewma {
            None => sample,
            Some(prev) => (prev as i128 + (sample as i128 - prev as i128) / 8) as u64,
        };
        self.bdp_ewma = Some(next);
        next
    }
    /// Queue a datagram on its flow bucket, registering a fresh bucket with
    /// the DRR scheduler
    pub(super) fn enqueue(&mut self, bucket: u32, datagram: QueuedDatagram) {
        self.outgoing_total += datagram.datagram.data.len();
        match self.flows.get_mut(&bucket) {
            Some(flow) => {
                flow.bytes += datagram.datagram.data.len();
                flow.queue.push_back(datagram);
            }
            None => {
                let mut flow = FlowQueue::new();
                flow.bytes = datagram.datagram.data.len();
                flow.queue.push_back(datagram);
                self.flows.insert(bucket, flow);
                self.new_flows.push_back(bucket);
            }
        }
    }

    /// Drop the head of the flow with the largest backlog (RFC 8290's
    /// overlimit behavior), returning its payload length
    ///
    /// With a single active flow this is exactly the historic oldest-first
    /// eviction; with several it protects sparse flows from a flooder that
    /// overruns the shared buffer.
    pub(super) fn evict_from_fattest_flow(&mut self) -> Option<usize> {
        let bucket = self
            .flows
            .iter()
            .filter(|(_, q)| !q.queue.is_empty())
            .max_by_key(|(_, q)| q.bytes)
            .map(|(&b, _)| b)?;
        let flow = self.flows.get_mut(&bucket)?;
        let prev = flow.queue.pop_front()?;
        let len = prev.datagram.data.len();
        flow.bytes -= len;
        self.outgoing_total -= len;
        Some(len)
    }

    /// Whether no outgoing datagram is queued on any flow
    pub(super) fn is_empty(&self) -> bool {
        self.flows.values().all(|q| q.queue.is_empty())
    }

    /// Whether some queued datagram's frame would fit in `max_size` bytes
    pub(super) fn can_write(&self, max_size: usize) -> bool {
        self.flows.values().any(|q| {
            q.queue
                .front()
                .is_some_and(|d| d.frame_size(true) <= max_size)
        })
    }
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
        for flow in self.flows.values_mut() {
            let bytes = &mut flow.bytes;
            flow.queue.retain(|queued| {
                let result = queued.datagram.data.len() < max_payload;
                if !result {
                    trace!(
                        "dropping {} byte datagram violating {} byte limit",
                        queued.datagram.data.len(),
                        max_payload
                    );
                    *outgoing_total -= queued.datagram.data.len();
                    *bytes -= queued.datagram.data.len();
                }
                result
            });
        }
    }

    /// Attempt to write a datagram frame into `buf`, consuming it from the flow queues
    ///
    /// Returns whether a frame was written. At most `max_size` bytes will be written, including
    /// framing.
    ///
    /// Flows are served DRR round-robin with new-flow priority (RFC 8290);
    /// when `aqm` is configured, each flow runs its own CoDel: heads whose
    /// sojourn time keeps the queue above the latency target are dropped
    /// (and counted in `stats`) instead of transmitted. A drained flow is
    /// unregistered, so an idle connection carries no per-flow state.
    pub(super) fn write(
        &mut self,
        buf: &mut Vec<u8>,
        max_size: usize,
        now: Instant,
        aqm: Option<&DatagramAqmConfig>,
        stats: &mut DatagramTxStats,
    ) -> bool {
        loop {
            let (list, bucket) = match (self.new_flows.front(), self.old_flows.front()) {
                (Some(&b), _) => (FlowList::New, b),
                (None, Some(&b)) => (FlowList::Old, b),
                (None, None) => return false,
            };
            let flow = self
                .flows
                .get_mut(&bucket)
                .expect("scheduled flow bucket must exist");

            if flow.deficit <= 0 {
                // Out of credit: refill and move to the back of the old
                // list; some other flow gets this turn.
                flow.deficit += FQ_QUANTUM;
                match list {
                    FlowList::New => self.new_flows.pop_front(),
                    FlowList::Old => self.old_flows.pop_front(),
                };
                self.old_flows.push_back(bucket);
                continue;
            }

            let queued = match flow.queue.pop_front() {
                Some(x) => x,
                None => {
                    // Drained flow: a new-list flow gets one final round on
                    // the old list (RFC 8290: prevents cycling through the
                    // priority list), an old-list flow is unregistered.
                    match list {
                        FlowList::New => {
                            self.new_flows.pop_front();
                            self.old_flows.push_back(bucket);
                        }
                        FlowList::Old => {
                            self.old_flows.pop_front();
                            self.flows.remove(&bucket);
                        }
                    }
                    continue;
                }
            };

            if let Some(config) = aqm {
                let sojourn = now.saturating_duration_since(queued.queued_at);
                if flow
                    .codel
                    .should_drop(sojourn, self.outgoing_total, now, config)
                {
                    trace!(
                        len = queued.datagram.data.len(),
                        sojourn_ms = sojourn.as_millis() as u64,
                        "AQM dropping outgoing datagram"
                    );
                    self.outgoing_total -= queued.datagram.data.len();
                    flow.bytes -= queued.datagram.data.len();
                    stats.dropped_aqm += 1;
                    continue;
                }
            }

            let size = queued.frame_size(true);
            if buf.len() + size > max_size {
                // Future work: we could be more clever about cramming small datagrams into
                // mostly-full packets when a larger one is queued first
                flow.queue.push_front(queued);
                return false;
            }

            let datagram = queued.datagram;
            trace!(len = datagram.data.len(), "DATAGRAM");

            self.outgoing_total -= datagram.data.len();
            flow.bytes -= datagram.data.len();
            flow.deficit -= size as i64;
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

    /// Enqueue a `len`-byte payload whose bytes tag its flow, on `bucket`,
    /// at `queued_at`.
    fn push(state: &mut DatagramState, bucket: u32, tag: u8, len: usize, queued_at: Instant) {
        let data = Bytes::from(vec![tag; len]);
        state.enqueue(
            bucket,
            QueuedDatagram {
                queued_at,
                datagram: Datagram { data },
            },
        );
    }

    /// Total bytes queued across every flow, recomputed from scratch.
    fn recount(state: &DatagramState) -> usize {
        state
            .flows
            .values()
            .flat_map(|q| q.queue.iter())
            .map(|d| d.datagram.data.len())
            .sum()
    }

    #[test]
    fn write_head_drops_stale_datagrams_and_counts_them() {
        let mut state = DatagramState::default();
        let config = aqm();
        let mut stats = DatagramTxStats::default();
        let start = Instant::now();
        // A queue whose head has been waiting far beyond target for well over
        // an interval, deep enough to stay above the backlog floor.
        for _ in 0..64 {
            push(&mut state, SHARED_BUCKET, 0, 1000, start);
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
        assert_eq!(
            state.outgoing_total,
            recount(&state),
            "outgoing_total must stay in sync through AQM drops"
        );
    }

    /// Minimal IPv4 packet: 20-byte header + 4 port bytes.
    fn ipv4_pkt(tos: u8, proto: u8) -> Vec<u8> {
        let mut pkt = vec![0u8; 24];
        pkt[0] = 0x45;
        pkt[1] = tos;
        pkt[9] = proto;
        pkt[12..16].copy_from_slice(&[10, 0, 0, 1]);
        pkt[16..20].copy_from_slice(&[10, 0, 0, 2]);
        pkt[20..22].copy_from_slice(&1234u16.to_be_bytes());
        pkt[22..24].copy_from_slice(&443u16.to_be_bytes());
        pkt
    }

    /// Minimal IPv6 packet: 40-byte header + 4 port bytes.
    fn ipv6_pkt(traffic_class: u8, next_header: u8) -> Vec<u8> {
        let mut pkt = vec![0u8; 44];
        pkt[0] = 0x60 | (traffic_class >> 4);
        pkt[1] = (traffic_class & 0x0f) << 4;
        pkt[6] = next_header;
        pkt[8..24].copy_from_slice(&[0xfd; 16]);
        pkt[24..40].copy_from_slice(&[0xfe; 16]);
        pkt[40..42].copy_from_slice(&1234u16.to_be_bytes());
        pkt[42..44].copy_from_slice(&80u16.to_be_bytes());
        pkt
    }

    #[test]
    fn classify_ipv4_ecn_codepoints() {
        // RFC 3168: 0b00 Not-ECT, 0b01 ECT(1), 0b10 ECT(0), 0b11 CE. The
        // DSCP bits above them must not leak into the classification.
        for (tos, expected) in [
            (0b0000_0000, DatagramEcn::NotEct),
            (0b0000_0001, DatagramEcn::Ect1),
            (0b0000_0010, DatagramEcn::Ect0),
            (0b0000_0011, DatagramEcn::Ce),
            (0b1010_1000, DatagramEcn::NotEct),
            (0b1010_1010, DatagramEcn::Ect0),
        ] {
            let class = DatagramClass::of_inner_ip_packet(&ipv4_pkt(tos, 6));
            assert_eq!(class.ecn, Some(expected), "tos {tos:#010b}");
        }
    }

    #[test]
    fn classify_ipv6_ecn_spans_the_nibble_boundary() {
        // The IPv6 traffic class straddles bytes 0 and 1; the ECN bits are
        // its two low bits, which live in byte 1's high nibble.
        for (tc, expected) in [
            (0b0000_0000, DatagramEcn::NotEct),
            (0b0000_0001, DatagramEcn::Ect1),
            (0b0000_0010, DatagramEcn::Ect0),
            (0b0000_0011, DatagramEcn::Ce),
            (0b1011_1011, DatagramEcn::Ce),
        ] {
            let class = DatagramClass::of_inner_ip_packet(&ipv6_pkt(tc, 17));
            assert_eq!(class.ecn, Some(expected), "traffic class {tc:#010b}");
        }
    }

    #[test]
    fn classify_non_ip_payload_is_unclassified() {
        // Cover datagrams (0xFF fill), sealed frames, and truncated headers
        // carry no inner IP header: they must count in no ECN bucket and land
        // in the shared flow bucket.
        for payload in [&[][..], &[0xff; 64][..], &[0x45; 12][..], &[0x60; 39][..]] {
            assert_eq!(
                DatagramClass::of_inner_ip_packet(payload),
                DatagramClass::default()
            );
        }
    }

    #[test]
    fn classify_flow_key_for_tcp_udp_only() {
        let tcp = DatagramClass::of_inner_ip_packet(&ipv4_pkt(0, 6));
        let udp = DatagramClass::of_inner_ip_packet(&ipv4_pkt(0, 17));
        let icmp = DatagramClass::of_inner_ip_packet(&ipv4_pkt(0, 1));
        assert!(tcp.flow.is_some());
        assert!(udp.flow.is_some());
        assert_ne!(tcp.flow, udp.flow, "protocol is part of the 5-tuple");
        assert!(icmp.flow.is_none(), "ICMP has no ports: shared bucket");
        assert_eq!(icmp.ecn, Some(DatagramEcn::NotEct), "ECN still classified");

        let v6 = DatagramClass::of_inner_ip_packet(&ipv6_pkt(0, 6));
        assert!(v6.flow.is_some());
        assert_ne!(v6.flow, tcp.flow);
    }

    #[test]
    fn classify_flow_key_is_stable_per_flow() {
        let a = DatagramClass::of_inner_ip_packet(&ipv4_pkt(0, 6));
        let mut longer = ipv4_pkt(0, 6);
        longer.extend_from_slice(&[0xab; 100]);
        // Payload bytes past the 5-tuple must not change the key; the ECN
        // bits must not either (a CE remark mid-flow keeps the flow sticky).
        let b = DatagramClass::of_inner_ip_packet(&longer);
        let c = DatagramClass::of_inner_ip_packet(&ipv4_pkt(0b11, 6));
        assert_eq!(a.flow, b.flow);
        assert_eq!(a.flow, c.flow);
    }

    #[test]
    fn classify_ipv4_non_first_fragment_has_no_flow_key() {
        // A non-first fragment carries payload bytes where the ports would
        // be; hashing them would scatter one flow across buckets.
        let mut pkt = ipv4_pkt(0, 6);
        pkt[6] = 0x00;
        pkt[7] = 0x08; // fragment offset 8
        let class = DatagramClass::of_inner_ip_packet(&pkt);
        assert!(class.flow.is_none());
        assert_eq!(class.ecn, Some(DatagramEcn::NotEct));
    }

    #[test]
    fn record_ecn_counts_classified_only() {
        let mut stats = DatagramTxStats::default();
        stats.record_ecn(None);
        stats.record_ecn(Some(DatagramEcn::NotEct));
        stats.record_ecn(Some(DatagramEcn::Ect0));
        stats.record_ecn(Some(DatagramEcn::Ect0));
        stats.record_ecn(Some(DatagramEcn::Ect1));
        stats.record_ecn(Some(DatagramEcn::Ce));
        assert_eq!(stats.ecn_not_ect, 1);
        assert_eq!(stats.ecn_ect0, 2);
        assert_eq!(stats.ecn_ect1, 1);
        assert_eq!(stats.ecn_ce, 1);
        assert_eq!(
            stats.ecn_not_ect + stats.ecn_ect0 + stats.ecn_ect1 + stats.ecn_ce,
            5,
            "unclassified datagrams must not be counted anywhere"
        );
    }

    #[test]
    fn write_without_aqm_never_drops() {
        let mut state = DatagramState::default();
        let mut stats = DatagramTxStats::default();
        let start = Instant::now();
        for _ in 0..64 {
            push(&mut state, SHARED_BUCKET, 0, 1000, start);
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

    #[test]
    fn single_bucket_preserves_fifo_order() {
        // The single-queue fallback (flow_queues = 1, or unclassified
        // traffic) must deliver in exact enqueue order: the DRR machinery
        // degenerates to the historic FIFO.
        let mut state = DatagramState::default();
        let mut stats = DatagramTxStats::default();
        let now = Instant::now();
        for tag in 0..32u8 {
            push(&mut state, SHARED_BUCKET, tag, 1000, now);
        }
        let mut order = Vec::new();
        loop {
            let mut buf = Vec::new();
            if !state.write(&mut buf, usize::MAX, now, Some(&aqm()), &mut stats) {
                break;
            }
            // The payload sits at the end of the encoded frame; every one of
            // its bytes is the tag.
            order.push(*buf.last().unwrap());
        }
        assert_eq!(order, (0..32u8).collect::<Vec<_>>());
        assert_eq!(stats.dropped_aqm, 0, "a fresh queue must not drop");
        assert!(state.is_empty());
        assert_eq!(state.outgoing_total, 0);
    }

    #[test]
    fn bucket_for_reserves_the_shared_bucket() {
        let config = aqm();
        // Classified flows never land on the shared bucket, whatever the key.
        for f in 0..2048u64 {
            assert_ne!(bucket_for(Some(f), Some(&config)), SHARED_BUCKET);
        }
        // Unclassified always does, as does everything without AQM or with
        // per-flow queueing collapsed to one queue.
        assert_eq!(bucket_for(None, Some(&config)), SHARED_BUCKET);
        assert_eq!(bucket_for(Some(42), None), SHARED_BUCKET);
        let mut single = aqm();
        single.flow_queues(1);
        assert_eq!(bucket_for(Some(42), Some(&single)), SHARED_BUCKET);
        let mut zero = aqm();
        zero.flow_queues(0);
        assert_eq!(
            bucket_for(Some(42), Some(&zero)),
            SHARED_BUCKET,
            "flow_queues(0) must clamp to the single-queue fallback"
        );
    }

    #[test]
    fn sparse_flow_is_not_starved_or_dropped_by_a_flooding_flow() {
        // A bulk flow with a standing queue deep in CoDel's dropping state
        // and a sparse flow with one fresh packet: the sparse packet must go
        // out promptly (new-flow priority) and must never be CoDel-dropped
        // (its own sojourn is below target even though the bulk queue is way
        // above).
        let mut state = DatagramState::default();
        let config = aqm();
        let mut stats = DatagramTxStats::default();
        let start = Instant::now();
        for _ in 0..512 {
            push(&mut state, 1, 0xbb, 1200, start);
        }
        // Serve the bulk queue with time advancing so its CoDel walks
        // through first_above_time into the dropping state, and past its
        // FIRST DRR quantum so it has rotated onto the old-flows list (a
        // flow only holds new-list priority for one quantum after birth).
        let mut now = start + Duration::from_millis(400);
        for _ in 0..16 {
            let mut buf = Vec::new();
            state.write(&mut buf, usize::MAX, now, Some(&config), &mut stats);
            now += Duration::from_millis(50);
        }
        assert!(
            stats.dropped_aqm > 0,
            "bulk standing queue must be dropping"
        );

        // The sparse flow arrives now, on its own bucket.
        let later = now + Duration::from_millis(10);
        push(&mut state, 2, 0x55, 100, later);
        let dropped_before = stats.dropped_aqm;
        let mut buf = Vec::new();
        assert!(state.write(&mut buf, usize::MAX, later, Some(&config), &mut stats));
        assert_eq!(
            *buf.last().unwrap(),
            0x55,
            "the fresh sparse packet must be scheduled before the bulk backlog"
        );
        assert_eq!(
            stats.dropped_aqm, dropped_before,
            "serving the sparse flow must not drop anything"
        );
        assert_eq!(state.outgoing_total, recount(&state));
    }

    #[test]
    fn drr_shares_bandwidth_between_two_bulk_flows() {
        // Two backlogged flows with very different packet sizes: the DRR
        // quantum must interleave service byte-fairly instead of draining
        // one flow before touching the other.
        let mut state = DatagramState::default();
        let mut stats = DatagramTxStats::default();
        let now = Instant::now();
        for _ in 0..64 {
            push(&mut state, 1, 0xaa, 1200, now);
        }
        for _ in 0..192 {
            push(&mut state, 2, 0xcc, 400, now);
        }
        let (mut a_bytes, mut c_bytes) = (0usize, 0usize);
        let mut both_served_at_quarter = false;
        let mut served = 0;
        loop {
            let mut buf = Vec::new();
            if !state.write(&mut buf, usize::MAX, now, None, &mut stats) {
                break;
            }
            served += 1;
            match *buf.last().unwrap() {
                0xaa => a_bytes += 1200,
                0xcc => c_bytes += 400,
                other => panic!("unexpected tag {other}"),
            }
            if served == 64 {
                both_served_at_quarter = a_bytes.min(c_bytes) > 0;
            }
        }
        assert_eq!(a_bytes, 64 * 1200);
        assert_eq!(c_bytes, 192 * 400);
        assert!(
            both_served_at_quarter,
            "DRR must interleave flows, not drain them serially"
        );
    }

    #[test]
    fn overflow_eviction_hits_the_fattest_flow() {
        let mut state = DatagramState::default();
        let now = Instant::now();
        push(&mut state, 1, 0xbb, 1200, now);
        push(&mut state, 1, 0xbb, 1200, now);
        push(&mut state, 2, 0x55, 100, now);
        let evicted = state.evict_from_fattest_flow().unwrap();
        assert_eq!(evicted, 1200, "the bulk flow must pay for the overflow");
        assert_eq!(state.outgoing_total, recount(&state));
        // Draining continues from the fattest until nothing is left.
        assert_eq!(state.evict_from_fattest_flow(), Some(1200));
        assert_eq!(state.evict_from_fattest_flow(), Some(100));
        assert_eq!(state.evict_from_fattest_flow(), None);
        assert_eq!(state.outgoing_total, 0);
    }

    #[test]
    fn drained_flows_are_unregistered() {
        // Per-flow state must not accumulate: after a drain the flow map is
        // empty again (the write scheduler removes exhausted flows).
        let mut state = DatagramState::default();
        let mut stats = DatagramTxStats::default();
        let now = Instant::now();
        for bucket in 1..=16u32 {
            push(&mut state, bucket, bucket as u8, 500, now);
        }
        let mut buf = Vec::new();
        while state.write(&mut buf, usize::MAX, now, Some(&aqm()), &mut stats) {
            buf.clear();
        }
        assert!(state.is_empty());
        assert!(
            state.flows.is_empty(),
            "drained flow queues must be removed, not leak per-flow state"
        );
        assert!(state.new_flows.is_empty() && state.old_flows.is_empty());
    }

    #[test]
    fn drop_oversized_prunes_every_flow() {
        let mut state = DatagramState::default();
        let now = Instant::now();
        push(&mut state, 1, 0xaa, 1400, now);
        push(&mut state, 1, 0xaa, 200, now);
        push(&mut state, 2, 0xcc, 1400, now);
        state.drop_oversized(1000);
        assert_eq!(state.outgoing_total, 200);
        assert_eq!(state.outgoing_total, recount(&state));
    }

    #[test]
    fn adaptive_limit_clamps_between_floor_and_configured() {
        let config = DatagramBdpBufferConfig::default();
        let configured = 16 * 1024 * 1024;
        // Slow path: 4 x 62 KB BDP is below the 1 MiB floor.
        assert_eq!(
            adaptive_send_buffer_limit(configured, &config, 62_000),
            1024 * 1024
        );
        // Mid path: the multiple applies untouched.
        assert_eq!(
            adaptive_send_buffer_limit(configured, &config, 1_000_000),
            4_000_000
        );
        // Fast path: never above the configured cap.
        assert_eq!(
            adaptive_send_buffer_limit(configured, &config, 100_000_000),
            configured
        );
        // A configured cap below the floor wins: adaptation only shrinks.
        assert_eq!(
            adaptive_send_buffer_limit(64 * 1024, &config, 62_000),
            64 * 1024
        );
    }

    #[test]
    fn adaptive_multiple_clamps_below_one() {
        let mut config = DatagramBdpBufferConfig::default();
        config.multiple(0.25);
        // A sub-BDP buffer cannot keep the pipe full: 1x is the minimum.
        assert_eq!(
            adaptive_send_buffer_limit(16 * 1024 * 1024, &config, 2_000_000),
            2_000_000
        );
    }

    #[test]
    fn bdp_ewma_seeds_then_converges() {
        let mut state = DatagramState::default();
        assert_eq!(state.update_bdp_ewma(80_000), 80_000, "first sample seeds");
        // A sustained 8x drop must pull the estimate most of the way down
        // within a few dozen samples (alpha 1/8).
        let mut last = 0;
        for _ in 0..32 {
            last = state.update_bdp_ewma(10_000);
        }
        assert!(
            last < 12_000,
            "EWMA must converge toward the sustained sample, got {last}"
        );
        // And a single outlier barely moves it.
        let after_spike = state.update_bdp_ewma(1_000_000);
        assert!(
            after_spike < 150_000,
            "one outlier must not swing the estimate, got {after_spike}"
        );
    }

    #[test]
    fn can_write_and_is_empty_see_every_flow() {
        let mut state = DatagramState::default();
        assert!(state.is_empty());
        assert!(!state.can_write(usize::MAX));
        let now = Instant::now();
        push(&mut state, 7, 0xaa, 1000, now);
        assert!(!state.is_empty());
        assert!(state.can_write(usize::MAX));
        assert!(
            !state.can_write(8),
            "a head that cannot fit must not report writability"
        );
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
