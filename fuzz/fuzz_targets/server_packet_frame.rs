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

fuzz_target!(|data: &[u8]| {
    let framing = fuzz::read_messages(data);

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
