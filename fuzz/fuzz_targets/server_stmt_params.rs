//! Arbitrary bytes as the bound parameters of a `COM_STMT_EXECUTE`.
//!
//! The property is that a declared type and the bytes behind it are allowed to
//! disagree, and the decoder says so rather than reading something else. This
//! is the deepest client-controlled surface the protocol has: seventeen
//! type-specific decoders, each one reading a different number of bytes, each
//! one able to misframe every parameter after it if it reads the wrong count.
//! It is also where the one panic this path ever had lived — a `TIME`
//! parameter's four-byte day count multiplied by twenty-four, which
//! `u32::MAX` overflowed straight into a panicking connection thread.
//!
//! ## Why the input is structured
//!
//! Two things the decoder needs do not come off the wire. The parameter
//! *count* comes from the prepared statement, and the `VECTOR` dimension of
//! each placeholder comes from the plan — a client cannot say either, so a
//! target that parsed them out of the body would be fuzzing a shape no client
//! can produce. `arbitrary` supplies them beside the body instead.
//!
//! The corpus seeds are hand-built against `arbitrary`'s derived layout, which
//! reads fields front to back: for each `Vec` element a continuation byte
//! (odd continues, even stops), then the element; `bool` is one byte's low
//! bit; `u16` is two bytes little-endian; and the trailing `&[u8]` takes
//! everything left. A future `arbitrary` that changes that encoding does not
//! break this target — every byte string is still a valid input — it only
//! makes the seeds decode to something other than what their names say.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_server::fuzz;

#[path = "counted_alloc.rs"]
mod counted_alloc;

#[global_allocator]
static ALLOCATOR: counted_alloc::Counting = counted_alloc::Counting;

/// The widest text a temporal decoder renders out of a fixed number of bytes:
/// `-4294967295:59:59.999999` is 24 characters and
/// `2026-09-05 12:00:00.000001` is 26.
const MAX_RENDERED: usize = 32;

#[derive(arbitrary::Arbitrary, Debug)]
struct Execute<'a> {
    /// What the client declared each placeholder to be: a MySQL type byte and
    /// the unsigned flag.
    types: Vec<(u8, bool)>,
    /// What the *statement* says each placeholder is, for the placeholders
    /// that are embeddings.
    vector_dims: Vec<Option<u16>>,
    /// The wire bytes, starting at the NULL bitmap.
    body: &'a [u8],
}

fuzz_target!(|input: Execute| {
    // A real `param_count` is bounded by the statement that was prepared. An
    // unbounded one is not a shape a client can send, and fuzzing it would
    // only measure how long a loop takes.
    let types = if input.types.len() > fuzz::MAX_FUZZED_PARAMS {
        &input.types[..fuzz::MAX_FUZZED_PARAMS]
    } else {
        &input.types[..]
    };
    let dims: Vec<Option<usize>> = input
        .vector_dims
        .iter()
        .map(|dim| dim.map(usize::from))
        .collect();

    counted_alloc::start();
    let decoded = fuzz::decode_params(input.body, types, &dims);
    let peak = counted_alloc::peak();

    // Never allocates out of proportion to the body, whether the decode
    // succeeded or not. Sixteen bytes per placeholder for the `Vec`s the loop
    // keeps, plus the body four times over for the copies each value makes of
    // its own bytes and the doubling those copies do while they grow.
    //
    // The bound is on the *failing* path too, and that is the half that
    // matters: a decoder that allocates from a declared length and then
    // discovers the bytes are not there has already spent the memory.
    let bound = 4096 + 16 * types.len() + 4 * input.body.len();
    assert!(
        peak <= bound,
        "decoding {} parameters from a {} byte body held {peak} at once, past the {bound} bound",
        types.len(),
        input.body.len()
    );

    let Ok(params) = decoded else {
        return;
    };

    // Read inside the packet and nowhere else.
    assert!(
        params.consumed <= input.body.len(),
        "the decoder read {} bytes of a {} byte body",
        params.consumed,
        input.body.len()
    );

    // Allocation is set by the bytes the body actually carried, plus a
    // fixed-width rendering per parameter and nothing more.
    //
    // Every decoder is one of two shapes. Most copy their own bytes — a blob,
    // a string, a `VECTOR`'s components — so they own exactly what they
    // consumed. The two temporal decoders *render*: five bytes of `DATE`
    // become the ten characters `2026-09-05`, and that is not a finding, it is
    // the engine having no temporal type and taking the text a client would
    // have sent for a string column. Their widest output is a `DATETIME` with
    // microseconds at 26 characters and a `TIME` whose hour count has ten
    // digits at 24, so [`MAX_RENDERED`] bounds both with room to spare.
    //
    // What the bound therefore says is the thing worth saying: no parameter's
    // cost is proportional to a *number in the packet*. A trusted `VECTOR`
    // dimension or a believed temporal length is exactly what would break it.
    let bound = params.consumed + MAX_RENDERED * types.len();
    assert!(
        params.owned_bytes() <= bound,
        "{} parameters that read {} of {} body bytes own {} bytes, past the {} bound",
        params.owned.len(),
        params.consumed,
        input.body.len(),
        params.owned_bytes(),
        bound
    );

    // A parse that succeeded decoded every placeholder it was given.
    assert_eq!(params.owned.len(), types.len());
});
