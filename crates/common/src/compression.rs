//! Chunk compression, decided per chunk by the client before encryption.
//!
//! Every algorithm ever used must stay decodable forever: chunks keep the one
//! they were stored with.

use std::io::{self, Read, Write};

use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use serde::{Deserialize, Serialize};

/// How a stored chunk is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// Stored as is.
    None,
    /// LZ4 frame format (standard, decodable with the `lz4` command).
    Lz4,
    /// An algorithm this version doesn't know (added by a newer version).
    #[serde(other)]
    Unknown,
}

/// When to compress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Skip high-entropy chunks, keep the result only if it saves enough.
    Auto,
    /// Always compress.
    Always,
    /// Never compress.
    Never,
}

/// A client's compression settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompressionPolicy {
    /// When to compress.
    pub mode: Mode,
    /// Above this Shannon entropy (bits per byte, 0 to 8), a chunk is stored as is.
    pub max_entropy: f64,
    /// Minimum saving to keep a compressed chunk (0.10 = 10%).
    pub min_saving: f64,
    /// The algorithm to use when compressing.
    pub algorithm: Compression,
}

impl Default for CompressionPolicy {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            max_entropy: 7.5,
            min_saving: 0.10,
            algorithm: Compression::Lz4,
        }
    }
}

/// Errors when compressing or decompressing.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// The algorithm is unknown to this version.
    #[error("unsupported compression algorithm")]
    Unsupported,
    /// The compressed data is corrupted.
    #[error("corrupted compressed data: {0}")]
    Io(#[from] io::Error),
}

/// Compresses a plaintext chunk according to the policy. Returns the algorithm
/// actually used (possibly `None`) and the bytes to store.
pub fn compress(
    policy: &CompressionPolicy,
    chunk: &[u8],
) -> Result<(Compression, Vec<u8>), CompressionError> {
    let try_it = match policy.mode {
        Mode::Never => false,
        Mode::Always => true,
        Mode::Auto => entropy(chunk) <= policy.max_entropy,
    };
    if !try_it || policy.algorithm == Compression::None {
        return Ok((Compression::None, chunk.to_vec()));
    }
    let compressed = encode(policy.algorithm, chunk)?;
    let saving = 1.0 - compressed.len() as f64 / chunk.len().max(1) as f64;
    if policy.mode == Mode::Auto && saving < policy.min_saving {
        return Ok((Compression::None, chunk.to_vec()));
    }
    Ok((policy.algorithm, compressed))
}

/// Restores the plaintext of a stored chunk.
pub fn decompress(compression: Compression, data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => {
            let mut out = Vec::new();
            FrameDecoder::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        Compression::Unknown => Err(CompressionError::Unsupported),
    }
}

fn encode(algorithm: Compression, chunk: &[u8]) -> Result<Vec<u8>, CompressionError> {
    match algorithm {
        Compression::Lz4 => {
            let mut encoder = FrameEncoder::new(Vec::new());
            encoder.write_all(chunk)?;
            encoder
                .finish()
                .map_err(|e| CompressionError::Io(io::Error::other(e)))
        }
        Compression::None => Ok(chunk.to_vec()),
        Compression::Unknown => Err(CompressionError::Unsupported),
    }
}

/// Shannon entropy of the bytes, in bits per byte (0 to 8).
pub fn entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Vec<u8> {
        b"Theca stores files as chunks. ".repeat(10_000)
    }

    fn random() -> Vec<u8> {
        let mut seed = 0x5eed_u64;
        (0..300_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    #[test]
    fn round_trip() {
        let (algo, stored) = compress(&CompressionPolicy::default(), &text()).unwrap();
        assert_eq!(algo, Compression::Lz4);
        assert!(stored.len() < text().len() / 2);
        assert_eq!(decompress(algo, &stored).unwrap(), text());
    }

    #[test]
    fn high_entropy_is_stored_as_is() {
        let data = random();
        assert!(entropy(&data) > 7.5);
        assert_eq!(
            compress(&CompressionPolicy::default(), &data).unwrap(),
            (Compression::None, data)
        );
    }

    #[test]
    fn small_saving_is_not_kept() {
        // Low byte entropy but nothing LZ4 can exploit: random bytes over 64 values.
        let data: Vec<u8> = random().iter().map(|b| b % 64).collect();
        assert!(entropy(&data) < 7.5);
        let policy = CompressionPolicy {
            min_saving: 0.5,
            ..CompressionPolicy::default()
        };
        assert_eq!(compress(&policy, &data).unwrap().0, Compression::None);
    }

    #[test]
    fn modes() {
        let always = CompressionPolicy {
            mode: Mode::Always,
            ..CompressionPolicy::default()
        };
        let never = CompressionPolicy {
            mode: Mode::Never,
            ..CompressionPolicy::default()
        };
        assert_eq!(compress(&always, &random()).unwrap().0, Compression::Lz4);
        assert_eq!(compress(&never, &text()).unwrap().0, Compression::None);
    }

    #[test]
    fn unknown_algorithm() {
        assert_eq!(
            serde_json::from_str::<Compression>("\"zstd\"").unwrap(),
            Compression::Unknown
        );
        assert!(decompress(Compression::Unknown, b"x").is_err());
    }

    #[test]
    fn entropy_bounds() {
        assert_eq!(entropy(&[]), 0.0);
        assert_eq!(entropy(&[7; 100]), 0.0);
        assert!((entropy(&(0..=255).collect::<Vec<u8>>()) - 8.0).abs() < 1e-9);
    }
}
