//! Arbitrary bytes as a MySQL connection's framing.
//!
//! The property is not "it frames" — most inputs are not packets — but that
//! the framing layer's cost is set by the bytes that *arrived* rather than by
//! the number a client wrote in a header. Three of those four header bytes are
//! a length, and every one of them is readable before a credential has been
//! checked, so this is the surface an unauthenticated peer reaches first.
//!
//! One input is a whole session rather than one packet: the continuation rule
//! (a payload of exactly `MAX_PAYLOAD` means "more follows") is a loop, and a
//! loop is only interesting when it can run more than once.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_server::fuzz;

#[path = "counted_alloc.rs"]
mod counted_alloc;

#[global_allocator]
static ALLOCATOR: counted_alloc::Counting = counted_alloc::Counting;

/// What the harness itself holds, whatever the input is: two eight-kibibyte
/// stream buffers and one sixty-four kibibyte read chunk, with room for the
/// `Vec` of framed messages to double.
const FIXED: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    counted_alloc::start();
    let framing = fuzz::read_messages(data);
    let peak = counted_alloc::peak();

    // Never allocates out of proportion to the bytes that arrived.
    //
    // This is the invariant the whole apparatus exists for. Three of the four
    // header bytes are a length, and committing that length before a byte of
    // it has been received is what made four bytes and then silence cost this
    // server sixteen mebibytes per connection, held until a read timeout whose
    // default is eight hours, from a peer that had not authenticated.
    // `MAX_MESSAGE` bounds the reassembled total and never bounded this; a
    // documented bound on an adjacent quantity is worse than no bound, because
    // it stops the question being asked.
    //
    // Reverting the chunked read in `packet.rs` and running this target fails
    // here in under a second, which is the only way to know the assertion is
    // load-bearing rather than decorative.
    let bound = FIXED + 4 * data.len();
    assert!(
        peak <= bound,
        "framing {} bytes held {peak} at once, past the {bound} bound",
        data.len()
    );

    // Never loops unboundedly. Every iteration of every loop in the framing
    // path asks the socket for bytes, and a read either delivers at least one
    // byte — of which there are only `data.len()` — or reports the end of the
    // stream and ends the read it was part of. A count is the instrument
    // rather than a clock so the assertion means the same thing on a laptop
    // and on a loaded runner.
    assert!(
        framing.reads <= 2 * data.len() + 64,
        "{} reads for {} bytes is not a bounded loop",
        framing.reads,
        data.len()
    );

    // Never invents payload. A message is stitched out of bytes that crossed
    // the wire, so the framing cannot hand back more than it was given — and
    // a length taken from a header rather than from the socket is exactly how
    // it would.
    assert!(
        framing.payload_bytes() <= data.len(),
        "framing handed back {} payload bytes from {} input bytes",
        framing.payload_bytes(),
        data.len()
    );

    // Never past the documented ceiling. `MAX_MESSAGE` is the bound the module
    // doc claims, asserted rather than trusted.
    for message in &framing.messages {
        assert!(
            message.len() <= fuzz::MAX_MESSAGE,
            "a message of {} bytes is past the {} byte limit",
            message.len(),
            fuzz::MAX_MESSAGE
        );
    }
});
