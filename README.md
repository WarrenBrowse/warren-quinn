# warren-quinn

A thin fork of [quinn](https://github.com/quinn-rs/quinn) (quinn 0.11.11,
quinn-proto 0.11.15, quinn-udp 0.6.1) carrying a small set of transport
deltas, published as renamed crates so downstreams inherit them transitively
(no `[patch.crates-io]` required):

- `warren-quinn` (lib `quinn`)
- `warren-quinn-proto` (lib `quinn_proto`)
- `warren-quinn-udp` (lib `quinn_udp`)

The lib names are unchanged, so consumers depend with a package rename and keep
`use quinn` untouched:

```toml
quinn = { git = "https://github.com/WarrenBrowse/warren-quinn", tag = "v0.11.15-fork.6", package = "warren-quinn" }
```

The next tag will be `v0.11.15-fork.7`; it is cut only after a Hetzner
real-exit bench validates the CUBIC fast-convergence behavior change folded
from upstream 0.11.15 (see below). Until then consumers keep pinning
`v0.11.15-fork.6`.

## Upstream base (no gap)

The fork tree now matches its upstream tags exactly, deltas below aside:
`quinn-proto-0.11.15`, `quinn-0.11.11` (same release commit `a7499b84`), and
`quinn-udp-0.6.1`. In particular the genuine 0.11.15 content is fully folded
in, including:

- **CUBIC fast-convergence fix** (upstream `fe5ac49f`, RFC 9438): `ssthresh`
  is derived from the pre-reduction window, no longer double-reducing after
  fast convergence lowered `w_max`. Behavior change vs fork.6; needs the
  Hetzner re-bench before the next tag.
- **Saturation silent-drop** (upstream `6f03ca34`): Initials that would be
  rejected because `max_incoming` is full or CIDs are exhausted are dropped
  without deriving initial keys or replying `CONNECTION_REFUSED`, so an
  Initial flood cannot starve packet processing (regression test
  `silently_drop_rejected_initials` included).
- Upstream PR #2694 (RUSTSEC-2026-0185, bounded out-of-order stream
  reassembly) and PR `c1e903bc` (overdue async timers honoured via
  `runtime.now()`), previously carried as fork backports, now simply part of
  the matching base.

## Deltas vs upstream

1. **Initial-packet fragmentation control** (`TransportConfig::initial_datagram_min_size`,
   `TransportConfig::initial_crypto_first_fragment_size`): pad the first Initial
   datagram(s) to a configurable floor and cap the first CRYPTO fragment so the
   handshake spans two or more UDP datagrams. Anti-ossification; defaults are
   no-ops (RFC 9000 floor / no fragmentation). Spec-compliant (RFC 9000 sect 7.5).
   The padding floor is clamped to the RFC 9000 minimum from below and to the
   current path MTU from above (an over-MTU floor previously emitted an
   undeliverable datagram and stalled the handshake; raise
   `TransportConfig::initial_mtu` alongside the floor to go past 1200). Both
   knobs are covered in-fork by sans-io pair tests in `quinn-proto/src/tests`
   (`initial_datagram_min_size_*`, `initial_crypto_first_fragment_*`),
   including the defaults-match-upstream and above-MTU-clamp cases.
2. **GSO transmit sizing**: `MAX_TRANSMIT_DATAGRAMS` 20 -> 80,
   `MAX_TRANSMIT_SEGMENTS` 10 -> 40, send-buffer pre-allocation.
3. **Socket buffer sizing**: kernel send/recv buffers auto-sized at socket
   creation on unix and windows (upstream only exposes manual setters).
4. **Apple fast datapath** (quinn-udp): upstream PR #2672 partial-send tail
   buffering, ported with buffering enabled, auto-enabled when symbols resolve.

## Patch files (portable form of the deltas)

Each fork delta is also committed as an isolated patch at the repo root, so it
survives a move to a fresh upstream base even though this repo's history cannot
be `git rebase`d (the fork root is an orphan commit with no ancestry shared
with the upstream tags):

- `upstream-initial-fragmentation.patch`: the two Initial-fragmentation knobs,
  the only delta proposed for upstream (see `UPSTREAM-PR.md`). Applies on
  upstream commit `41c8527c`.
- `fork-gso.patch`: GSO transmit sizing in `quinn/src/connection.rs`
  (fork-local). Applies on tag `quinn-0.11.11`.
- `fork-windows-sockbuf.patch`: kernel socket-buffer auto-sizing at socket
  creation, `quinn-udp/src/windows.rs` plus the matching `unix.rs` hunk
  (fork-local). Applies on tag `quinn-udp-0.6.1`.
- `fork-apple-datapath.patch`: the PR #2672 port (partial `sendmsg_x` tail
  buffering, auto-enable via `dlsym`) in `quinn-udp/src/unix.rs`, its
  `parking_lot` feature wiring, and its tests (fork-local). Applies on tag
  `quinn-udp-0.6.1`.

The `fork-` prefix marks deltas that stay fork-local per `UPSTREAM-PR.md`;
only the `upstream-` patch is intended for submission.

**Rebase-onto-fresh-tag strategy.** To move the fork to a new upstream release:
check out the new upstream tag into a fresh tree, re-apply the crate identity
(package names `warren-quinn*`, `[lib]` names, `-fork.<N>` version suffix, this
README/patch set), then `git apply` each patch file, fixing drift by hand where
upstream refactored (for example, upstream extracted the Apple fast path into
its own module after `quinn-udp-0.6.1`, so `fork-apple-datapath.patch` needs
manual porting there). Finally regenerate every patch from the new base
(`git diff <new-tag> HEAD -- <paths>`) so the next rebase starts clean, and run
the proto test suite (`cargo test -p warren-quinn-proto`), which covers the two
knobs in-fork.

Licensed `MIT OR Apache-2.0`, same as upstream quinn.
