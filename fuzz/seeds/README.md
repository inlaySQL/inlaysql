# Seeds for the `server_*` targets

These are *inputs*, not a corpus. `fuzz/corpus/` is where libFuzzer writes what
it discovers and is `.gitignore`d for that reason — a directory that grows on
every run is not a thing to commit. The seeds below are hand-written, do not
change unless somebody changes them, and are passed to libFuzzer as a
read-only second corpus directory: `trust.yml` runs
`cargo fuzz run <target> fuzz/corpus/<target> fuzz/seeds/<target>`, which reads
both and writes only the first.


The four `server_*` targets are the only ones in this directory that ship a
corpus, and the reason is what their first bytes mean. `sql_parser` takes
arbitrary text and `storage`, `row_codec` and `json_parser` take arbitrary
bytes, both of which are dense in interesting inputs — a coverage-guided
fuzzer finds its way in from nothing. A MySQL handshake response has a
mandatory capability bit at byte 4 and a 23-byte reserved field after it; a
campaign starting from nothing spends its budget failing at byte 4 and never
reaches the fields behind it.

So these seeds are the shapes that are expensive to discover and cheap to
write down. Every file is named after what it is, and the names are the
documentation:

* **`server_packet_frame/`** — the framing layer's edges. The
  `claims-sixteen-mebibytes-*` pair is the input that cost this server 16 MiB
  per connection before the payload read was chunked; the truncation cases
  cover a header and a body that stop early; `a-thousand-empty-packets`
  exercises the continuation loop.
* **`server_handshake/`** — a real `HandshakeResponse41`, the 32-byte
  `SSLRequest`, the same bit set on a full post-upgrade response (the
  distinction that has to be a phase and not a flag), a length-encoded token
  claiming `u64::MAX`, and the response truncated at each field boundary.
* **`server_stmt_params/`** — one encoded parameter of each handled MySQL type
  byte, the `TIME` encoding whose day count is `u32::MAX`, a `DATETIME`
  declaring a length of 100, and the `VECTOR` refusals. These are
  `arbitrary`-structured; `fuzz_targets/server_stmt_params.rs` documents the
  layout they were built against and what happens if `arbitrary` ever changes
  it.
* **`server_command/`** — the ten command bytes this server names, three it
  does not, and a body for each command that decodes one. All 256 bytes are
  covered exhaustively by a unit test in `crates/inlaysql-server/src/fuzz.rs`,
  which is a better instrument than a corpus for a property that is finite.

Two inputs the framing seeds deliberately do *not* include: a packet of
exactly `MAX_PAYLOAD` bytes, and the five full-size packets it takes to reach
the `MAX_MESSAGE` refusal. Both are 16 MiB or more on disk, and libFuzzer
sizes its whole campaign from the largest seed it is given, so checking them
in would trade the entire throughput of the target for two inputs that already
have unit tests in `packet.rs`.
