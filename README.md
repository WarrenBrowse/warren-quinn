# warren-quinn

A thin fork of [quinn](https://github.com/quinn-rs/quinn) (quinn 0.11.11,
quinn-proto 0.11.16, quinn-udp 0.6.1) carrying a small set of transport
deltas, published as renamed crates so downstreams inherit them transitively
(no `[patch.crates-io]` required):

- `warren-quinn` (lib `quinn`)
- `warren-quinn-proto` (lib `quinn_proto`)
- `warren-quinn-udp` (lib `quinn_udp`)

The lib names are unchanged, so consumers depend with a package rename and keep
`use quinn` untouched:

```toml
quinn = { git = "https://github.com/WarrenBrowse/warren-quinn", tag = "v0.11.16-fork.8", package = "warren-quinn" }
```

The fork level `N` in `-fork.<N>` is repo-wide: all three crates bump it in
lockstep (quinn `0.11.11-fork.8`, quinn-proto `0.11.16-fork.8`, quinn-udp
`0.6.1-fork.8`).

`v0.11.16-fork.8` was cut only after a Hetzner A/B bench cleared the behaviour
changes folded in from upstream 0.11.15/0.11.16 — chiefly the **BBR RNG switch
to PCG**, which lands in the default congestion controller, and the CUBIC
fast-convergence fix. Tunnel TCP came back flat within ±0.5% against the
`fork.6` baseline on the same hardware (report:
`warren-core/bench/results/2026-07-13_QUINN-FORK8_ab-hetzner.md`). A tag is
never cut on this repo without that bench.

## Upstream base (true git ancestry)

`main` sits directly on upstream git history: upstream branch `0.11.x` at
commit `a96949f6` — the released `quinn-proto-0.11.16` state, which also
contains quinn 0.11.11 (tag `quinn-0.11.11`) — followed by one fork commit per
concern. `git log upstream/0.11.x..main` therefore lists exactly the fork
surface, and moving to a newer upstream 0.11.x state is a plain `git rebase`
(or merge) instead of a tree reconstruction.

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
   stay near target_window; ramp below target still grows). Proposed for
   upstream.
6. **Datagram send-queue AQM** (`TransportConfig::datagram_send_aqm`,
   CoDel/RFC 8289): the outgoing datagram buffer is a deep FIFO; on a path
   slower than the offered load it holds seconds of standing queue before the
   drop-oldest overflow fires. With the AQM (on by default: 15 ms target /
   100 ms interval), queue heads whose sojourn time keeps the queue above
   target for a full interval are head-dropped with the CoDel control law,
   bounding queue latency at any link speed; drops are counted in the new
   `ConnectionStats::datagram_tx` (`dropped_aqm`, plus `dropped_overflow` for
   the pre-existing silent overflow evictions). `send`/`write` now carry
   `now: Instant` for sojourn timestamping.

## Patch files (portable form of the deltas)

Each fork delta is also committed as an isolated patch at the repo root,
regenerated from the rebased history (each file is the exact diff of its fork
commit). They are the fallback when upstream refactors force a manual port;
the primary upgrade path is now a plain `git rebase`.

- `upstream-initial-fragmentation.patch`: the two Initial-fragmentation knobs
  plus their pair tests, the only delta proposed for upstream (see
  `UPSTREAM-PR.md`). Applies clean on upstream `0.11.x` commit `a96949f6`;
  a submission against upstream **main** needs a manual port (main has moved
  to the 0.12 line).
- `fork-gso.patch`: GSO transmit sizing in `quinn/src/connection.rs`
  (fork-local). Applies on tag `quinn-0.11.11` content, unchanged through
  `a96949f6`.
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

- `upstream-bbr-startup-cwnd.patch`: the BBR STARTUP cwnd bound fix (delta 5)
  plus its regression tests and the `pub(crate)` widening of
  `RttEstimator::new` they need. Applies clean on `a96949f6`; the same
  one-token bug is present on upstream main.
- `fork-datagram-aqm.patch`: the CoDel datagram-queue AQM (delta 6): config,
  queue timestamping, dequeue-time controller, drop stats, and the
  `now`-carrying `send`/`write` signatures with their call-site updates.

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
