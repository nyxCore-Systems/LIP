use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// Header sent over the Unix socket so the client can seek the mmap region.
/// Carries the offset and byte length of the FlatBuffers blob written by the daemon.
/// (Used in v0.2+ when FlatBuffers IPC replaces JSON.)
#[derive(Debug, Clone, Copy)]
pub struct MmapHeader {
    pub offset: u64,
    pub length: u64,
}

impl MmapHeader {
    pub const SIZE: usize = 16; // 2 × u64

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[..8].copy_from_slice(&self.offset.to_be_bytes());
        buf[8..].copy_from_slice(&self.length.to_be_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            offset: u64::from_be_bytes(buf[..8].try_into().unwrap()),
            length: u64::from_be_bytes(buf[8..].try_into().unwrap()),
        }
    }
}

/// Shared memory region managed by the daemon (write side).
///
/// The daemon writes FlatBuffers blobs into this region and sends an
/// `MmapHeader` over the socket. The client maps the same file with
/// `MAP_PRIVATE` and reads the blob at the declared offset.
pub struct SharedMmapRegion {
    map:  MmapMut,
    path: PathBuf,
    /// Cursor: byte offset for the next write.
    head: usize,
}

impl SharedMmapRegion {
    /// Create (or truncate) a memory-mapped file at `path` with the given `size`.
    pub fn create(path: impl AsRef<Path>, size: usize) -> anyhow::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(size as u64)?;
        // SAFETY: we just created this file; no other process holds it.
        let map = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self { map, path, head: 0 })
    }

    /// Write `data` at the next available offset.
    /// Returns the `MmapHeader` the client needs to locate the blob.
    pub fn write_blob(&mut self, data: &[u8]) -> anyhow::Result<MmapHeader> {
        let offset = self.head;
        let end    = offset + data.len();
        if end > self.map.len() {
            anyhow::bail!(
                "mmap region full: need {} bytes at offset {}, capacity {}",
                data.len(), offset, self.map.len()
            );
        }
        self.map[offset..end].copy_from_slice(data);
        self.map.flush_range(offset, data.len())?;
        self.head = end;
        Ok(MmapHeader {
            offset: offset as u64,
            length: data.len() as u64,
        })
    }

    /// Reset the write cursor to 0 (reuse the region for a new batch).
    pub fn reset(&mut self) {
        self.head = 0;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn capacity(&self) -> usize {
        self.map.len()
    }
}
