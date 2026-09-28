<p align="center">
  <img src=".github/warren-logo.svg" alt="Warren" width="130"/>
</p>

# warren-quinn

A thin fork of [quinn](https://github.com/quinn-rs/quinn) (quinn 0.11.11,
quinn-proto 0.11.17, quinn-udp 0.6.1) carrying a small set of transport
deltas, published as renamed crates so downstreams inherit them transitively
(no `[patch.crates-io]` required):

- `warren-quinn` (lib `quinn`)
- `warren-quinn-proto` (lib `quinn_proto`)
- `warren-quinn-udp` (lib `quinn_udp`)

The lib names are unchanged, so consumers depend with a package rename and keep
`use quinn` untouched:

```toml
quinn = { git = "https://github.com/WarrenBrowse/warren-quinn", tag = "v0.11.17-fork.14", package = "warren-quinn" }
```

The fork level `N` in `-fork.<N>` is repo-wide: all three crates bump it in
lockstep (quinn `0.11.11-fork.14`, quinn-proto `0.11.17-fork.14`, quinn-udp
`0.6.1-fork.14`).

`v0.11.16-fork.8` was cut only after a Hetzner A/B bench cleared the behaviour
changes folded in from upstream 0.11.15/0.11.16, chiefly the **BBR RNG switch
to PCG**, which lands in the default congestion controller, and the CUBIC
fast-convergence fix. Tunnel TCP came back flat within ±0.5% against the
`fork.6` baseline on the same hardware (report:
`warren-core/bench/results/2026-07-13_QUINN-FORK8_ab-hetzner.md`). A tag is
never cut on this repo without that bench.

## Upstream base (true git ancestry)

`main` sits directly on upstream git history: upstream branch `0.11.x` at
commit `d2cf48f1` (the `quinn-proto-0.11.17` line, which also contains quinn
0.11.11 via tag `quinn-0.11.11`), followed by one fork commit per concern.
`git log upstream/0.11.x..main` therefore lists exactly the fork surface, and moving to a newer upstream 0.11.x state is a plain `git rebase`
(or merge) instead of a tree reconstruction.

### What the move from `33ce0c21` to `d2cf48f1` brought in

33 upstream commits, of which four change behaviour Warren depends on:

- **`dcb9eabe`, the black-hole detector fixes** (upstream #2799, backporting
  #2792 and #2400, closing #2791). On the pre-backport code a bulk transfer of
  uniformly full-size packets re-arms the detector on every loss burst: an
  equal-size delivery returned early from `on_non_probe_acked` without
  advancing `largest_post_loss_packet`, so bursts that preceded it stayed
  suspicious, and `finish_loss_burst`'s strict comparisons kept judging
  1200-byte bursts suspicious once the connection had already fallen to
  `min_mtu`. The reporter measured thousands of detections per connection and
  a PMTU pinned at 1200 for the rest of the transfer, against 1452 with the
  fixes. That is the symptom the Warren field report
  (`incidents/2026-09-12-bufferbloat-fixed-probe-budget-reconnect-storm.md`)
  measured on a member's line: PMTU parked at 1200 to 1230 on a path whose
  real PMTU was 1492. Every consumer pinning `fork.12` or older carries the
  bug; this re-sync is what removes it.
- **`f650e0f2`**, a double subtraction of `payload_bytes` when evicting
  outgoing datagrams, and the `DatagramBuffer` rework around it: the send and
  receive queues now charge per-entry bookkeeping, not payload alone, so a
  queue of empty datagrams is bounded by memory rather than by a byte count it
  never moves. The fork's FQ-CoDel queue carries the same accounting
  (`QUEUED_OVERHEAD`).
- **`912d648e`**, ACKs bundled into packets that already carry DATAGRAM or
  STREAM frames, which is every tunnel packet.
- **`73e168d9`**, pacing state reset on path reset.

One deliberate mix: `quinn-udp` is not the 0.5.15 of branch 0.11.x but an
overlay of tag `quinn-udp-0.6.1` (commit `38c036ad`, upstream **main**
lineage), because the Apple fast-datapath work targets the udp 0.6 line. The
overlay is its own commit and brings the `[workspace.lints]` table that udp
0.6.1 expects into the 0.11.x workspace root.

History note: up to the `v0.11.15-fork.*` tags the repo was an orphan tree
with no upstream ancestry. That history stays reachable through the released
tags (and the `archive/orphan-history-fork.7` branch), so consumers pinning
old tags are unaffected; only `main` was rebuilt.

## Deltas vs upstream

1. **Initial-packet fragmentation control** (`TransportConfig::initial_datagram_min_size`,
   `TransportConfig::initial_crypto_first_fragment_size`): pad the first Initial
   datagram(s) to a configurable floor and cap the first CRYPTO fragment so the
   handshake spans two or more UDP datagrams. Anti-ossification; defaults are
   no-ops (RFC 9000 floor / no fragmentation). Spec-compliant (RFC 9000 sect 7.5).
   The padding floor is clamped to the RFC 9000 minimum from below and to the
   current path MTU from above (an over-MTU floor previously emitted an
   undeliverable datagram and stalled the handshake; raise
   `TransportConfig::initial_mtu` alongside the floor to go past 1200). A
   `Some(0)` fragment cap is clamped to `Some(1)` (a zero cap can never advance
   the CRYPTO offset and would stall the handshake in an endless empty-CRYPTO
   datagram loop). Both knobs are covered in-fork by sans-io pair tests in
   `quinn-proto/src/tests` (`initial_datagram_min_size_*`,
   `initial_crypto_first_fragment_*`), including the defaults-match-upstream,
   above-MTU-clamp and zero-cap-clamp cases.
2. **GSO transmit sizing**: `MAX_TRANSMIT_DATAGRAMS` 20 -> 80,
   `MAX_TRANSMIT_SEGMENTS` 10 -> 40, send-buffer pre-allocation.
3. **Socket buffer sizing**: kernel send/recv buffers auto-sized at socket
   creation on unix and windows (upstream only exposes manual setters).
4. **Apple fast datapath** (quinn-udp): upstream PR #2672 partial-send tail
   buffering, ported with buffering enabled, auto-enabled when the private
   `sendmsg_x`/`recvmsg_x` symbols resolve. Includes release hardening beyond
   the PR: an out-of-contract `Ok(0)` from `sendmsg_x` is treated as
   backpressure (`WouldBlock`) instead of spinning on a zero-progress loop.
5. **BBR STARTUP cwnd bound fix** (upstream bugs, two related port defects):
   (a) `calculate_cwnd` compared `cwnd_gain` (a gain factor) against
   `target_window` (bytes), a condition that is always true, so a connection
   stuck in STARTUP (every app-limited round skips full-bandwidth detection,
   and a tunnel is app-limited whenever inner traffic does not fill the
   window) grew cwnd by every acked byte, unbounded. The fix compares `cwnd`
   as upstream Chromium/quiche does. (b) the bandwidth estimator refused
   app-limited samples entirely (`!app_limited && ...`), so a connection
   app-limited from birth kept a zero estimate; with
   `expected_bytes_acked = 0` the ack-aggregation epoch never resets and
   `excess_acked` (= cumulative acked bytes) re-inflates `target_window`
   without bound, reopening the same hole (a) closed. quiche's admission
   rule is restored: non-app-limited samples always feed the windowed max
   filter (which is also what lets the estimate decay), app-limited samples
   only when they raise it. Observed in production before the fixes:
   half-gigabyte congestion windows on VPN-exit connections, i.e.
   congestion control effectively off; reproduced deterministically by the
   warren-core `lastmile-paired.sh` harness (cwnd == cumulative acked bytes
   after 30 s of app-limited streaming). Regression-tested by
   `congestion::bbr::tests` (seeded and from-birth app-limited scenarios
   stay near target_window; ramp below target still grows). Proposed
   upstream as quinn-rs/quinn#2798 (draft). Upstream plans to DELETE the
   whole bbr module in favour of a spec-faithful BBRv3
   (quinn-rs/quinn#2481): that implementation carries both fixes natively
   (draft-05 admission rule in `update_max_bw`; cwnd capped by
   `max_inflight` on every update), replaces `BbrConfig` with `Bbr3Config`,
   and reworks pacing. The re-sync that brings it in drops this delta,
   migrates the `warrenguard` config call site, and is a BEHAVIOUR change:
   it does not ship without the interleaved A/B bench gate.
6. **Datagram send-queue AQM** (`TransportConfig::datagram_send_aqm`,
   FQ-CoDel/RFC 8289+8290): the outgoing datagram buffer is a deep FIFO; on a
   path slower than the offered load it holds seconds of standing queue before
   the drop-oldest overflow fires. With the AQM (on by default: 15 ms target /
   100 ms interval), queue heads whose sojourn time keeps the queue above
   target for a full interval are head-dropped with the CoDel control law,
   bounding queue latency at any link speed; drops are counted in
   `ConnectionStats::datagram_tx` (`dropped_aqm`, plus `dropped_overflow` for
   the pre-existing silent overflow evictions). `send`/`write` carry
   `now: Instant` for sojourn timestamping. Since fork.11 the queue is
   FQ-CoDel-shaped: the caller classifies each datagram
   (`Connection::send_datagram_classified` with a `DatagramClass` holding an
   inner-packet flow key + ECN codepoint, computed pre-encryption via
   `DatagramClass::of_inner_ip_packet`); flows spread over per-flow queues
   (`DatagramAqmConfig::flow_queues`, default 1024, `1` = the plain
   single-queue CoDel), scheduled DRR with new-flow priority and a
   12-datagram packet-train quantum (a per-packet round-robin shreds GSO/GRO
   batching: 13-20% clean-path cost measured), each flow running its own
   CoDel, and overflow evicting from the fattest flow. A bulk flow's standing
   queue can no longer starve or delay a sparse flow multiplexed on the same
   connection.

   Since fork.14 that reordering is bounded (`DatagramAqmConfig::max_reorder`,
   default 768, `None` = unbounded). A receiver that numbers its datagrams and
   gates them with a sliding anti-replay window (RFC 6479; a tunnel's is
   typically 1024 wide) discards any datagram that arrives more than the window
   behind the newest one it has seen. New-flow priority sends a sparse datagram
   ahead of a bulk flow's whole backlog, so once that backlog is deeper than
   the window, every bulk datagram left behind is rejected on arrival: on a
   loopback probe with a 1024 window, up to 37k of 38k bulk datagrams under a
   3.6x overload and 3-30k per run at 200-400 Mbps. The scheduler now indexes
   the enqueue order (one `u32` per queued datagram, charged in
   `QUEUED_OVERHEAD`) and never lets a queued datagram fall `max_reorder` or
   more positions behind the newest one sent: when DRR's choice would, the
   oldest queued datagram goes first. It is dropped instead when it has
   already waited past the CoDel target AND its flow holds at least its fair
   share of the queued bytes (RFC 8290's overlimit victim, the flow building
   the backlog), so a sparse flow is not held over target by a backlog it did
   not build (its own CoDel would then punish it), while a thin flow left
   oldest by a sender stall or by overflow eviction is always sent, never made
   the victim. Two earlier cuts got the victim wrong and a Hetzner A/B caught
   both: dropping any stale datagram killed a tunnel's path-health probes, and
   comparing only against the flow DRR picked let a ping make a 1 Mbit/s flow
   the victim (39% loss under overload). `send_buffer_space()` also counts
   the bound: with several flows queued it is capped at the free slots under
   `max_reorder` (in datagrams of `max_size()`), because past that depth a new
   datagram forces older ones out. Without it a caller that backs off on low
   space (the tunnel exit's reader-side tail-drop) never saw the queue fill,
   kept sealing datagrams the queue then discarded, and its sender slowed.
   `datagram_tx.reorder_forced` and `dropped_reorder` count the two outcomes.
   Below the bound the scheduler is unchanged, which keeps the fork.11
   behaviour wherever the backlog is shallower than the window.
7. **BDP-adaptive datagram send buffer**
   (`TransportConfig::datagram_send_buffer_bdp`, on by default): the fixed
   `datagram_send_buffer_size` is a worst-case constant, 1-2 orders of
   magnitude above a slow last mile's real BDP; the adaptation shrinks the
   effective limit to `clamp(4 x EWMA(bw x min_rtt), 1 MiB, configured)`
   using a new `congestion::Controller::bdp_estimate` hook (implemented by
   BBR; loss-based controllers keep the fixed size). Bench: queue capped at
   ~1 MiB instead of 16 MiB on a 40 Mbit last mile with no clean-path cost.
8. **Inner-ECN distribution counters** (measurement only):
   `ConnectionStats::datagram_tx.ecn_{not_ect,ect0,ect1,ce}` count the
   caller-classified inner-packet ECN codepoints at enqueue, the data basis
   for any future mark-instead-of-drop AQM decision.
9. **A lost MTU probe is evidence about SIZE only on a path that is otherwise
   delivering** (`MtuDiscovery::on_probe_lost` takes `path_lossy`): upstream
   counts every lost probe toward `MAX_PROBE_RETRANSMITS` and then calls
   `next_mtu_to_probe(false)`, which lowers the binary search's upper bound. On
   a congested link the probe is dropped by the queue like everything else, so
   the search walks its bound down on each congestion drop and settles below
   the real PMTU while retransmitting for as long as the link stays congested.
   When ordinary packets are declared lost in the same detection pass the probe
   result is inconclusive, so the round ENDS with the MTU untouched and is
   retried at the next activation; ending it rather than re-probing is what
   keeps a permanently lossy link from reproducing the probe storm. A path that
   only loses the probe, which is what a real MTU ceiling looks like, is
   unaffected and still narrows the search. RFC 8899 sect 3, requirement 4:
   "The PL is REQUIRED to be robust in the case where probe packets are lost
   due to other reasons (including link transmission error, congestion)".
   Covered by `connection::mtud::tests`
   (`a_probe_lost_while_the_path_drops_ordinary_packets_does_not_narrow_the_search`,
   `a_probe_lost_on_an_otherwise_healthy_path_still_narrows_the_search`).

   This delta is independent of the black-hole detector fixes the `d2cf48f1`
   re-sync brought in, and smaller than them: the detector bug was what pinned
   a production connection at `min_mtu`, and its fix is upstream's. Both tests
   above pass unchanged against the pre-backport detector, which is how the
   two were told apart. The delta has no A/B bench of its own yet, because the
   local narrow-link harness never reproduced the search-bound walk
   (`warren-core/bench/results/2026-09-13_lastmile_local-container_mtu-probe-loss.md`).

   **Checked against quiche** (cloudflare/quiche `c8da372`, `quiche/src/pmtud.rs`
   and the call sites in `path.rs` / `lib.rs`), because a delta the other major
   QUIC implementation does not need is a delta worth doubting:

   - quiche does **not** discriminate either. `Pmtud::failed_probe` is called
     from the lost-frame handler for a `Ping { mtu_probe }` with no notion of
     whether ordinary packets were lost in the same pass, and after
     `max_probes` (3) consecutive failures it records
     `smallest_failed_probe_size` and binary-searches down. The gap this delta
     closes is common to both implementations, not a quinn quirk.
   - quiche **gates the probe on the congestion window**:
     `Path::should_send_pmtu_probe` requires
     `recovery.cwnd_available() > pmtud.get_probe_size()` and an otherwise
     empty frame set. quinn sends the probe whenever the send buffer is empty
     and the connection is established, with no window check and no
     `congestion.on_sent` accounting, so a collapsed cwnd does not slow probing
     down at all. RFC 8899 section 3, requirement 7 permits either ("A PL MAY
     use a congestion controller to decide when to send a probe packet"). This
     is a real second lever and it is deliberately NOT taken here: delta 9
     already prevents the harmful consequence, and a thin fork does not carry
     two overlapping unbenched behaviour changes. Revisit it if the bench ever
     reproduces the search-bound walk with delta 9 in place.
   - quiche has **no black-hole detector**. Its operating MTU is
     `largest_successful_probe_size`, which only moves on PROBE outcomes, so
     the class of bug upstream quinn fixed in `dcb9eabe` (ordinary full-size
     loss bursts pinning the connection at `min_mtu`) cannot occur there. That
     is independent corroboration that the detector, not the search, was what
     pinned the field connection at 1200.
   - The cost of quiche's design is the other way round: once `pmtu` is set it
     never searches upward again, and `revalidate_pmtu()` re-probes the same
     size and is left to the application to call. quinn re-searches on its own
     every `interval` (600 s by default), so a PMTU that settled too low
     recovers without help.

10. **A rebind to another local address restarts the path's congestion state**
    (`Connection::local_path_changed`, called by `quinn`'s rebind handler).
    Upstream keeps the congestion controller and the RTT estimator across
    `Endpoint::rebind` (`local_address_changed` only rotates the CID and
    pings), so a connection formed on one network and moved to another keeps
    the old path's round trip. BBR sizes its window as `bw x min_rtt` and reads
    `min_rtt` from the estimator's lifetime minimum: a client whose tunnel was
    born on a 0.4 ms path and moved to a 40 ms one kept a window of a few
    kilobytes and uploaded at 6.1 Mbit/s against 292 once restarted over the
    first 30 s after the move (Hetzner A/B, n=8,
    `warren-core/bench/results/2026-09-28_quinn-path-change_hetzner_migration-ab.md`).
    RFC 9000 section 9.4: "Packets sent on the old path MUST NOT contribute to
    congestion control or RTT estimation for the new path", and state may be
    kept only when the change is known to be port-only.

    The rule: `quinn` compares the old and the new socket's local address;
    two sockets bound to the same specific IP stay on the same path (a new
    port, what a NAT rebinding is to the peer) and keep everything, any other
    rebind (another IP, a wildcard socket whose source the routing table picks
    per packet, an unreadable address) calls `local_path_changed`. That
    restarts the RTT estimator, the controller, the pacer and the in-flight
    accounting under a new path generation, and keeps the MTU (the black hole
    detector lowers it if the new path is narrower, where restarting it would
    refuse every datagram above the initial MTU until discovery runs again).
    Two gates make the old path's packets inert: an ACK passes a packet to the
    controller only if it was sent under the current generation, and the RTT
    estimator only takes a sample from a newly largest acknowledged packet of
    the current generation (a sample from a packet sent before the move took
    half its round trip on the old path: 24.75 ms instead of 40 in the pair
    test without this gate). The server side is upstream's and unchanged: a
    peer that arrives from another IP gets a fresh `PathData`, a port-only
    change on the same IPv4 keeps its state. `min_rtt` on a stable path is
    untouched; the five BBR-internal candidates that tried to renew it are on
    the `bench/bbr-*` branches with the reasons they were rejected.

    Covered by `quinn-proto/src/tests/path_change.rs` (a 20 Mbit/s simulated
    bottleneck: the uplink after a move collapses to 2 Mbit/s without the
    restart; old-path packets must not sample the new path; the server's own
    reset; a same-address rebind keeps state) and `connection::rebind_tests`
    in `quinn` (the same-path rule).

## Patch files (portable form of the deltas)

Each fork delta is also committed as an isolated patch at the repo root,
regenerated from the rebased history (each file is the exact diff of its fork
commit). They are the fallback when upstream refactors force a manual port;
the primary upgrade path is now a plain `git rebase`.

- `upstream-initial-fragmentation.patch`: the two Initial-fragmentation knobs
  plus their pair tests, the only delta proposed for upstream (see
  `UPSTREAM-PR.md`). Applies clean on upstream `0.11.x` commit `d2cf48f1`;
  a submission against upstream **main** needs a manual port (main has moved
  to the 0.12 line).
- `fork-mtud-probe-loss.patch`: delta 9, the probe-loss discriminant in
  `quinn-proto/src/connection/mtud.rs` plus its one call site in
  `connection/mod.rs` (fork-local, two tests included).
- `fork-gso.patch`: GSO transmit sizing in `quinn/src/connection.rs`
  (fork-local). Applies on tag `quinn-0.11.11` content, unchanged through
  `d2cf48f1`.
- `fork-windows-sockbuf.patch`: kernel socket-buffer auto-sizing at socket
  creation, `quinn-udp/src/windows.rs` plus the matching `unix.rs` hunk
  (fork-local). Applies on tag `quinn-udp-0.6.1`.
- `fork-apple-datapath.patch`: the PR #2672 port (partial `sendmsg_x` tail
  buffering, auto-enable via `dlsym`, `Ok(0)` hardening) in
  `quinn-udp/src/unix.rs`, its `parking_lot` feature wiring, and its tests
  (fork-local). Applies on its parent fork commit (tag `quinn-udp-0.6.1` +
  `fork-windows-sockbuf.patch`); its manifest hunks reference the fork's
  renamed Cargo.toml files, so on a pristine upstream base apply the patches
  in the order listed here and fix the manifest context by hand.

- `upstream-bbr-startup-cwnd.patch`: the BBR repair fixes (delta 5) plus
  their regression tests. Applies clean on `d2cf48f1`; the `pub(crate)`
  widening of `RttEstimator::new` the tests need landed upstream in
  `33ce0c21`, so it is no longer part of the fork delta. The same one-token
  STARTUP bug is present on upstream main.
- `fork-path-change-reset.patch`: delta 10, `local_path_changed` and the
  generation gates in `quinn-proto`, the rebind rule in `quinn`, the
  bottleneck link model in the pair harness and the path-change tests
  (fork-local).
- `fork-datagram-fqcodel-bdp.patch`: the whole datagram send-queue delta
  (deltas 6-8): the AQM config and queue timestamping, FQ-CoDel per-flow
  queues + DRR, the `DatagramClass` classification API, the BDP-adaptive send
  buffer with the `Controller::bdp_estimate` hook, the inner-ECN counters,
  and the `now`-carrying `send`/`write` signatures with their call sites.
  One patch since the `d2cf48f1` re-sync: upstream reworked the same queue
  into a `DatagramBuffer` with per-entry accounting, so the fork's four
  historical steps were re-applied as a single resolution against it rather
  than re-resolved four times.
- `fork-datagram-reorder-bound.patch`: the reorder bound on that queue
  (delta 6, since fork.14) with its tests. Applies on top of
  `fork-datagram-fqcodel-bdp.patch`.

The `fork-` prefix marks deltas that stay fork-local per `UPSTREAM-PR.md`;
the `upstream-` patches are intended for submission.

**Cutting a new fork tag**: derive the next `-fork.N` from
`git tag -l 'v*-fork.*' | sort -V | tail -1` and cross-check
`git ls-remote --tags origin`. Never a bare `git tag -l | tail`: tag listings
sort lexicographically and hide double-digit versions behind single-digit ones
(the 2026-07-19 warren-app v1.9.1-vs-v1.11.0 mis-tag class). Consumers pin
fork tags explicitly, so a mis-numbered tag confuses pins rather than shipping
a regression, but the discipline is the same.

**Moving to a new upstream 0.11.x state**: `git rebase` the fork commits onto
the new upstream commit (or merge upstream in), re-run the proto and udp test
suites (`cargo test -p warren-quinn-proto`, `cargo test -p warren-quinn-udp
--features fast-apple-datapath` on a Mac), then regenerate every patch file
(`git diff <commit>^ <commit> > <file>.patch`) so the next move starts clean.

Licensed `MIT OR Apache-2.0`, same as upstream quinn.
