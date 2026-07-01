//! Reed-Solomon FEC encoder parameterized after the Constellation whitepaper's
//! pslice/pshred erasure code: a (Γp, γp) = (256, 64) code where any 64-of-256
//! pshreds reconstruct the original pslice payload (a rate-1/4 code, unlike
//! Agave's 1:1 32:32 shredder FEC sets).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use reed_solomon_erasure::galois_8::ReedSolomon;
use reed_solomon_erasure::Error as RsError;

/// γp: pshreds required to reconstruct a pslice.
pub const DATA_PSHREDS: usize = 64;
/// Γp - γp: redundant pshreds.
pub const PARITY_PSHREDS: usize = 192;
/// Γp = q: total pshreds sent (one per attester).
pub const TOTAL_PSHREDS: usize = DATA_PSHREDS + PARITY_PSHREDS;

/// Per-shard header: 4-byte BE shard index + 4-byte BE total payload length.
const HEADER_LEN: usize = 8;

type CacheEntry = Arc<OnceLock<Result<Arc<ReedSolomon>, Arc<RsError>>>>;

/// Caches constructed `ReedSolomon` matrices keyed by `(data_shards, parity_shards)`,
/// mirroring Agave's `ReedSolomonCache` (`ledger/src/shredder.rs`): matrix
/// construction is the expensive part, not encode/decode, so it's built at
/// most once per shape. Read-lock fast path; write-lock only on a cache miss,
/// with the actual construction happening inside a `OnceLock` so it isn't
/// done while holding the cache's write lock.
pub struct ReedSolomonCache(RwLock<HashMap<(usize, usize), CacheEntry>>);

impl ReedSolomonCache {
    pub fn new() -> Self {
        Self(RwLock::new(HashMap::new()))
    }

    pub fn get(
        &self,
        data_shards: usize,
        parity_shards: usize,
    ) -> Result<Arc<ReedSolomon>, Arc<RsError>> {
        let key = (data_shards, parity_shards);
        let entry = self.0.read().unwrap().get(&key).cloned();
        let entry = entry.unwrap_or_else(|| {
            let mut cache = self.0.write().unwrap();
            cache.get(&key).cloned().unwrap_or_else(|| {
                let entry: CacheEntry = Arc::new(OnceLock::new());
                cache.insert(key, Arc::clone(&entry));
                entry
            })
        });
        entry
            .get_or_init(|| {
                ReedSolomon::new(data_shards, parity_shards)
                    .map(Arc::new)
                    .map_err(Arc::new)
            })
            .clone()
    }
}

impl Default for ReedSolomonCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum EncodeError {
    /// Payload length doesn't fit in the 32-bit length header.
    PayloadTooLarge,
    RsError(Arc<RsError>),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::PayloadTooLarge => write!(f, "payload exceeds maximum encodable size"),
            EncodeError::RsError(e) => write!(f, "reed-solomon encode error: {e:?}"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug)]
pub enum DecodeError {
    /// Fewer than `DATA_PSHREDS` shreds were present; decoding fails closed
    /// rather than returning partial/incorrect data.
    InsufficientShreds { have: usize, need: usize },
    /// Input shred count wasn't `TOTAL_PSHREDS`, or present shreds have
    /// inconsistent lengths.
    ShardSizeMismatch,
    /// Reconstructed data shards disagree on the encoded payload length.
    Corrupt,
    RsError(Arc<RsError>),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::InsufficientShreds { have, need } => {
                write!(f, "insufficient shreds to reconstruct: have {have}, need {need}")
            }
            DecodeError::ShardSizeMismatch => write!(f, "shred size mismatch"),
            DecodeError::Corrupt => write!(f, "reconstructed data is internally inconsistent"),
            DecodeError::RsError(e) => write!(f, "reed-solomon decode error: {e:?}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Encodes/decodes payloads into Constellation's fixed 64-data/192-parity
/// pshred layout.
pub struct ConstellationEncoder {
    rs_cache: ReedSolomonCache,
}

impl ConstellationEncoder {
    pub fn new() -> Self {
        Self {
            rs_cache: ReedSolomonCache::new(),
        }
    }

    /// Splits `payload` into `DATA_PSHREDS` equal, header-prefixed chunks and
    /// erasure-codes them into `TOTAL_PSHREDS` pshreds, any `DATA_PSHREDS` of
    /// which suffice to reconstruct `payload` via [`Self::decode`].
    pub fn encode(&self, payload: &[u8]) -> Result<Vec<Vec<u8>>, EncodeError> {
        if payload.len() > u32::MAX as usize {
            return Err(EncodeError::PayloadTooLarge);
        }
        let chunk_len = payload.len().div_ceil(DATA_PSHREDS);
        let shard_len = HEADER_LEN + chunk_len;
        let total_len = payload.len() as u32;

        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_PSHREDS);
        for i in 0..DATA_PSHREDS {
            let mut shard = vec![0u8; shard_len];
            shard[0..4].copy_from_slice(&(i as u32).to_be_bytes());
            shard[4..8].copy_from_slice(&total_len.to_be_bytes());
            let start = i * chunk_len;
            let end = (start + chunk_len).min(payload.len());
            if start < end {
                shard[HEADER_LEN..HEADER_LEN + (end - start)].copy_from_slice(&payload[start..end]);
            }
            shards.push(shard);
        }
        for _ in 0..PARITY_PSHREDS {
            shards.push(vec![0u8; shard_len]);
        }

        let rs = self
            .rs_cache
            .get(DATA_PSHREDS, PARITY_PSHREDS)
            .map_err(EncodeError::RsError)?;
        let refs: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
        rs.encode(refs).map_err(|e| EncodeError::RsError(Arc::new(e)))?;

        Ok(shards)
    }

    /// Reconstructs the original payload from up to `TOTAL_PSHREDS` pshreds,
    /// where `shreds[i] == None` means pshred `i` was not received. Requires
    /// at least `DATA_PSHREDS` present shreds; fails closed otherwise.
    pub fn decode(&self, shreds: &[Option<Vec<u8>>]) -> Result<Vec<u8>, DecodeError> {
        if shreds.len() != TOTAL_PSHREDS {
            return Err(DecodeError::ShardSizeMismatch);
        }
        let present = shreds.iter().filter(|s| s.is_some()).count();
        if present < DATA_PSHREDS {
            return Err(DecodeError::InsufficientShreds {
                have: present,
                need: DATA_PSHREDS,
            });
        }

        let shard_len = shreds
            .iter()
            .find_map(|s| s.as_ref().map(|v| v.len()))
            .ok_or(DecodeError::ShardSizeMismatch)?;
        if shard_len < HEADER_LEN {
            return Err(DecodeError::ShardSizeMismatch);
        }
        for s in shreds.iter().flatten() {
            if s.len() != shard_len {
                return Err(DecodeError::ShardSizeMismatch);
            }
        }

        let mut buffers: Vec<Vec<u8>> = shreds
            .iter()
            .map(|s| s.clone().unwrap_or_else(|| vec![0u8; shard_len]))
            .collect();
        let present_mask: Vec<bool> = shreds.iter().map(|s| s.is_some()).collect();

        let rs = self
            .rs_cache
            .get(DATA_PSHREDS, PARITY_PSHREDS)
            .map_err(DecodeError::RsError)?;
        let mut slices: Vec<(&mut [u8], bool)> = buffers
            .iter_mut()
            .zip(present_mask.iter())
            .map(|(b, &present)| (b.as_mut_slice(), present))
            .collect();
        // Only the data shards are needed back (unlike Agave's shred::recover,
        // which also needs parity shards for repair-shred gossip), so
        // reconstruct_data is the cheaper, correct choice here.
        rs.reconstruct_data(&mut slices)
            .map_err(|e| DecodeError::RsError(Arc::new(e)))?;

        let mut total_len: Option<u32> = None;
        for buf in buffers.iter().take(DATA_PSHREDS) {
            let header_total = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            match total_len {
                None => total_len = Some(header_total),
                Some(t) if t == header_total => {}
                Some(_) => return Err(DecodeError::Corrupt),
            }
        }
        let total_len = total_len.ok_or(DecodeError::Corrupt)? as usize;

        let mut payload = Vec::with_capacity(DATA_PSHREDS * (shard_len - HEADER_LEN));
        for buf in buffers.iter().take(DATA_PSHREDS) {
            payload.extend_from_slice(&buf[HEADER_LEN..]);
        }
        payload.truncate(total_len);
        Ok(payload)
    }
}

impl Default for ConstellationEncoder {
    fn default() -> Self {
        Self::new()
    }
}
