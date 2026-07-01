use constellation_encoder::{ConstellationEncoder, DATA_PSHREDS, PARITY_PSHREDS, TOTAL_PSHREDS};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

const PAYLOAD_SIZES: [usize; 4] = [1024, 16 * 1024, 256 * 1024, 1024 * 1024];
// 0 = no loss, 191/192 straddle the max-tolerable-loss boundary (192 is the
// max: DATA_PSHREDS=64 still present).
const NUM_MISSING: [usize; 5] = [0, 64, 128, 191, 192];

fn bench_encode(c: &mut Criterion) {
    let encoder = ConstellationEncoder::new();
    for &size in &PAYLOAD_SIZES {
        let payload = vec![0xABu8; size];
        c.bench_function(&format!("encode_payload{size}"), |b| {
            b.iter(|| encoder.encode(&payload).unwrap())
        });
    }
}

/// Extends the shape of Agave PR #5695's `run_recover_shreds`/
/// `bench_recover_shreds` (ledger/benches/make_shreds_from_entries.rs), which
/// sweeps `num_packets x num_code` for the fixed 32:32 shredder FEC set.
/// Constellation's shard counts are fixed by the whitepaper (64 data / 192
/// parity), so the equivalent sweep axes here are payload size and amount of
/// simulated loss.
fn run_recover(c: &mut Criterion, payload_size: usize, num_missing: usize) {
    let name = format!("recover_payload{payload_size}_missing{num_missing}");
    let encoder = ConstellationEncoder::new();
    let payload = vec![0xABu8; payload_size];
    let shreds = encoder.encode(&payload).unwrap();

    c.bench_function(&name, |b| {
        b.iter_batched(
            || {
                let mut opts: Vec<Option<Vec<u8>>> =
                    shreds.iter().cloned().map(Some).collect();
                for i in 0..num_missing {
                    opts[(TOTAL_PSHREDS - 1 - i) % TOTAL_PSHREDS] = None;
                }
                opts
            },
            |opts| encoder.decode(&opts).unwrap(),
            BatchSize::SmallInput,
        )
    });
}

fn bench_recover_shreds(c: &mut Criterion) {
    for &payload_size in &PAYLOAD_SIZES {
        for &num_missing in &NUM_MISSING {
            run_recover(c, payload_size, num_missing);
        }
    }
}

criterion_group!(benches, bench_encode, bench_recover_shreds);
criterion_main!(benches);

#[allow(dead_code)]
fn _assert_params() {
    assert_eq!(DATA_PSHREDS + PARITY_PSHREDS, TOTAL_PSHREDS);
}
