# constellation-encoder

A Reed-Solomon erasure-coding module parameterized after Solana's
**Constellation** protocol (the multiple-concurrent-proposers / attester
scheme described in Anza's whitepaper, *Solana Constellation: Internet
Capital Markets*, v0.9), built on the same `reed-solomon-erasure` crate that
Agave's own `ledger/src/shredder.rs` uses for its shred FEC sets — but tuned
to Constellation's actual (Γp, γp) = (256, 64) code instead of Agave's 32:32
shredder ratio.

It ships with three independent ways to convince yourself the code is
correct: an in-process round-trip test suite, a criterion benchmark, and a
real `tc netem` packet-loss simulation running over actual UDP sockets in
Docker.

## Why this exists

The task was: take Constellation's erasure-coding parameters, implement them
with the same crate Agave uses, and prove the implementation is correct
under realistic loss — not just "it compiles."

Two things drove the design:

1. **The parameters come from a primary source, not a guess.** The
   whitepaper (§2.2 "Erasure Code", Table 1) defines pslices as encoded with
   a `(Γp, γp)` Reed-Solomon code where `Γp = q` (number of attesters, 256)
   and `γp = q/4` (64). That's a **rate-1/4** code — any 64 of 256 pshreds
   reconstruct the pslice — which is a very different shape from Agave's own
   shredder FEC sets (fixed 32 data : 32 coding, i.e. rate-1/2, no variable
   sizing table despite some older docs suggesting one exists).

2. **Round-trip is the only test that proves RS correctness.** The
   `reed-solomon-erasure` crate's math is already tested upstream; what
   actually needs proving here is that *this* chunking/header/padding scheme
   and *these* two library calls (`encode`, `reconstruct_data`) are wired up
   correctly. So the test suite is built entirely around: encode a payload,
   throw away shreds according to some loss pattern, decode, and assert the
   original bytes come back exactly. Nothing else in this repo substitutes
   for that.

The `tc netem` harness exists because those two properties (in-process
correctness) don't tell you anything about real network behavior — UDP
reordering, partial datagrams, ARP interactions, and genuinely probabilistic
loss. It's a sanity check on top of the round-trip tests, not a replacement
for them.

## How this compares to Agave's `shredder.rs`

Verified directly against the live `anza-xyz/agave` `master` branch (not
from memory or stale docs):

| Aspect | Agave (`ledger/src/shredder.rs` + `shred/merkle.rs`) | This crate |
| --- | --- | --- |
| Shard ratio | Fixed 32 data : 32 coding (`DATA_SHREDS_PER_FEC_BLOCK` / `CODING_SHREDS_PER_FEC_BLOCK`) — rate 1/2, no table-driven sizing despite what some older summaries claim | Fixed 64 data : 192 parity — rate 1/4, from the Constellation whitepaper's `(Γp, γp) = (256, 64)` |
| Matrix cache | `ReedSolomonCache`: `RwLock<LruCache<(usize,usize), Arc<OnceLock<Result<Arc<ReedSolomon>, Error>>>>>`, capacity `4 * DATA_SHREDS_PER_FEC_BLOCK` | Same shape, hand-rolled with a plain `HashMap` instead of an LRU (this crate only ever has one `(64,192)` key in practice, so eviction isn't needed) |
| Encode call | `finish_erasure_batch()` calls `.encode(shards)` on **one combined `Vec`** of data+parity shards (not `encode_sep`) | Same: `rs.encode(refs)` on a combined `Vec<&mut [u8]>` |
| Decode call | `recover()` calls `.reconstruct(&mut shards)` where `shards: Vec<(&mut [u8], bool)>` (bool = "present" flag) — note **not** `Vec<Option<Shard>>`, and **not** `reconstruct_data` (Agave needs parity shards back too, for repair-shred gossip) | `.reconstruct_data(&mut slices)` on the same `(&mut [u8], bool)` tuple shape — deliberately the cheaper call, since this crate only needs the *payload* back, not reconstructed parity shards |
| Dependency | `reed-solomon-erasure = "6.0.0"`, with `simd-accel` enabled | Same crate version, `simd-accel` **not** enabled (avoids a C toolchain dependency; this is a correctness/harness deliverable, not the hot path) |

The crate's own benchmark structure (`benches/fec_bench.rs`) also extends the
real, merged Agave PR
[#5695](https://github.com/anza-xyz/agave/pull/5695) ("adds benchmarks for
recovering (chained) Merkle shreds from erasure codes"), which added
`bench_recover_shreds` to `ledger/benches/make_shreds_from_entries.rs`,
sweeping `num_packets × num_code`. Since Constellation's shard counts are
fixed by the whitepaper (no variable FEC-set-size axis to sweep the way
Agave's is), this crate swaps that sweep for **payload size × loss amount**
instead — the two axes that actually matter for a fixed-shape code.

## Module layout

```text
src/
  lib.rs           re-exports the fec module and its constants
  fec.rs           ConstellationEncoder, ReedSolomonCache, EncodeError/DecodeError
  main.rs          placeholder bin (unused by the actual deliverable)
  bin/
    sender.rs      encodes a deterministic payload, sends 256 UDP pshreds
    receiver.rs    collects pshreds, decodes, verifies, exits 0/1
tests/
  roundtrip.rs     the correctness deliverable (see below)
benches/
  fec_bench.rs     criterion benchmarks, extends Agave PR #5695's shape
docker/
  Dockerfile       rust:1.85-slim + iproute2, builds sender/receiver
  compose.yaml     two persistent Linux containers on a bridge network
  run-netem-sweep.sh   orchestrates the loss sweep from the macOS host
```

### `src/fec.rs`

```rust
pub const DATA_PSHREDS: usize = 64;    // γp
pub const PARITY_PSHREDS: usize = 192; // Γp - γp
pub const TOTAL_PSHREDS: usize = 256;  // Γp = q (attesters)

pub struct ConstellationEncoder { /* wraps a ReedSolomonCache */ }

impl ConstellationEncoder {
    pub fn new() -> Self;

    /// Splits payload into 64 equal, header-prefixed chunks and erasure-codes
    /// them into 256 pshreds. Any 64 of the 256 returned shreds suffice to
    /// recover the original payload via `decode`.
    pub fn encode(&self, payload: &[u8]) -> Result<Vec<Vec<u8>>, EncodeError>;

    /// Reconstructs the original payload from up to 256 shreds, where
    /// `shreds[i] == None` means pshred i wasn't received. Requires at least
    /// 64 present; fails closed (no partial/incorrect output) otherwise.
    pub fn decode(&self, shreds: &[Option<Vec<u8>>]) -> Result<Vec<u8>, DecodeError>;
}
```

Internals, in short:

- Each data shard is prefixed with an 8-byte header: a 4-byte big-endian
  shard index and a 4-byte big-endian *original payload length*. The length
  is what lets `decode` correctly depad after reconstruction, since the last
  chunk is generally zero-padded to align to 64 equal-size shards.
- `encode` zero-fills 192 parity shard buffers, then calls
  `ReedSolomon::encode` on the combined 256-shard `Vec`, exactly mirroring
  Agave's `finish_erasure_batch`.
- `decode` fails closed with `DecodeError::InsufficientShreds` if fewer than
  64 shreds are present — no attempt at partial recovery. If ≥64 are present,
  it builds the `(shard_bytes, present_bool)` tuples Agave's `recover()`
  uses, calls `reconstruct_data` (not the full `reconstruct`, since parity
  shards aren't needed back here), cross-checks that all recovered data
  shards agree on the payload length (defense against internal
  inconsistency → `DecodeError::Corrupt`), then concatenates and truncates.

### `ReedSolomonCache`

Same concurrency shape as Agave's: a read-lock fast path for cache hits, and
on a miss, a write-lock just long enough to insert an empty `OnceLock` slot,
with the actual (expensive) `ReedSolomon::new()` matrix construction
happening inside `OnceLock::get_or_init` — so it's built at most once per
`(data_shards, parity_shards)` key, and never while holding the cache's write
lock. In practice this crate only ever asks for the single `(64, 192)` key,
so the cache is somewhat overkill, but it's kept for structural fidelity to
Agave's design and in case a future variant needs a second shape.

## Running it

Three independent verification paths, from the repo root.

### 1. Round-trip correctness tests (the one that matters)

```bash
cargo test --release
```

Runs `tests/roundtrip.rs` — 9 tests, each: encode → drop shreds according to
some pattern → decode → assert the recovered bytes equal the original
exactly:

- all 64 data shreds present, zero parity used
- **zero** data shreds present, recovery purely from 64 parity shreds
- random 64-of-256 subsets (10 different seeds)
- above-threshold slack (100-of-256, 200-of-256, all 256)
- exactly 63-of-256 present → asserts it fails closed with
  `DecodeError::InsufficientShreds { have: 63, need: 64 }`
- non-64-aligned payload lengths (0, 1, 63, 64, 65, 127, 128, 129, 1000,
  65537 bytes)
- empty payload, single-byte payload, multi-MB payload

All 9 currently pass.

### 2. Benchmarks

```bash
cargo bench --bench fec_bench
```

Sweeps encode time by payload size (`1KiB, 16KiB, 256KiB, 1MiB`) and decode
(reconstruction) time by payload size × amount of simulated loss
(`num_missing ∈ {0, 64, 128, 191, 192}`, where 192 is the maximum tolerable —
losing one more would drop below the 64-shred threshold). For a quick
spot-check instead of the full statistical sweep:

```bash
cargo bench --bench fec_bench -- --quick encode_payload1024
```

### 3. Real network loss simulation (`tc netem` in Docker)

This host is macOS, and `tc`/`iproute2` are Linux-only, so this runs inside
two Linux containers via Docker Desktop (confirmed installed/running before
building this):

```bash
cd docker
PAYLOAD_BYTES=4096 ./run-netem-sweep.sh
```

What it does:

1. Builds a `rust:1.85-slim` image with `iproute2` installed, builds the
   `sender`/`receiver` binaries in release mode.
2. Brings up two **persistent** containers (`sender`, `receiver`) on a
   user-defined bridge network (`10.88.0.2`/`10.88.0.3`) — persistent
   rather than recreated per test, for a reason explained below.
3. Warms up ARP resolution between them (`ping`) *before* introducing any
   loss.
4. Sweeps loss percentages `{0, 25, 50, 70, 74, 75, 76, 80, 90}`, applying
   `tc qdisc replace dev eth0 root netem loss X%` **on the sender's**
   interface for each. For each percentage: starts the receiver process,
   runs the sender (single pass, no retransmission), waits for the receiver
   to finish its 2-second collection window, and checks its exit code (0 =
   recovered payload matches exactly, 1 = failed).
5. Tears everything down at the end regardless of outcome.

This is a **real** simulation, not a canned/mocked result: `sender` and
`receiver` are genuinely separate Linux containers with their own network
namespaces, `tc qdisc replace dev eth0 root netem loss X%` installs a real
kernel packet-scheduler rule, the 256 pshreds are sent as real UDP
datagrams, and the kernel actually drops each one independently with
probability X%. Because of that, every run produces slightly different
received-shred counts — netem loss is a per-packet Bernoulli trial, not a
deterministic script. Two genuine runs at the same payload size (4096
bytes), both matching the theoretical 64/256 = 25% survival threshold, but
landing on opposite sides of the fuzzy boundary around 75% purely from
binomial noise:

```text
run A                                  run B
loss 0%:  PASS (256/256 received)      loss 0%:  PASS (256/256 received)
loss 25%: PASS (197/256 received)      loss 25%: PASS (189/256 received)
loss 50%: PASS (118/256 received)      loss 50%: PASS (122/256 received)
loss 70%: PASS (77/256 received)       loss 70%: PASS (76/256 received)
loss 74%: PASS (69/256 received)       loss 74%: PASS (68/256 received)
loss 75%: PASS (66/256 received)       loss 75%: FAIL (54/256 received)
loss 76%: PASS (78/256 received)       loss 76%: FAIL (58/256 received)
loss 80%: FAIL (53/256 received)       loss 80%: FAIL (39/256 received)
loss 90%: FAIL (24/256 received)       loss 90%: FAIL (26/256 received)
```

Both runs agree well below and well above the boundary (0-74% always
passes, 80-90% always fails); the 75-76% rows are exactly where you'd expect
disagreement between runs, since the expected received count at 75% loss
(64/256) sits right on the recovery threshold itself. This is the correct,
expected behavior of a probabilistic network simulation — it is *not* what
you'd want from the deterministic correctness test, which is why
`tests/roundtrip.rs`'s `below_threshold_63_of_256_fails_closed` (an exact,
in-process boundary test) is the one that actually proves correctness; this
harness is a realistic sanity check layered on top of it.

## Bugs found and fixed while actually running this (not just written)

Writing plausible-looking code for a network harness and never running it is
how subtle networking bugs survive review. These were caught by actually
executing the Docker sweep, not by inspection:

1. **Parity shreds have no self-describing index.** Only data shards get an
   index written into their header *before* RS encoding — parity shard bytes
   are pure RS output afterward, with no recoverable index. The receiver
   needs to know which of the 256 slots each UDP datagram belongs to
   independently of the shard's own content, so `sender.rs` prefixes every
   datagram with an explicit 4-byte index at the UDP-framing layer, separate
   from the shard bytes themselves.

2. **`tc netem loss` shapes egress, not ingress.** The first version applied
   the loss rule to the *receiver's* interface, which only affects traffic
   the receiver sends out — its ARP replies, and otherwise nothing, since it
   doesn't send data back. Result: at high loss, ARP resolution itself failed
   (sender could never learn the receiver's MAC), producing a hard "0
   packets received" cliff at 70%+ that had nothing to do with FEC recovery.
   Fixed by moving the netem rule to the **sender's** egress interface — the
   actual direction pshreds travel.

3. **Recreating containers per-iteration reset ARP/MAC state every time.**
   Even after fixing (2), each fresh `docker compose run` gave the sender a
   new virtual interface (and MAC), forcing ARP re-resolution under the very
   loss conditions being tested. Fixed by making both containers
   long-lived (`sleep infinity`, `docker compose exec` per iteration instead
   of recreating), with a one-time ARP warm-up via `ping` before any loss is
   introduced.

4. **3-pass retransmission was masking the real threshold.** An earlier
   version had the sender send all 256 pshreds three times "for reliability."
   That gives each shred three independent chances to survive, which pushes
   the effective breaking point far past 75% and no longer measures the
   single-shot 64-of-256 threshold. Removed — pshreds are sent once per
   attester in Constellation anyway, so single-pass is also the more
   faithful model.

## Known limitations / deliberate scope decisions

- **`simd-accel` not enabled.** Agave enables it for its shredder; this
  crate doesn't, to avoid a C toolchain dependency for what's a
  correctness/harness deliverable rather than a perf-critical path. Easy to
  add later (`reed-solomon-erasure = { version = "6.0.0", features =
  ["simd-accel"] }`) if throughput becomes the concern.
- **Small-payload inefficiency.** For payloads under 64 bytes, most of the
  64 data shards are pure zero-padding (e.g. a 1-byte payload still produces
  64 data shards, 63 of which carry no real content). This is correct, not a
  bug — just an accepted inefficiency of using a fixed 64-way split
  regardless of payload size, consistent with the whitepaper's fixed
  `(Γp, γp)` shape.
- **The Docker harness is a local dev tool, not wired into CI**, per an
  explicit scope decision — Docker-in-Docker and `linux/amd64` vs.
  `linux/aarch64` runner differences add complexity that wasn't judged worth
  it for this deliverable.
- **A single trial per loss percentage in the sweep.** `tc netem` loss is
  probabilistic per-packet, so a single run at exactly the 75% boundary can
  land on either side of the threshold by chance (see the 76% row above,
  which passed only because noise pushed the received count to 78). The
  *exact*, deterministic boundary test is `tests/roundtrip.rs`'s
  `below_threshold_63_of_256_fails_closed` — the Docker sweep is a realistic
  sanity check on top of that, not a replacement for it.
