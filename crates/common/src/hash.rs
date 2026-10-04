//! Chunk hashes: `xxh3_128` of the plaintext chunk.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use twox_hash::XxHash3_128;

/// The `xxh3_128` hash of a plaintext chunk, the dedup key within a user's chunks.
///
/// Displayed and serialized as 32 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkHash(pub [u8; 16]);

impl ChunkHash {
    /// Hashes a plaintext chunk.
    pub fn of(data: &[u8]) -> Self {
        Self(XxHash3_128::oneshot(data).to_be_bytes())
    }
}

impl fmt::Display for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", u128::from_be_bytes(self.0))
    }
}

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkHash({self})")
    }
}

/// A string that isn't 32 hex characters.
#[derive(Debug, thiserror::Error)]
#[error("a chunk hash is 32 hex characters")]
pub struct ParseChunkHashError;

impl FromStr for ChunkHash {
    type Err = ParseChunkHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseChunkHashError);
        }
        let value = u128::from_str_radix(s, 16).map_err(|_| ParseChunkHashError)?;
        Ok(Self(value.to_be_bytes()))
    }
}

impl Serialize for ChunkHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ChunkHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_values() {
        // Reference value of XXH3_128 for the empty input.
        assert_eq!(
            ChunkHash::of(b"").to_string(),
            "99aa06d3014798d86001c324468d497f"
        );
    }

    #[test]
    fn hex_round_trip() {
        let hash = ChunkHash::of(b"theca");
        assert_eq!(hash.to_string().parse::<ChunkHash>().unwrap(), hash);
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(serde_json::from_str::<ChunkHash>(&json).unwrap(), hash);
    }

    #[test]
    fn rejects_invalid_hex() {
        assert!("".parse::<ChunkHash>().is_err());
        assert!("zz".repeat(16).parse::<ChunkHash>().is_err());
        assert!("+".repeat(32).parse::<ChunkHash>().is_err());
        assert!("ab".repeat(17).parse::<ChunkHash>().is_err());
    }
}
