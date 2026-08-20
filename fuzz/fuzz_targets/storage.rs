//! Arbitrary bytes as a database image.
//!
//! Opening a file somebody else wrote is the most exposed surface a storage
//! engine has. The property is that every malformed image is *rejected*, not
//! that it is understood: an error is a pass, a panic is a finding, and a
//! successful open of nonsense is a finding too if it then misbehaves.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_core::sim::SimDisk;
use inlaysql_core::storage::TreeStorage;
use inlaysql_core::Storage;

fuzz_target!(|data: &[u8]| {
    let Ok(storage) = TreeStorage::open_on(SimDisk::with_image(512, data)) else {
        return;
    };
    // If it opened, reading it must not panic either — a corrupt page reached
    // through a valid-looking header is exactly the interesting case.
    let _ = inlaysql_core::traits::scan_all(&storage, "t");
    let _ = storage.get_meta("catalog");
    let _ = storage.get_row("t", 1);
});
