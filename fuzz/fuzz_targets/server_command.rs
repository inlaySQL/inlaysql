//! Arbitrary bytes as a command message.
//!
//! The property is that all 256 command bytes are *handled*: `Command::Unknown`
//! is a legal outcome and a panic is not. This is the cheapest kind of
//! regression to prevent and the easiest to introduce — a new `COM_*` whose
//! body decoder trusts a length is one match arm away, and it would be
//! reachable by any authenticated client on its first packet.
//!
//! The engine is deliberately not reached. A target that planned and ran the
//! SQL in a `COM_QUERY` body would be `sql_parser` with a one-byte prefix and
//! would spend its whole budget there; what is fuzzed here is everything
//! `dispatch` does to a message *before* the statement text becomes a
//! statement.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_server::fuzz;

fuzz_target!(|data: &[u8]| {
    let dispatched = fuzz::dispatch_stateless(data);

    // The command byte is the first byte and nothing else; an empty message is
    // no command at all, which is the case a `split_first` would panic on.
    assert_eq!(dispatched.command, data.first().copied());

    // The one copy a command body makes is `String::from_utf8_lossy`, whose
    // widest expansion is a three-byte replacement character per input byte.
    // Anything above that is a body read past its own end.
    assert!(
        dispatched.owned <= 3 * data.len(),
        "a {} byte command copied {} bytes",
        data.len(),
        dispatched.owned
    );
});
