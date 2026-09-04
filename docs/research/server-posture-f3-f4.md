# Track F3/F4 — safe-by-default binding, and an adversarial pass on the packet path

Design brief for `PLAN.md` §8 items F3 and F4. Written so the implementation is
execution rather than discovery: every decision below is made, argued and cited
to the line it lands on. Nothing here was built or measured — a gated benchmark
run is waiting for the machine, so this document is deliberately all reading.

Read alongside: `PLAN.md` §8 (lines 289–316), `docs/server.md`, `README.md`
roadmap item 6 (lines 1339–1348).

---

## 0. Findings from reading the code

A brief that reads the packet path and reports nothing did not read it. These
are ordered by how much they change what gets built, not by severity.

### Finding 1 — `decode_time` multiplies a client-supplied `u32` by 24

`crates/inlaysql-server/src/connection.rs:2288-2289`

```rust
let days = reader.u32()?;
let hours = reader.u8()? as u32 + days * 24;
```

`days` is four bytes taken straight off the wire in a `COM_STMT_EXECUTE`
parameter the client declared as MySQL type `0x0b` (TIME). Nothing bounds it.
`days * 24` overflows `u32` for any `days > 178_956_970`, and the `+` overflows
for values just under that.

* **Debug and test builds** (`overflow-checks = true`, the default for
  `cargo test`): this is `attempt to multiply with overflow` — a **panic
  reachable from the wire by any authenticated client**, in a
  thread-per-connection server. It kills the connection thread; whether it
  kills more depends on the panic strategy, and the workspace does not set
  `panic = "abort"` for the dev profile.
* **Release builds**: it wraps silently and produces a nonsense `TIME` string
  that is then handed to the engine as `Value::Text`.

`decode_binary_param`'s contract is `Result<Value, Malformed>`
(connection.rs:2192-2196) and `packet.rs`'s module doc claims every accessor
"reports a short packet as an error, so a truncated or hostile packet cannot
panic the connection thread" (packet.rs:257-258). That claim is true of
`Reader`; it is not true of the two decoders built on top of it. This is the
single clearest reason F4 is worth doing, and it is a two-line fix
(`days.checked_mul(24).ok_or(Malformed)?`) that should not wait for the fuzzer.

### Finding 2 — `read_message` pre-allocates a client-declared length before a byte of it arrives

`crates/inlaysql-server/src/packet.rs:136-147`

```rust
let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
...
if payload.len() + length > MAX_MESSAGE { ...refuse... }
let start = payload.len();
payload.resize(start + length, 0);          // <- 16 MiB from 4 bytes
self.reader.read_exact(&mut payload[start..])?;
```

`MAX_MESSAGE` (64 MiB, packet.rs:28) bounds the *total* a message may reach. It
does not bound the *pre-read* allocation: a client sends four bytes declaring
`0xffffff` and the server commits 16 MiB, then blocks in `read_exact`. The
socket read timeout is `wait_timeout`, whose default is **28 800 seconds**
(lib.rs:130, applied at lib.rs:842), and this is reachable **before
authentication** — `authenticate` calls `read_message` at connection.rs:198
before any credential has been checked.

`DEFAULT_MAX_CONNECTIONS` is 64 (lib.rs:121). So 64 sockets and 256 bytes of
traffic commit **1 GiB of resident memory for eight hours**, from an
unauthenticated peer. The module doc directly above the code says the
`MAX_MESSAGE` cap "removes the cheapest denial-of-service this protocol offers"
(packet.rs:22-27) — it removes one of them. This one is cheaper.

The fix is a chunked read: `resize` to `min(length, 64 KiB)` and grow as bytes
actually arrive, so the allocation is bounded by what the peer has delivered.
That is exactly the invariant F4's targets are being written to assert, which is
why this finding and the fuzz work belong in the same slice.

### Finding 3 — a re-sent `CLIENT_SSL` inside TLS parses as a credential-free login instead of an error

`connection.rs:213-242`, `connection.rs:2366-2374`

`parse_handshake_response` returns early with an empty username, empty
`auth_response`, no database and no plugin whenever `CLIENT_SSL` is set
(connection.rs:2366), because an `SSLRequest` is only the first 32 bytes. The
caller handles that once, at connection.rs:213, upgrades, and re-parses the
inner packet at connection.rs:235 — but the `CLIENT_SSL` early return is not
suppressed on the second call and the `if` at 213 is not re-entered. A client
that sets `CLIENT_SSL` again inside TLS therefore reaches the account lookup
with `username = ""` and a zero-length token, instead of getting
`malformed handshake response`.

It is refused today (the empty name misses, and `Account::unknown`,
acl.rs:370-377, gives it a verifier no password produces). But it is refused by
accident of the account store rather than by the parser, and it is the kind of
"silently produced a degenerate struct" path that the fuzz harness will produce
in the first minute. `parse_handshake_response` should take a
`expect_ssl_request: bool` and treat `CLIENT_SSL` in the post-upgrade packet as
malformed.

### Finding 4 — `is_public` is a string comparison, and it is the only thing standing between a typo and the network

`crates/inlaysql-server/src/lib.rs:466-473`

```rust
match self.bind.parse::<IpAddr>() {
    Ok(address) => !address.is_loopback(),
    Err(_) => !self.bind.eq_ignore_ascii_case("localhost"),
}
```

Two guesses live here. The name branch hard-codes one hostname; every other
name is assumed public (conservative, fine) and `localhost` is assumed loopback
without asking the resolver (not always true — a host whose `/etc/hosts` maps
`localhost` elsewhere is unusual but the code does not check). The IP branch
misses `::ffff:127.0.0.1`, for which `Ipv6Addr::is_loopback()` is `false`, so a
mapped-loopback bind warns when it should not. Neither direction is currently
*unsafe*, but F3 promotes this predicate from "chooses a warning" to "chooses
whether the process starts", and a string comparison is not enough for that
job. §1.2 replaces it.

### Finding 5 — the docs' `--bind` row will be wrong the moment F3 lands, and one error message is already stale

`docs/server.md:142` says `--bind`'s effect is "Anything else warns";
`docs/server.md:96-98` says "doing so prints a warning that names the risk".
Both become false under F3 and are listed in §1.6.

Separately, `connection.rs:479-484` still tells a client asking for the RSA
public-key exchange that "this server is plaintext-localhost only (see
docs/server.md)" — that sentence has been false since TLS landed on 2026-09-01
(F1). It is a user-visible error string claiming a posture the server no longer
has. Fix it in the F3 slice that rewrites the other localhost prose.

### Finding 6 — eight fuzz targets will not fit in `trust.yml`'s hour

`.github/workflows/trust.yml:35-37, 73`. The job is `timeout-minutes: 60` for
four targets at 300 s each plus a `cargo install cargo-fuzz` and four instrumented
builds. Adding four `server_*` targets makes it 8 × 300 s = 40 minutes of
fuzzing alone. The backstop must move to 90, or the per-target default drops to
180 s. Recommendation in §3.4: move the backstop, keep 300 s — the budget is the
thing that finds bugs.

Related, from the standing gate-gap note: `fuzz/` is a separate workspace and is
**not** covered by `cargo check --workspace`, so a `server_*` target that stops
compiling is only discovered by the nightly Trust run. §3.4 adds a
`cargo +nightly fuzz build` step to `ci.yml`.

---

## 1. F3 — safe-by-default binding, and saying so in the process

### 1.1 Where the refusal goes

The bind address is parsed in exactly one place —
`crates/inlaysql-mcp/src/bin/inlaysql.rs:279-284`, which copies the argument
string into `ServerOptions::bind` with no validation at all — and the listener
is created in exactly one place, `crates/inlaysql-server/src/lib.rs:584`:

```rust
let listener = TcpListener::bind((options.bind.as_str(), options.port))?;
```

Everything the refusal needs already exists above that line, inside
`Server::bind`:

| What the predicate needs | Where it already is |
| --- | --- |
| the requested address | `options.bind`, lib.rs:428 / :584 |
| whether the file has real accounts | `installed`, computed at lib.rs:523-536 |
| whether a certificate is configured | `options.tls_cert`, checked at lib.rs:559-582 |
| whether TLS is *required* | `tls_config.policy()`, lib.rs:561-565 |
| whether the bootstrap password is empty | `Installed::{Bootstrap,Reset}.empty_password`, acl.rs:443/457 |

**Decision.** A new private function

```rust
fn refuse_unsafe_exposure(
    options: &ServerOptions,
    installed: &acl::Installed,
    policy: tls::TlsPolicy,
) -> Result<(), String>
```

in `lib.rs`, called from `Server::bind` immediately after the TLS config is
loaded (i.e. after lib.rs:582) and before lib.rs:584. That places it 61 lines
after `acl::install` and one line before the socket, so the check runs on real
state and the socket is never bound for a configuration that will be refused.

**It goes in `Server::bind`, not in the CLI.** `print_exposure_warning`
(lib.rs:1008) lives in the library for exactly this reason — "so the text lives
beside the behaviour it describes rather than in an argument parser" — and a
refusal only the CLI performs is a refusal every embedder silently skips.
`Server::bind` already refuses three other configurations here
(`wait_timeout == 0` at lib.rs:512, `--strong-passwords` without a certificate
at lib.rs:547, `--tls-required` without one at lib.rs:569); this is the fourth,
and it belongs in the same block, in the same order the options table lists it.

`print_exposure_warning` stays — it still carries the `--page-reuse` and
`--statement-text` lines — but its `is_public()` branch (lib.rs:1028-1035)
becomes reachable only for a configuration that passed the refusal, i.e. only
under the escape hatch of §1.4, and its text changes accordingly.

### 1.2 The predicate: what counts as "not localhost"

**Decision: resolve, then judge every resolved address, and refuse if any one
of them is not loopback.**

```rust
/// Every address `(bind, port)` would actually listen on.
fn resolved(bind: &str, port: u16) -> Result<Vec<IpAddr>, String>;

fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback(),
        // `Ipv6Addr::is_loopback()` is false for ::ffff:127.0.0.1 — Finding 4.
        IpAddr::V6(v6) => v6.is_loopback()
            || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

fn reaches_the_network(bind: &str, port: u16) -> Result<bool, String> {
    Ok(resolved(bind, port)?.into_iter().any(|a| !is_loopback(a)))
}
```

Resolution uses `std::net::ToSocketAddrs` on `(bind, port)` — the same tuple
`TcpListener::bind` takes at lib.rs:584, so the addresses judged are exactly the
addresses that would have been bound. A resolution failure is returned as a
startup error naming the address, which is what `TcpListener::bind` would have
produced one line later anyway.

Case by case, and this table is the specification:

| `--bind` | Resolves to | Verdict |
| --- | --- | --- |
| `127.0.0.1` (default, lib.rs:118) | `127.0.0.1` | loopback |
| `::1` | `::1` | loopback |
| `::ffff:127.0.0.1` | mapped v4 loopback | loopback (fixes Finding 4) |
| `localhost` | whatever the resolver says | judged, not assumed |
| `0.0.0.0` | `0.0.0.0` | **not loopback** — the wildcard includes every public interface |
| `::` | `::` | **not loopback**, same reason |
| `192.168.1.10`, `10.0.0.5`, `fd00::1` | themselves | **not loopback** (private, but reachable — see §1.4) |
| any routable address | itself | **not loopback** |
| a hostname resolving off-loopback | itself | **not loopback** |
| a hostname resolving to a *mix* | mixed | **not loopback** — `any`, not `all` |

The wildcard is deliberately not special-cased as "probably a container". It is
the single most common way a database ends up on the internet by accident, and
"I meant the bridge network" is expressible as the bridge network's own name
(§1.6, the compose change).

**Unix sockets.** There is no unix-socket support today: the server is
`std::net::TcpListener` only (lib.rs:98, lib.rs:584, and `docs/architecture.md`
line 53 states the design). If one is ever added it is loopback by construction
— there is no network peer — so it is **exempt from the TLS conditions** and
**not exempt from the account conditions**: filesystem permissions decide who
may connect, but they do not decide who the connecting party *is*, and
"`--user`/`--password` are the whole credential" is still a claim about
identity. Recorded here so the decision is not re-litigated at implementation
time; out of scope for F3.

### 1.3 The predicate: TLS, and the account store

**"TLS off" is two different states and the difference is the remedy.**

`TlsPolicy` (tls.rs:48-64) already names all three:

* `Disabled` — no certificate. `CLIENT_SSL` is not advertised
  (tls.rs:68-70); the credential and every result cross in the clear. Remedy:
  *get a certificate*.
* `Available` — a certificate is configured but `--tls-required` was not given
  (lib.rs:561-565). A client that does not ask is still served, so an on-path
  attacker simply does not offer the upgrade and gets the plaintext session.
  The server's own test comment already calls this "the posture most likely to
  be mistaken for a safe one" (lib.rs:1202-1203). Remedy: *one more flag*.
* `Required` — plaintext logins refused (connection.rs:248-260).

**Decision: only `TlsPolicy::Required` satisfies the TLS condition.** Both
`Disabled` and `Available` refuse a non-loopback bind, with different text,
because an operator who reads the wrong remedy goes and buys a certificate they
already have.

**"Only the bootstrap credential" is a question the code already answers.**
`acl::install` (acl.rs:484-514) returns:

* `Installed::Bootstrap { user, empty_password }` — acl.rs:491-498, returned
  when `db.catalog().table(USER_TABLE).is_none()`. This is *exactly* the plan's
  condition: there is no account store, `--user`/`--password` are the whole
  account model, and not a byte has been written (acl.rs:467-477).
* `Installed::Existing` — acl.rs:499-501, the file has a store and the flags
  were not consulted.
* `Installed::Reset { user, empty_password }` — acl.rs:502-513, the file has a
  store and `--reset-superuser` overwrote one account from the flags.

So the account predicate is `matches!(installed, Installed::Bootstrap { .. })`.
No new state, no new query, no new failure mode — `installed` is already in
scope at lib.rs:523 and already drives `notices_for` at lib.rs:590.

**The four conditions, evaluated in this order, first match wins.** All four
require `reaches_the_network(...) == true`.

| # | Condition | Overridable? |
| --- | --- | --- |
| C1 | the bootstrap or reset password is **empty** (`empty_password == true`) | **no** |
| C2 | the account store does not exist (`Installed::Bootstrap`) | **no** |
| C3 | `TlsPolicy::Disabled` | yes, §1.4 |
| C4 | `TlsPolicy::Available` (certificate, no `--tls-required`) | yes, §1.4 |

C1 before C2 because an empty password is the loudest fact and the operator
should read it first. C1 also catches `Installed::Reset` with an empty password,
which C2 does not.

### 1.4 The escape hatch: a position, argued

The brief's framing is right: a refusal with no override gets patched around,
and `--i-know-this-is-insecure` is a warning wearing a costume. Both failures
have the same root — a flag whose argument is the operator's *state of mind*
rather than a *fact about the deployment*. `--i-know-this-is-insecure` is
satisfiable by anyone, says nothing, and constrains nothing, so it degrades to
"paste this to make the error stop".

**Decision: one flag, `--plaintext-network`, which asserts a checkable fact and
is refused when the fact is false.**

```
--plaintext-network   Assert that this bind address is a private network
                      segment on which plaintext is acceptable. Relaxes the
                      TLS requirement for --bind, and nothing else.
```

Its rules, and each rule is the reason it is not a costume:

1. **It relaxes C3 and C4 only.** It has no effect on C1 or C2. An
   empty-password database and an account-less database stay refused on every
   non-loopback address, under every flag combination. That is what makes the
   flag unable to express the disaster case.
2. **It is itself refused unless every resolved address is private.** Private
   means RFC1918 (`Ipv4Addr::is_private()`), link-local
   (`Ipv4Addr::is_link_local()`, `Ipv6Addr` `fe80::/10`), CGNAT `100.64.0.0/10`,
   or IPv6 ULA `fc00::/7`. (Note for the implementer: `Ipv6Addr::is_unique_local`
   and `Ipv4Addr::is_shared` are both unstable, so the last two are two-line
   hand-rolled masks with a unit test each.) A public routable address plus
   plaintext has no honest deployment, so the flag does not reach one.
3. **The wildcard is refused.** `0.0.0.0` and `::` are not private; they are
   *every* interface, which on a host with a public address includes the public
   one. An operator who means "the container bridge" writes the bridge's name
   or address, and that is strictly better than what they have today.
4. **It never becomes silent.** Every start under it prints, to stderr, before
   anything else:
   ```
   inlaysql: --plaintext-network: serving 172.19.0.4:3306 WITHOUT TLS. Every statement,
   inlaysql:          result and credential on this port crosses the network in the clear
   inlaysql:          and any host on this segment can read them. This is not a
   inlaysql:          production posture; --tls-cert/--tls-key/--tls-required is.
   ```
   That is what `print_exposure_warning`'s `is_public()` branch
   (lib.rs:1028-1035) becomes.

The one legitimate in-repo user is `bench/external/compose.yml` (§1.6), and
adopting the flag there *improves* that deployment: the benchmark server stops
listening on every interface of the developer's host.

### 1.5 The exact text a refused start prints

One line an operator reads and knows what to do. Each is a single `io::Error`
from `Server::bind`, which the CLI already prints via
`MysqlServer::bind(...).map_err(|error| error.to_string())?`
(inlaysql.rs:349). House style, matching lib.rs:549 and lib.rs:571: name the
address, name the fact, name the flag that fixes it, name the way back to safe.

**C1 — empty password on a network address:**

```
refusing to start: --bind 192.168.1.10 is reachable from other machines and the account
`root` has an EMPTY password, so any host that can reach port 3306 can read and write this
database. Set one with --password-env, or drop --bind to stay on 127.0.0.1.
```

**C2 — no account store:**

```
refusing to start: --bind 192.168.1.10 is reachable from other machines and this database
has no accounts of its own, so `root` from --user/--password is the whole credential and a
forgotten flag on any restart is a way back in. Run `inlaysql user add <database>` once,
then restart with --bind. Or drop --bind to stay on 127.0.0.1.
```

**C3 — no certificate:**

```
refusing to start: --bind 192.168.1.10 is reachable from other machines and no certificate
is configured, so every statement, result and credential would cross the network in the
clear. Serve it with --tls-cert <pem> --tls-key <pem> --tls-required. Drop --bind to stay on
127.0.0.1, or --plaintext-network if this is a private segment you accept plaintext on.
```

**C4 — certificate, but not required:**

```
refusing to start: --bind 192.168.1.10 is reachable from other machines and TLS is
available but NOT required, so a client that does not ask for it still sends its credential
in the clear and an on-path attacker need only decline to offer it. Add --tls-required.
Drop --bind to stay on 127.0.0.1, or --plaintext-network if this is a private segment you
accept plaintext on.
```

**The flag refusing itself:**

```
refusing to start: --plaintext-network says this is a private segment, but --bind 0.0.0.0
listens on every interface, including any public one this host has. Name the private address
or hostname to listen on instead.
```

(and the routable-address variant, same shape, "…but 203.0.113.7 is a publicly
routable address. There is no deployment where a database on a public address
should be plaintext.")

### 1.6 What breaks, and how each changes

Every place in the repository that starts the server on a non-loopback address
or documents the current warning. Grepped for `--bind`, `0.0.0.0`, `localhost
only`, `plaintext`.

**Actually starts a server off-loopback — one place, and no CI workflow does.**

* `bench/external/compose.yml:172` —
  `--bind 0.0.0.0 --port 3306 --user root --password bench`. Fires C2 (no
  account store) and C3 (no certificate). **Change:** `--bind inlaysql-server`
  (the compose service name, which resolves to the RFC1918 bridge address) plus
  `--plaintext-network`, and a seeding step before `serve` that creates a real
  account — see §1.7, which is why closing the bootstrap gap is not optional
  scope. The header comment at compose.yml:150-154 ("no TLS, because the
  protocol does not implement any yet") is stale since F1 and gets rewritten in
  the same edit. `bench/README.md`'s Server-to-server section documents the
  credential/TLS asymmetries and gains one sentence.
* `.github/workflows/ci.yml`, `benchmark.yml`, `benchmark-published.yml`,
  `trust.yml`, `release.yml`, `wasm.yml`, `docker/test.sh` — **none of them
  start `serve --mysql`**. Verified by grep for `serve --mysql` across
  `*.yml`/`*.sh`/`Dockerfile*`: the only hit is the compose file. Stated here so
  nobody re-greps.

**Tests that assert the current behaviour.**

* `crates/inlaysql-server/src/lib.rs:1092-1104` — the `is_public` unit test,
  which asserts `0.0.0.0`, `192.168.1.10`, `::`, `example.com` are public and
  `127.0.0.1`, `::1`, `localhost` are not. **Change:** becomes the
  `reaches_the_network` table test, gains `::ffff:127.0.0.1` on the loopback
  side (Finding 4) and a private-vs-public case per §1.4 rule 2.
* `crates/inlaysql-server/src/lib.rs:1180-1231`
  (`the_warning_states_this_servers_tls_posture_and_flags_the_risky_defaults`)
  — constructs `bind: "0.0.0.0"` and asserts the warning text contains
  "reachable from other machines". **Change:** that half becomes a
  `Server::bind` refusal test; the TLS-posture half stays as it is, since it
  never binds. Keep the `assert!(!text.contains("hunter2"))` line — no refusal
  message may carry a password either.
* Four new `Server::bind` tests, one per condition, plus one per escape-hatch
  refusal. They need a temp file and `port: 0`; `tls_wire.rs:584-630` is the
  existing pattern for "`Server::bind` refuses and the message names the flag".
* `crates/inlaysql-server/tests/{wire.rs,tls_wire.rs,streaming_memory.rs}` all
  bind `127.0.0.1` (wire.rs:86, :133, :2669, :5298; tls_wire.rs:61, :591, :615;
  streaming_memory.rs:241) — **unaffected**, and that is the point: the default
  path does not change.

**Documentation and site copy.**

| File:line | Says today | Becomes |
| --- | --- | --- |
| `docs/server.md:96-98` | "doing so prints a warning that names the risk" | "doing so is refused unless TLS is required and the database has accounts of its own" + a new **Deploying on a private network** section (§4) |
| `docs/server.md:142` | `--bind` … "Anything else warns." | "Anything else is refused unless TLS is required and the database has its own accounts — see Deploying on a private network." Plus a `--plaintext-network` row. |
| `docs/server.md:126` | "v1 is documented plaintext-localhost from the top of this file down" | the RSA-fallback paragraph is rewritten around `TlsPolicy`, not around "plaintext-localhost" |
| `crates/inlaysql-server/src/connection.rs:479-484` | error text: "this server is plaintext-localhost only" | **already false** (Finding 5) — rewrite to name the certificate state of *this* server |
| `crates/inlaysql-mcp/src/bin/inlaysql.rs:35` | "(default 127.0.0.1, loopback only)" | "(default 127.0.0.1; anything else needs TLS and real accounts)" |
| `crates/inlaysql-mcp/src/bin/inlaysql.rs:164` | "It listens on 127.0.0.1 unless --bind says" | same sentence, plus the refusal |
| `README.md:382-389` | "It binds `127.0.0.1` by default and the wire is plaintext until…" | "…and it refuses to bind anything else without TLS" |
| `README.md:1339-1348` | roadmap item 6, future tense | struck when F3+F4 land |
| `SECURITY.md:38` | "Binds `127.0.0.1` unless told otherwise. **Plaintext by default**" | keep "plaintext by default"; add "and refuses a non-loopback bind without TLS" |
| `crates/inlaysql-wasm/www/index.html:296-298` | "the MySQL server is plaintext and localhost-first" | **deleted**, per §3.5 — this is the plan's own done-criterion |
| `crates/inlaysql-wasm/www/index.html:643-648` | "All three are off by default, and it binds to 127.0.0.1. Without a certificate, do not put it on a network you do not own." | last sentence becomes "Without a certificate it will not bind to one." |
| `crates/inlaysql-wasm/demos/clients/index.html:333-336` | "beyond localhost, use `--tls-cert` and `--tls-required`" | "beyond localhost, `--tls-cert` and `--tls-required` are required" |
| `docs/clients.md:381, :397-398` | same phrasing | same change |
| `.github/workflows/release.yml:247` | release-note line "The MySQL server mode is plaintext and localhost-first unless" | rewritten |

### 1.7 The deliberate gap: close it

`PLAN.md:304-309` records that the bootstrap `--user`/`--password` pair is not
covered by `--strong-passwords`, and names the honest fix: "stop needing them —
a first-run `CREATE USER` flow, unscoped."

**Recommendation: close it, in F3, and scope it as one CLI subcommand.**

Three reasons, in order of weight:

1. **F3's C2 refusal is not shippable without it.** A refusal whose remedy is
   "run `CREATE USER`" is useless if the only way to run `CREATE USER` is to
   connect to a server you have just been refused permission to start. Today
   there is no non-serving entry point to the database at all: the CLI's
   subcommands are `serve`, `changes`, `backup`, `vacuum` (inlaysql.rs:242-245).
   The refusal would be a loop.
2. **The compose file cannot be fixed without it** (§1.6), and the compose file
   is the repository's own proof that the posture is usable.
3. **It is genuinely small.** `acl::ensure_store` (acl.rs:525) and the
   `CREATE USER`/`GRANT` statements already exist and are already exercised by
   `wire.rs`. The subcommand opens the file, runs the statements through the
   same shim the server does, and closes it:

   ```
   inlaysql user add <database> --user <name> --password-env <VAR> [--superuser]
   inlaysql user list <database>
   ```

   `--password-env` only, never `--password` — this is the one credential entry
   point being built from scratch, so it does not need to inherit
   inlaysql.rs:357-362's "visible to `ps`" warning.

Once it exists, `--user`/`--password` are demoted to what they honestly are: a
first-run convenience on loopback, ignored the moment the file has accounts
(acl.rs:499-501). `docs/server.md`'s Divergences entry stops recording a gap and
starts recording a design.

The alternative — keep recording it — is defensible only if C2 is dropped, and
C2 is the half of F3 that the plan text puts *first* ("or while the only account
is the flag-provided one"). Dropping it would make F3 a TLS check with extra
steps.

---

## 2. F4 — an adversarial pass on the packet path

### 2.1 The attack surface: every client-supplied length, count and offset

Post-framing, everything below is reachable from a socket. "Auth" means whether
the client must have authenticated first.

| # | Site | What arrives from the client | Auth | What it does today |
| --- | --- | --- | --- | --- |
| A1 | `packet.rs:136` | 3-byte payload length, per packet | **no** | bounded against `MAX_MESSAGE` in aggregate; **pre-allocated in full before any of it arrives** — Finding 2 |
| A2 | `packet.rs:137` | sequence id | **no** | copied, `wrapping_add(1)`; continuity never validated, so a client can desynchronise its own numbering. Harmless today; a fuzz target will produce it |
| A3 | `packet.rs:154` | "is this the last packet" (length == `MAX_PAYLOAD`) | **no** | loops until a short packet; bounded only by `MAX_MESSAGE` |
| A4 | `packet.rs:281-286` `Reader::take` | `n` | — | `checked_add` + `get(..)` → `Malformed`. Correct |
| A5 | `packet.rs:314-325` `lenenc_int` | 1/3/5/9-byte length | — | bounds-checked; the `0xfe` form yields a full `u64` |
| A6 | `packet.rs:328-336` `lenenc_bytes` | that `u64` as a byte count | — | `usize::try_from` then `take`, so it cannot over-read. It also cannot over-allocate, because `take` borrows. **This is the pattern the rest of the path should copy** |
| A7 | `packet.rs:339-345` `nul_str` | terminator position | — | `position` + `from_utf8`, bounds-checked |
| A8 | `connection.rs:2354` | 4-byte capability bitmask | **no** | drives which fields are parsed next; `CLIENT_PROTOCOL_41` required (2358), `CLIENT_SSL` early-returns (2366) — Finding 3 |
| A9 | `connection.rs:2388-2389` | 1-byte auth-response length | **no** | `take(length)` — bounded by the packet |
| A10 | `connection.rs:2381-2386` | lenenc auth-response length | **no** | via A6, then `.to_vec()` — allocation is bounded by the packet, so ≤ 64 MiB |
| A11 | `connection.rs:2405` | auth plugin name | **no** | `unwrap_or("")` — the only field that swallows a parse error instead of reporting it |
| A12 | `connection.rs:586` `dispatch` | command byte | yes | `split_first` handles empty; `Command::from_byte` (protocol.rs:112-124) totalises to `Unknown` |
| A13 | `connection.rs:1553-1555` | statement id, flags, iteration count | yes | `u32` reads; iteration count read and **discarded**, never used as a loop bound (correct — MySQL's own is always 1) |
| A14 | `connection.rs:1569-1572` | null bitmap | yes | length is `count.div_ceil(8)` where `count = prepared.param_count` — **server-side**, from the plan, not the wire. Correct by construction |
| A15 | `connection.rs:1576-1581` | per-parameter type + flag bytes | yes | `count` iterations, `count` server-side. `Vec::with_capacity(count)` is safe for the same reason |
| A16 | `connection.rs:1608-1622` | the parameter payloads | yes | decoded in declared-type order; a wrong type misframes everything after it, which is why `decode_vector_param` refuses non-string types (connection.rs:2145-2151) |
| A17 | `connection.rs:2153-2169` | lenenc vector payload | yes | length checked `!= dim*4` and refused; `dim` is server-side from the plan (connection.rs:1600-1605) |
| A18 | `connection.rs:2171-2187` | f32 components | yes | `with_capacity(dim)` — `dim` server-side; non-finite refused |
| A19 | `connection.rs:2246, 2248` | blob/string lenenc | yes | `.to_vec()`, bounded by the packet |
| A20 | `connection.rs:2258-2279` `decode_datetime` | 1-byte declared length | yes | **the declared length is read and then only compared to 0/4/11** — any other value (say 100) still consumes exactly 7 bytes, desynchronising the rest of the parameter list. Bounded, but the frame and the declaration are allowed to disagree |
| A21 | `connection.rs:2283-2299` `decode_time` | 1-byte length, then a `u32` days | yes | **Finding 1: `days * 24` overflows** |
| A22 | `connection.rs:656, 662, 684` | statement / connection id | yes | `u32` reads; ids looked up in a map, absent → proper error |
| A23 | `connection.rs:628, 638, 643` | `COM_INIT_DB` / `COM_QUERY` / `COM_STMT_PREPARE` bodies | yes | `String::from_utf8_lossy(body).to_string()` — one full copy of up to 64 MiB per command, then into `sql::plan`, which is where AHL-500 lived |

The shape of the surface: **the `Reader` primitives are right, and the two
layers built on them are where the bugs are** — the framing layer above it
(A1) and the type-specific decoders below it (A20, A21). That is precisely
where the targets go.

### 2.2 The targets

Four, following the existing conventions exactly: `#![no_main]`, a
`fuzz_target!` macro, a module doc whose first line states *the property*
rather than the mechanism (`sql_parser.rs` line 3: "The property is not 'it
parses'…"), `let _ =` on every fallible call, and a `[[bin]]` block in
`fuzz/Cargo.toml` with `test = false, doc = false, bench = false`.

**Prerequisite, and it is the only structural change F4 needs.** Every relevant
module is private (`mod packet;` lib.rs:90, `mod connection;` lib.rs:82); only
`tls` is `pub`. `fuzz/Cargo.toml` depends on `inlaysql-core` alone. So:

* add to `crates/inlaysql-server/Cargo.toml` a non-default feature
  `fuzzing = []`;
* add to `lib.rs` a `#[cfg(feature = "fuzzing")] #[doc(hidden)] pub mod fuzz;`
  exporting exactly four functions, one per target, each a thin wrapper over the
  private item;
* add to `fuzz/Cargo.toml`
  `inlaysql-server = { path = "../crates/inlaysql-server", features = ["fuzzing"] }`.

A feature rather than `pub(crate)`-widening keeps the shipped public API of
`inlaysql-server` unchanged (`MysqlError`, `SERVER_VERSION`, `tls`), which
matters because `#![warn(missing_docs)]` is on (lib.rs:70) and a widened
surface is a documentation obligation.

---

**T1 — `server_packet_frame`**

```rust
fuzz_target!(|data: &[u8]| {
    let _ = inlaysql_server::fuzz::read_message(data);
});
```

Drives `Stream::read_message` (packet.rs:121) over a cursor on `data`, in a
loop until it returns `Ok(None)` or an error, so one input exercises a whole
session's framing.

*Invariants asserted inside the target:*
1. never panics;
2. peak allocation ≤ `max(64 KiB, 4 × data.len())` — this is the one that fails
   today (Finding 2), and it is why the counting allocator of §2.3 exists;
3. every returned `Vec` has length ≤ `MAX_MESSAGE`;
4. the loop makes progress: the number of iterations is ≤ `data.len()`.

*Seeds* (`fuzz/corpus/server_packet_frame/`):
`ff ff ff 00` and nothing else (the 16 MiB-from-4-bytes input);
`05 00 00 00 68 65 6c 6c 6f` (a valid `hello`, the round-trip from
packet.rs:418);
one `MAX_PAYLOAD` packet followed by `00 00 00 01` (the exact-multiple case
packet.rs:439 exists for);
five consecutive `ff ff ff nn` headers with no bodies;
`ff ff ff 00` × 5 with full bodies, to reach the `MAX_MESSAGE` refusal at
packet.rs:139.

---

**T2 — `server_handshake`**

```rust
fuzz_target!(|data: &[u8]| {
    let _ = inlaysql_server::fuzz::parse_handshake_response(data);
});
```

*Invariants:* never panics; returns a `Result`; allocates ≤ 2 × `data.len()`
(the response owns copies of the username, token, database and plugin, so 2×
is the honest bound and anything above it is a finding); consumes no more than
`data.len()` reader bytes; **runs in time linear in `data.len()`** — the
AHL-500 property, §2.4.

*Seeds:* a real `HandshakeResponse41` lifted byte-for-byte from
`connection.rs`'s own test at :2594 and from `tls_wire.rs`; the 32-byte
`SSLRequest` (the `CLIENT_SSL` early return, connection.rs:2366); the same with
`CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA` set and a `0xfe` lenenc declaring
`u64::MAX`; the same with `CLIENT_CONNECT_WITH_DB` set and no NUL after the
username; a valid response truncated at each of the eight field boundaries.
The last group matters: without it the campaign spends its budget failing at
byte 4 on `CLIENT_PROTOCOL_41` (connection.rs:2358) and never reaches the
fields.

---

**T3 — `server_stmt_params`** — the highest-value target, because A20/A21 live
here.

Structured input via `arbitrary` (already a `fuzz/Cargo.toml` dependency with
`features = ["derive"]`), because `decode_execute` needs server-side state the
wire does not carry:

```rust
#[derive(arbitrary::Arbitrary, Debug)]
struct Execute<'a> {
    types: Vec<(u8, bool)>,          // MySQL type byte + unsigned flag
    vector_dims: Vec<Option<u16>>,   // what the plan says, per placeholder
    body: &'a [u8],                  // the wire bytes
}
```

The wrapper caps `types.len()` at 4096 (a real `param_count` comes from the
plan and is bounded by the statement) and calls the parameter loop
(connection.rs:1607-1622) directly.

*Invariants:* never panics — **this fails today on `types = [(0x0b, _)]` and a
body whose `days` field is large** (Finding 1); allocation ≤ 2 × `body.len()`;
consumes ≤ `body.len()`; and a declared temporal length that does not match the
bytes consumed is reported rather than silently reframed (A20).

*Seeds:* one encoded parameter of each of the 17 handled type bytes, taken from
the existing round-trip tests at connection.rs:2449-2497; the `TIME` encoding
with `days = 0xffffffff`; a `DATETIME` declaring `length = 100`; a `VECTOR`
parameter bound as a `LONGLONG` (the refusal at connection.rs:2145).

---

**T4 — `server_command`**

```rust
fuzz_target!(|data: &[u8]| {
    let _ = inlaysql_server::fuzz::dispatch_stateless(data);
});
```

`Command::from_byte` plus the body decoders that need no engine:
`COM_STMT_CLOSE`/`RESET`/`PROCESS_KILL`'s `u32` reads (connection.rs:656, 662,
684), `check_database` (connection.rs:2305), and the `from_utf8_lossy` bodies
(A23) *without* executing them.

*Invariants:* never panics; allocates ≤ 2 × `data.len()`; every one of the 256
command bytes is handled (`Command::Unknown` is a legal outcome, a panic is not)
— this is the target that catches a future `COM_*` added without bounds, which
is the cheapest kind of regression to prevent and the easiest to introduce.

*Seed:* all 256 single-byte commands, plus each with a 1 KiB body.

---

**Deferred: T5 — `server_session`**, the whole connection loop over
`mem::engine()` and an in-memory `Upgradable`. Highest coverage, most
machinery (an `Upgradable` impl for a memory pipe, a scripted handshake). Worth
building *after* T1–T4 are green, not instead of them.

### 2.3 The allocation invariant, and how it is asserted

The four invariants the brief asks for — no panic, no allocation proportional to
a client-supplied length before validating it, no unbounded loop, no read past
the buffer — split into two kinds. Rust's slice bounds and `Reader`'s
`checked_add` give the last one for free, and libFuzzer gives the first. The
middle two need instrumentation, and CI's `-rss_limit_mb=2048` and `-timeout=25`
(trust.yml:66-68) are not it: they are *process-level* backstops that report
"the fuzzer died", not "this input made a 4-byte header into 16 MiB".

**Decision: a counting global allocator in the fuzz crate.**

```rust
// fuzz/fuzz_targets/counted_alloc.rs — included by the four server targets.
struct Counting;
static PEAK: AtomicUsize = ...;
static LIVE: AtomicUsize = ...;
unsafe impl GlobalAlloc for Counting { /* System, plus add/sub and a peak max */ }
```

Each target resets `PEAK` before the call and asserts it against the bound after
— so an amplification is a **crash, with the input written to
`fuzz/artifacts/`**, exactly like a panic. That is the difference between a
finding that reports and a finding that has to be noticed.

Loop-boundedness is asserted the same way: a counter incremented per framing
iteration, checked against `data.len()`.

### 2.4 How AHL-500 got in, and what property would have caught it

**This is the paragraph the rest of the document exists to earn.**

AHL-500 was not a bug this project wrote. `inlaysql-core` depends on `sqlparser`
with `default-features = false`, because the crate is `no_std` on purpose and
the WASM and simulation stories rest on that. Turning the features off also
turns off `recursive-protection`, and with it off `sqlparser` compiles
`RecursionCounter::try_decrease` as a **stub that always succeeds**
(`crates/inlaysql-core/src/sql.rs:180-186`). The guard the dependency documents
as present was, in this build, absent — and nothing in the build, the type
system, or the test suite said so. `sqlparser`'s leading-`IF` path then
backtracks exponentially across dot-separated identifiers: 74 bytes cost ~30
seconds of CPU before returning a parse error (sql.rs:93-110). On a
thread-per-connection server that is one thread pinned per statement, and 64 of
them is `DEFAULT_MAX_CONNECTIONS` — the whole server, from one connection, with
a packet small enough to fit in this sentence.

Three things let it through, and each has a direct analogue in the packet path:

1. **The reviewable property was never written down.** Every test in the suite
   asserted on the *value* a parse returns. Not one asserted on what it *cost*.
   "Every input reaches a `Result` in time bounded by its length" is a property
   nobody had stated, so nobody could have noticed it was untested.
2. **The bound that existed bounded the wrong thing.** A recursion limit was
   assumed to be inherited; it was not. In `packet.rs` the exact same shape is
   present today: `MAX_MESSAGE` is documented as the thing that "removes the
   cheapest denial-of-service this protocol offers" (packet.rs:22-27), and it
   bounds the *total* a reassembled message may reach — not the *per-header
   pre-allocation*, which is Finding 2. A documented bound that bounds an
   adjacent quantity is worse than no bound, because it stops the question being
   asked.
3. **The tooling reported it as its own failure.** libFuzzer's default
   per-input timeout is 1200 s, so the first sighting was a 46-minute Trust job
   with no named input — a runner problem, until `-timeout=25` was pinned
   (trust.yml:63-68) and the same input reported in 25 seconds.

**What, tested, would have caught it before it shipped: a bounded-work assertion
inside the fuzz harness, not around it.** Concretely, for AHL-500 that is a
target that measures elapsed time per input and fails above a bound linear in
input length. For the packet path it is §2.3's counting allocator plus the
iteration counter, which turn "allocation and work are proportional to the
input, not to a number in the input" from a hope into an assertion with a
reproducer file. The value of putting it *inside* the target is that the
harness, not the CI budget, owns the property — the same target run for 60
seconds on a laptop fails identically, and the input lands in
`fuzz/artifacts/` where the existing convention
(`crates/inlaysql-core/tests/fuzz-regressions/`, read by `fixture()` at
`fuzz_regressions.rs:20-25`) can pin it forever.

Which is the fourth lesson, and the one the repository already learned: AHL-500's
two inputs are vendored byte-for-byte with the comment "a crash that has been
fixed but not pinned comes back" (`fuzz_regressions.rs:369-371`). F4 mirrors the
whole apparatus for the server: `crates/inlaysql-server/tests/fuzz-regressions/`
and `crates/inlaysql-server/tests/fuzz_regressions.rs`, with the same
`fixture()` helper and the same rule — every crash gets a checked-in input and a
`#[test]` that asserts the *property*, not which guard fired.

### 2.5 `trust.yml` and `ci.yml`

* `trust.yml:73` — the target list becomes
  `sql_parser storage row_codec json_parser server_packet_frame server_handshake server_stmt_params server_command`.
* `trust.yml:35-37` — `timeout-minutes: 60` → `90` (Finding 6). Keep the 300 s
  per-target budget; the budget is what finds bugs, and the backstop is only a
  backstop. The existing comment ("An hour of budget for four targets") is
  updated to say eight.
* `fuzz/corpus/server_*/` is checked in. The other four targets have no
  corpus and do not need one — arbitrary text and arbitrary bytes are both
  dense in interesting inputs. A handshake packet with a mandatory capability
  bit at byte 4 is not.
* `ci.yml` gains a `cargo +nightly fuzz build` step, because `fuzz/` is a
  separate workspace and is **not** covered by `cargo check --workspace` — a
  broken target is otherwise invisible until the nightly run.

---

## 3. Done

Tied to the plan's own criterion (`PLAN.md:314-316`): the site's
"localhost-plaintext" bullet can be **deleted** rather than reworded.

That bullet is `crates/inlaysql-wasm/www/index.html:296-298`:

> the MySQL server is plaintext and localhost-first — see What this is not

It may be deleted when all five hold, and not before, because until then it is a
true statement:

1. `Server::bind` refuses C1–C4 (§1.3), with one test per condition and one per
   escape-hatch refusal.
2. `inlaysql user add` exists (§1.7), so C2's remedy is performable.
3. T1–T4 exist, are wired into `trust.yml`, and have each run a full 300 s
   campaign clean on `main` on three consecutive nightly runs.
4. Findings 1, 2, 3 and 5 are fixed, each with a pinned regression test in
   `crates/inlaysql-server/tests/fuzz_regressions.rs`.
5. `docs/server.md` has a **Deploying on a private network** section giving the
   exact command line a competent operator would accept — certificate,
   `--tls-required`, an account created before the first `--bind`,
   `--max-connections`, `--wait-timeout` — and `bench/external/compose.yml` runs
   that shape (§1.6), so the repository's own deployment is the worked example.

Note what is *not* on this list, per `PLAN.md:311-312`: client certificate
authentication (tls.rs:34-37 records why), authorization beyond table
privileges, audit logging, multi-tenant isolation. Those are a hosting product's
requirements and their absence does not keep the bullet alive.

---

## 4. Slices, smallest first

"Blocked" means the slice compiles something, and a compile contaminates the
gated benchmark run currently waiting for a quiet machine. "Safe" means prose,
data files, or YAML.

| # | Slice | Gate | Machine? |
| --- | --- | --- | --- |
| S0 | This brief. | review | **safe** |
| S1 | Finding 1: `decode_time`'s `checked_mul`, plus a unit test with `days = u32::MAX`. Two lines and independent of everything else — it should not wait for the fuzzer that would have found it. | `cargo test -p inlaysql-server` | blocked |
| S2 | Finding 5: the stale "plaintext-localhost only" error text (connection.rs:481) and the `docs/server.md:126` paragraph. Text only, but it is in a `.rs` file. | `cargo test -p inlaysql-server` | blocked (trivially) |
| S3 | `reaches_the_network` replaces `is_public` (§1.2). Behaviour-neutral: the warning still only warns. Fixes Finding 4. | `cargo test -p inlaysql-server` | blocked |
| S4 | `inlaysql user add` / `user list` (§1.7). Independently useful, and C2's prerequisite. | `cargo test --workspace`, plus a manual create-then-serve | blocked |
| S5 | `refuse_unsafe_exposure` with C1–C4 and **no** escape hatch (§1.3, §1.5). Converts the lib.rs:1180 test. | `cargo test --workspace` | blocked |
| S6 | `--plaintext-network` (§1.4) + the compose change (`--bind inlaysql-server`, the seeding step, the stale header comment). Lands with S5 or the benchmark harness breaks. | `cargo test --workspace`; `bench/compare.sh` brings the stack up | blocked, **and needs Docker** |
| S7 | Doc sweep: the fifteen rows of §1.6's table, minus the site bullet. | prose review | **safe** |
| S8 | Write T1–T4, their seed corpora, and `counted_alloc.rs` as files. They do not compile until S9. | none yet | **safe** |
| S9 | The `fuzzing` feature, `lib.rs`'s `pub mod fuzz`, `fuzz/Cargo.toml`'s four `[[bin]]` blocks. | `cargo +nightly fuzz build`; 60 s local run per target | blocked |
| S10 | The allocation and iteration invariants inside the targets (§2.3). Expect T1 to fail immediately — that is Finding 2. | each target fails on the known input, then passes after the fix | blocked |
| S11 | Finding 2's chunked read in `read_message`; Finding 3's `expect_ssl_request`. Both pinned in `crates/inlaysql-server/tests/fuzz_regressions.rs`. | T1 and T2 clean for 300 s | blocked |
| S12 | `trust.yml` target list + `timeout-minutes: 90`; `ci.yml`'s `cargo fuzz build` step (Finding 6). | the workflow's own run | **safe** |
| S13 | Three consecutive clean nightlies, then delete the site bullet and strike `README.md`'s roadmap item 6. | §3 | **safe** |

**Safe to start now:** S0, S7, S8, S12 — and S8 is the substantial one, because
writing four targets and their corpora is most of F4's design work and none of
its compilation.

**Ordering constraint that is easy to get wrong:** S5 must not land before S4,
or the C2 refusal has no remedy; and S6 must not land after S5 by more than one
commit, or `main` has a broken benchmark harness in between. Land S4→S5→S6 as
one series.
