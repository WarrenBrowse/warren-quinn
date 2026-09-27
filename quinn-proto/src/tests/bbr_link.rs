//! BBR over a simulated bottleneck: a datagram sender on the server (greedy, or a
//! constant-rate flow), a rate-limited, delayed, jitter-free link towards the client.
//!
//! The `probe_` tests print per-second goodput and window figures rather than assert
//! (`cargo test --release -p warren-quinn-proto --lib probe_bbr -- --ignored --nocapture`,
//! `BBR_LINK_TRACE=1` for per-second connection stats, `BBR_LINK_GREEDY=1` to add the
//! greedy sender to the RTT step probe).

use std::sync::Arc;

use bytes::Bytes;

use super::util::*;
use crate::{Duration, TransportConfig, VarInt, congestion::BbrConfig};

const PAYLOAD: usize = 1200;

struct Outcome {
    /// Goodput in bits per second for each simulated second
    per_second: Vec<u64>,
    max_cwnd: u64,
    final_cwnd: u64,
    link_drops: u64,
    /// Datagrams the sender's own queue discarded (AQM, overflow, reorder bound)
    local_drops: u64,
}

fn run(rate: u64, one_way: Duration, batch: Option<Duration>, secs: u64) -> Outcome {
    run_offered(rate, one_way, batch, secs, None)
}

/// `offered`: a constant-rate source in bits per second, `None` for a greedy one
fn run_offered(
    rate: u64,
    one_way: Duration,
    batch: Option<Duration>,
    secs: u64,
    offered: Option<u64>,
) -> Outcome {
    run_stepped(rate, one_way, one_way, batch, secs, offered)
}

/// `connect_one_way`: the path's one-way delay while the handshake runs, before it moves to
/// `one_way` for the measurement, as a network change or a route change does mid-connection
fn run_stepped(
    rate: u64,
    connect_one_way: Duration,
    one_way: Duration,
    batch: Option<Duration>,
    secs: u64,
    offered: Option<u64>,
) -> Outcome {
    let mut transport = TransportConfig::default();
    let mut bbr = BbrConfig::default();
    bbr.initial_window(32 * 1200);
    transport
        .congestion_controller_factory(Arc::new(bbr))
        .datagram_send_buffer_size(4 * 1024 * 1024)
        .receive_window(VarInt::from_u32(u32::MAX >> 2));
    let mut server = server_config();
    server.transport = Arc::new(transport);
    let mut pair = Pair::new(Default::default(), server);
    let mut client_transport = TransportConfig::default();
    client_transport.datagram_receive_buffer_size(Some(64 * 1024 * 1024));
    let mut client = client_config();
    client.transport_config(Arc::new(client_transport));
    pair.latency = connect_one_way;
    let (client_ch, server_ch) = pair.connect_with(client);
    pair.latency = one_way;
    let link = Link::new(rate, Duration::from_millis(50));
    pair.server_to_client_link = Some(match batch {
        Some(batch) => link.with_batch(batch),
        None => link,
    });

    let payload = Bytes::from(vec![0u8; PAYLOAD]);
    let start = pair.time;
    let mut per_second = vec![0u64; secs as usize];
    let mut max_cwnd = 0;
    let mut last_report = usize::MAX;
    let mut offered_sent = 0u64;
    while pair.time < start + Duration::from_secs(secs) {
        let now = pair.time;
        {
            let due = offered.map(|bps| {
                let elapsed = now.saturating_duration_since(start).as_nanos() as u64;
                (elapsed as u128 * bps as u128 / 8 / 1_000_000_000) as u64 / PAYLOAD as u64
            });
            let mut dg = pair.server_datagrams(server_ch);
            loop {
                if let Some(due) = due {
                    if offered_sent >= due {
                        break;
                    }
                } else if dg.send_buffer_space() < 2 * PAYLOAD {
                    break;
                }
                // A source that outruns the queue still counts as offered: the queue drops it
                let _ = dg.send(payload.clone(), false, now);
                offered_sent += 1;
            }
        }
        let before = pair.time;
        let mut steps = 0u64;
        while pair.time == before && steps < 64 {
            pair.step();
            steps += 1;
        }
        if pair.time == before || offered.is_some() {
            // A paced source must be polled even while the connection has nothing to do
            let cap = before + Duration::from_micros(100);
            if pair.time == before || pair.time > cap {
                pair.time = pair.time.min(cap).max(before + Duration::from_micros(1));
            }
        }
        let elapsed = pair.time.saturating_duration_since(start).as_secs() as usize;
        while let Some(d) = pair.client_datagrams(client_ch).recv() {
            if elapsed < per_second.len() {
                per_second[elapsed] += d.len() as u64 * 8;
            }
        }
        let stats = pair.server_conn_mut(server_ch).stats();
        max_cwnd = max_cwnd.max(stats.path.cwnd);
        if elapsed != last_report && std::env::var_os("BBR_LINK_TRACE").is_some() {
            last_report = elapsed;
            println!(
                "  t={elapsed}s cwnd={} rtt={:?} min_rtt={:?} sent={} lost={} dg_tx={:?} closed={}",
                stats.path.cwnd,
                stats.path.rtt,
                stats.path.min_rtt,
                stats.path.sent_packets,
                stats.path.lost_packets,
                stats.datagram_tx,
                pair.server_conn_mut(server_ch).is_closed()
            );
        }
    }
    let dg_tx = pair.server_conn_mut(server_ch).stats().datagram_tx;
    Outcome {
        local_drops: dg_tx.dropped_aqm + dg_tx.dropped_overflow + dg_tx.dropped_reorder,
        per_second,
        max_cwnd,
        final_cwnd: pair.server_conn_mut(server_ch).stats().path.cwnd,
        link_drops: pair.server_to_client_link.as_ref().map_or(0, |l| l.dropped),
    }
}

#[test]
#[ignore]
fn probe_bbr_goodput_over_a_delayed_bottleneck() {
    for (rate, rtt_ms, batch_us) in [
        (100_000_000u64, 40u64, 0u64),
        (100_000_000, 40, 1000),
        (100_000_000, 150, 1000),
        (20_000_000, 40, 1000),
        (100_000_000, 2, 1000),
    ] {
        let batch = (batch_us > 0).then(|| Duration::from_micros(batch_us));
        println!("rate {} rtt {} batch_us {}", rate, rtt_ms, batch_us);
        let o = run(rate, Duration::from_millis(rtt_ms / 2), batch, 10);
        let mbit: Vec<String> = o
            .per_second
            .iter()
            .map(|b| format!("{:.1}", *b as f64 / 1e6))
            .collect();
        println!(
            "rate {} Mbit/s rtt {} ms: per-second goodput [{}] max_cwnd {} final_cwnd {} link_drops {}",
            rate / 1_000_000,
            rtt_ms,
            mbit.join(" "),
            o.max_cwnd,
            o.final_cwnd,
            o.link_drops
        );
    }
}

#[test]
#[ignore]
fn probe_bbr_after_an_rtt_step() {
    let greedy = std::env::var_os("BBR_LINK_GREEDY").is_some();
    for (offered, rtt_ms) in [
        (None, 40u64),
        (Some(50_000_000u64), 40),
        (None, 150),
        (Some(50_000_000), 150),
    ] {
        if offered.is_none() && !greedy {
            continue;
        }
        for connect_us in [250u64, rtt_ms * 500] {
            let o = run_stepped(
                100_000_000,
                Duration::from_micros(connect_us),
                Duration::from_millis(rtt_ms / 2),
                Some(Duration::from_millis(1)),
                30,
                offered,
            );
            let mbit: Vec<String> = o
                .per_second
                .iter()
                .map(|b| format!("{:.1}", *b as f64 / 1e6))
                .collect();
            println!(
                "offered {:?} rtt {} ms connected at {} us one-way: [{}] max_cwnd {} final_cwnd {} link_drops {} local_drops {}",
                offered.map(|o| o / 1_000_000),
                rtt_ms,
                connect_us,
                mbit.join(" "),
                o.max_cwnd,
                o.final_cwnd,
                o.link_drops,
                o.local_drops
            );
        }
    }
}

/// Goodput over the seconds from `from` to the end of the run, in bits per second
fn goodput_from(o: &Outcome, from: usize) -> u64 {
    let tail = &o.per_second[from..];
    tail.iter().sum::<u64>() / tail.len() as u64
}

/// A connection whose path RTT rises after the handshake (a network change, a route change,
/// or latency added under a live tunnel) must size its window on the RTT the path has now.
/// BBR's model is `bw x min_rtt`; a `min_rtt` that can only ever go down keeps the window at
/// the old, shorter path's size, and a sender carrying a steady flow then delivers a fraction
/// of it and drops the rest from its own queue.
#[test]
fn bbr_follows_a_path_rtt_that_rises_after_the_handshake() {
    // BBR lets a min_rtt estimate stand for 10 s (`K_MIN_RTT_EXPIRY`), so the window is
    // judged once that has run out and the model has had a few round trips to regrow.
    for (rtt_ms, secs, judged_from) in [(40u64, 16u64, 12usize), (150, 20, 15)] {
        let o = run_stepped(
            100_000_000,
            Duration::from_micros(250),
            Duration::from_millis(rtt_ms / 2),
            Some(Duration::from_millis(1)),
            secs,
            Some(50_000_000),
        );
        let goodput = goodput_from(&o, judged_from);
        assert!(
            goodput >= 45_000_000,
            "a 50 Mbit/s flow over a 100 Mbit/s path whose RTT rose to {rtt_ms} ms after the \
             handshake must be carried in full once BBR has seen the new RTT, got {:.1} Mbit/s \
             (per second: {:?}, final cwnd {}, local drops {})",
            goodput as f64 / 1e6,
            o.per_second,
            o.final_cwnd,
            o.local_drops
        );
    }
}
