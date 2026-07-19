# Upstream PR plan: Initial-packet fragmentation + padding control

This fork carries six deltas vs upstream quinn. Two are proposed for
upstream: the **two Initial-fragmentation knobs** (this document) and the
**BBR STARTUP cwnd bound fix** (below). The GSO constants, socket-buffer
sizing, Apple fast-datapath port and datagram-queue AQM are deployment
tuning and stay fork-local.

## Second upstream candidate: BBR STARTUP cwnd bound fix

Two related port defects versus Chromium/quiche, one combined patch
(`upstream-bbr-startup-cwnd.patch`):

1. `quinn-proto/src/congestion/bbr/mod.rs`, `calculate_cwnd`: the STARTUP
   growth condition reads `self.cwnd_gain < target_window as f32`, comparing
   a gain factor (~2.885) against a byte count, which is always true; STARTUP
   cwnd therefore grows by every acked byte with no target_window bound. Any
   connection that stays app-limited (which skips full-bandwidth detection,
   and becomes self-sustaining once cwnd outruns the real BDP: the sender is
   never congestion-blocked again) keeps STARTUP forever and grows an
   unbounded window - we measured half-gigabyte cwnds on production VPN-exit
   connections. quiche's `BbrSender::CalculateCongestionWindow` compares
   `congestion_window`; the fix is that one-token substitution.
2. `quinn-proto/src/congestion/bbr/bw_estimation.rs`, `on_ack`: bandwidth
   samples were admitted only when `!app_limited`, so a connection that is
   app-limited from birth keeps a zero estimate. `expected_bytes_acked` is
   then 0, the ack-aggregation epoch never resets, and `excess_acked`
   equals cumulative acked bytes, re-inflating `target_window` without
   bound even with fix 1 applied (caught by a real-network A/B harness the
   unit test's seeded bandwidth had masked). quiche admits app-limited
   samples when they RAISE the estimate (a path cannot fake delivering
   faster than it can) and always admits non-app-limited samples, which is
   also what lets the windowed max filter rotate and decay; the fix
   restores both admissions and rejects zero-rate same-instant artifacts.

Three regression tests in `congestion::bbr::tests` (seeded app-limited,
app-limited-from-birth, below-target ramp) plus a `pub(crate)` widening of
`RttEstimator::new` they need. The same lines are present on upstream main,
so the patch should port trivially past the 0.11 line.

## Scope of the PR (isolate these, drop the rest)

Include only the additions to:

- `quinn-proto/src/config/transport.rs`: the two `TransportConfig` fields
  (`initial_datagram_min_size`, `initial_crypto_first_fragment_size`), their
  builder methods (with the RFC-floor, path-MTU and `Some(0)` clamps),
  `Default` init, and `Debug` fields.
- `quinn-proto/src/connection/mod.rs`: the `pad_to(...)` substitutions (config
  floor clamped to the path MTU) at the two packet-finish sites, the
  first-CRYPTO-fragment cap in `populate_packet`, and the
  `force_finish_first_datagram` signal on `SentFrames`.
- `quinn-proto/src/tests/mod.rs`: the six sans-io pair tests
  (`initial_datagram_min_size_*`, `initial_crypto_first_fragment_*`) covering
  the defaults-match-upstream, raised-floor, below-floor clamp, above-MTU
  clamp, zero-cap clamp and ClientHello-split cases.

Exclude: `quinn/src/connection.rs` GSO constants, `quinn-udp/*` (sockbuf + Apple
PR #2672 port), all version/changelog edits.

## Proposed API (already named generically in this fork)

```rust
impl TransportConfig {
    /// Minimum UDP payload size targeted when padding the handshake Initial
    /// datagram(s). Default 1200 (RFC 9000 floor); values below are clamped,
    /// values above the path MTU are clamped to it at the pad site.
    pub fn initial_datagram_min_size(&mut self, value: u16) -> &mut Self;

    /// Maximum CRYPTO bytes in the first Initial packet. None (default) packs
    /// as much as fits; Some(n) defers the rest to a following Initial packet
    /// (Some(0) is clamped to Some(1)).
    pub fn initial_crypto_first_fragment_size(&mut self, value: Option<u16>) -> &mut Self;
}
```

## PR title

`transport: configurable Initial-packet padding and CRYPTO-fragment chunking`

## PR body

quinn already exposes `TransportConfig::pad_to_mtu` ("mitigates traffic analysis
by network observers", off by default). These two knobs extend that line of
anti-ossification / anti-fingerprinting control to the **Initial** flight, with
no-op defaults:

1. `initial_datagram_min_size(u16)` — raises the pad floor at the existing
   `pad_to(MIN_INITIAL_SIZE)` sites. Default 1200 reproduces current behaviour;
   values below 1200 are clamped (RFC 9000 sect 14.1) and values above the
   current path MTU are clamped to it (padding past the MTU would emit an
   undeliverable datagram and stall the handshake).
2. `initial_crypto_first_fragment_size(Option<u16>)` — caps the first CRYPTO
   fragment so the handshake spans two or more Initial packets/datagrams.
   `None` (default) is exactly current behaviour. CRYPTO frames may be
   fragmented and are reassembled by offset (RFC 9000 sect 7.5), so this is
   spec-compliant; the 1200-byte-per-datagram minimum is preserved.

**Motivation: parity with the rest of the ecosystem.** Chrome (Chaos
Protection), Firefox/neqo (Initial greasing, v0.12.0), and quic-go (SNI-slicing,
v0.52.0) all disperse the Initial flight for anti-ossification. quinn is
currently the outlier with no knob for it. Both defaults are no-ops, so existing
users are unaffected.

## Recommended sequencing

1. Open a discussion/issue first to gauge maintainer appetite, referencing
   `pad_to_mtu` as precedent and the cross-stack parity argument.
2. PR the padding knob first (highest-confidence; direct sibling of `pad_to_mtu`).
3. PR the fragmentation knob second, with the RFC 7.5/12.2 compliance argument.

The fork already ships these names, so if upstream merges, downstream migration
is a version bump, not a rewrite.

## Ready-to-submit artifact

`upstream-initial-fragmentation.patch` (in this repo) is the **isolated diff**:
the two knobs plus their pair tests on clean upstream quinn (quinn-proto config
+ populate_packet + tests), zero GSO/sockbuf/Apple/version noise, zero
brand/censorship wording. It is the exact diff of the fork's knob commit and
applies `git apply`-clean on the upstream `0.11.x` base (commit `a96949f6`,
the released quinn-proto 0.11.16 state).

**Caveat**: upstream development happens on `main`, which has moved to the
0.12 line; a PR against `main` needs a manual port of this patch (same logic,
possibly drifted context). The 0.11.x-based patch remains the reference for
what to port.

Turnkey submit (run by a maintainer who decides to disclose upstream):

```bash
gh repo fork quinn-rs/quinn --clone --remote
cd quinn && git checkout a96949f6 -b initial-fragmentation-control
git apply /path/to/upstream-initial-fragmentation.patch
git commit -am "transport: configurable Initial-packet padding and CRYPTO-fragment chunking"
# then rebase/port onto upstream main before opening the PR
git push -u origin initial-fragmentation-control
gh pr create --repo quinn-rs/quinn --title "transport: configurable Initial-packet padding and CRYPTO-fragment chunking" --body-file UPSTREAM-PR.md
```

Deliberately not auto-submitted: opening a PR on the third-party public
`quinn-rs/quinn` is a one-time disclosure decision (it ties the org to QUIC
anti-ossification work), so it is left as the single human step above.
