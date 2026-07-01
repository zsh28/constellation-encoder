//! Round-trip correctness tests: encode -> drop shreds -> decode -> verify
//! original bytes recovered. This is the only test that actually proves the
//! Reed-Solomon integration is correct (the underlying RS math is already
//! tested upstream by `reed-solomon-erasure`); these tests validate the
//! chunking/header/padding scheme and that encode/decode are wired up
//! correctly against Constellation's fixed 64-data/192-parity/256-total
//! pshred layout.

use constellation_encoder::{ConstellationEncoder, DecodeError, DATA_PSHREDS, TOTAL_PSHREDS};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

fn shreds_with_present(all: &[Vec<u8>], present_indices: &[usize]) -> Vec<Option<Vec<u8>>> {
    let mut out: Vec<Option<Vec<u8>>> = vec![None; TOTAL_PSHREDS];
    for &i in present_indices {
        out[i] = Some(all[i].clone());
    }
    out
}

fn roundtrip(payload: &[u8], present_indices: &[usize]) -> Result<Vec<u8>, DecodeError> {
    let encoder = ConstellationEncoder::new();
    let shreds = encoder.encode(payload).expect("encode should succeed");
    assert_eq!(shreds.len(), TOTAL_PSHREDS);
    let input = shreds_with_present(&shreds, present_indices);
    encoder.decode(&input)
}

#[test]
fn roundtrip_all_data_shreds_present() {
    let payload = b"the quick brown fox jumps over the lazy dog".repeat(50);
    let present: Vec<usize> = (0..DATA_PSHREDS).collect();
    let recovered = roundtrip(&payload, &present).expect("decode should succeed");
    assert_eq!(recovered, payload);
}

#[test]
fn roundtrip_pure_parity_recovery() {
    // Exactly 64 parity shreds present, zero data shreds: must still recover.
    let payload = b"pslice payload reconstructed entirely from pshreds".to_vec();
    let present: Vec<usize> = (DATA_PSHREDS..DATA_PSHREDS * 2).collect();
    let recovered = roundtrip(&payload, &present).expect("decode should succeed");
    assert_eq!(recovered, payload);
}

#[test]
fn roundtrip_random_64_of_256_subsets() {
    let payload = vec![0x42u8; 10_000];
    for seed in 0u64..10 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut indices: Vec<usize> = (0..TOTAL_PSHREDS).collect();
        indices.shuffle(&mut rng);
        let present = &indices[..DATA_PSHREDS];
        let recovered = roundtrip(&payload, present)
            .unwrap_or_else(|e| panic!("seed {seed} failed to decode: {e}"));
        assert_eq!(recovered, payload, "seed {seed} produced wrong payload");
    }
}

#[test]
fn roundtrip_above_threshold_slack() {
    let payload = b"redundant attestations arriving with slack".to_vec();
    for count in [100, 200, TOTAL_PSHREDS] {
        let present: Vec<usize> = (0..count).collect();
        let recovered = roundtrip(&payload, &present).expect("decode should succeed");
        assert_eq!(recovered, payload);
    }
}

#[test]
fn below_threshold_63_of_256_fails_closed() {
    let payload = b"not enough attesters responded".to_vec();
    let present: Vec<usize> = (0..DATA_PSHREDS - 1).collect();
    let err = roundtrip(&payload, &present).expect_err("decode should fail closed");
    match err {
        DecodeError::InsufficientShreds { have, need } => {
            assert_eq!(have, DATA_PSHREDS - 1);
            assert_eq!(need, DATA_PSHREDS);
        }
        other => panic!("expected InsufficientShreds, got {other:?}"),
    }
}

#[test]
fn roundtrip_various_non_aligned_payload_lengths() {
    let present: Vec<usize> = (0..DATA_PSHREDS).collect();
    for &len in &[0usize, 1, 63, 64, 65, 127, 128, 129, 1000, 65537] {
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let recovered = roundtrip(&payload, &present)
            .unwrap_or_else(|e| panic!("len {len} failed to decode: {e}"));
        assert_eq!(recovered, payload, "len {len} mismatch");
    }
}

#[test]
fn roundtrip_empty_payload() {
    let present: Vec<usize> = (0..DATA_PSHREDS).collect();
    let recovered = roundtrip(&[], &present).expect("decode should succeed");
    assert_eq!(recovered, Vec::<u8>::new());
}

#[test]
fn roundtrip_single_byte_payload() {
    let present: Vec<usize> = (64..64 + DATA_PSHREDS).collect();
    let recovered = roundtrip(&[0x99], &present).expect("decode should succeed");
    assert_eq!(recovered, vec![0x99]);
}

#[test]
fn roundtrip_large_payload_multi_mb() {
    let payload: Vec<u8> = (0..(3 * 1024 * 1024)).map(|i| (i % 256) as u8).collect();
    let mut rng = StdRng::seed_from_u64(42);
    let mut indices: Vec<usize> = (0..TOTAL_PSHREDS).collect();
    indices.shuffle(&mut rng);
    let present = &indices[..DATA_PSHREDS];
    let recovered = roundtrip(&payload, present).expect("decode should succeed");
    assert_eq!(recovered, payload);
}
