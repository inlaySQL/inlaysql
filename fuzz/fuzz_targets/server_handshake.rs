//! Arbitrary bytes as a client's login packet.
//!
//! The property is that a login *fails*, not that it parses: this is the
//! second thing an unauthenticated peer can send and the first thing that is
//! interpreted field by field, so every refusal has to be a `Result` and never
//! a panic, and no field may be larger than the packet it came out of.
//!
//! Both handshake phases are driven from the same input. Before a TLS upgrade
//! a client that sets `CLIENT_SSL` has sent 32 bytes and stopped; after the
//! upgrade the full response arrives with that same bit still set, because the
//! capability flags describe the connection rather than a request.
//! Distinguishing those by the flag instead of the phase produced a
//! credential-free login and an unloginnable real client at once, which is why
//! the target fuzzes both rather than picking one.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_server::fuzz;

#[path = "counted_alloc.rs"]
mod counted_alloc;

#[global_allocator]
static ALLOCATOR: counted_alloc::Counting = counted_alloc::Counting;

fuzz_target!(|data: &[u8]| {
    for expect_ssl_request in [true, false] {
        counted_alloc::start();
        let parsed = fuzz::parse_handshake_response(data, expect_ssl_request);
        let peak = counted_alloc::peak();

        // Never allocates out of proportion to the packet, whether the parse
        // succeeded or not.
        //
        // The response owns a copy of the user name, the token, the database
        // and the plugin name, and every one of them is sized by a length in
        // the packet — the length-encoded token form can declare `u64::MAX`,
        // which is the shape this bound exists to catch. Four times the input
        // rather than once, because the copies are `String` and `Vec` and both
        // may double while they grow.
        let bound = 4096 + 4 * data.len();
        assert!(
            peak <= bound,
            "parsing a {} byte handshake held {peak} at once, past the {bound} bound",
            data.len()
        );

        // An error is the expected outcome for almost every input. Only a
        // parse that succeeded has anything further to assert about.
        let Ok(handshake) = parsed else {
            continue;
        };

        // Every field is a copy taken out of the packet, so all of them
        // together cannot exceed the packet. A larger number means a length
        // was believed rather than checked.
        assert!(
            handshake.owned_bytes() <= data.len(),
            "a {} byte handshake response owns {} bytes: {handshake:?}",
            data.len(),
            handshake.owned_bytes()
        );

        // A response this server accepted claims the 4.1 protocol; nothing
        // else reaches the account lookup.
        assert!(
            handshake.capabilities & fuzz::CLIENT_PROTOCOL_41 != 0,
            "a pre-4.1 response parsed: {handshake:?}"
        );
    }
});
