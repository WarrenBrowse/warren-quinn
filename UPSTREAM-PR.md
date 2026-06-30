# Upstream PR plan: Initial-packet fragmentation + padding control

This fork carries four deltas vs upstream quinn. Only the **two
Initial-fragmentation knobs** are proposed for upstream; the GSO constants,
socket-buffer sizing and Apple fast-datapath port are deployment tuning and
stay fork-local.

## Scope of the PR (isolate these, drop the rest)

Include only the additions to:

- `quinn-proto/src/config/transport.rs`: the two `TransportConfig` fields
  (`initial_datagram_min_size`, `initial_crypto_first_fragment_size`), their
  builder methods, `Default` init, and `Debug` fields.
- `quinn-proto/src/connection/mod.rs`: the `pad_to(self.config.initial_datagram_min_size)`
  substitutions at the two packet-finish sites, the first-CRYPTO-fragment cap in
  `populate_packet`, and the `force_finish_first_datagram` signal on `SentFrames`.

Exclude: `quinn/src/connection.rs` GSO constants, `quinn-udp/*` (sockbuf + Apple
PR #2672 port), all version/changelog edits.

## Proposed API (already named generically in this fork)

```rust
impl TransportConfig {
    /// Minimum UDP payload size targeted when padding the handshake Initial
    /// datagram(s). Default 1200 (RFC 9000 floor); values below are clamped.
    pub fn initial_datagram_min_size(&mut self, value: u16) -> &mut Self;

    /// Maximum CRYPTO bytes in the first Initial packet. None (default) packs
    /// as much as fits; Some(n) defers the rest to a following Initial packet.
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
   values below 1200 are clamped (RFC 9000 sect 14.1).
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
