//! The Linux implementation. See the crate documentation for the safety
//! argument covering the single `unsafe` block below.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use io_uring::{opcode, types, IoUring};

use inlaysql_core::btree::Device;
use inlaysql_core::{Error, Result};

/// A database file driven through an `io_uring` submission queue.
///
/// Implements the same [`Device`] contract as the blocking backend: byte
/// offsets, writes that are not durable until [`Device::sync`], and reads that
/// see everything written so far. Only the mechanism differs, so the engine
/// above it — and every test written against it — is unchanged.
pub struct UringDevice {
    file: File,
    /// `Device::read` takes `&self`, but submitting needs `&mut` access to the
    /// queues. The ring is never shared across threads (the device lives on
    /// InlaySQL's I/O thread), so a `RefCell` is the whole synchronisation
    /// story.
    ring: RefCell<IoUring>,
}

impl UringDevice {
    /// Open (or create) `path` with a submission queue of `entries` slots.
    ///
    /// `entries` is rounded up to a power of two by the kernel. 32 is a
    /// sensible default: deep enough that the ring is never the bottleneck for
    /// a single-threaded engine, small enough to be free.
    pub fn open(path: impl AsRef<Path>, entries: u32) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_error)?;
        let ring = IoUring::new(entries.max(1)).map_err(|error| {
            Error::Storage(format!(
                "io_uring is unavailable on this kernel ({error}); \
                 use the blocking backend instead"
            ))
        })?;
        Ok(Self {
            file,
            ring: RefCell::new(ring),
        })
    }

    /// Submit one prepared entry and wait for its completion.
    ///
    /// Returns the kernel's result, which is a byte count for reads and writes
    /// and zero for `fsync`.
    fn run(&self, entry: io_uring::squeue::Entry) -> Result<usize> {
        let mut ring = self
            .ring
            .try_borrow_mut()
            .map_err(|_| Error::Storage("the io_uring ring is already in use".to_string()))?;

        // SAFETY: the buffer this entry points at is borrowed by the caller for
        // the whole of this function, and the completion is reaped before
        // returning, so the kernel never sees a dangling or moved buffer. See
        // the crate-level safety argument.
        unsafe {
            ring.submission()
                .push(&entry)
                .map_err(|error| Error::Storage(format!("io_uring submission failed: {error}")))?;
        }

        ring.submit_and_wait(1).map_err(io_error)?;

        let completion = ring
            .completion()
            .next()
            .ok_or_else(|| Error::Storage("io_uring returned no completion".to_string()))?;
        let result = completion.result();
        if result < 0 {
            return Err(io_error(io::Error::from_raw_os_error(-result)));
        }
        Ok(result as usize)
    }
}

impl Device for UringDevice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let fd = types::Fd(self.file.as_raw_fd());
        let mut done = 0;
        while done < buf.len() {
            let remaining = buf.len() - done;
            let entry = opcode::Read::new(fd, buf[done..].as_mut_ptr(), remaining as u32)
                .offset((offset + done) as u64)
                .build();
            match self.run(entry)? {
                // A short read at end of file. The contract is read-exactly,
                // matching `pread`-based `read_exact_at`, so this is an error —
                // and it is the signal `CowBTree::open_or_create` reads as
                // "this device holds no database yet".
                0 => {
                    return Err(Error::Storage(format!(
                        "short read at offset {}: wanted {} more bytes",
                        offset + done,
                        remaining
                    )))
                }
                n => done += n,
            }
        }
        Ok(())
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let fd = types::Fd(self.file.as_raw_fd());
        let mut done = 0;
        while done < data.len() {
            let remaining = data.len() - done;
            let entry = opcode::Write::new(fd, data[done..].as_ptr(), remaining as u32)
                .offset((offset + done) as u64)
                .build();
            match self.run(entry)? {
                0 => {
                    return Err(Error::Storage(format!(
                        "short write at offset {}: {} bytes not written",
                        offset + done,
                        remaining
                    )))
                }
                n => done += n,
            }
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let entry = opcode::Fsync::new(types::Fd(self.file.as_raw_fd())).build();
        self.run(entry).map(|_| ())
    }
}

fn io_error(error: io::Error) -> Error {
    Error::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("inlaysql-uring-{name}-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Open a ring, or `None` when this environment will not give us one.
    ///
    /// Being compiled for Linux is not the same as being *allowed* `io_uring`.
    /// A container under Docker's default seccomp profile is the common case —
    /// `io_uring_setup` comes back `EPERM` — and a hardened kernel can refuse
    /// it too. `UringDevice::open` already reports that as an error telling the
    /// caller to use the blocking backend, which is the library behaving
    /// correctly; a test that unwrapped it turned "this sandbox forbids
    /// io_uring" into "io_uring is broken".
    ///
    /// So these tests assert that the ring works *where there is one*, and skip
    /// where there is not. The gap that leaves is real and worth stating: run
    /// them somewhere `io_uring` is permitted — CI's Ubuntu runners are, a
    /// default Docker container is not — or this backend goes unexercised.
    fn ring_or_skip(path: &std::path::Path, entries: u32) -> Option<UringDevice> {
        match UringDevice::open(path, entries) {
            Ok(device) => Some(device),
            Err(error) => {
                eprintln!("skipping: io_uring is not available here ({error})");
                None
            }
        }
    }

    #[test]
    fn round_trips_bytes_through_the_ring() {
        let path = temp_path("roundtrip");
        let Some(mut device) = ring_or_skip(&path, 8) else {
            return;
        };
        device.write(4096, b"inlaysql").unwrap();
        device.sync().unwrap();

        let mut buf = [0u8; 8];
        device.read(4096, &mut buf).unwrap();
        assert_eq!(&buf, b"inlaysql");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_zeros() {
        let path = temp_path("short");
        let Some(device) = ring_or_skip(&path, 8) else {
            return;
        };
        let mut buf = [0u8; 24];
        assert!(device.read(0, &mut buf).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
