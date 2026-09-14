//! The client session: [`SgcClient`] and its [`SgcEvent`]s.
//!
//! CLIENT-SIDE RESOURCE OWNERSHIP: the client holds the granted fds (one
//! per resource) and LENDS them out — [`SgcClient::fd`] returns a fresh
//! dup, never the canonical fd. The canonical is owned by the client and
//! dropped when the resource is revoked or the session ends — the library
//! cannot leak a resource the app forgot about.
//!
//! Single-threaded: one app thread drives acquire + the event loop;
//! borrowers (e.g. a render task) talk to the app via channels and never
//! to the client. The app thread must stay in the event loop while
//! resources are held — the server's 5s revoke/ack deadlines are real.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, ErrorKind, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr as StdSocketAddr, UnixStream},
    },
    time::Duration,
};

use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};

use crate::error::SgcError;

/// How long [`SgcClient::acquire`] waits for its own reply before calling the
/// request queued. Every non-queued answer (grant, deny) comes back in
/// milliseconds; a queued one gets NO reply, and its `Grant` arrives later as
/// an event — so a short wait here is what keeps a queued ask from stalling the
/// caller (and a UI's timer with it) for the daemon's whole revoke deadline.
const ACQUIRE_REPLY_WAIT: Duration = Duration::from_millis(250);

/// Events delivered to the app through the event-loop callback.
///
/// The library maps the wire protocol to these; the app maps them to its
/// own commands (e.g. a render task's `Draw`/`Stop`).
#[derive(Debug)]
pub enum SgcEvent {
    /// The server revoked `resource`: drop the fd and stop drawing. The
    /// library already dropped the canonical and replied `Release` (the
    /// revoke-ack; the server waits up to 5s for it) before returning
    /// this event.
    Revoked { resource: Resource },
    /// The server granted `resource` without an `acquire` of ours — either it
    /// re-granted after a revoke (we were requeued), or the device behind a
    /// resource we already hold went away and came back, in which case this is
    /// simply the device again under the same name. Either way it is a fresh
    /// dup of the device fd, owned by the app, valid until the next
    /// [`SgcEvent::Revoked`], and it REPLACES the fd we had for that resource.
    /// The protocol `Ack` was already sent by the library — no second `acquire`
    /// needed.
    Granted { resource: Resource, fd: OwnedFd },
    /// The server's list of available resources changed — a device was plugged
    /// in or removed. The app's initial view came from [`SgcClient::connect`];
    /// this is the update, and it is the only way a connection that is already
    /// up learns about a device that appeared later. The list is the whole list
    /// (not a delta), so a missed event costs nothing.
    Advertised { available_resources: Vec<Resource> },
}

/// A connected session to the graphics controller, driven by one thread.
///
/// Owns the granted fds: `acquire` stores them in [`SgcClient::held`],
/// [`SgcClient::fd`] lends dups, revoke/disconnect drops them.
#[derive(Debug)]
pub struct SgcClient {
    stream: UnixStream,
    /// Canonical fds of the resources we hold, owned by the client. Never
    /// handed out by value — [`SgcClient::fd`] dups.
    held: HashMap<Resource, OwnedFd>,
    /// The error that ended the connection, if any. Once the stream dies,
    /// [`SgcClient::pump`] first surfaces [`SgcEvent::Revoked`] for every
    /// held resource (one per call), then this error.
    fatal: Option<SgcError>,
    /// Events that arrived while [`SgcClient::acquire`] was waiting for its
    /// reply: the daemon pushes the resource list, revokes, and grants queued
    /// requests whenever it likes, so a reply is not necessarily the next
    /// frame. They are handed out by [`SgcClient::pump`] before the socket.
    pending: VecDeque<SgcEvent>,
}

impl SgcClient {
    /// Connect to the controller's abstract socket `@sgc` and read its
    /// `Advertise`. Returns the client plus the advertised resources.
    pub fn connect() -> Result<(Self, Vec<Resource>), SgcError> {
        Self::connect_at(b"sgc")
    }

    /// Like [`SgcClient::connect`], but to an arbitrary abstract socket
    /// name (used by tests; the name must not include the leading NUL).
    pub(crate) fn connect_at(name: &[u8]) -> Result<(Self, Vec<Resource>), SgcError> {
        let addr = StdSocketAddr::from_abstract_name(name).map_err(SgcError::Io)?;
        let mut stream = UnixStream::connect_addr(&addr).map_err(SgcError::ConnectFailed)?;

        let (msg, fds) = read_framed(&mut stream)?;
        if !fds.is_empty() {
            eprintln!("warning: Advertise carried {} unexpected fds", fds.len());
        }
        match msg {
            ServerMessage::Advertise {
                available_resources,
            } => {
                println!(
                    "connected to @{}; available: {available_resources:?}",
                    String::from_utf8_lossy(name)
                );
                Ok((
                    Self {
                        stream,
                        held: HashMap::new(),
                        fatal: None,
                        pending: VecDeque::new(),
                    },
                    available_resources,
                ))
            }
            other => Err(SgcError::UnexpectedMessage(other)),
        }
    }

    /// Request `resource` and wait for the server's answer to THAT request.
    ///
    /// The answer is not necessarily the next frame: the server pushes its
    /// resource list, revokes and re-grants whenever it likes, and a request it
    /// QUEUES gets no answer at all. Frames that answer something else are kept
    /// for [`SgcClient::pump`]; a queued request returns [`SgcError::Queued`]
    /// after a short wait, and its `Grant` arrives later as
    /// [`SgcEvent::Granted`]. On a grant the fd is stored in `held`
    /// (client-owned) and the app borrows it via [`SgcClient::fd`]; on a `Deny`
    /// nothing is held.
    pub fn acquire(&mut self, resource: Resource) -> Result<(), SgcError> {
        self.write_frame(&ClientRequest::Acquire {
            resource: resource.clone(),
        })?;

        // The reply may not be the next frame: the daemon pushes its resource
        // list, revokes, and re-grants whenever it likes, and an acquire it
        // QUEUES gets no reply at all (the Grant arrives later as an event, see
        // `AcquireOutcome::Queued`). Reading one frame and calling anything else
        // an error desynchronised the stream for every later call, which is how
        // a re-acquire after a handover used to lose devices.
        let deadline = std::time::Instant::now() + ACQUIRE_REPLY_WAIT;

        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() || !self.poll_readable(Some(left))? {
                return Err(SgcError::Queued { resource });
            }

            // A read error here means the connection is over: fail this
            // acquire, and let the next `pump` end the session properly (its
            // read sees the same dead stream and drains what we still hold).
            let (msg, fds) = read_framed(&mut self.stream)?;

            match msg {
                ServerMessage::Grant { resource: granted } if granted == resource => {
                    // Our answer. `handle_message` stores the canonical, acks it
                    // and lends the dup; the caller borrows with `fd()`.
                    self.handle_message(ServerMessage::Grant { resource: granted }, fds)?;
                    return Ok(());
                }
                ServerMessage::Deny { reason } => {
                    close_fds(fds);
                    return Err(SgcError::Denied { reason });
                }
                // Not ours: keep it for the event loop and keep waiting.
                other => {
                    if let Some(event) = self.handle_message(other, fds)? {
                        self.pending.push_back(event);
                    }
                }
            }
        }
    }

    /// Borrow `resource`: returns a DUP of the held fd (the client keeps
    /// the canonical). The borrower owns the dup and must drop it when the
    /// resource is revoked (it learns that via [`SgcEvent::Revoked`]).
    pub fn fd(&self, resource: &Resource) -> Result<OwnedFd, SgcError> {
        self.held
            .get(resource)
            .ok_or_else(|| SgcError::NotHeld {
                resource: resource.clone(),
            })
            .and_then(|fd| fd.try_clone().map_err(SgcError::Io))
    }

    /// The resources this session currently holds.
    pub fn held(&self) -> Vec<Resource> {
        self.held.keys().cloned().collect()
    }

    /// Drive the protocol: wait for one server frame and return the
    /// resulting event.
    ///
    /// Blocks up to `timeout` (`None` = wait until a frame or error
    /// arrives). [`SgcEvent`]s come back one per call, so loop-shaped
    /// consumers drive this from their own main loop. `Ok(None)` means no
    /// event this call — the wait elapsed, a signal interrupted it, or an
    /// anomalous frame was consumed and skipped (a grant without its fd,
    /// an unexpected message; noted on stderr) — re-pump.
    ///
    /// The protocol work for an event is performed here, before the event
    /// is returned: the `Ack` after a grant, the `Release` revoke-ack
    /// after a revoke. The caller never touches the wire.
    ///
    /// Ownership: a [`SgcEvent::Granted`] fd is a fresh dup owned by the
    /// caller; drop it when the resource is revoked. The client keeps the
    /// canonical until then.
    ///
    /// Errors: the connection is over (EOF, socket failure). The client
    /// disowns everything it holds first — [`SgcEvent::Revoked`] is
    /// returned for each held resource, one per `pump` call, in no
    /// particular order — then the error surfaces.
    pub fn pump(&mut self, timeout: Option<Duration>) -> Result<Option<SgcEvent>, SgcError> {
        // Events kept aside while `acquire` waited for its reply come first.
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        // Disconnect drain: the stream died on an earlier call; surface
        // one Revoked per still-held resource, then the fatal error.
        if self.fatal.is_some() {
            return self.drain_disconnect();
        }
        if !self.poll_readable(timeout)? {
            return Ok(None);
        }
        let (msg, fds) = match read_framed(&mut self.stream) {
            Ok(frame) => frame,
            Err(err) => return self.begin_disconnect(err),
        };
        self.handle_message(msg, fds)
    }

    /// Turn one server frame into the client's state change and, when there is
    /// one, the event the application sees. Shared by [`SgcClient::pump`] and
    /// [`SgcClient::acquire`] so both keep the same semantics.
    fn handle_message(
        &mut self,
        msg: ServerMessage,
        fds: Vec<RawFd>,
    ) -> Result<Option<SgcEvent>, SgcError> {
        match msg {
            ServerMessage::Revoke { resource } => {
                // Drop the canonical; the app drops its dup on Revoked.
                self.held.remove(&resource);
                close_fds(fds);
                // Revoke-ack: Release doubles as the acknowledgment.
                let _ = self.write_frame(&ClientRequest::Release {
                    resource: resource.clone(),
                });
                Ok(Some(SgcEvent::Revoked { resource }))
            }
            ServerMessage::Grant { resource } => {
                if fds.len() != 1 {
                    eprintln!("grant carried {} fds; ignoring", fds.len());
                    close_fds(fds);
                    return Ok(None);
                }
                // Safety: SCM_RIGHTS transfers ownership to us.
                let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                let _ = self.write_frame(&ClientRequest::Ack);
                // Re-grant: store the canonical, lend a dup to the app.
                self.held.insert(resource.clone(), fd);
                match self.fd(&resource) {
                    Ok(fd) => Ok(Some(SgcEvent::Granted { resource, fd })),
                    Err(e) => {
                        eprintln!("lend failed: {e}");
                        Ok(None)
                    }
                }
            }
            ServerMessage::Advertise {
                available_resources,
            } => {
                close_fds(fds);
                Ok(Some(SgcEvent::Advertised {
                    available_resources,
                }))
            }
            other => {
                eprintln!("unexpected server message: {other:?}; ignoring");
                Ok(None)
            }
        }
    }

    /// The client's event loop, callback-shaped: pump in a loop and
    /// dispatch [`SgcEvent`]s to `on_event` until the connection ends.
    ///
    /// A convenience over [`SgcClient::pump`] for callback-style apps;
    /// loop-shaped consumers (LVGL, game loops, kmscube-style select
    /// loops) drive `pump()` directly instead.
    pub fn start_event_loop<F>(&mut self, mut on_event: F)
    where
        F: FnMut(SgcEvent),
    {
        loop {
            match self.pump(None) {
                Ok(Some(event)) => on_event(event),
                Ok(None) => continue,
                Err(err) => {
                    println!("connection lost: {err}");
                    break;
                }
            }
        }
    }

    /// Disconnect path: stash the fatal error, then drain the held
    /// resources as [`SgcEvent::Revoked`] (one per `pump` call).
    fn begin_disconnect(&mut self, err: SgcError) -> Result<Option<SgcEvent>, SgcError> {
        self.fatal = Some(err);
        self.drain_disconnect()
    }

    fn drain_disconnect(&mut self) -> Result<Option<SgcEvent>, SgcError> {
        if let Some(resource) = self.held.keys().next().cloned() {
            self.held.remove(&resource);
            Ok(Some(SgcEvent::Revoked { resource }))
        } else {
            Err(self.fatal.take().expect("fatal error stashed before drain"))
        }
    }

    /// poll(2) the socket for readability. `None` blocks until readable;
    /// `Some(0)` polls once (the C ABI's "poll once"); `Ok(false)` = the
    /// wait elapsed (or a signal interrupted a bounded wait).
    fn poll_readable(&self, timeout: Option<Duration>) -> Result<bool, SgcError> {
        let timeout_ms: i32 = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as i32,
        };
        let mut pfd = libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // Safety: poll on our own socket fd; pfd is stack-local.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if n >= 0 {
                return Ok(n > 0);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                if timeout.is_some() {
                    return Ok(false); // bounded wait: a signal is "nothing happened"
                }
                continue; // indefinite wait: retry on signal
            }
            return Err(SgcError::Io(err));
        }
    }

    /// Serialize + write one request frame.
    fn write_frame(&mut self, req: &ClientRequest) -> Result<(), SgcError> {
        let frame = serialize_framed(req)?;
        self.stream.write_all(&frame).map_err(SgcError::Io)
    }
}

/// Blocking framed read; collects any SCM_RIGHTS fds (only `Grant` carries
/// them). `Err` on EOF — the caller treats that as "connection over".
fn read_framed(stream: &mut UnixStream) -> Result<(ServerMessage, Vec<RawFd>), SgcError> {
    let mut fds: Vec<RawFd> = Vec::new();
    let mut fd_buf = [0i32; 16];

    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut got = 0;
    while got < FRAME_HEADER_LEN {
        let (n, nfds) = stream
            .recv_with_fd(&mut header[got..], &mut fd_buf)
            .map_err(SgcError::Io)?;
        if n == 0 {
            return Err(SgcError::Io(io::Error::from(ErrorKind::UnexpectedEof)));
        }
        got += n;
        fds.extend_from_slice(&fd_buf[..nfds]);
    }

    let len = parse_frame_header(&header)?;
    let mut payload = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let (n, nfds) = stream
            .recv_with_fd(&mut payload[got..], &mut fd_buf)
            .map_err(SgcError::Io)?;
        if n == 0 {
            return Err(SgcError::Io(io::Error::from(ErrorKind::UnexpectedEof)));
        }
        got += n;
        fds.extend_from_slice(&fd_buf[..nfds]);
    }

    let msg = deserialize(&payload)?;
    Ok((msg, fds))
}

/// Close SCM_RIGHTS fds we received but are not consuming (anomalous
/// grants, or fds attached to messages that carry none). Ownership of the
/// fds was transferred to this process by the receive.
fn close_fds(fds: Vec<RawFd>) {
    for fd in fds {
        // Safety: ownership of `fd` was transferred to us by SCM_RIGHTS;
        // we choose not to keep it.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendfd::SendWithFd;
    use std::{
        io::{Read, Write},
        os::{
            fd::{AsRawFd, IntoRawFd},
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
        sync::mpsc,
        thread,
    };

    /// Like [`fake_server`], but sends a SEQUENCE of messages after the opening
    /// `Advertise` — for the cases where the reply is not the next frame.
    fn fake_server_seq(
        name: &'static [u8],
        frames: Vec<(ServerMessage, Option<RawFd>)>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = thread::spawn(move || {
            let addr = SocketAddr::from_abstract_name(name).expect("address");
            let listener = UnixListener::bind_addr(&addr).expect("bind");
            ready_tx.send(()).expect("ready signal");
            let (mut stream, _) = listener.accept().expect("accept");
            let adv = serialize_framed(&ServerMessage::Advertise {
                available_resources: vec![Resource::Fbdev],
            })
            .expect("serialize advertise");
            stream.write_all(&adv).expect("write advertise");
            for (msg, fd) in frames {
                let frame = serialize_framed(&msg).expect("serialize frame");
                match fd {
                    Some(fd) => {
                        stream
                            .send_with_fd(&frame, &[fd])
                            .expect("send frame with fd");
                    }
                    None => stream.write_all(&frame).expect("write frame"),
                }
            }
            let mut buf = [0u8; 64];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        (handle, ready_rx)
    }

    /// The daemon pushes its list whenever it changes, so a reply can arrive
    /// behind an `Advertise`. `acquire` must keep it and still return Ok.
    #[test]
    fn acquire_survives_an_event_that_arrives_first() {
        let fd = std::fs::File::open("/dev/null").expect("fd").into_raw_fd();
        let (server, ready) = fake_server_seq(
            b"sgc-test-interleaved",
            vec![
                (
                    ServerMessage::Advertise {
                        available_resources: vec![Resource::Fbdev, Resource::Drm { card: 1 }],
                    },
                    None,
                ),
                (
                    ServerMessage::Grant {
                        resource: Resource::Fbdev,
                    },
                    Some(fd),
                ),
            ],
        );
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-interleaved").expect("connect");

        client
            .acquire(Resource::Fbdev)
            .expect("the interleaved push must not break the acquire");
        assert!(client.held().contains(&Resource::Fbdev), "granted and held");

        // ... and the event it skipped is not lost.
        let event = client.pump(Some(Duration::from_millis(100))).expect("pump");
        assert!(
            matches!(
                event,
                Some(SgcEvent::Advertised { ref available_resources })
                    if available_resources.len() == 2
            ),
            "expected the pushed list, got {event:?}"
        );

        drop(client);
        let _ = server.join();
    }

    /// A queued acquire gets no reply: it must come back as `Queued` (not as a
    /// desynchronised error) and quickly, because the Grant arrives later as an
    /// event.
    #[test]
    fn acquire_reports_a_queue_instead_of_desyncing() {
        let (server, ready) = fake_server_seq(b"sgc-test-queued", vec![]);
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-queued").expect("connect");

        let started = std::time::Instant::now();
        let err = client
            .acquire(Resource::Fbdev)
            .expect_err("nothing was granted");
        assert!(
            matches!(err, SgcError::Queued { ref resource } if *resource == Resource::Fbdev),
            "expected Queued, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the queued answer must not hang the caller"
        );

        drop(client);
        let _ = server.join();
    }

    /// Minimal fake controller: bind an abstract listener, signal `ready`,
    /// accept one client, send `Advertise`, optionally reply with `reply`
    /// (written immediately — the client's request sits in the socket
    /// buffer), then hold the connection open until the client drops.
    fn fake_server(
        name: &'static [u8],
        reply: Option<(ServerMessage, Option<RawFd>)>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = thread::spawn(move || {
            let addr = SocketAddr::from_abstract_name(name).expect("address");
            let listener = UnixListener::bind_addr(&addr).expect("bind");
            ready_tx.send(()).expect("ready signal");
            let (mut stream, _) = listener.accept().expect("accept");
            let adv = serialize_framed(&ServerMessage::Advertise {
                available_resources: vec![Resource::Fbdev],
            })
            .expect("serialize advertise");
            stream.write_all(&adv).expect("write advertise");
            if let Some((msg, fd)) = reply {
                let frame = serialize_framed(&msg).expect("serialize reply");
                match fd {
                    Some(fd) => {
                        stream
                            .send_with_fd(&frame, &[fd])
                            .expect("send reply with fd");
                    }
                    None => stream.write_all(&frame).expect("write reply"),
                }
            }
            // Drain requests/acks until the client drops (EOF), then exit.
            let mut buf = [0u8; 64];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });
        (handle, ready_rx)
    }

    #[test]
    fn connect_fails_cleanly_when_no_server() {
        let err = SgcClient::connect_at(b"sgc-test-nowhere").expect_err("must fail");
        assert!(
            matches!(err, SgcError::ConnectFailed(_)),
            "expected ConnectFailed, got {err:?}"
        );
    }

    #[test]
    fn connect_reads_advertise_and_returns_resources() {
        let (server, ready) = fake_server(b"sgc-test-adv", None);
        ready.recv().expect("server ready");
        let (client, available) = SgcClient::connect_at(b"sgc-test-adv").expect("connect");
        assert_eq!(available, vec![Resource::Fbdev]);
        assert!(client.held().is_empty());
        drop(client);
        server.join().expect("server thread finished");
    }

    #[test]
    fn acquire_denied_surfaces_reason() {
        let (server, ready) = fake_server(
            b"sgc-test-deny",
            Some((
                ServerMessage::Deny {
                    reason: "first-owner policy".into(),
                },
                None,
            )),
        );
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-deny").expect("connect");
        let err = client.acquire(Resource::Fbdev).expect_err("must be denied");
        assert!(
            matches!(err, SgcError::Denied { ref reason } if reason == "first-owner policy"),
            "expected Denied, got {err:?}"
        );
        assert!(client.held().is_empty(), "nothing held on deny");
        drop(client);
        server.join().expect("server thread finished");
    }

    #[test]
    fn acquire_grant_holds_fd_and_lends_dup() {
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        let (server, ready) = fake_server(
            b"sgc-test-grant",
            Some((
                ServerMessage::Grant {
                    resource: Resource::Fbdev,
                },
                Some(devnull.as_raw_fd()),
            )),
        );
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-grant").expect("connect");
        client.acquire(Resource::Fbdev).expect("grant");
        assert_eq!(client.held(), vec![Resource::Fbdev]);
        let fd = client.fd(&Resource::Fbdev).expect("lend");
        assert!(fd.as_raw_fd() >= 0, "lent fd must be valid");
        drop(client);
        server.join().expect("server thread finished");
    }

    // --- Interactive fake controller for pump tests ---
    //
    // Unlike `fake_server` (write-a-reply-then-drain), this one is
    // bidirectional: the test sends server frames at will and asserts on
    // the client's requests, in either order. The writer exits when
    // `writer_tx` is dropped; the reader polls (50ms) and exits when
    // killed or the client closes.

    struct FakeController {
        writer_tx: mpsc::Sender<(ServerMessage, Option<RawFd>)>,
        requests: mpsc::Receiver<ClientRequest>,
        reader_kill: Option<mpsc::Sender<()>>,
    }

    fn fake(name: &'static [u8], advertised: Vec<Resource>) -> FakeController {
        let addr = SocketAddr::from_abstract_name(name).expect("address");
        let listener = UnixListener::bind_addr(&addr).expect("bind");
        let (writer_tx, writer_rx) = mpsc::channel();
        let (req_tx, req_rx) = mpsc::channel();
        let (kill_tx, kill_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let adv = serialize_framed(&ServerMessage::Advertise {
                available_resources: advertised,
            })
            .expect("serialize advertise");
            let writer_stream = stream.try_clone().expect("dup for writer");
            let reader_stream = stream.try_clone().expect("dup for reader");
            // Writer: Advertise first (the client's connect blocks on it),
            // then relay whatever the test sends.
            thread::spawn(move || {
                let mut stream = writer_stream;
                stream.write_all(&adv).expect("write advertise");
                loop {
                    match writer_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok((msg, fd)) => {
                            let frame = serialize_framed(&msg).expect("serialize message");
                            match fd {
                                Some(fd) => {
                                    stream.send_with_fd(&frame, &[fd]).expect("send with fd");
                                }
                                None => stream.write_all(&frame).expect("write message"),
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            // Reader: relay the client's requests to the test.
            thread::spawn(move || {
                let mut stream = reader_stream;
                loop {
                    if kill_rx.try_recv().is_ok() {
                        break;
                    }
                    let mut pfd = libc::pollfd {
                        fd: stream.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // Safety: poll on our own socket fd; pfd is stack-local.
                    let n = unsafe { libc::poll(&mut pfd, 1, 50) };
                    if n <= 0 {
                        continue; // timeout or signal: check kill again
                    }
                    match read_client_frame(&mut stream) {
                        Ok(req) => {
                            if req_tx.send(req).is_err() {
                                break; // test went away
                            }
                        }
                        Err(_) => break, // client closed
                    }
                }
            });
        });
        FakeController {
            writer_tx,
            requests: req_rx,
            reader_kill: Some(kill_tx),
        }
    }

    impl FakeController {
        fn send(&self, msg: ServerMessage, fd: Option<RawFd>) {
            self.writer_tx.send((msg, fd)).expect("controller alive");
        }

        /// Block until the client sends its next request.
        fn next_request(&self) -> ClientRequest {
            self.requests.recv().expect("client sent a request")
        }
    }

    impl Drop for FakeController {
        fn drop(&mut self) {
            // Kill the reader so it drops its socket dup; the writer exits
            // on its own once `writer_tx` goes away. Only when BOTH server
            // ends are closed does the client see EOF.
            if let Some(kill) = self.reader_kill.take() {
                let _ = kill.send(());
            }
        }
    }

    /// Read one raw client frame (header + payload). Client requests carry
    /// no fds, so a plain read suffices.
    fn read_client_frame(stream: &mut UnixStream) -> io::Result<ClientRequest> {
        let mut header = [0u8; FRAME_HEADER_LEN];
        stream.read_exact(&mut header)?;
        let len = parse_frame_header(&header).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("bad frame header: {e}"))
        })?;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        deserialize(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad frame: {e}")))
    }

    #[test]
    fn pump_times_out_with_no_frame() {
        let controller = fake(b"sgc-test-pump-timeout", vec![Resource::Fbdev]);
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-pump-timeout").expect("connect");
        assert!(
            client
                .pump(Some(Duration::from_millis(50)))
                .expect("bounded pump")
                .is_none(),
            "no frame within 50ms must be Ok(None)"
        );
        assert!(
            client
                .pump(Some(Duration::ZERO))
                .expect("zero-timeout pump")
                .is_none(),
            "zero timeout polls once and must not block"
        );
        drop(client);
        drop(controller);
    }

    #[test]
    fn pump_delivers_revoke_and_replies_release() {
        let controller = fake(b"sgc-test-revoke", vec![Resource::Fbdev]);
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-revoke").expect("connect");
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");

        // acquire() blocks until the grant, so drive it on a thread while
        // the test services the request/response.
        let handle = thread::spawn(move || {
            client.acquire(Resource::Fbdev).expect("acquire");
            client
        });
        assert_eq!(
            controller.next_request(),
            ClientRequest::Acquire {
                resource: Resource::Fbdev
            }
        );
        controller.send(
            ServerMessage::Grant {
                resource: Resource::Fbdev,
            },
            Some(devnull.as_raw_fd()),
        );
        let mut client = handle.join().expect("acquire thread");
        assert_eq!(client.held(), vec![Resource::Fbdev]);
        // acquire() already acked its grant.
        assert_eq!(controller.next_request(), ClientRequest::Ack);

        controller.send(
            ServerMessage::Revoke {
                resource: Resource::Fbdev,
            },
            None,
        );
        match client.pump(None).expect("pump") {
            Some(SgcEvent::Revoked { resource }) => assert_eq!(resource, Resource::Fbdev),
            other => panic!("expected Revoked, got {other:?}"),
        }
        // Revoke-ack: the library replies Release before returning the event.
        assert_eq!(
            controller.next_request(),
            ClientRequest::Release {
                resource: Resource::Fbdev
            }
        );
        assert!(client.held().is_empty(), "canonical dropped on revoke");
        drop(client);
        drop(controller);
    }

    #[test]
    fn pump_delivers_regrant_and_acks() {
        let controller = fake(b"sgc-test-regrant", vec![Resource::Fbdev]);
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-regrant").expect("connect");
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");

        // Unsolicited re-grant (the requeue path): no acquire involved.
        controller.send(
            ServerMessage::Grant {
                resource: Resource::Fbdev,
            },
            Some(devnull.as_raw_fd()),
        );
        match client.pump(None).expect("pump") {
            Some(SgcEvent::Granted { resource, fd }) => {
                assert_eq!(resource, Resource::Fbdev);
                assert!(fd.as_raw_fd() >= 0, "granted fd must be a valid dup");
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert_eq!(controller.next_request(), ClientRequest::Ack);
        assert_eq!(client.held(), vec![Resource::Fbdev]);
        drop(client);
        drop(controller);
    }

    /// The server pushes its list again whenever a device appears or goes away.
    /// It is the only way a connection that is already up learns about a device
    /// plugged in later, so it has to surface as an event rather than being
    /// swallowed as an unexpected message.
    #[test]
    fn pump_delivers_a_pushed_advertise() {
        use simple_graphics_protocol::InputResource;

        let controller = fake(b"sgc-test-push", vec![Resource::Fbdev]);
        let (mut client, advertised) = SgcClient::connect_at(b"sgc-test-push").expect("connect");
        assert_eq!(advertised, vec![Resource::Fbdev]);

        let keyboard = Resource::Input(InputResource::Keyboard(0));
        controller.send(
            ServerMessage::Advertise {
                available_resources: vec![Resource::Fbdev, keyboard.clone()],
            },
            None,
        );
        match client.pump(None).expect("pump") {
            Some(SgcEvent::Advertised {
                available_resources,
            }) => assert_eq!(available_resources, vec![Resource::Fbdev, keyboard]),
            other => panic!("expected Advertised, got {other:?}"),
        }
        drop(client);
        drop(controller);
    }

    #[test]
    fn disconnect_emits_revoked_per_held_then_errors() {
        let controller = fake(
            b"sgc-test-disconnect",
            vec![Resource::Fbdev, Resource::Drm { card: 0 }],
        );
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-disconnect").expect("connect");
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");

        // Hold both advertised resources via two unsolicited grants.
        controller.send(
            ServerMessage::Grant {
                resource: Resource::Fbdev,
            },
            Some(devnull.as_raw_fd()),
        );
        assert!(client.pump(None).expect("grant fbdev").is_some());
        controller.send(
            ServerMessage::Grant {
                resource: Resource::Drm { card: 0 },
            },
            Some(devnull.as_raw_fd()),
        );
        assert!(client.pump(None).expect("grant drm").is_some());
        assert_eq!(client.held().len(), 2);

        // Server goes away: every held resource is revoked, then an error.
        drop(controller);
        let mut revoked: Vec<Resource> = Vec::new();
        loop {
            match client.pump(None) {
                Ok(Some(SgcEvent::Revoked { resource })) => revoked.push(resource),
                Ok(_) => continue,
                Err(err) => {
                    assert!(
                        matches!(err, SgcError::Io(_)),
                        "expected io error after drain, got {err:?}"
                    );
                    break;
                }
            }
        }
        assert_eq!(revoked.len(), 2, "one Revoked per held resource");
        assert!(revoked.contains(&Resource::Fbdev));
        assert!(revoked.contains(&Resource::Drm { card: 0 }));
        assert!(client.held().is_empty(), "nothing held after disconnect");
    }
}
