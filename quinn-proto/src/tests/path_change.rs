//! Congestion state across a change of network path
//!
//! A connection formed on one path and moved to another (a client that changes networks, or
//! rebinds onto another interface) must not carry the old path's round trip and window onto the
//! new one: RFC 9000 section 9.4 asks for the congestion controller and the RTT estimator to
//! restart from their initial values, unless only the port changed. BBR sizes its window as
//! `bw x min_rtt`, so a `min_rtt` kept from a 0.5 ms path holds a 40 ms path to a few Mbit/s.
//!
//! The connection forms on a 0.5 ms path, the client moves to another address whose path has
//! a 40 ms round trip, and a greedy datagram source (a tunnel carrying a bulk download) then
//! runs over a simulated 20 Mbit/s bottleneck with a 5 ms tail-drop queue. Datagrams are
//! 1100 bytes: a loss burst can make the black hole detector drop the MTU to its 1200-byte
//! floor, where a 1200-byte datagram no longer fits, and that stall is not what these tests
//! are about.
//! `PATH_CHANGE_TRACE=1` prints the sender's state every simulated second.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use bytes::Bytes;

use super::util::*;
use crate::{ConnectionHandle, Duration, TransportConfig, VarInt, congestion::BbrConfig};

const PAYLOAD: usize = 1100;
const LINK_RATE: u64 = 20_000_000;
const SHORT_ONE_WAY: Duration = Duration::from_micros(250);
const LONG_ONE_WAY: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sender {
    Client,
    Server,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Keep-alive pings only, as a tunnel carries between two downloads
    Idle,
    /// A greedy source that keeps the datagram queue full, so the congestion window alone sets
    /// the rate
    Greedy,
}

fn transport() -> TransportConfig {
    let mut bbr = BbrConfig::default();
    bbr.initial_window(32 * 1200);
    let mut transport = TransportConfig::default();
    transport
        .congestion_controller_factory(Arc::new(bbr))
        .datagram_send_buffer_size(1024 * 1024)
        .datagram_receive_buffer_size(Some(64 * 1024 * 1024))
        .receive_window(VarInt::from_u32(u32::MAX >> 2));
    transport
}

fn address(ip: [u8; 4]) -> SocketAddr {
    SocketAddr::new(
        Ipv4Addr::from(ip).into(),
        CLIENT_PORTS.lock().unwrap().next().unwrap(),
    )
}

/// A connected pair on the short path, the client at `192.0.2.10`
fn connected() -> (Pair, ConnectionHandle, ConnectionHandle) {
    let mut server = server_config();
    server.transport = Arc::new(transport());
    let mut pair = Pair::new(Default::default(), server);
    pair.client.addr = address([192, 0, 2, 10]);
    pair.latency = SHORT_ONE_WAY;
    // The receiver reads the link every millisecond, as a real host's interrupt coalescing
    // does, rather than packet by packet
    let link =
        || Link::new(LINK_RATE, Duration::from_millis(5)).with_batch(Duration::from_millis(1));
    pair.client_to_server_link = Some(link());
    pair.server_to_client_link = Some(link());
    let mut client = client_config();
    client.transport_config(Arc::new(transport()));
    let (client_ch, server_ch) = pair.connect_with(client);
    (pair, client_ch, server_ch)
}

/// Runs `source` from `sender` for `secs` seconds and returns the goodput the receiver saw in
/// each of them, in bits per second
fn pump(
    pair: &mut Pair,
    client_ch: ConnectionHandle,
    server_ch: ConnectionHandle,
    sender: Sender,
    source: Source,
    secs: u64,
) -> Vec<u64> {
    let payload = Bytes::from(vec![0u8; PAYLOAD]);
    let start = pair.time;
    let mut per_second = vec![0u64; secs as usize];
    let mut last_report = usize::MAX;
    let mut last_ping = start;
    while pair.time < start + Duration::from_secs(secs) {
        let now = pair.time;
        match source {
            Source::Idle => {
                if now >= last_ping + Duration::from_millis(100) {
                    last_ping = now;
                    match sender {
                        Sender::Client => pair.client_conn_mut(client_ch).ping(),
                        Sender::Server => pair.server_conn_mut(server_ch).ping(),
                    }
                }
            }
            Source::Greedy => {
                let mut datagrams = match sender {
                    Sender::Client => pair.client_datagrams(client_ch),
                    Sender::Server => pair.server_datagrams(server_ch),
                };
                while datagrams.send_buffer_space() >= 2 * PAYLOAD {
                    if datagrams.send(payload.clone(), false, now).is_err() {
                        break;
                    }
                }
            }
        }
        let before = pair.time;
        let mut steps = 0;
        while pair.time == before && steps < 64 {
            pair.step();
            steps += 1;
        }
        // The source must be polled even while the connection has nothing to do
        let cap = before + Duration::from_micros(500);
        pair.time = pair.time.min(cap).max(before + Duration::from_micros(1));
        let second = pair.time.saturating_duration_since(start).as_secs() as usize;
        loop {
            let datagram = match sender {
                Sender::Client => pair.server_datagrams(server_ch).recv(),
                Sender::Server => pair.client_datagrams(client_ch).recv(),
            };
            let Some(datagram) = datagram else { break };
            if second < per_second.len() {
                per_second[second] += datagram.len() as u64 * 8;
            }
        }
        if second != last_report && std::env::var_os("PATH_CHANGE_TRACE").is_some() {
            last_report = second;
            let stats = match sender {
                Sender::Client => pair.client_conn_mut(client_ch).stats(),
                Sender::Server => pair.server_conn_mut(server_ch).stats(),
            };
            println!(
                "  {sender:?} t={second}s cwnd={} rtt={:?} min_rtt={:?} sent={} lost={}",
                stats.path.cwnd,
                stats.path.rtt,
                stats.path.min_rtt,
                stats.path.sent_packets,
                stats.path.lost_packets,
            );
        }
    }
    per_second
}

fn mean_from(per_second: &[u64], from: usize) -> u64 {
    let tail = &per_second[from..];
    tail.iter().sum::<u64>() / tail.len() as u64
}

fn mbit(per_second: &[u64]) -> Vec<String> {
    per_second
        .iter()
        .map(|b| format!("{:.1}", *b as f64 / 1e6))
        .collect()
}

/// The client moves to another address, on a path with a 40 ms round trip, and tells the
/// connection its local path changed, as `quinn::Endpoint::rebind` does for a socket bound to
/// another address
fn move_client_to_a_longer_path(pair: &mut Pair, client_ch: ConnectionHandle) {
    pair.client.addr = address([198, 51, 100, 20]);
    pair.latency = LONG_ONE_WAY;
    let now = pair.time;
    pair.client_conn_mut(client_ch).local_path_changed(now);
}

/// The collapse the 2026-09-27 Hetzner bench measured through the tunnel (4 Mbit/s on a
/// 40 ms path for a connection born on a sub-millisecond one), on the uplink, where the
/// client is the sender and its own congestion state is the one that must restart
#[test]
fn a_client_that_moves_to_a_longer_path_sizes_its_window_for_it() {
    let (mut pair, client_ch, server_ch) = connected();
    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Idle,
        1,
    );

    move_client_to_a_longer_path(&mut pair, client_ch);
    let after = pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Greedy,
        6,
    );
    let stats = pair.client_conn_mut(client_ch).stats();
    assert!(
        mean_from(&after, 2) >= LINK_RATE * 8 / 10,
        "a greedy uplink over a {} Mbit/s path must fill it once the client has moved to it, \
         got {:?} Mbit/s per second (client min_rtt {:?}, cwnd {})",
        LINK_RATE / 1_000_000,
        mbit(&after),
        stats.path.min_rtt,
        stats.path.cwnd
    );
}

/// Packets sent on the old path and acknowledged after the move took half of their round trip
/// on the old path: RFC 9000 section 9.4 forbids them from contributing to the new path's RTT
/// estimate, and a `min_rtt` taken from them would hold the window at half the path's BDP
#[test]
fn packets_sent_before_a_move_do_not_sample_the_new_path() {
    let (mut pair, client_ch, server_ch) = connected();
    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Greedy,
        1,
    );

    move_client_to_a_longer_path(&mut pair, client_ch);
    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Greedy,
        1,
    );
    let min_rtt = pair.client_conn_mut(client_ch).stats().path.min_rtt;
    assert!(
        min_rtt >= 2 * LONG_ONE_WAY,
        "the new path's min_rtt must be one of its own round trips (at least {:?}), got {min_rtt:?}",
        2 * LONG_ONE_WAY
    );
}

/// The server's side of the same move: it sees its peer arrive from another IP address and
/// restarts on its own (upstream's `migrate`); a regression guard for the downlink
#[test]
fn a_server_whose_client_moves_to_a_longer_path_sizes_its_window_for_it() {
    let (mut pair, client_ch, server_ch) = connected();
    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Server,
        Source::Idle,
        1,
    );

    move_client_to_a_longer_path(&mut pair, client_ch);
    let after = pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Server,
        Source::Greedy,
        6,
    );
    assert!(
        mean_from(&after, 2) >= LINK_RATE * 8 / 10,
        "a greedy downlink must fill the path once the client has moved, got {:?}",
        mbit(&after)
    );
}

/// A rebind that keeps the IP address and changes only the port stays on the same path, and a
/// NAT rebinding looks the same to the server: RFC 9000 section 9.4 lets both keep their
/// congestion state, and restarting it would cost a slow start for nothing
#[test]
fn a_rebind_on_the_same_address_keeps_the_congestion_state() {
    let (mut pair, client_ch, server_ch) = connected();
    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Greedy,
        1,
    );
    let client_before = pair.client_conn_mut(client_ch).stats().path;
    let server_min_rtt_before = pair.server_conn_mut(server_ch).stats().path.min_rtt;

    let ip = pair.client.addr.ip();
    pair.client.addr = SocketAddr::new(ip, CLIENT_PORTS.lock().unwrap().next().unwrap());
    pair.client_conn_mut(client_ch).local_address_changed();
    let client_after = pair.client_conn_mut(client_ch).stats().path;
    assert_eq!(client_after.min_rtt, client_before.min_rtt);
    assert_eq!(client_after.cwnd, client_before.cwnd);

    pump(
        &mut pair,
        client_ch,
        server_ch,
        Sender::Client,
        Source::Greedy,
        1,
    );
    assert_eq!(
        pair.server_conn_mut(server_ch).remote_address(),
        pair.client.addr
    );
    assert_eq!(
        pair.server_conn_mut(server_ch).stats().path.min_rtt,
        server_min_rtt_before,
        "a NAT rebinding must keep the server's RTT estimate"
    );
}
