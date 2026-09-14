# libsgc-rs

Rust client library for the [**simple-graphics-controller**](https://github.com/lulkien/simple-graphics-controller)
daemon (`@sgc`) — the process that owns a machine's display and input devices and
hands them out as revocable leases.

An app links this crate, connects to the daemon, acquires what it wants (the DRM
card it renders on, its keyboard/mouse/touch devices) and pumps events.
Everything on the wire — framing, `SCM_RIGHTS` fd passing, the `Ack` after a
grant, the `Release` revoke-ack — happens synchronously on the app's own thread.
No background threads, no shared state, no callbacks required.

## The model

- **Connect** — [`SgcClient::connect`] returns the client and the resources the
  daemon currently advertises. That list is a snapshot: the daemon pushes a fresh
  one ([`SgcEvent::Advertised`]) whenever it changes, which is how a device
  plugged in later reaches an app that is already running.
- **Acquire** — [`SgcClient::acquire`] blocks until the daemon grants or denies
  the resource.
- **Hold and borrow** — the client owns the granted fd; [`SgcClient::fd`] returns
  a **dup** of it, never the canonical. The canonical is dropped on revoke or
  disconnect, so the library cannot leak a resource the app forgot about.
- **Pump** — [`SgcClient::pump`] returns at most one event per call
  (`Ok(None)` means nothing happened yet — a timeout, a signal — so call again):

| event | meaning |
| --- | --- |
| [`SgcEvent::Granted { resource, fd }`] | a fresh fd, owned by the caller. Either the answer to an acquire, or unsolicited: a re-grant after a revoke, or the device behind a resource you already hold coming back — it **replaces** the fd you had. The `Ack` is already sent; do not acquire again. |
| [`SgcEvent::Revoked { resource }`] | the daemon wants it back. Drop the fd and stop using the resource; the library has already dropped the canonical and sent the revoke-ack. |
| [`SgcEvent::Advertised { available_resources }`] | the daemon's list changed (a device was plugged in or removed). The whole list, not a delta, so a missed event costs nothing. |

When the connection dies, `pump` first returns one `Revoked` per resource still
held (one per call), then the error that ended the session. [`SgcClient::held`]
lists what is currently held; [`SgcClient::start_event_loop`] is the callback
convenience wrapper over `pump`.

## What the daemon enforces

- **The display comes first.** Input belongs to the client holding the display,
  so acquire the `Drm` (or `Fbdev`) resource before the devices. A client with no
  display may still hold a device nobody else asks for, but it never takes one
  from another holder and never queues for one.
- **A device that is unplugged is suspended, not revoked.** The holder keeps the
  resource and is handed a fresh fd for it when the device comes back — no
  `Revoke`, no re-acquire. Losing the *display* is a real revoke: the devices go
  with the seat, and an app that is re-granted the display asks for them again.

## Example

```rust,no_run
use libsgc_rs::{Resource, SgcClient, SgcEvent};
use std::os::fd::AsRawFd;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut client, advertised) = SgcClient::connect()?;

    // The display first: input belongs to the client that holds it.
    let drm = advertised
        .iter()
        .find(|resource| matches!(resource, Resource::Drm { .. }))
        .cloned()
        .ok_or("the daemon advertises no DRM card")?;
    client.acquire(drm)?;

    // Then the devices, best effort.
    for resource in advertised
        .iter()
        .filter(|resource| matches!(resource, Resource::Input(_)))
        .cloned()
    {
        if let Err(err) = client.acquire(resource.clone()) {
            eprintln!("no {resource:?}: {err}");
        }
    }

    loop {
        match client.pump(None) {
            Ok(Some(SgcEvent::Granted { resource, fd })) => {
                println!("{resource:?} on fd {}", fd.as_raw_fd());
            }
            Ok(Some(SgcEvent::Revoked { resource })) => println!("lost {resource:?}"),
            Ok(Some(SgcEvent::Advertised { available_resources })) => {
                println!("the daemon now offers {available_resources:?}");
            }
            Ok(None) => continue, // nothing this call; pump again
            Err(err) => {
                eprintln!("the session is over: {err}");
                break;
            }
        }
    }
    Ok(())
}
```

## Build and test

```console
cargo build              # the library
cargo test               # unit tests (the wire codec against a mock daemon)
```

The crate is standalone: it depends on the protocol crate, not on the daemon.

## See also

- [`docs/libsgc.md`](https://github.com/lulkien/libsgc-rs/blob/master/docs/libsgc.md) —
  the client-side session in detail (ownership, the revoke handshake, the pump).
- [simple-graphics-protocol](https://github.com/lulkien/simple-graphics-protocol) —
  the wire contract this crate speaks.
- [libsgc-c](https://github.com/lulkien/libsgc-c) — the same client as a C ABI,
  for C and C++ apps.

## License

Unlicense — public domain, see [LICENSE](LICENSE).

[`SgcClient::connect`]: SgcClient::connect
[`SgcClient::acquire`]: SgcClient::acquire
[`SgcClient::fd`]: SgcClient::fd
[`SgcClient::held`]: SgcClient::held
[`SgcClient::pump`]: SgcClient::pump
[`SgcClient::start_event_loop`]: SgcClient::start_event_loop
[`SgcEvent::Granted { resource, fd }`]: SgcEvent::Granted
[`SgcEvent::Revoked { resource }`]: SgcEvent::Revoked
[`SgcEvent::Advertised { available_resources }`]: SgcEvent::Advertised
