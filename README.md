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

## Deltas vs upstream

1. **Initial-packet fragmentation control** (`TransportConfig::initial_datagram_min_size`,
   `TransportConfig::initial_crypto_first_fragment_size`): pad the first Initial
   datagram(s) to a configurable floor and cap the first CRYPTO fragment so the
   handshake spans two or more UDP datagrams. Anti-ossification; defaults are
   no-ops (RFC 9000 floor / no fragmentation). Spec-compliant (RFC 9000 sect 7.5).
2. **GSO transmit sizing**: `MAX_TRANSMIT_DATAGRAMS` 20 -> 80,
   `MAX_TRANSMIT_SEGMENTS` 10 -> 40, send-buffer pre-allocation.
3. **Socket buffer sizing** on unix and windows (Windows closes a gap upstream
   only handles on unix).
4. **Apple fast datapath** (quinn-udp): upstream PR #2672 partial-send tail
   buffering, ported with buffering enabled, auto-enabled when symbols resolve.
5. **Security backport** (quinn-proto -> 0.11.15): upstream PR #2694
   (RUSTSEC-2026-0185) bounds out-of-order stream reassembly. `Assembler::insert`
   yields `TooManyChunks` past 1024 buffered chunks, mapped to a connection
   `INTERNAL_ERROR`, so a peer sending maliciously gapped frames can no longer
   exhaust receiver memory.
6. **Timer correctness backport** (quinn -> 0.11.11): upstream PR `c1e903bc`
   detects deadline expiry via `runtime.now()` instead of polling the async
   timer, so a PTO / loss-detection / idle deadline is honoured even when
   Tokio's cooperative budget is exhausted while draining a busy conn-event
   channel. Matters here because the GSO sizing above amplifies exactly that
   busy-channel case.

Licensed `MIT OR Apache-2.0`, same as upstream quinn.
