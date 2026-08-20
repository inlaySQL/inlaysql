//! An `io_uring` I/O backend for InlaySQL.
//!
//! # Why this exists
//!
//! The portable backend (`inlaysql::FileDevice`) issues `pread`/`pwrite`/
//! `fsync` and blocks the calling thread for each one. That is fine — it is
//! what SQLite does — but it costs two context switches per page and it cannot
//! express "here are eight reads, tell me when they are done".
//! `io_uring` can: submissions and completions are shared ring buffers in
//! memory, so a batch of I/O costs one syscall (and, with `SQPOLL`, none).
//!
//! # Why it is a separate crate
//!
//! `io_uring` submission is inherently `unsafe`: the kernel reads the buffer
//! pointer out of the submission queue entry after the call returns, so the
//! caller must keep the buffer alive and unmoved until the completion arrives.
//! Both `inlaysql-core` and `inlaysql` are `#![forbid(unsafe_code)]` and stay
//! that way. Confining the unsafety to one small, Linux-only crate is what
//! keeps that guarantee meaningful.
//!
//! # Safety argument for the `unsafe` in this crate
//!
//! There is exactly one unsafe operation — pushing a submission queue entry —
//! and it appears once, in `UringDevice::run`. It is sound because every
//! operation here is *synchronous*: `run` pushes one entry, calls
//! `submit_and_wait(1)`, and reaps the completion before returning. The buffer
//! it points at is borrowed by `run` for that whole span, so it cannot be
//! moved, freed or aliased while the kernel owns it. The cost of that
//! simplicity is that this backend does not yet exploit batching; that win is
//! real but it belongs with a deeper change to the tree's page-fetch path.
//!
//! # Usage
//!
//! ```no_run
//! # #[cfg(target_os = "linux")]
//! # fn main() -> Result<(), inlaysql_core::Error> {
//! use inlaysql_uring::UringDevice;
//!
//! let device = UringDevice::open("app.inlay", 32)?;
//! // hand it to `inlaysql::Database::open_on(device)`
//! # Ok(())
//! # }
//! # #[cfg(not(target_os = "linux"))]
//! # fn main() {}
//! ```

// This is the one crate in the workspace that may use `unsafe`, and it uses it
// exactly once. Everything else is `#![forbid(unsafe_code)]`; keeping the
// unsafety here, small and argued for, is what makes that guarantee mean
// something.
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::UringDevice;

/// Whether this build has a working `io_uring` backend.
///
/// `false` on every non-Linux target. Callers use it to pick a backend without
/// `cfg` attributes of their own.
pub const SUPPORTED: bool = cfg!(target_os = "linux");
