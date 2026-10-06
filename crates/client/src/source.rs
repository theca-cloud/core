//! What an upload reads: a file natively, a `Blob` in the browser, bytes in
//! memory (tests). The engine reads it twice (hashing, then only the chunks
//! to upload), by range.

use std::io;

use crate::rt::{BoxFuture, MaybeSync};

/// Something to upload, read by range.
pub trait Source: MaybeSync {
    /// Its size in bytes.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `len` bytes from `offset` (fewer only at the end).
    fn read(&self, offset: u64, len: usize) -> BoxFuture<'_, io::Result<Vec<u8>>>;
}

/// Bytes in memory.
pub struct MemorySource(pub Vec<u8>);

impl Source for MemorySource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&self, offset: u64, len: usize) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        let start = (offset as usize).min(self.0.len());
        let end = start.saturating_add(len).min(self.0.len());
        Box::pin(std::future::ready(Ok(self.0[start..end].to_vec())))
    }
}

/// A local file. Reads block the calling thread briefly (a few MiB from local
/// disk at a time).
#[cfg(not(target_arch = "wasm32"))]
pub struct FileSource {
    file: std::sync::Mutex<std::fs::File>,
    len: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: std::sync::Mutex::new(file),
            len,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, offset: u64, len: usize) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        use std::io::{Read, Seek, SeekFrom};
        let result = (|| {
            let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
            file.seek(SeekFrom::Start(offset))?;
            let mut buffer = Vec::with_capacity(len);
            file.by_ref().take(len as u64).read_to_end(&mut buffer)?;
            Ok(buffer)
        })();
        Box::pin(std::future::ready(result))
    }
}
