use std::collections::VecDeque;
use std::fmt::{Debug, Display, Formatter};

use super::min_max::MinMax;
use crate::{Duration, Instant};

/// Delivery-rate sampler (draft-cheng-iccrg-delivery-rate-estimation), feeding BBR's
/// windowed max bandwidth filter.
///
/// Every send records how much had been delivered at that moment; when a packet is
/// acknowledged, the bytes delivered since its send over the longer of its send and ack
/// intervals is one rate sample. The port this replaces divided a single packet's size by
/// the time since the previous ACK frame, which reads a delivery rate divided by the
/// number of packets each ACK covers, and a burst of ACKs as a spike.
#[derive(Clone, Debug, Default)]
pub(crate) struct BandwidthEstimation {
    total_acked: u64,
    acked_at_last_window: u64,
    /// Bytes delivered over the connection's life
    delivered: u64,
    /// When `delivered` last grew
    delivered_time: Option<Instant>,
    /// Send time of the most recently delivered packet
    first_sent_time: Option<Instant>,
    /// Delivery state at each send instant still in flight, oldest first
    sends: VecDeque<SendState>,
    /// The sample of the most recently sent packet the current ACK acknowledges
    pending: Option<RateSample>,
    max_filter: MinMax,
}

#[derive(Clone, Copy, Debug)]
struct SendState {
    sent: Instant,
    delivered: u64,
    delivered_time: Instant,
    first_sent_time: Instant,
}

#[derive(Clone, Copy, Debug)]
struct RateSample {
    /// `delivered` at the acknowledged packet's send, which orders samples by recency
    prior_delivered: u64,
    bytes_per_second: u64,
}

/// Send instants tracked at most; past this the oldest are forgotten and their packets,
/// when acknowledged, yield no sample
const MAX_TRACKED_SENDS: usize = 16_384;

impl BandwidthEstimation {
    pub(crate) fn on_sent(&mut self, now: Instant, _bytes: u64) {
        if self.sends.back().is_some_and(|s| s.sent == now) {
            // Same instant, same delivery state: one entry serves the whole batch.
            return;
        }
        if self.sends.is_empty() {
            // Nothing in flight: intervals start afresh rather than spanning the idle time.
            self.first_sent_time = Some(now);
            self.delivered_time = Some(now);
        }
        if self.sends.len() == MAX_TRACKED_SENDS {
            self.sends.pop_front();
        }
        self.sends.push_back(SendState {
            sent: now,
            delivered: self.delivered,
            delivered_time: self.delivered_time.unwrap_or(now),
            first_sent_time: self.first_sent_time.unwrap_or(now),
        });
    }

    pub(crate) fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, min_rtt: Duration) {
        self.total_acked += bytes;
        self.delivered += bytes;
        self.delivered_time = Some(now);
        // Sends older than this packet's are either acknowledged already, lost, or
        // reordered behind it: none of them will give a sample worth more than this one.
        while self.sends.front().is_some_and(|s| s.sent < sent) {
            self.sends.pop_front();
        }
        let Some(state) = self.sends.front().copied().filter(|s| s.sent == sent) else {
            return;
        };
        self.first_sent_time = Some(sent);
        let send_elapsed = sent.saturating_duration_since(state.first_sent_time);
        let ack_elapsed = now.saturating_duration_since(state.delivered_time);
        let interval = send_elapsed.max(ack_elapsed);
        // An interval shorter than the path's round trip is ACK compression, not a rate.
        if interval.is_zero() || interval < min_rtt {
            return;
        }
        let Some(bytes_per_second) =
            Self::bw_from_delta(self.delivered - state.delivered, interval)
        else {
            return;
        };
        if self
            .pending
            .is_none_or(|p| state.delivered >= p.prior_delivered)
        {
            self.pending = Some(RateSample {
                prior_delivered: state.delivered,
                bytes_per_second,
            });
        }
    }

    pub(crate) fn bytes_acked_this_window(&self) -> u64 {
        self.total_acked - self.acked_at_last_window
    }

    pub(crate) fn end_acks(&mut self, round: u64, app_limited: bool) {
        self.acked_at_last_window = self.total_acked;
        let Some(sample) = self.pending.take() else {
            return;
        };
        // quiche's admission rule: a non-app-limited sample always feeds the windowed
        // max filter, which is also what lets the estimate decay; an app-limited one only
        // when it raises it, since a sender short of data says nothing about capacity.
        let rate = sample.bytes_per_second;
        if rate > 0 && (!app_limited || rate > self.max_filter.get()) {
            self.max_filter.update_max(round, rate);
        }
    }

    pub(crate) fn get_estimate(&self) -> u64 {
        self.max_filter.get()
    }

    pub(crate) const fn bw_from_delta(bytes: u64, delta: Duration) -> Option<u64> {
        let window_duration_ns = delta.as_nanos();
        if window_duration_ns == 0 {
            return None;
        }
        let b_ns = bytes * 1_000_000_000;
        let bytes_per_second = b_ns / (window_duration_ns as u64);
        Some(bytes_per_second)
    }
}

impl Display for BandwidthEstimation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.3} MB/s",
            self.get_estimate() as f32 / (1024 * 1024) as f32
        )
    }
}
