//! The database file, as a `Vec<u8>`.

use std::cell::Cell;

use inlaysql_core::btree::Device;
use inlaysql_core::{Error, Result};

/// A byte-addressable device backed by memory.
///
/// The layout is identical to what the native build writes to a file — this is
/// the same `Device` seam, with a vector where the file would be — so the bytes
/// are portable in both directions.
///
/// `sync` is a no-op, and that is the honest description of durability here:
/// there is nothing below this to flush to. Durability in a browser is whatever
/// the embedder does with [`MemoryDevice::bytes`].
#[derive(Debug, Default, Clone)]
pub struct MemoryDevice {
    bytes: Vec<u8>,
    /// Commits that have completed on this device. See
    /// [`MemoryDevice::commit_generation`].
    generation: Cell<u64>,
    /// Whether a core handle has opted this device into page reuse.
    reuse_enabled: Cell<bool>,
}

impl MemoryDevice {
    /// A device with nothing on it.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A device holding an existing database image.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            generation: Cell::new(0),
            reuse_enabled: Cell::new(false),
        }
    }

    /// The current image.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Device for MemoryDevice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let end = offset.checked_add(buf.len()).ok_or_else(|| {
            Error::Storage(alloc_format(offset, buf.len(), "read range overflows"))
        })?;
        // Reading past the end is an error rather than zeros, matching
        // `read_exact_at`. `CowBTree::open_or_create` reads that as "this
        // device holds no database yet", which is what makes an empty
        // `MemoryDevice` create one.
        if end > self.bytes.len() {
            return Err(Error::Storage(alloc_format(
                offset,
                buf.len(),
                "read past the end of the device",
            )));
        }
        buf.copy_from_slice(&self.bytes[offset..end]);
        Ok(())
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len()).ok_or_else(|| {
            Error::Storage(alloc_format(offset, data.len(), "write range overflows"))
        })?;
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[offset..end].copy_from_slice(data);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_commit(&self) -> Option<u64> {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        Some(generation)
    }

    /// Counting commits is trivially authoritative here: this device *is* the
    /// database, it is owned by one `Database` in one WASM instance, and there
    /// is no file for anybody else to open. Reporting it keeps the browser
    /// build off the log-scanning path that
    /// [`inlaysql_core::btree::CowBTree::refresh`] would otherwise take before
    /// every statement — see [`Device::commit_generation`] for the contract and
    /// for what would have to change if this device were ever shared.
    fn commit_generation(&self) -> Option<u64> {
        Some(self.generation.get())
    }

    fn note_page_reuse_enabled(&self) {
        self.reuse_enabled.set(true);
    }

    fn page_reuse_enabled(&self) -> bool {
        self.reuse_enabled.get()
    }
}

fn alloc_format(offset: usize, len: usize, what: &str) -> String {
    format!("{what}: {len} bytes at offset {offset}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_grow_the_device_and_read_back() {
        let mut device = MemoryDevice::empty();
        device.write(100, b"inlaysql").unwrap();
        assert_eq!(device.bytes().len(), 108);

        let mut buf = [0u8; 8];
        device.read(100, &mut buf).unwrap();
        assert_eq!(&buf, b"inlaysql");
        // The gap is zeros, as a sparse file would be.
        assert!(device.bytes()[..100].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_zeros() {
        let device = MemoryDevice::from_bytes(b"short");
        let mut buf = [0u8; 24];
        assert!(device.read(0, &mut buf).is_err());
    }

    #[test]
    fn an_empty_device_refuses_a_header_read() {
        // This is the signal `open_or_create` reads as "create me".
        let mut buf = [0u8; 24];
        assert!(MemoryDevice::empty().read(0, &mut buf).is_err());
    }
}
