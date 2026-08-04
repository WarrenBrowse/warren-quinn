# warren-quinn: rules for Claude Code

A **thin published fork of quinn**, consumed as a git-dep pinned by tag by
`warrenguard`, `warren-core`, `warren-sdk-rs` and `warren-app`. The crates are
renamed
(`warren-quinn`/`-proto`/`-udp`) but their lib names stay
`quinn`/`quinn_proto`/`quinn_udp`, so every `use quinn` in the consumers is
unchanged.

There is no vendored tree anywhere, no `[patch.crates-io]` and no setup script:
consumers fetch this repo like any other git dependency.

> Shared Warren rules (single source of truth: WarrenBrowse/warren-workspace).
> They resolve when this repo is checked out inside the workspace (mani sync);
> cloned standalone, the imports just warn harmlessly. Never restate one of them
> here: import it.
@../shared/rules/00-conventions.md
@../shared/rules/30-git-commits.md

10-tdd and 20-errors-secrets are not imported on purpose: this is a thin
upstream fork whose behaviour gate is the interleaved A/B bench (below), and
the code is upstream's, covered by upstream's own test suite.

## Prime directive: stay a THIN fork

The whole value of this repo is that it can be rebased onto upstream. Every line
that diverges from upstream is a line someone re-resolves at the next re-sync.

- **Nothing Warren-specific lands here.** No product policy, no Warren naming, no
  control-plane concept. If a change can live in `warrenguard`, it lives there.
- **The delta is deliberately small and enumerable**: the authoritative
  per-delta list is README.md section "Deltas vs upstream" (8 deltas as of
  fork.11: the Initial-fragmentation knobs, GSO transmit sizing, socket-buffer
  autosizing, the Apple fast datapath, the BBR repair fixes, the FQ-CoDel
  datagram send-queue AQM, the BDP-adaptive send buffer, and the inner-ECN
  counters), each also committed as an isolated patch at the repo root. Adding
  a delta needs a reason written down in that list.
- **Since fork.8 the fork has a real git ancestry** on `upstream/0.11.x` (commit
  `a96949f6`): a re-sync is a `git rebase`, no longer a tree reconstruction. The
  old orphan history is archived in `archive/orphan-history-fork.7`.
- **`quinn-udp` comes from a different upstream lineage** (tag `quinn-udp-0.6.1`,
  branch `main`) than the other two crates (branch `0.11.x`). That is deliberate:
  the Apple fast datapath targets the udp 0.6 line. Do not "fix" it back to
  0.5.15.
- **The `-fork.<N>` level is common to all three crates**, bumped in lockstep.

## Releasing: the tag IS the contract

Consumers pin this repo by TAG, so a tag is immutable once pushed.

1. Bump the three crates in lockstep to the same `-fork.<N>`.
2. Push the tag.
3. Bump the `tag` in each consumer's `Cargo.toml`: `warrenguard` root (quinn,
   quinn-udp), `warren-core` root, `warren-sdk-rs` root, and `warren-app` root
   `[patch.crates-io]` (quinn, quinn-proto, quinn-udp).

**Never re-cut an existing tag.** `warren-app` pins this repo by tag, and a re-cut
tag orphans the locked sha in every local mirror, including the ones inside the
Windows test VM, where the failure reads as `upload-pack: not our ref <sha>` and
accuses GitHub while nothing ever left the machine.

## A behaviour change needs a re-bench

Any change to what the fork DOES can introduce a silent performance regression in
the tunnel. The gate is an **interleaved A/B on the same pair of machines** (old
fork vs new, same warren-core tree, only the pin changes), with an out-of-tunnel
`REF direct` scenario as the network-drift control. Comparing against an older
report on other hardware proves nothing.

Procedure: `warren-core/docs/22-QUINN-FORK.md` § "The bench gate, concretely". A
pure re-tag (rename, doc) is neutral and needs no bench.

**Never prefix bench machines with `warren-`**: the default of
`teardown-hetzner.sh` is exactly `warren-` and would match the prod fleet.

## The anti-depatch guard

`warrenguard/crates/warrenguard-transport-core/src/transport_config.rs` calls
`.initial_crypto_first_fragment_size(...)` and `.initial_datagram_min_size(...)`,
which exist only on this fork. Repointing a consumer at upstream quinn therefore
fails to compile with E0599 rather than silently dropping the GSO gain and the
obfuscation. Do not remove those call sites: they are the guard.

## Verify before commit

`cargo fmt --all -- --check` (the repo carries upstream's rustfmt.toml, so the
check is upstream's style, not Warren's) and
`cargo check --workspace --all-targets`. CI runs exactly these plus two guards:
the fork-delta symbols are still present, and the three crates share one
`-fork.N` level. It deliberately runs no cargo test: upstream's suite stays
upstream's job.
