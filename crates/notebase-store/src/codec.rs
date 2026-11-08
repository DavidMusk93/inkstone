//! Byte-level encoding of what lands in SQLite.
//!
//! Every payload is zstd-compressed behind a 4-byte magic header. The header
//! matters: without it a stored blob that happens to be valid zstd would be
//! "decoded" into garbage, and a payload that does not compress well would be
//! indistinguishable from a compressed one.

use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"IKS1";
const LEVEL: i32 = 3;

/// Compress for storage. Tiny payloads are stored raw (header only) — the zstd
/// frame overhead is bigger than the win below a few hundred bytes.
pub fn pack(raw: &[u8]) -> Vec<u8> {
    if raw.len() < 256 {
        let mut out = Vec::with_capacity(MAGIC.len() + raw.len());
        out.extend_from_slice(MAGIC);
        out.push(0);
        out.extend_from_slice(raw);
        return out;
    }
    match zstd::bulk::compress(raw, LEVEL) {
        Ok(compressed) if compressed.len() + 1 < raw.len() => {
            let mut out = Vec::with_capacity(MAGIC.len() + 1 + compressed.len());
            out.extend_from_slice(MAGIC);
            out.push(1);
            out.extend_from_slice(&compressed);
            out
        }
        _ => {
            let mut out = Vec::with_capacity(MAGIC.len() + 1 + raw.len());
            out.extend_from_slice(MAGIC);
            out.push(0);
            out.extend_from_slice(raw);
            out
        }
    }
}

pub fn unpack(bytes: &[u8]) -> Result<Vec<u8>> {
    let (magic, rest) = bytes
        .split_at_checked(MAGIC.len())
        .ok_or_else(|| Error::Corrupt("payload is shorter than its header".to_string()))?;
    if magic != MAGIC {
        return Err(Error::Corrupt("payload header is not notebase".to_string()));
    }
    let (flag, body) = rest
        .split_first()
        .ok_or_else(|| Error::Corrupt("payload has no encoding flag".to_string()))?;
    match flag {
        0 => Ok(body.to_vec()),
        1 => {
            zstd::bulk::decompress(body, 64 << 20).map_err(|e| Error::Corrupt(format!("zstd: {e}")))
        }
        other => Err(Error::Corrupt(format!("unknown encoding flag {other}"))),
    }
}

/// Content address for blobs: BLAKE3 of the uncompressed bytes.
pub fn content_hash(raw: &[u8]) -> String {
    blake3::hash(raw).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_small_and_large() {
        for payload in [
            b"".to_vec(),
            b"x".to_vec(),
            vec![b'a'; 511],
            "笔记 · notebase ".repeat(200).into_bytes(),
        ] {
            let packed = pack(&payload);
            assert_eq!(unpack(&packed).unwrap(), payload);
        }
    }

    #[test]
    fn rejects_foreign_bytes() {
        assert!(unpack(b"not ours").is_err());
        assert!(unpack(b"").is_err());
    }

    #[test]
    fn compresses_repetitive_payloads() {
        let payload = vec![b'z'; 64 * 1024];
        let packed = pack(&payload);
        assert!(packed.len() < payload.len() / 10, "{}", packed.len());
    }
}
