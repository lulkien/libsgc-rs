# AGENT.md — libsgc-rs

## Purpose
Rust client library for the simple-graphics-controller daemon (@sgc). Drives `SgcClient`: connect, acquire a resource (holds the granted fd), borrow it via `SgcClient::fd` (a dup, never the canonical), and pump for events — one frame per call. All wire work (framing, SCM_RIGHTS fd passing, Ack, Revoke handshake) happens synchronously on the app's own thread — no background threads, no shared state.

## Architecture
- **Single-threaded event loop** — one app thread drives acquire + the event loop; borrowers (e.g. a render task) talk to the app via channels, never to the client directly
- **Client-side resource ownership** — client owns the granted fds (`held: HashMap<Resource, OwnedFd>`); `fd()` lends dups; canonical dropped on revoke/disconnect
- **No shared state across threads** — the library is entirely single-threaded; `Rc<dyn Trait>` preferred over `Arc<dyn Trait>` when data never leaves the event-loop thread
- **Crate import** — import as `libsgc_rs::...`; re-exports `InputResource, Resource` from `simple-graphics-protocol`

## Rust Best Practices (per rust-skills)
- [`own-borrow-over-clone`] — `fd()` returns a DUP of the held fd; the canonical stays owned by the client; borrowers must drop their dup
- [`own-arc-shared`] — Use `Arc<T>` for thread-safe shared ownership only when needed; most data stays on one thread
- [`own-refcell-interior`] — Use `RefCell<T>` for interior mutability in single-threaded code (not currently needed, but pattern to keep in mind)
- [`own-cow-conditional`] — Use `Cow<'a, T>` for conditional ownership where appropriate
- [`err-result-over-panic`] — Return `Result<T, E>` instead of panicking for recoverable errors (see `acquire`, `pump`, `fd`)
- [`err-from-impl`] — `SgcError` implements `From<ProtocolError>` and `From<std::io::Error>` via `#[from]` to enable `?` operator
- [`err-question-mark`] — Use `?` operator for clean error propagation throughout
- [`err-context-chain`] — Add context with `.context()` or `.with_context()` when wrapping errors from I/O or protocol layer
- [`err-no-unwrap-prod`] — Avoid `unwrap()` in production code; use `?`, `expect()`, or handle errors
- [`expect-bugs-only`] — Use `expect()` only for invariants that indicate bugs, not user errors or runtime conditions
- [`mem-with-capacity`] — Use `Vec::with_capacity()` when size is known (e.g. `held.insert`, `fd.try_clone()`)
- [`perf-iter-over-index`] — Prefer iterators over manual indexing (see `held.keys().collect()`)
- [`num-nonzero`] — Use `NonZero*` types to forbid zero and unlock niche optimization (e.g. resource indices)
- [`api-from-not-into`] — Implement `From<T>`, not `Into<U>` — `SgcError::from` gives you `Into` for free
- [`api-must-use`] — Mark types and functions with `#[must_use]` when ignoring results is likely a bug (e.g. `acquire` return value, `fd()` result)
- [`doc-all-public`] — Document all public items with `///` doc comments
- [`doc-errors-section`] — Include `# Errors` section documenting all error variants
- [`doc-panics-section`] — Include `# Panics` section for functions that can panic under documented conditions
- [`doc-question-mark`] — Use `?` in examples, not `.unwrap()` — examples should demonstrate proper error handling
- [`obs-tracing-over-log`] — Use `tracing` for structured, span-aware diagnostics instead of `println!` or bare `log`
- [`obs-structured-fields`] — Record structured key-value fields, not values interpolated into the message string
- [`anti-lock-across-await`] — Never hold `Mutex`/`RwLock` across `.await` — this crate is synchronous, but the principle applies when migrating to async
- [`anti-clone-excessive`] — Don't clone when borrowing works — `fd()` clones the `OwnedFd` (intended), but `held` entries should be borrowed via `&Resource` where possible
- [`anti-type-erasure`] — Don't use `Box<dyn Trait>` when `impl Trait` works — the client struct uses concrete types

## Key Types & Functions
- `SgcClient` — owns granted fds in `held: HashMap<Resource, OwnedFd>`; `fd(resource)` lends a dup; `acquire(resource)` blocks until granted; `pump(timeout)` waits for one server frame
- `SgcEvent` — `Revoked { resource }` (drop fd, stop drawing) or `Granted { resource, fd }` (fresh dup, owned by caller)
- `SgcError` — variants: `ConnectFailed`, `Denied { reason }`, `NotHeld { resource }`, `NotAvailable { resource }`, `Protocol(ProtocolError)`, `UnexpectedMessage(ServerMessage)`, `Io(io::Error)`
- `OwnedFd` — Unix fd ownership via `ownership::OwnedFd`; `fd.as_raw_fd()` for C ABI interop
- `read_framed`, `write_frame` — internal helpers for framing + SCM_RIGHTS fd passing
- `fake_server`, `FakeController` — test helpers in `#[cfg(test)] mod tests`

## Common Pitfalls to Avoid
- ❌ Do NOT call `.unwrap()` in production paths — use `?`, `expect()` only for invariants indicating bugs
- ❌ Do NOT hold locks across await points — this crate is synchronous; if migrating to async, use `spawn_blocking` for CPU-intensive work
- ❌ Do NOT accept `&Vec<T>` when `&[T]` works — resource types use enums directly
- ❌ Do NOT clone the canonical fd unnecessarily — `fd()` explicitly returns a DUP; the client keeps the canonical
- ❌ Do NOT ignore error returns from `acquire`, `pump`, or `fd()` — all return `Result`; use `?` or handle explicitly
- ❌ Do NOT mix `Arc` and `Rc` carelessly — if data never leaves the event-loop thread, `Rc` is preferred (avoids atomic refcount ops)
- ❌ Do NOT forget to `Ack` a grant — the server waits up to 5s for the client's Ack; missing Ack only logs, never gates the queue

## Test Conventions
- All tests in `#[cfg(test)] mod tests { }` within `client.rs`
- Use `fake_server` / `FakeController` for bidirectional protocol tests
- Tests demonstrate: connect, acquire-denied, acquire-grant-holds-fd-and-lends-dup, pump flow, roundtrip mappings
- Use `sendfd::SendWithFd`/`RecvWithFd` for SCM_RIGHTS fd passing in tests
- Tests should compile and run without external dependencies (fake in-process server)