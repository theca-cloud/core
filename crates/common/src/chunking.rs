//! Content-defined chunking: FastCDC 2020 with Theca's fixed sizes.
//!
//! These parameters are a global constant: changing them doesn't break anything,
//! but new uploads stop deduplicating against existing chunks.

use std::io::{self, Read};

use fastcdc::v2020::{AsyncStreamCDC, ChunkData, Error, Normalization, StreamCDC};
use futures::{AsyncRead, Stream, StreamExt};

/// Minimum chunk size (512 KiB).
pub const MIN_SIZE: usize = 512 * 1024;
/// Average chunk size (1 MiB).
pub const AVG_SIZE: usize = 1024 * 1024;
/// Maximum chunk size (8 MiB), which also bounds the memory used per chunk.
pub const MAX_SIZE: usize = 8 * 1024 * 1024;

/// A chunk of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position of the chunk's first byte in the file.
    pub offset: u64,
    /// The chunk's bytes (plaintext).
    pub data: Vec<u8>,
}

/// Splits a reader into chunks. Holds at most one chunk in memory.
pub fn chunks<R: Read>(reader: R) -> impl Iterator<Item = io::Result<Chunk>> {
    StreamCDC::with_level(reader, MIN_SIZE, AVG_SIZE, MAX_SIZE, Normalization::Level1).map(convert)
}

/// Splits an async reader into chunks (e.g. a browser `Blob` read from WASM).
/// Produces the same chunks as [`chunks`] for the same bytes.
pub struct AsyncChunker<R>(AsyncStreamCDC<R>);

impl<R: AsyncRead + Unpin> AsyncChunker<R> {
    /// Wraps an async reader.
    pub fn new(reader: R) -> Self {
        Self(AsyncStreamCDC::with_level(
            reader,
            MIN_SIZE,
            AVG_SIZE,
            MAX_SIZE,
            Normalization::Level1,
        ))
    }

    /// The chunks, in order. Holds at most one chunk in memory.
    pub fn stream(&mut self) -> impl Stream<Item = io::Result<Chunk>> + '_ {
        self.0.as_stream().map(convert)
    }
}

fn convert(result: Result<ChunkData, Error>) -> io::Result<Chunk> {
    match result {
        Ok(chunk) => Ok(Chunk {
            offset: chunk.offset,
            data: chunk.data,
        }),
        Err(Error::IoError(e)) => Err(e),
        Err(e) => Err(io::Error::other(format!("{e:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes (xorshift64), so the golden test needs no fixture file.
    fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    fn boundaries(data: &[u8]) -> Vec<(u64, usize)> {
        chunks(data)
            .map(|c| c.unwrap())
            .map(|c| (c.offset, c.data.len()))
            .collect()
    }

    /// If this fails after a dependency update, the cut points changed: new uploads
    /// would stop deduplicating against existing chunks. Don't just update the values.
    #[test]
    fn golden_boundaries() {
        let data = pseudo_random(20 * 1024 * 1024, 0x7e5ca);
        assert_eq!(boundaries(&data), GOLDEN);
    }

    /// Recorded with `fastcdc` 5.0.0.
    const GOLDEN: &[(u64, usize)] = &[
        (0, 820812),
        (820812, 2303768),
        (3124580, 1756384),
        (4880964, 1697793),
        (6578757, 2175876),
        (8754633, 2437828),
        (11192461, 631678),
        (11824139, 1279019),
        (13103158, 1456688),
        (14559846, 1525267),
        (16085113, 1202217),
        (17287330, 1733890),
        (19021220, 1136537),
        (20157757, 813763),
    ];

    #[test]
    fn chunks_cover_the_input_within_bounds() {
        let data = pseudo_random(20 * 1024 * 1024, 42);
        let all: Vec<Chunk> = chunks(&data[..]).map(|c| c.unwrap()).collect();
        let mut offset = 0;
        for (i, chunk) in all.iter().enumerate() {
            assert_eq!(chunk.offset, offset);
            assert!(chunk.data.len() <= MAX_SIZE);
            if i + 1 < all.len() {
                assert!(chunk.data.len() >= MIN_SIZE);
            }
            offset += chunk.data.len() as u64;
        }
        assert_eq!(all.concat_data(), data);
    }

    trait ConcatData {
        fn concat_data(&self) -> Vec<u8>;
    }
    impl ConcatData for Vec<Chunk> {
        fn concat_data(&self) -> Vec<u8> {
            self.iter().flat_map(|c| c.data.iter().copied()).collect()
        }
    }

    #[test]
    fn small_and_empty_inputs() {
        assert_eq!(boundaries(&[]), vec![]);
        assert_eq!(boundaries(b"hello"), vec![(0, 5)]);
    }

    #[test]
    fn async_matches_sync() {
        let data = pseudo_random(10 * 1024 * 1024, 7);
        let mut chunker = AsyncChunker::new(futures::io::Cursor::new(data.clone()));
        let async_chunks: Vec<Chunk> =
            futures::executor::block_on(chunker.stream().map(|c| c.unwrap()).collect());
        let sync_chunks: Vec<Chunk> = chunks(&data[..]).map(|c| c.unwrap()).collect();
        assert_eq!(async_chunks, sync_chunks);
    }
}
