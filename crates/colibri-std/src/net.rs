//! `net`: TCP sockets behind the `net` capability (DESIGN §13.1).
//!
//! `net.listen(addr)` gives a `net.Listener`, `net.connect(addr)` (or `net.connect_timeout`) a
//! `net.Conn`, and `listener.accept()` another one: the two ends of a connection are the same
//! type. Every address is `"host:port"` (`"127.0.0.1:8080"`, `"[::1]:0"`, `"localhost:9000"`);
//! port `0` asks the system for a free port, and `addr()` says which one it picked.
//!
//! # How waiting works (the same on Linux, macOS and Windows)
//!
//! Everything that can wait runs on a background thread (`Ctx::run_async_cancellable`), so other
//! coroutines keep running meanwhile. Every socket (listener, accepted and connected streams) is
//! non-blocking. A worker waits for readiness with `poll(2)` (Unix) or `WSAPoll` (Windows) for
//! at most one slice (`SLICE`, 50 ms), and only when the socket is ready does it touch a buffer
//! and make its system call (a write and an accept try theirs first and wait on `WouldBlock`),
//! so an idle wait costs one `poll` per slice. Between slices it looks at three things: whether its
//! coroutine was cancelled (or the `Vm` ended), whether the object was closed, and whether the
//! deadline has passed. So a cancelled operation stops within a slice, a `close()` on another
//! coroutine's object ends that coroutine's wait, and no thread outlives its `Vm` by more than
//! a slice. No socket is ever left in a state where a timed-out call made it unusable (Windows
//! documents that for `SO_RCVTIMEO`, which is why the sockets carry no kernel timeouts).
//!
//! Timeouts (`set_timeout`, `connect_timeout`) are deadlines that start when the script calls
//! the method, on the interpreter thread, not when a worker gets round to it.
//!
//! # Cancelling never loses data
//!
//! A read only fills the connection's buffer on its worker; the bytes are handed out by the
//! interpreter thread after the coroutine has resumed, while it still holds the read turn. A
//! coroutine that is cancelled after its worker finished takes nothing, so the bytes stay for
//! the next read. An accept that finished for a coroutine that was cancelled meanwhile puts the
//! connection into a queue on the listener, and the next `accept` returns it first.
//!
//! # `close()` never suspends and never fails
//!
//! It sets the closed flag, takes the object's reference to the socket, shuts the socket down
//! (a blocked read wakes at once), wakes everything that waits for a turn, and returns. The
//! socket is reference counted: every operation holds a reference while it works, and the
//! descriptor is released when the last one is gone (at once, or within one slice for a worker
//! whose `poll` the shutdown did not wake), so a `poll` never sees a closed or reused descriptor
//! and `close` waits for nobody.
//!
//! A `Conn` is full duplex. Reads wait for one turn queue, writes for another, and `close` and
//! `close_write` take neither: two operations of one kind are never interleaved, and they are
//! served in the order they were called. An operation that waits for its turn sleeps on a
//! condition variable until the turn comes, the object is closed, its coroutine is cancelled
//! (a hook of the cancel token wakes it) or its deadline passes: it does not tick.
//!
//! # Where nothing else can run
//!
//! A call made from a `Drop` hook or a callback of a host function cannot suspend: it blocks the
//! interpreter thread (`Ctx::can_suspend` is `false`). A read (or accept) holds its turn until
//! its coroutine has resumed and taken the result, so one that is called there while an earlier
//! read (accept) is outstanding would wait for a coroutine that can never resume. It returns a
//! `net.Error.Other` at once instead. A write takes no turn that outlives its worker, and
//! `close_write` waits only for writes, so neither has the problem.
//!
//! # A write that stops part way breaks the write side
//!
//! A write that is cancelled, times out or fails after it sent some of its data but not all
//! leaves the stream out of step. Every later write on that connection fails with `BrokenPipe`
//! at once (reads keep working); a write that sent nothing changes nothing. A write to a peer
//! that is gone is `BrokenPipe` on every system (Windows says "reset" for it).
//!
//! What a read gets from the socket is kept in a buffer inside the `Conn`; `read_line` and
//! `read_exact` consume from it only when they can return, so a timeout in the middle of a line
//! leaves everything for the next call. A read that finds what it needs in the buffer does not
//! leave the coroutine at all.

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};

use colibri_runtime::{Bytes, CancelToken, Cap, Ctx, Error, HostType, TypeBuilder, TypedError, Vm};

use crate::bytes::utf8_problem;
use crate::errors::error_kind;
use crate::fs::reason;
use crate::needs;

error_kind!(
    "net",
    /// Why a socket operation failed: what a script branches on (`net.Error.Refused`).
    Kind {
        /// Nothing is listening at the address.
        Refused,
        /// A read, a write or the wait of a read took longer than `set_timeout` allows (the
        /// connection stays usable for reading; for writing too, unless the write sent only
        /// part of its data), or a connect took longer than `connect_timeout` allows or than
        /// the system waits.
        Timeout,
        /// The peer reset the connection (or the network dropped it).
        Reset,
        /// Writing to a connection that cannot take more: its writing end is shut down
        /// (`close_write`), the peer is gone (reported the same way on every system, also where
        /// the system says "reset"), or an earlier write stopped part way and left the stream
        /// out of step. Reading is not affected.
        BrokenPipe,
        /// `listen` on an address that another socket already uses.
        AddrInUse,
        /// `listen` on an address that no interface of this machine has.
        AddrNotAvailable,
        /// The machine or network cannot be reached.
        Unreachable,
        /// The name did not resolve.
        Dns,
        /// An address that is not `host:port`.
        InvalidAddress,
        PermissionDenied,
        /// A `net.Listener` or `net.Conn` that was closed here.
        Closed,
        /// `read_exact` met the end of the stream before it had `n` bytes.
        UnexpectedEnd,
        /// `read_line` found no end of line in the first mebibyte.
        LineTooLong,
        /// `read_line` got bytes that are not UTF-8 text.
        InvalidData,
        /// Anything else, with the system's own text (also: out of descriptors or memory, the
        /// helper thread of a name lookup that cannot start; a thread of the pool that cannot
        /// start is a panic, not this). The one variant whose text is not Colibri's own.
        Other,
    }
);

/// How long a worker waits for readiness before it looks at its stop conditions again.
const SLICE: Duration = Duration::from_millis(50);

/// The longest line `read_line` accepts, without its end of line.
pub(crate) const MAX_LINE: usize = 1 << 20;

/// The most bytes one `read_exact` buffers.
const MAX_EXACT: usize = 1 << 30;

/// The most one `send` is given, so that a cancel or a deadline is noticed between sends.
const SEND_CHUNK: usize = 256 * 1024;

/// What `listen` asks the system for: connections that wait to be accepted.
const BACKLOG: i32 = 1024;

/// From this many bytes on, reads hand out the buffer itself instead of a copy of it.
const HAND_OUT: usize = 64 * 1024;

/// The variant of `net.Error` a Windows socket error code is, for the codes `std` has no
/// `ErrorKind` for or maps differently. The numbers are the `WSAE*` constants of `winerror.h`.
#[cfg_attr(not(windows), allow(dead_code))]
fn kind_of_windows_code(code: i32) -> Option<Kind> {
    Some(match code {
        // WSAECONNREFUSED
        10061 => Kind::Refused,
        // WSAETIMEDOUT
        10060 => Kind::Timeout,
        // WSAECONNRESET, WSAECONNABORTED, WSAENETRESET, WSAENOTCONN
        10054 | 10053 | 10052 | 10057 => Kind::Reset,
        // WSAESHUTDOWN: a send after `close_write` (`shutdown(SD_SEND)`)
        10058 => Kind::BrokenPipe,
        // WSAEADDRINUSE
        10048 => Kind::AddrInUse,
        // WSAEADDRNOTAVAIL
        10049 => Kind::AddrNotAvailable,
        // WSAENETDOWN, WSAENETUNREACH, WSAEHOSTDOWN, WSAEHOSTUNREACH
        10050 | 10051 | 10064 | 10065 => Kind::Unreachable,
        // WSAEACCES
        10013 => Kind::PermissionDenied,
        _ => return None,
    })
}

/// The variant of `net.Error` a Unix `errno` is, where `ErrorKind` has none.
#[cfg(unix)]
fn kind_of_unix_code(code: i32) -> Option<Kind> {
    match code {
        // The socket was shut down for writing. macOS answers a send on a shut-down socket
        // with `EPROTOTYPE` at times.
        libc::ESHUTDOWN => Some(Kind::BrokenPipe),
        #[cfg(target_vendor = "apple")]
        libc::EPROTOTYPE => Some(Kind::BrokenPipe),
        libc::ENETDOWN | libc::EHOSTDOWN => Some(Kind::Unreachable),
        libc::ENETRESET => Some(Kind::Reset),
        _ => None,
    }
}

/// The variant of `net.Error` an I/O error is.
pub(crate) fn kind_of(e: &io::Error) -> Kind {
    if let Some(code) = e.raw_os_error() {
        #[cfg(windows)]
        if let Some(kind) = kind_of_windows_code(code) {
            return kind;
        }
        #[cfg(unix)]
        if let Some(kind) = kind_of_unix_code(code) {
            return kind;
        }
        #[cfg(not(any(unix, windows)))]
        let _ = code;
    }
    match e.kind() {
        ErrorKind::ConnectionRefused => Kind::Refused,
        ErrorKind::TimedOut | ErrorKind::WouldBlock => Kind::Timeout,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::NotConnected => {
            Kind::Reset
        }
        ErrorKind::BrokenPipe => Kind::BrokenPipe,
        ErrorKind::AddrInUse => Kind::AddrInUse,
        ErrorKind::AddrNotAvailable => Kind::AddrNotAvailable,
        ErrorKind::PermissionDenied => Kind::PermissionDenied,
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => Kind::Unreachable,
        ErrorKind::UnexpectedEof => Kind::UnexpectedEnd,
        ErrorKind::InvalidData => Kind::InvalidData,
        _ => Kind::Other,
    }
}

/// Whether the system ran out of descriptors (the process limit or the machine's).
fn out_of_descriptors(e: &io::Error) -> bool {
    match e.raw_os_error() {
        #[cfg(unix)]
        Some(code) => code == libc::EMFILE || code == libc::ENFILE,
        // WSAEMFILE
        #[cfg(windows)]
        Some(code) => code == 10024,
        _ => false,
    }
}

/// What went wrong, in Colibri's own words for every kind that has a variant (so the text is
/// the same on every system), and in the system's for `Other`.
fn describe(kind: Kind, e: &io::Error, listening: bool) -> String {
    let (text, hint) = match kind {
        Kind::Refused => ("connection refused", " (nothing is listening there)"),
        Kind::Timeout => ("timed out", " (the other side did not answer in time)"),
        Kind::Reset => ("connection reset by the peer", ""),
        Kind::BrokenPipe => (
            "broken pipe",
            " (the writing end is shut down, or the peer is gone)",
        ),
        Kind::AddrInUse => (
            "address already in use",
            " (another socket has that port: stop it, or listen on port 0 for a free one)",
        ),
        Kind::AddrNotAvailable if listening => (
            "address not available",
            " (the address is not one of this machine's: use `0.0.0.0` for all of them)",
        ),
        Kind::AddrNotAvailable => (
            "address not available",
            " (this machine cannot use that address: check it, and that the network is set up for it)",
        ),
        Kind::Unreachable => (
            "network or host unreachable",
            " (check the address and the network)",
        ),
        Kind::PermissionDenied => (
            "permission denied",
            " (ports below 1024 may need more rights: use a higher port)",
        ),
        Kind::UnexpectedEnd => ("unexpected end of stream", ""),
        Kind::InvalidData => ("invalid data", ""),
        _ if out_of_descriptors(e) => {
            return "too many open files (this program has no descriptor left: close the connections it does not need)".to_string();
        }
        _ => return reason(e),
    };
    format!("{text}{hint}")
}

/// An I/O error as a `(variant, message)` that says what was attempted and what to do about it.
fn failure(action: &str, what: &str, e: &io::Error) -> (Kind, String) {
    let kind = kind_of(e);
    (
        kind,
        format!(
            "cannot {action} `{what}`: {}",
            describe(kind, e, action.starts_with("listen"))
        ),
    )
}

/// A failure of a write. Windows says "reset" or "aborted" where Linux and macOS say "broken
/// pipe" for a send to a peer that is gone; here both are `BrokenPipe` on every system (nothing
/// more can be sent). The read path keeps `Reset`.
fn write_kind(e: &io::Error) -> Kind {
    match kind_of(e) {
        Kind::Reset => Kind::BrokenPipe,
        kind => kind,
    }
}

fn write_failure(action: &str, what: &str, e: &io::Error) -> (Kind, String) {
    let kind = write_kind(e);
    (
        kind,
        format!("cannot {action} `{what}`: {}", describe(kind, e, false)),
    )
}

/// An I/O error as a `net.Error`.
pub(crate) fn fail(action: &str, what: &str, e: &io::Error) -> TypedError {
    let (kind, message) = failure(action, what, e);
    kind.error(message)
}

/// The note a write adds to its error when some of its data is out already.
fn sent_note(sent: Option<(usize, usize)>) -> String {
    match sent {
        Some((done, total)) if done > 0 => format!(
            " ({done} of {total} bytes were sent, so the stream is out of step: close the connection)"
        ),
        _ => String::new(),
    }
}

// ---- addresses ----

/// Splits `host:port` and checks both. IPv6 hosts are written in brackets.
fn split_addr(addr: &str) -> Result<(&str, u16), TypedError> {
    let invalid =
        |why: String| Kind::InvalidAddress.error(format!("`{addr}` is not an address: {why}"));
    let Some((host, port)) = addr.rsplit_once(':') else {
        return Err(invalid(
            "it needs a port (write `host:port`, like `127.0.0.1:8080` or `[::1]:8080`)".into(),
        ));
    };
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']').ok_or_else(|| {
            invalid("an IPv6 host in brackets must end with `]` before the port".into())
        })?,
        None if host.contains(':') => {
            return Err(invalid(
                "an IPv6 host goes in brackets (write `[::1]:8080`)".into(),
            ));
        }
        None => host,
    };
    if host.is_empty() {
        return Err(invalid(
            "the host is missing (write `127.0.0.1:8080` for this machine only, `0.0.0.0:8080` for every interface)"
                .into(),
        ));
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/')
    {
        return Err(invalid(
            "the host has a space or another character a host cannot have".into(),
        ));
    }
    let port: u16 = port.parse().map_err(|_| {
        invalid(format!(
            "`{port}` is not a port (use a number from 0 to 65535)"
        ))
    })?;
    Ok((host, port))
}

/// The name did not resolve. The text is fixed: what the system's resolver says differs on
/// every system (and is translated on Windows), so it is not shown.
fn dns_error(host: &str) -> TypedError {
    Kind::Dns.error(format!(
        "cannot find the address of `{host}` (check the spelling, or use an IP address)"
    ))
}

fn no_address(host: &str) -> TypedError {
    Kind::Dns.error(format!("cannot find the address of `{host}`: it has none"))
}

/// Splits `host:port` and resolves the host, waiting for the resolver (it cannot be
/// interrupted: `connect` uses [`resolve_cancellable`]).
fn resolve(addr: &str) -> Result<Vec<SocketAddr>, TypedError> {
    let (host, port) = split_addr(addr)?;
    let found: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| dns_error(host))?
        .collect();
    if found.is_empty() {
        return Err(no_address(host));
    }
    Ok(found)
}

/// Why a connect that waits stopped before it had a result.
enum Halt {
    Cancelled,
    TimedOut,
}

/// What `connect` is allowed: its caller's token and the deadline of `connect_timeout`.
#[derive(Clone, Copy)]
struct Patience<'a> {
    token: &'a CancelToken,
    deadline: Option<Instant>,
}

impl Patience<'_> {
    fn check(&self) -> Result<(), Halt> {
        if self.token.is_cancelled() {
            return Err(Halt::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Halt::TimedOut);
        }
        Ok(())
    }

    /// How long the next wait may take.
    fn slice(&self) -> Duration {
        self.deadline.map_or(SLICE, |d| {
            d.saturating_duration_since(Instant::now()).min(SLICE)
        })
    }
}

/// Resolves the host of `addr` for `connect`: an IP address needs no resolver; a name goes to
/// a helper thread (the system's resolver cannot be interrupted), and this waits for it in
/// slices, so a cancel or the deadline ends the wait at once. The helper thread ends when the
/// resolver does; it owns nothing the program needs.
fn resolve_cancellable(addr: &str, patience: Patience<'_>) -> Result<Vec<SocketAddr>, Resolved> {
    let (host, port) = split_addr(addr).map_err(Resolved::Failed)?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let (tx, rx) = mpsc::channel();
    let name = host.to_string();
    std::thread::Builder::new()
        .name("colibri-net-resolve".into())
        .spawn(move || {
            let found = (name.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<SocketAddr>>);
            let _ = tx.send(found);
        })
        .map_err(|e| {
            Resolved::Failed(Kind::Other.error(format!(
                "cannot connect to `{addr}`: cannot start a thread to look up `{host}`: {}",
                reason(&e)
            )))
        })?;
    loop {
        patience.check().map_err(Resolved::Halted)?;
        match rx.recv_timeout(patience.slice().max(Duration::from_millis(1))) {
            Ok(Ok(found)) if found.is_empty() => {
                return Err(Resolved::Failed(no_address(host)));
            }
            Ok(Ok(found)) => return Ok(found),
            Ok(Err(_)) => return Err(Resolved::Failed(dns_error(host))),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Resolved::Failed(no_address(host)));
            }
        }
    }
}

enum Resolved {
    Failed(TypedError),
    Halted(Halt),
}

// ---- stopping ----

/// Why a wait ends before its operation has a result.
enum Stop {
    /// The coroutine that waits was cancelled, or the `Vm` ended. A script never sees this:
    /// cancelling unwinds the coroutine and drops the worker's result.
    Cancelled,
    /// The object was closed.
    Closed,
    /// The deadline of `set_timeout` passed.
    Timeout,
}

/// The deadline an operation has, taken on the interpreter thread when the method is called.
#[derive(Clone, Copy)]
struct Limit {
    deadline: Option<Instant>,
    timeout: Duration,
}

impl Limit {
    const NONE: Limit = Limit {
        deadline: None,
        timeout: Duration::ZERO,
    };
}

/// What a worker looks at between slices.
struct Wait<'a> {
    token: Option<&'a CancelToken>,
    closed: &'a AtomicBool,
    limit: Limit,
    /// What to call the thing in messages: the peer or listening address.
    name: &'a str,
}

fn closed_error(name: &str, action: &str) -> TypedError {
    Kind::Closed.error(format!(
        "cannot {action} `{name}`: it was closed (do not use a connection or listener after `close()`)"
    ))
}

impl Wait<'_> {
    fn closed_error(&self, action: &str) -> TypedError {
        closed_error(self.name, action)
    }

    fn check_open(&self, action: &str) -> Result<(), TypedError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(self.closed_error(action))
        } else {
            Ok(())
        }
    }

    /// The first reason to stop, if there is one: cancelled, closed, or out of time.
    fn stop(&self) -> Option<Stop> {
        self.stop_unless_late()
            .or_else(|| self.late().then_some(Stop::Timeout))
    }

    /// Cancelled or closed. An operation looks at this before the deadline, and always makes
    /// one try of its system call before it gives up on the deadline: a deadline that passed
    /// while a worker was starting must not turn data that is there into a timeout.
    fn stop_unless_late(&self) -> Option<Stop> {
        if self.token.is_some_and(CancelToken::is_cancelled) {
            Some(Stop::Cancelled)
        } else if self.closed.load(Ordering::SeqCst) {
            Some(Stop::Closed)
        } else {
            None
        }
    }

    /// Whether the deadline has passed.
    fn late(&self) -> bool {
        self.limit.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// The error of a stop. `sent` is `(done, total)` for a write.
    fn stop_error(&self, stop: Stop, action: &str, sent: Option<(usize, usize)>) -> TypedError {
        let note = sent_note(sent);
        match stop {
            Stop::Cancelled => Kind::Other.error(format!(
                "cannot {action} `{}`: its caller was cancelled{note}",
                self.name
            )),
            Stop::Closed => Kind::Closed.error(format!(
                "cannot {action} `{}`: it was closed (do not use a connection or listener after `close()`){note}",
                self.name
            )),
            Stop::Timeout if note.is_empty() => Kind::Timeout.error(format!(
                "cannot {action} `{}`: nothing happened within {} seconds (see `set_timeout`; the connection is still usable)",
                self.name,
                self.limit.timeout.as_secs_f64()
            )),
            Stop::Timeout => Kind::Timeout.error(format!(
                "cannot {action} `{}`: nothing happened within {} seconds (see `set_timeout`){note}",
                self.name,
                self.limit.timeout.as_secs_f64()
            )),
        }
    }

    fn check_stop(&self, action: &str) -> Result<(), TypedError> {
        match self.stop() {
            None => Ok(()),
            Some(stop) => Err(self.stop_error(stop, action, None)),
        }
    }

    /// How long until the deadline, if there is one.
    fn time_left(&self) -> Option<Duration> {
        self.limit
            .deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// How long the next wait for readiness may take.
    fn slice(&self) -> Duration {
        match self.limit.deadline {
            None => SLICE,
            Some(d) => d
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(1), SLICE),
        }
    }
}

/// The order in which operations of one kind (reads, writes, accepts) on one object are served:
/// the order their calls were made. A ticket is taken on the interpreter thread when the method
/// is called, and the operation waits on its worker for its turn, so which pool thread happens
/// to start first decides nothing. A ticket that is dropped without having had its turn (the
/// coroutine was cancelled, the operation stopped while it waited, its job never started) is
/// skipped, so it neither runs later nor holds up the ones behind it.
struct Turns {
    state: Mutex<TurnState>,
    advanced: Condvar,
}

struct TurnState {
    next: u64,
    serving: u64,
    skipped: BTreeSet<u64>,
}

struct Ticket {
    turns: Arc<Turns>,
    id: u64,
}

impl Turns {
    fn new() -> Arc<Turns> {
        Arc::new(Turns {
            state: Mutex::new(TurnState {
                next: 0,
                serving: 0,
                skipped: BTreeSet::new(),
            }),
            advanced: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, TurnState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next ticket, behind every one issued before.
    fn ticket(self: &Arc<Self>) -> Ticket {
        let mut st = self.lock();
        let id = st.next;
        st.next += 1;
        Ticket {
            turns: Arc::clone(self),
            id,
        }
    }

    /// Wakes every operation that waits for its turn, so that it looks at its stop conditions
    /// now. Whoever cancels or closes calls this after it has set the condition: taking the
    /// lock first means a waiter either has seen the condition already or is waiting and gets
    /// the notification.
    fn wake(&self) {
        drop(self.lock());
        self.advanced.notify_all();
    }

    /// Whether an operation holds the queue or waits in it.
    fn busy(&self) -> bool {
        let st = self.lock();
        st.next != st.serving
    }

    /// A ticket that has its turn already, if nobody is ahead of it.
    fn immediate(self: &Arc<Self>) -> Option<Ticket> {
        let ready = {
            let st = self.lock();
            st.next == st.serving
        };
        ready.then(|| self.ticket())
    }
}

/// The longest a queued operation sleeps without being woken. Cancel and close wake it at once
/// and a deadline ends the sleep on time, so this is only a safety net against a wake-up that
/// was missed; it is long so that a thousand queued operations cost nothing.
const TURN_FALLBACK: Duration = if cfg!(test) {
    // So that a unit test that waits for a wake-up cannot be saved by the fallback.
    Duration::from_secs(60)
} else {
    Duration::from_secs(1)
};

impl Ticket {
    /// Waits for this ticket's turn, on the condition variable of the queue: the turn passing
    /// on, a cancel, a close and the deadline all end the sleep (no polling).
    fn wait_turn(&self, wait: &Wait<'_>, action: &str) -> Result<(), TypedError> {
        if self.turns.lock().serving == self.id {
            return Ok(());
        }
        // Before the lock is taken: a token that is cancelled already runs the hook at once.
        if let Some(token) = wait.token {
            let turns = Arc::clone(&self.turns);
            token.on_cancel(move || turns.wake());
        }
        let mut st = self.turns.lock();
        loop {
            if st.serving == self.id {
                return Ok(());
            }
            // Looked at under the lock `wake` takes, so a cancel or close cannot slip between
            // this look and the sleep.
            wait.check_stop(action)?;
            let nap = wait
                .time_left()
                .map_or(TURN_FALLBACK, |left| left.min(TURN_FALLBACK))
                .max(Duration::from_millis(1));
            st = self
                .turns
                .advanced
                .wait_timeout(st, nap)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut st = self.turns.lock();
        if st.serving == self.id {
            st.serving += 1;
            loop {
                let at = st.serving;
                if !st.skipped.remove(&at) {
                    break;
                }
                st.serving += 1;
            }
            drop(st);
            self.turns.advanced.notify_all();
        } else {
            st.skipped.insert(self.id);
        }
    }
}

/// The socket of a connection or listener, shared by reference counting: every operation takes
/// its own reference while it works, and `close` takes the one the object holds. The descriptor
/// is released when the last of them is gone, so a readiness wait never sees a descriptor that
/// is closed (or has been given to another socket), and `close` never has to wait for one.
struct Handle<T>(Mutex<Option<Arc<T>>>);

impl<T> Handle<T> {
    fn new(sock: T) -> Handle<T> {
        Handle(Mutex::new(Some(Arc::new(sock))))
    }

    /// A reference for one operation; `None` once the object is closed.
    fn get(&self) -> Option<Arc<T>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Takes the object's own reference away (what `close` does).
    fn take(&self) -> Option<Arc<T>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

// ---- the readiness wait ----

/// Milliseconds for `poll`: rounded up, so a wait of a fraction of one is not a busy loop.
fn poll_millis(timeout: Duration) -> i32 {
    i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
}

#[cfg(unix)]
mod sys {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// Waits until `sock` can be read (`write` false) or written, or `timeout` passes. `true`
    /// means the socket is ready, or in an error or hung-up state: the call that follows
    /// reports which. `false` means nothing happened (or a signal came): the caller looks at
    /// its stop conditions and asks again.
    pub(super) fn wait_ready(
        sock: &impl AsRawFd,
        write: bool,
        timeout: Duration,
    ) -> io::Result<bool> {
        let mut p = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };
        // SAFETY: `p` is one valid `pollfd` that lives through the call, and the count is 1.
        let r = unsafe { libc::poll(&mut p, 1, super::poll_millis(timeout)) };
        if r >= 0 {
            return Ok(r > 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::os::windows::io::AsRawSocket;
    use std::time::Duration;

    use windows_sys::Win32::Networking::WinSock::{
        POLLRDNORM, POLLWRNORM, SOCKET_ERROR, WSAGetLastError, WSAPOLLFD, WSAPoll,
    };

    /// Waits until `sock` can be read (`write` false) or written, or `timeout` passes. `true`
    /// means the socket is ready, or in an error or hung-up state (`POLLERR` and `POLLHUP`
    /// come in `revents` whatever is asked for, and that is how a failed non-blocking connect
    /// shows): the call that follows reports which. `false` means nothing happened.
    pub(super) fn wait_ready(
        sock: &impl AsRawSocket,
        write: bool,
        timeout: Duration,
    ) -> io::Result<bool> {
        let mut p = WSAPOLLFD {
            fd: sock.as_raw_socket() as usize,
            events: if write { POLLWRNORM } else { POLLRDNORM },
            revents: 0,
        };
        // SAFETY: `p` is one valid `WSAPOLLFD` that lives through the call, and the count is
        // 1. Winsock is initialised: `sock` is a socket, and making one starts it.
        let r = unsafe { WSAPoll(&mut p, 1, super::poll_millis(timeout)) };
        if r == SOCKET_ERROR {
            // SAFETY: a plain call that reads the thread's last Winsock error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(r > 0)
    }
}

// ---- sockets ----

/// Makes a socket from `listen` or `accept` ready for use: non-blocking (a socket accepted
/// from a non-blocking listener inherits that on some systems and not on others, so it is set
/// explicitly), small writes sent at once, and no SIGPIPE on Apple systems (Linux sends with
/// `MSG_NOSIGNAL` inside `std`).
fn prepare(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    // Small writes (a line, a header) go out at once instead of waiting for the ack of the one
    // before. A peer that has reset the connection already makes this fail on some systems,
    // and the first read or write reports the reset, which is what the script should see.
    let _ = stream.set_nodelay(true);
    #[cfg(target_vendor = "apple")]
    let _ = SockRef::from(stream).set_nosigpipe(true);
    Ok(())
}

/// Windows only: `SO_EXCLUSIVEADDRUSE`, so that no other socket can bind a port this one has,
/// not even by naming one specific address (`127.0.0.1:P` while `0.0.0.0:P` is held) and not
/// with `SO_REUSEADDR`. `socket2` has no call for it.
#[cfg(windows)]
fn set_exclusive_addr_use(sock: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, setsockopt,
    };
    let on: i32 = 1;
    // SAFETY: `on` is a valid `i32` that lives through the call, the length is its size, and
    // the socket is open.
    let r = unsafe {
        setsockopt(
            sock.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            std::ptr::from_ref(&on).cast::<u8>(),
            size_of::<i32>() as i32,
        )
    };
    if r == SOCKET_ERROR {
        // SAFETY: a plain call that reads the thread's last Winsock error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

/// A new listening socket, non-blocking, with a deep backlog.
///
/// What it sets decides what a second `listen` on a port in use gets, and it is chosen so that
/// it is `AddrInUse` everywhere:
///
/// - Unix: `SO_REUSEADDR`, so that a restart does not wait for the old connections of the port
///   to time out (`TIME_WAIT`). On Linux that still refuses a second listener on the same
///   address, and also `127.0.0.1:P` while `0.0.0.0:P` is held. On macOS (BSD rules) it does
///   not refuse the second case: `SO_REUSEADDR` there lets a socket bind a specific address
///   when another is bound to the wildcard on the same port (Stevens, UNP section 7.5). That
///   one difference is documented and accepted, because without `SO_REUSEADDR` a server could
///   not be restarted for a minute on either system.
/// - Windows: `SO_EXCLUSIVEADDRUSE` and never `SO_REUSEADDR` (which there lets another program
///   take over the port). A second bind of any overlapping address is `AddrInUse`.
/// - `[::]` is dual-stack on every system (`IPV6_V6ONLY` off: Windows defaults to on), so that
///   it accepts IPv4 connections as well, as on Linux.
fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    sock.set_reuse_address(true)?;
    #[cfg(windows)]
    set_exclusive_addr_use(&sock)?;
    if let SocketAddr::V6(v6) = addr
        && v6.ip().is_unspecified()
    {
        sock.set_only_v6(false)?;
    }
    sock.bind(&SockAddr::from(addr))?;
    sock.listen(BACKLOG)?;
    sock.set_nonblocking(true)?;
    #[cfg(target_vendor = "apple")]
    let _ = sock.set_nosigpipe(true);
    Ok(sock.into())
}

/// A connect attempt that ended without a connection.
enum Attempt {
    Io(io::Error),
    Halt(Halt),
}

fn in_progress(e: &io::Error) -> bool {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
        return true;
    }
    match e.raw_os_error() {
        #[cfg(unix)]
        Some(code) => code == libc::EINPROGRESS,
        // WSAEINPROGRESS (a non-blocking connect that is under way is WSAEWOULDBLOCK, which
        // is `WouldBlock` above)
        #[cfg(windows)]
        Some(code) => code == 10036,
        _ => false,
    }
}

/// Connects to one address without blocking: starts the connect, waits for the socket to
/// become writable in slices (looking at the cancel token and the deadline between them), and
/// reads `SO_ERROR` to learn whether it worked.
fn connect_one(addr: SocketAddr, patience: Patience<'_>) -> Result<TcpStream, Attempt> {
    // `listen("0.0.0.0:0")` followed by `connect(listener.addr())` is natural, and must work
    // everywhere: Windows refuses to connect to an unspecified address.
    let addr = loopback_for(addr);
    // Looked at before the socket exists: a call that is cancelled or late does nothing.
    patience.check().map_err(Attempt::Halt)?;
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(Attempt::Io)?;
    sock.set_nonblocking(true).map_err(Attempt::Io)?;
    #[cfg(target_vendor = "apple")]
    let _ = sock.set_nosigpipe(true);
    let _ = sock.set_tcp_nodelay(true);
    match sock.connect(&SockAddr::from(addr)) {
        Ok(()) => {}
        Err(e) if in_progress(&e) => loop {
            patience.check().map_err(Attempt::Halt)?;
            if wait_connected(&sock, patience.slice()).map_err(Attempt::Io)? {
                break;
            }
        },
        Err(e) => return Err(Attempt::Io(e)),
    }
    if let Some(e) = sock.take_error().map_err(Attempt::Io)? {
        return Err(Attempt::Io(e));
    }
    Ok(sock.into())
}

/// One slice of waiting for a connect: `true` when it has ended (well or badly; the caller
/// reads `SO_ERROR`). A failure that the readiness wait did not report is looked for as well,
/// because old Windows versions never report a failed connect through `WSAPoll`.
fn wait_connected(sock: &Socket, slice: Duration) -> io::Result<bool> {
    if sys::wait_ready(sock, true, slice.max(Duration::from_millis(1)))? {
        return Ok(true);
    }
    match sock.take_error()? {
        Some(e) => Err(e),
        None => Ok(false),
    }
}

// ---- connections ----

/// What was read from the socket and not yet handed to the script.
struct Reader {
    data: Vec<u8>,
    pos: usize,
    /// Bytes after `pos` known to hold no `\n`.
    scanned: usize,
    eof: bool,
}

impl Reader {
    fn available(&self) -> &[u8] {
        &self.data[self.pos..]
    }

    /// Hands out the first `n` bytes of what is buffered. A small piece is copied; a big one
    /// is the buffer itself, so that a big `read_exact` never holds the data twice.
    fn take(&mut self, n: usize) -> Vec<u8> {
        self.scanned = 0;
        if n >= HAND_OUT {
            if self.pos > 0 {
                self.data.drain(..self.pos);
                self.pos = 0;
            }
            let rest = self.data.split_off(n);
            return std::mem::replace(&mut self.data, rest);
        }
        let out = self.data[self.pos..self.pos + n].to_vec();
        self.pos += n;
        if self.pos == self.data.len() {
            self.data.clear();
            self.pos = 0;
        } else if self.pos >= HAND_OUT && self.pos * 2 >= self.data.len() {
            self.data.drain(..self.pos);
            self.pos = 0;
        }
        out
    }

    /// Makes room for the next read. `want` is how many more bytes a `read_exact` waits for
    /// (0 when unknown): the room is then exactly that, once, so the buffer does not double.
    fn make_room(
        &mut self,
        want: usize,
        chunk: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        if self.pos == self.data.len() {
            self.data.clear();
            self.pos = 0;
        }
        if want > 0 {
            if self.pos > 0 {
                self.data.drain(..self.pos);
                self.pos = 0;
            }
            self.data.try_reserve_exact(want)
        } else {
            self.data.try_reserve(chunk)
        }
    }
}

/// Whether a read has what it needs.
enum Probe {
    /// Not yet; it needs this many more bytes at least (0 when that is unknown).
    Pending(usize),
    Ready,
    Failed(TypedError),
}

struct Shared {
    sock: Handle<TcpStream>,
    closed: AtomicBool,
    /// A write stopped with some of its data sent and some not: the stream is out of step, and
    /// every later write fails (`BrokenPipe`). Reads are not affected.
    write_broken: AtomicBool,
    /// Nanoseconds; 0 means no timeout.
    timeout_ns: AtomicU64,
    reader: Mutex<Reader>,
    reads: Arc<Turns>,
    writes: Arc<Turns>,
    local: String,
    peer: String,
}

/// A connection, from `net.connect` or `listener.accept()`. When the last reference to it goes
/// away the socket is closed.
pub(crate) struct Conn {
    sh: Arc<Shared>,
}

impl Conn {
    /// Wraps a socket that `prepare` or `connect_one` has made ready.
    fn new(stream: TcpStream) -> Conn {
        // The peer may be gone already; the address is then only for messages.
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "unknown".to_string(), |a| a.to_string());
        let local = stream
            .local_addr()
            .map_or_else(|_| "unknown".to_string(), |a| a.to_string());
        Conn {
            sh: Arc::new(Shared {
                sock: Handle::new(stream),
                closed: AtomicBool::new(false),
                write_broken: AtomicBool::new(false),
                timeout_ns: AtomicU64::new(0),
                reader: Mutex::new(Reader {
                    data: Vec::new(),
                    pos: 0,
                    scanned: 0,
                    eof: false,
                }),
                reads: Turns::new(),
                writes: Turns::new(),
                local,
                peer,
            }),
        }
    }
}

impl Shared {
    /// The deadline a call made now has. Taken on the interpreter thread, when the method is
    /// called, so waiting for a worker or for earlier calls counts against it.
    fn limit(&self) -> Limit {
        let ns = self.timeout_ns.load(Ordering::SeqCst);
        let timeout = Duration::from_nanos(ns);
        Limit {
            deadline: (ns > 0)
                .then(|| Instant::now().checked_add(timeout))
                .flatten(),
            timeout,
        }
    }

    fn wait<'a>(&'a self, token: Option<&'a CancelToken>, limit: Limit) -> Wait<'a> {
        Wait {
            token,
            closed: &self.closed,
            limit,
            name: &self.peer,
        }
    }

    /// One step of reading more into the buffer: waits for the socket to be readable for up to
    /// one slice and, when it is, reads what is there. The buffer is not touched (not grown, not
    /// zero-filled) while nothing can be read, so an idle read costs one `poll` per slice.
    /// `want` is as for [`Reader::make_room`].
    fn fill(
        &self,
        r: &mut Reader,
        wait: &Wait<'_>,
        action: &str,
        want: usize,
    ) -> Result<(), TypedError> {
        let Some(sock) = self.sock.get() else {
            return Err(wait.closed_error(action));
        };
        match sys::wait_ready(&*sock, false, wait.slice()) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                wait.check_open(action)?;
                return Err(fail(action, &self.peer, &e));
            }
        }
        let chunk = if want == 0 {
            32 * 1024
        } else {
            want.min(1 << 20)
        };
        r.make_room(want, chunk).map_err(|_| {
            Kind::Other.error(format!(
                "cannot {action} `{}`: not enough memory for {} more bytes",
                self.peer,
                want.max(chunk)
            ))
        })?;
        let start = r.data.len();
        r.data.resize(start + chunk, 0);
        match (&*sock).read(&mut r.data[start..]) {
            Ok(n) => {
                r.data.truncate(start + n);
                r.eof |= n == 0;
            }
            Err(e) => {
                r.data.truncate(start);
                // Not readable after all (the readiness was an error that went away, or
                // another wake-up): the next step waits again.
                if !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
                    wait.check_open(action)?;
                    return Err(fail(action, &self.peer, &e));
                }
            }
        }
        Ok(())
    }

    /// Waits, on a worker, until `probe` says the buffer has what the read needs, reading
    /// from the socket in between. It takes nothing out of the buffer: that is done by the
    /// interpreter thread, once the coroutine has resumed (see the module comment).
    fn read_wait(
        &self,
        ticket: &Ticket,
        wait: &Wait<'_>,
        action: &str,
        probe: impl Fn(&mut Reader) -> Probe,
    ) -> Result<(), TypedError> {
        wait.check_open(action)?;
        ticket.wait_turn(wait, action)?;
        // Only the holder of the turn gets here, so this never waits.
        let mut r = self.reader.lock().unwrap_or_else(PoisonError::into_inner);
        let mut tried = false;
        loop {
            // Cancelled first: bytes that arrive after a cancel stay in the buffer for the
            // next read instead of going to a coroutine that is gone.
            if let Some(stop) = wait.stop_unless_late() {
                return Err(wait.stop_error(stop, action, None));
            }
            let want = match probe(&mut r) {
                Probe::Ready => return Ok(()),
                Probe::Failed(e) => return Err(e),
                Probe::Pending(want) => want,
            };
            if tried && wait.late() {
                return Err(wait.stop_error(Stop::Timeout, action, None));
            }
            self.fill(&mut r, wait, action, want)?;
            tried = true;
        }
    }

    /// Sends `data`, behind the writes called before it. A write that stops with some of its
    /// data sent and some not (cancelled, out of time, failed) leaves the stream out of step,
    /// so it marks the write side broken: later writes fail with `BrokenPipe` and say why. A
    /// write that stopped before it sent anything changes nothing.
    fn write_op(&self, ticket: &Ticket, wait: &Wait<'_>, data: &[u8]) -> Result<(), TypedError> {
        let mut done = 0;
        let result = self.send_all(ticket, wait, data, &mut done);
        if result.is_err() && done > 0 && done < data.len() {
            self.write_broken.store(true, Ordering::SeqCst);
        }
        result
    }

    fn send_all(
        &self,
        ticket: &Ticket,
        wait: &Wait<'_>,
        data: &[u8],
        done: &mut usize,
    ) -> Result<(), TypedError> {
        let action = "write to";
        let total = data.len();
        wait.check_open(action)?;
        ticket.wait_turn(wait, action)?;
        if self.write_broken.load(Ordering::SeqCst) {
            return Err(Kind::BrokenPipe.error(format!(
                "cannot write to `{}`: an earlier write stopped part way, so the stream is out of step (close the connection; reading still works)",
                self.peer
            )));
        }
        let mut tried = false;
        while *done < total {
            // Between every two sends, so a cancel or a deadline does not wait for the end of
            // a big write. The first send is tried whatever the deadline says.
            let stop = if tried {
                wait.stop()
            } else {
                wait.stop_unless_late()
            };
            if let Some(stop) = stop {
                return Err(wait.stop_error(stop, action, Some((*done, total))));
            }
            tried = true;
            let Some(sock) = self.sock.get() else {
                return Err(wait.stop_error(Stop::Closed, action, Some((*done, total))));
            };
            let end = total.min(*done + SEND_CHUNK);
            let failed = |e: &io::Error| {
                let (kind, message) = write_failure(action, &self.peer, e);
                kind.error(format!("{message}{}", sent_note(Some((*done, total)))))
            };
            match (&*sock).write(&data[*done..end]) {
                Ok(0) => {
                    return Err(Kind::BrokenPipe.error(format!(
                        "cannot write to `{}`: the connection accepts no more data{}",
                        self.peer,
                        sent_note(Some((*done, total)))
                    )));
                }
                Ok(n) => *done += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if let Err(e) = sys::wait_ready(&*sock, true, wait.slice()) {
                        return Err(failed(&e));
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    wait.check_open(action)
                        .map_err(|_| wait.stop_error(Stop::Closed, action, Some((*done, total))))?;
                    return Err(failed(&e));
                }
            }
        }
        Ok(())
    }

    /// Closes the connection: marks it closed, shuts the socket down (which ends a read or a
    /// write that waits on it, on every system we know of), wakes everything that waits for its
    /// turn, and lets go of the socket. It returns at once and never fails. The descriptor is
    /// released when the last operation that is still using it leaves; that is within one
    /// slice at the latest, and a readiness wait never sees it closed.
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(sock) = self.sock.take() {
            let _ = sock.shutdown(Shutdown::Both);
        }
        self.reads.wake();
        self.writes.wake();
    }
}

/// Runs a read of `this`: at once on the interpreter thread when the buffer has what it needs
/// and no earlier read waits; otherwise on a worker, behind the reads called before it. `probe`
/// says whether the buffer is enough, `take` takes the result out of it (`take` runs on the
/// interpreter thread, after the coroutine has resumed and while it still holds its turn).
fn on_read<R>(
    cx: &mut Ctx,
    this: &Conn,
    probe: impl Fn(&mut Reader) -> Probe + Send + 'static,
    take: impl Fn(&mut Reader) -> Result<R, TypedError>,
) -> Result<R, TypedError> {
    let sh = Arc::clone(&this.sh);
    if !sh.closed.load(Ordering::SeqCst)
        && let Some(_turn) = sh.reads.immediate()
    {
        let mut r = sh.reader.lock().unwrap_or_else(PoisonError::into_inner);
        match probe(&mut r) {
            Probe::Ready => return take(&mut r),
            Probe::Failed(e) => return Err(e),
            Probe::Pending(_) => {}
        }
    }
    // A read that cannot suspend (a `Drop` hook, a callback) blocks the interpreter thread, so a
    // coroutine that waits ahead of it can never resume and give up its turn: the two would wait
    // for each other for ever. It is refused instead.
    if !cx.can_suspend() && !sh.closed.load(Ordering::SeqCst) && sh.reads.busy() {
        return Err(Kind::Other.error(format!(
            "cannot read from `{}`: a read of this connection is already waiting in another coroutine, and it cannot finish while a `Drop` hook or a callback waits (read in the coroutine, or before that read starts)",
            sh.peer
        )));
    }
    let ticket = sh.reads.ticket();
    let limit = sh.limit();
    let worker = Arc::clone(&sh);
    let ticket = cx.run_async_cancellable(move |token| {
        let wait = worker.wait(Some(token), limit);
        worker.read_wait(&ticket, &wait, "read from", probe)?;
        Ok::<Ticket, TypedError>(ticket)
    })?;
    // The coroutine has resumed and holds the turn: nothing else touches the buffer.
    let mut r = sh.reader.lock().unwrap_or_else(PoisonError::into_inner);
    let out = if sh.closed.load(Ordering::SeqCst) {
        Err(closed_error(&sh.peer, "read from"))
    } else {
        take(&mut r)
    };
    drop(r);
    drop(ticket);
    out
}

/// Runs a write of `this` on a background thread, behind the writes called before it.
fn on_write(cx: &mut Ctx, this: &Conn, data: Bytes) -> Result<(), TypedError> {
    let sh = Arc::clone(&this.sh);
    let ticket = sh.writes.ticket();
    let limit = sh.limit();
    cx.run_async_cancellable(move |token| {
        let wait = sh.wait(Some(token), limit);
        sh.write_op(&ticket, &wait, &data)
    })
}

fn bad_count(this: &str, what: &str, n: i64) -> TypedError {
    Kind::Other.error(format!(
        "cannot {what} `{this}`: {n} is not a number of bytes (use 0 or more)"
    ))
}

fn text_of(line: Vec<u8>, peer: &str) -> Result<String, TypedError> {
    String::from_utf8(line).map_err(|e| {
        Kind::InvalidData.error(format!(
            "cannot read a line from `{peer}`: the line is not valid UTF-8 ({}); `read` gives the bytes of data that is not text",
            utf8_problem(&e.utf8_error())
        ))
    })
}

/// Where the next line is in the buffer.
enum Line {
    /// Its end is not there yet.
    More,
    /// The stream ended after the last line.
    End,
    /// The stream ended inside a last line of this many bytes, which has no end of line.
    Tail(usize),
    /// A line of `content` bytes without its end of line; `consumed` bytes with it.
    Full { content: usize, consumed: usize },
}

/// Looks for the next line in the buffer. A line may be `MAX_LINE` bytes long, not counting
/// its `\n` or `\r\n`.
fn find_line(r: &mut Reader, peer: &str) -> Result<Line, TypedError> {
    let start = r.pos;
    let from = start + r.scanned;
    if let Some(at) = r.data[from..].iter().position(|&b| b == b'\n') {
        let end = from + at;
        let mut content = end - start;
        if content > 0 && r.data[end - 1] == b'\r' {
            content -= 1;
        }
        if content > MAX_LINE {
            return Err(too_long(peer));
        }
        return Ok(Line::Full {
            content,
            consumed: end + 1 - start,
        });
    }
    let pending = r.data.len() - start;
    r.scanned = pending;
    if r.eof {
        if pending == 0 {
            return Ok(Line::End);
        }
        if pending > MAX_LINE {
            return Err(too_long(peer));
        }
        return Ok(Line::Tail(pending));
    }
    // A line of exactly `MAX_LINE` bytes may be waiting for its `\n` behind a `\r`.
    if pending > MAX_LINE + 1 || (pending == MAX_LINE + 1 && r.data.last() != Some(&b'\r')) {
        return Err(too_long(peer));
    }
    Ok(Line::More)
}

fn too_long(peer: &str) -> TypedError {
    Kind::LineTooLong.error(format!(
        "cannot read a line from `{peer}`: it is longer than {MAX_LINE} bytes without an end of line (close the connection, or read the data with `read`)"
    ))
}

fn line_probe(r: &mut Reader, peer: &str) -> Probe {
    match find_line(r, peer) {
        Ok(Line::More) => Probe::Pending(0),
        Ok(_) => Probe::Ready,
        Err(e) => Probe::Failed(e),
    }
}

fn take_line(r: &mut Reader, peer: &str) -> Result<Option<String>, TypedError> {
    match find_line(r, peer)? {
        Line::Full { content, consumed } => {
            let mut line = r.take(consumed);
            line.truncate(content);
            text_of(line, peer).map(Some)
        }
        Line::End => Ok(None),
        Line::Tail(n) => text_of(r.take(n), peer).map(Some),
        Line::More => Err(Kind::Other.error(format!(
            "cannot read a line from `{peer}`: the buffered line is gone (a bug in `net`)"
        ))),
    }
}

impl HostType for Conn {
    const NAME: &'static str = "Conn";

    fn describe(d: &mut TypeBuilder<Self>) {
        d.show(|c| format!("<Conn {} -> {}>", c.sh.local, c.sh.peer));
        // Up to `n` bytes: waits until at least one is there, and gives empty bytes at the end
        // of the stream.
        d.method("read", ["n"], |cx: &mut Ctx, this: &Conn, n: i64| {
            let Ok(n) = usize::try_from(n) else {
                return Err(bad_count(&this.sh.peer, "read from", n));
            };
            on_read(
                cx,
                this,
                move |r| {
                    if n == 0 || r.eof || !r.available().is_empty() {
                        Probe::Ready
                    } else {
                        Probe::Pending(0)
                    }
                },
                move |r| {
                    let have = r.available().len().min(n);
                    Ok(Bytes(if have == 0 { Vec::new() } else { r.take(have) }))
                },
            )
        });
        // Exactly `n` bytes, or `UnexpectedEnd` when the stream ends first.
        d.method("read_exact", ["n"], |cx: &mut Ctx, this: &Conn, n: i64| {
            let Ok(n) = usize::try_from(n) else {
                return Err(bad_count(&this.sh.peer, "read from", n));
            };
            if n > MAX_EXACT {
                return Err(Kind::Other.error(format!(
                    "cannot read from `{}`: {n} bytes at once is too many (read in pieces of at most {MAX_EXACT})",
                    this.sh.peer
                )));
            }
            let peer = this.sh.peer.clone();
            on_read(
                cx,
                this,
                move |r| {
                    let have = r.available().len();
                    if have >= n {
                        Probe::Ready
                    } else if r.eof {
                        Probe::Failed(Kind::UnexpectedEnd.error(format!(
                            "cannot read {n} bytes from `{peer}`: the stream ended after {have} (the {have} bytes stay readable with `read`)"
                        )))
                    } else {
                        Probe::Pending(n - have)
                    }
                },
                move |r| Ok(Bytes(r.take(n))),
            )
        });
        // The next line without its `\n` or `\r\n`, or `none` at the end of the stream.
        d.method("read_line", [], |cx: &mut Ctx, this: &Conn| {
            let (a, b) = (this.sh.peer.clone(), this.sh.peer.clone());
            on_read(
                cx,
                this,
                move |r| line_probe(r, &a),
                move |r| take_line(r, &b),
            )
        });
        // Writes all of `text`, or fails (a failure may leave part of it written).
        d.method(
            "write",
            ["text"],
            |cx: &mut Ctx, this: &Conn, text: String| on_write(cx, this, Bytes(text.into_bytes())),
        );
        d.method(
            "write_bytes",
            ["data"],
            |cx: &mut Ctx, this: &Conn, data: Bytes| on_write(cx, this, data),
        );
        // Closes both directions and releases the socket. Anything waiting on this
        // connection ends with `Closed`. Closing twice is fine. Never waits for anything but
        // a worker that is in the middle of one system call, and never fails.
        d.method("close", [], |this: &Conn| this.sh.close());
        // Tells the peer that nothing more comes (it reads the end of the stream) and keeps
        // reading possible.
        d.method("close_write", [], |this: &Conn| {
            let wait = this.sh.wait(None, Limit::NONE);
            wait.check_open("close the writing end of")?;
            if let Some(sock) = this.sh.sock.get() {
                match sock.shutdown(Shutdown::Write) {
                    Ok(()) => {}
                    // The peer is gone already: there is nothing left to tell it.
                    Err(e) if e.kind() == ErrorKind::NotConnected => {}
                    Err(e) => return Err(fail("close the writing end of", &this.sh.peer, &e)),
                }
            }
            Ok(())
        });
        d.method("local_addr", [], |this: &Conn| this.sh.local.clone());
        d.method("peer_addr", [], |this: &Conn| this.sh.peer.clone());
        // Seconds each later read or write may take altogether, counted from the call; `0`
        // for no limit. A read or write that runs out ends with `Timeout`; the connection
        // stays usable, except after a write that had sent only part of its data.
        d.method("set_timeout", ["seconds"], |cx: &mut Ctx, this: &Conn, seconds: f64| {
            if !seconds.is_finite() || seconds < 0.0 {
                cx.raise(format!(
                    "net.Conn.set_timeout: {seconds} is not a number of seconds (it must be finite and not negative; 0 means no timeout)"
                ));
            }
            let ns = Duration::try_from_secs_f64(seconds)
                .map_or(u64::MAX, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
            // A timeout too small to measure would turn the limit off: round up.
            let ns = if seconds > 0.0 { ns.max(1) } else { 0 };
            this.sh.timeout_ns.store(ns, Ordering::SeqCst);
        });
    }
}

// ---- listeners ----

struct ListenShared {
    sock: Handle<TcpListener>,
    closed: AtomicBool,
    accepts: Arc<Turns>,
    /// Connections accepted for a caller that was cancelled before it could take them. The
    /// next `accept` returns them first (see the module comment).
    pending: Mutex<VecDeque<TcpStream>>,
    addr: String,
}

/// A listening socket, from `net.listen`.
pub(crate) struct Listener {
    sh: Arc<ListenShared>,
}

/// A connection that has been accepted and not yet handed to a script. While the coroutine that
/// asked for it is cancelled before it takes it, dropping this puts the connection into the
/// listener's queue instead of closing it. The accept turn is released with it, after that.
struct Accepted {
    sh: Arc<ListenShared>,
    stream: Option<TcpStream>,
    _ticket: Ticket,
}

impl Accepted {
    /// The connection, for the coroutine that has resumed and takes it.
    fn into_stream(mut self) -> TcpStream {
        self.stream
            .take()
            .expect("an accepted connection is taken once")
    }
}

impl Drop for Accepted {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            // Under the queue's lock, so a `close` either sees the connection (and drops it
            // with the queue) or this sees the listener closed.
            let mut queue = self
                .sh
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !self.sh.closed.load(Ordering::SeqCst) {
                queue.push_back(stream);
            }
        }
    }
}

impl ListenShared {
    fn accept(
        self: &Arc<Self>,
        ticket: Ticket,
        token: &CancelToken,
    ) -> Result<Accepted, TypedError> {
        let wait = Wait {
            token: Some(token),
            closed: &self.closed,
            limit: Limit::NONE,
            name: &self.addr,
        };
        let action = "accept on";
        ticket.wait_turn(&wait, action)?;
        let done = |stream| Accepted {
            sh: Arc::clone(self),
            stream: Some(stream),
            _ticket: ticket,
        };
        loop {
            wait.check_stop(action)?;
            let queued = self
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            if let Some(stream) = queued {
                return Ok(done(stream));
            }
            let Some(listener) = self.sock.get() else {
                return Err(wait.closed_error(action));
            };
            match listener.accept() {
                Ok((stream, _)) => {
                    // `close` wakes the wait with a connection of its own: not for a script.
                    if self.closed.load(Ordering::SeqCst) {
                        return Err(wait.closed_error(action));
                    }
                    if let Err(e) = prepare(&stream) {
                        // The connection cannot be handed over (it would be a blocking socket
                        // that stalls a pool thread) and cannot be kept for the next accept
                        // for the same reason, so it is closed here, explicitly, and this
                        // accept reports why. The peer sees the end of the stream at once;
                        // the connection is not left half open for anyone.
                        let _ = stream.shutdown(Shutdown::Both);
                        drop(stream);
                        return Err(fail(action, &self.addr, &e));
                    }
                    return Ok(done(stream));
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        if let Err(e) = sys::wait_ready(&*listener, false, SLICE) {
                            wait.check_open(action)?;
                            return Err(fail(action, &self.addr, &e));
                        }
                    }
                    // A connection that was reset before it was accepted: the next one.
                    ErrorKind::Interrupted
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset => {}
                    _ => {
                        // Closing shuts the listener down first, which can make `accept` fail.
                        wait.check_open(action)?;
                        return Err(fail(action, &self.addr, &e));
                    }
                },
            }
        }
    }

    /// Stops listening: marks the listener closed, drops the connections that were queued, wakes
    /// an `accept` that waits (it ends with `Closed`) and lets go of the socket. Never suspends,
    /// never fails. The port is free when the last accept that is using the socket has left,
    /// which the wake makes immediate (and which has happened before this returns on Linux, where
    /// the shutdown ends the listening at once).
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        if let Some(listener) = self.sock.take() {
            // A shutdown wakes whoever waits in `accept` on Linux, but not on macOS and
            // Windows (as far as their documentation says), so a connection to ourselves
            // does: the wait sees a readable listener, the accept looks at `closed` and
            // leaves, and the connection goes away with the socket. It is made before the
            // shutdown, while the port is still ours: afterwards another program could have it.
            wake_listener(&self.addr);
            let _ = SockRef::from(&*listener).shutdown(Shutdown::Read);
        }
        self.accepts.wake();
    }
}

/// The address to connect to for a listener or a peer at `addr`: an unspecified address
/// (`0.0.0.0`, `[::]`) means "this machine" and is a valid destination on Linux and macOS but
/// not on Windows, so on every system it is replaced by the loopback address of its family.
fn loopback_for(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        let ip = match addr {
            SocketAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        };
        return SocketAddr::new(ip, addr.port());
    }
    addr
}

/// Makes a listener at `addr` readable by connecting to it (on loopback when it listens on every
/// interface), without waiting for the connection to complete.
fn wake_listener(addr: &str) {
    let Ok(addr) = addr.parse::<SocketAddr>().map(loopback_for) else {
        return;
    };
    let Ok(sock) = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP)) else {
        return;
    };
    if sock.set_nonblocking(true).is_ok() {
        // Started and dropped: on loopback the system finishes the handshake by itself.
        let _ = sock.connect(&SockAddr::from(addr));
    }
}

impl HostType for Listener {
    const NAME: &'static str = "Listener";

    fn describe(d: &mut TypeBuilder<Self>) {
        d.show(|l| format!("<Listener {}>", l.sh.addr));
        // The address it listens on, with the port the system picked for port 0.
        d.method("addr", [], |this: &Listener| this.sh.addr.clone());
        // Waits for the next connection. Closing the listener ends the wait with `Closed`. A
        // connection that arrives for a coroutine that is cancelled meanwhile is kept for
        // the next `accept`.
        d.method("accept", [], |cx: &mut Ctx, this: &Listener| {
            let sh = Arc::clone(&this.sh);
            // As for a read: an accept that blocks the interpreter thread cannot wait for a
            // coroutine that holds its turn until it resumes.
            if !cx.can_suspend() && !sh.closed.load(Ordering::SeqCst) && sh.accepts.busy() {
                return Err(Kind::Other.error(format!(
                    "cannot accept on `{}`: an accept on this listener is already waiting in another coroutine, and it cannot finish while a `Drop` hook or a callback waits (accept in the coroutine, or before that accept starts)",
                    sh.addr
                )));
            }
            let ticket = sh.accepts.ticket();
            let addr = sh.addr.clone();
            let accepted = cx
                .try_run_async_cancellable(move |token| sh.accept(ticket, token))
                .map_err(|why| no_thread("accept on", &addr, &why))??;
            Ok::<Conn, TypedError>(Conn::new(accepted.into_stream()))
        });
        // Stops listening: the port is free again, and an `accept` that waits ends with
        // `Closed`. Connections already accepted are not touched; connections that were
        // accepted but not handed out are closed. Closing twice is fine.
        d.method("close", [], |this: &Listener| this.sh.close());
    }
}

// ---- functions ----

fn listen(cx: &mut Ctx, addr: String) -> Result<Listener, TypedError> {
    cx.run_async(move || {
        let mut addrs = resolve(&addr)?;
        // IPv4 first: `localhost` gives both, and IPv4 is what a client that asks for
        // `localhost` tries as well.
        addrs.sort_by_key(SocketAddr::is_ipv6);
        let mut first = None;
        for a in &addrs {
            let bound = bind(*a).and_then(|l| {
                let shown = l.local_addr()?.to_string();
                Ok((l, shown))
            });
            match bound {
                Ok((l, shown)) => {
                    return Ok(Listener {
                        sh: Arc::new(ListenShared {
                            sock: Handle::new(l),
                            closed: AtomicBool::new(false),
                            accepts: Turns::new(),
                            pending: Mutex::new(VecDeque::new()),
                            addr: shown,
                        }),
                    });
                }
                Err(e) => first = first.or(Some(e)),
            }
        }
        let e = first.expect("an address list is never empty");
        Err(fail("listen on", &addr, &e))
    })
}

/// Opens a connection to `addr`, trying every address it resolves to. `timeout` limits the
/// whole call (the lookup and every attempt); without one the system's own limit applies.
fn connect(cx: &mut Ctx, addr: String, timeout: Option<Duration>) -> Result<Conn, TypedError> {
    // Counted from the call, on this thread, not from when a worker starts.
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let stream = cx.run_async_cancellable(move |token| {
        let patience = Patience { token, deadline };
        let halted = |halt: Halt| match halt {
            Halt::Cancelled => Kind::Other.error(format!(
                "cannot connect to `{addr}`: its caller was cancelled"
            )),
            Halt::TimedOut => Kind::Timeout.error(format!(
                "cannot connect to `{addr}`: no connection within {} seconds (see `net.connect_timeout`)",
                timeout.map_or(0.0, |t| t.as_secs_f64())
            )),
        };
        let addrs = resolve_cancellable(&addr, patience).map_err(|e| match e {
            Resolved::Failed(e) => e,
            Resolved::Halted(h) => halted(h),
        })?;
        let mut first = None;
        for a in addrs {
            patience.check().map_err(&halted)?;
            match connect_one(a, patience) {
                Ok(s) => return Ok(s),
                Err(Attempt::Halt(h)) => return Err(halted(h)),
                Err(Attempt::Io(e)) => first = first.or(Some(e)),
            }
        }
        let e = first.expect("an address list is never empty");
        Err(fail("connect to", &addr, &e))
    })?;
    Ok(Conn::new(stream))
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("net");
    m.register_type::<Conn>()?;
    m.register_type::<Listener>()?;
    // Starts listening on `addr` (`"127.0.0.1:8080"`; port 0 picks a free port).
    needs(
        Cap::Net,
        m.register_fn_named("listen", ["addr"], |cx: &mut Ctx, addr: String| {
            listen(cx, addr)
        }),
    )?;
    // Opens a connection to `addr`, trying every address the name resolves to. It has no
    // timeout of its own (the system's applies) and can be cancelled.
    needs(
        Cap::Net,
        m.register_fn_named("connect", ["addr"], |cx: &mut Ctx, addr: String| {
            connect(cx, addr, None)
        }),
    )?;
    // `connect` that gives up after `seconds`, counted from the call.
    needs(
        Cap::Net,
        m.register_fn_named(
            "connect_timeout",
            ["addr", "seconds"],
            |cx: &mut Ctx, addr: String, seconds: f64| {
                if !seconds.is_finite() || seconds <= 0.0 {
                    cx.raise(format!(
                        "net.connect_timeout: {seconds} is not a number of seconds (it must be finite and more than 0; use `net.connect` for no timeout of its own)"
                    ));
                }
                let timeout = Duration::try_from_secs_f64(seconds)
                    .unwrap_or(Duration::from_secs(u64::from(u32::MAX)));
                connect(cx, addr, Some(timeout))
            },
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn io_kinds_map_to_net_kinds() {
        let kind = |k| kind_of(&Error::from(k));
        assert_eq!(kind(ErrorKind::ConnectionRefused), Kind::Refused);
        assert_eq!(kind(ErrorKind::TimedOut), Kind::Timeout);
        assert_eq!(kind(ErrorKind::WouldBlock), Kind::Timeout);
        assert_eq!(kind(ErrorKind::ConnectionReset), Kind::Reset);
        assert_eq!(kind(ErrorKind::ConnectionAborted), Kind::Reset);
        assert_eq!(kind(ErrorKind::BrokenPipe), Kind::BrokenPipe);
        assert_eq!(kind(ErrorKind::AddrInUse), Kind::AddrInUse);
        assert_eq!(kind(ErrorKind::AddrNotAvailable), Kind::AddrNotAvailable);
        assert_eq!(kind(ErrorKind::PermissionDenied), Kind::PermissionDenied);
        assert_eq!(kind(ErrorKind::HostUnreachable), Kind::Unreachable);
        assert_eq!(kind(ErrorKind::Other), Kind::Other);
    }

    /// The Windows codes are tested on every system: the function is plain arithmetic.
    #[test]
    fn windows_socket_error_codes_map_to_net_kinds() {
        let kind = |code| kind_of_windows_code(code);
        assert_eq!(kind(10058), Some(Kind::BrokenPipe)); // WSAESHUTDOWN
        assert_eq!(kind(10053), Some(Kind::Reset)); // WSAECONNABORTED
        assert_eq!(kind(10054), Some(Kind::Reset)); // WSAECONNRESET
        assert_eq!(kind(10061), Some(Kind::Refused)); // WSAECONNREFUSED
        assert_eq!(kind(10060), Some(Kind::Timeout)); // WSAETIMEDOUT
        assert_eq!(kind(10048), Some(Kind::AddrInUse)); // WSAEADDRINUSE
        assert_eq!(kind(10049), Some(Kind::AddrNotAvailable)); // WSAEADDRNOTAVAIL
        assert_eq!(kind(10051), Some(Kind::Unreachable)); // WSAENETUNREACH
        assert_eq!(kind(10065), Some(Kind::Unreachable)); // WSAEHOSTUNREACH
        assert_eq!(kind(10013), Some(Kind::PermissionDenied)); // WSAEACCES
        assert_eq!(kind(10035), None); // WSAEWOULDBLOCK is `WouldBlock` for `std`
    }

    #[cfg(unix)]
    #[test]
    fn unix_errno_values_map_to_net_kinds() {
        let kind = |code| kind_of(&Error::from_raw_os_error(code));
        assert_eq!(kind(libc::ECONNREFUSED), Kind::Refused);
        assert_eq!(kind(libc::ECONNRESET), Kind::Reset);
        assert_eq!(kind(libc::EPIPE), Kind::BrokenPipe);
        assert_eq!(kind(libc::ESHUTDOWN), Kind::BrokenPipe);
        assert_eq!(kind(libc::EADDRINUSE), Kind::AddrInUse);
        assert_eq!(kind(libc::EADDRNOTAVAIL), Kind::AddrNotAvailable);
        assert_eq!(kind(libc::ETIMEDOUT), Kind::Timeout);
        assert_eq!(kind(libc::ENETUNREACH), Kind::Unreachable);
        assert_eq!(kind(libc::EHOSTUNREACH), Kind::Unreachable);
        assert_eq!(kind(libc::EACCES), Kind::PermissionDenied);
        // Running out of descriptors is `Other`, with a message that says so.
        let e = Error::from_raw_os_error(libc::EMFILE);
        assert_eq!(kind_of(&e), Kind::Other);
        assert!(
            fail("connect to", "x:1", &e)
                .message()
                .contains("too many open files")
        );
    }

    /// The text of a known kind is Colibri's own, whatever the system says.
    #[test]
    fn known_kinds_have_their_own_text() {
        let text = |k: ErrorKind| {
            fail("connect to", "h:1", &Error::new(k, "OS TEXT"))
                .message()
                .to_string()
        };
        assert_eq!(
            text(ErrorKind::ConnectionRefused),
            "cannot connect to `h:1`: connection refused (nothing is listening there)"
        );
        assert!(text(ErrorKind::AddrInUse).contains("address already in use"));
        assert!(text(ErrorKind::ConnectionReset).contains("connection reset by the peer"));
        assert!(text(ErrorKind::BrokenPipe).contains("broken pipe"));
        assert!(text(ErrorKind::TimedOut).contains("timed out"));
        for k in [
            ErrorKind::ConnectionRefused,
            ErrorKind::AddrInUse,
            ErrorKind::AddrNotAvailable,
            ErrorKind::ConnectionReset,
            ErrorKind::BrokenPipe,
            ErrorKind::TimedOut,
            ErrorKind::PermissionDenied,
            ErrorKind::HostUnreachable,
        ] {
            assert!(!text(k).contains("OS TEXT"), "{k:?}: {}", text(k));
        }
        // A connect's hint does not tell a script to use `0.0.0.0`; a listen's does.
        let hint = |action: &str| {
            fail(action, "h:1", &Error::from(ErrorKind::AddrNotAvailable))
                .message()
                .to_string()
        };
        assert!(hint("listen on").contains("0.0.0.0"));
        assert!(
            !hint("connect to").contains("0.0.0.0"),
            "{}",
            hint("connect to")
        );
        // Only `Other` shows the system's words.
        assert!(text(ErrorKind::Other).contains("TEXT"));
    }

    #[test]
    fn addresses_are_checked_before_anything_is_resolved() {
        let bad = |s: &str| resolve(s).unwrap_err();
        for s in [
            "",
            "localhost",
            "127.0.0.1",
            ":80",
            "::1:80",
            "[::1:80",
            "a b:80",
            "127.0.0.1:99999",
            "127.0.0.1:x",
            "127.0.0.1:-1",
            "[]:1",
        ] {
            let e = bad(s);
            assert!(
                e.message().contains("is not an address"),
                "{s}: {}",
                e.message()
            );
        }
        assert_eq!(resolve("127.0.0.1:80").unwrap()[0].port(), 80);
        assert!(resolve("[::1]:0").unwrap()[0].is_ipv6());
    }

    fn reader(bytes: &[u8], eof: bool) -> Reader {
        Reader {
            data: bytes.to_vec(),
            pos: 0,
            scanned: 0,
            eof,
        }
    }

    fn line_of(r: &mut Reader) -> Result<Option<String>, TypedError> {
        match line_probe(r, "peer") {
            Probe::Failed(e) => Err(e),
            Probe::Pending(_) => panic!("not ready"),
            Probe::Ready => take_line(r, "peer"),
        }
    }

    /// A line of exactly the limit is fine with `\n` and with `\r\n`; one byte more is too long
    /// with either, and while the end of line has not arrived yet.
    #[test]
    fn a_line_of_exactly_the_limit_is_not_too_long() {
        let line = |n: usize, end: &[u8]| {
            let mut v = vec![b'x'; n];
            v.extend_from_slice(end);
            v
        };
        for end in [&b"\n"[..], b"\r\n"] {
            let mut r = reader(&line(MAX_LINE, end), false);
            assert_eq!(line_of(&mut r).unwrap().unwrap().len(), MAX_LINE);
            let mut r = reader(&line(MAX_LINE + 1, end), false);
            assert!(line_of(&mut r).is_err());
        }
        // The `\n` has not come: the `\r` after a full line may be its end of line.
        let mut r = reader(&line(MAX_LINE, b"\r"), false);
        assert!(matches!(line_probe(&mut r, "p"), Probe::Pending(_)));
        let mut r = reader(&line(MAX_LINE, b"y"), false);
        assert!(matches!(line_probe(&mut r, "p"), Probe::Failed(_)));
        let mut r = reader(&line(MAX_LINE + 1, b""), false);
        assert!(matches!(line_probe(&mut r, "p"), Probe::Failed(_)));
        // A last line without an end of line.
        let mut r = reader(&line(MAX_LINE, b""), true);
        assert_eq!(line_of(&mut r).unwrap().unwrap().len(), MAX_LINE);
        let mut r = reader(&line(MAX_LINE + 1, b""), true);
        assert!(line_of(&mut r).is_err());
    }

    fn patience(token: &CancelToken, deadline: Option<Instant>) -> Patience<'_> {
        Patience { token, deadline }
    }

    /// A connect that is cancelled before it starts, or whose deadline has passed, ends at once
    /// with the right reason and does not touch the network at all (the address is a loopback
    /// one with a listener that must see nothing). Runs on every system: this is what covers
    /// cancelling and bounding a connect where a connect cannot be kept pending on loopback.
    #[test]
    fn a_connect_that_is_cancelled_or_late_stops_before_it_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();

        let cancelled = CancelToken::cancelled();
        let started = Instant::now();
        let r = connect_one(addr, patience(&cancelled, None));
        assert!(matches!(r, Err(Attempt::Halt(Halt::Cancelled))));
        // Cancelled wins over a deadline that has passed as well.
        let past = Instant::now();
        let r = connect_one(addr, patience(&cancelled, Some(past)));
        assert!(matches!(r, Err(Attempt::Halt(Halt::Cancelled))));

        let live = test_token();
        let r = connect_one(addr, patience(&live, Some(past)));
        assert!(matches!(r, Err(Attempt::Halt(Halt::TimedOut))));
        assert!(started.elapsed() < Duration::from_secs(5));

        // Nobody connected.
        let e = listener.accept().expect_err("no connection was made");
        assert_eq!(e.kind(), ErrorKind::WouldBlock);

        // And with time left, the same call does connect.
        let r = connect_one(
            addr,
            patience(&live, Some(Instant::now() + Duration::from_secs(60))),
        );
        assert!(r.is_ok());
        assert!(listener.accept().is_ok());
    }

    /// A token that is not cancelled.
    fn test_token() -> CancelToken {
        CancelToken::default()
    }

    /// `0.0.0.0:P` and `[::]:P` are connected to as the loopback address of their family.
    #[test]
    fn an_unspecified_address_is_connected_to_as_loopback() {
        let l = |s: &str| loopback_for(s.parse().unwrap()).to_string();
        assert_eq!(l("0.0.0.0:80"), "127.0.0.1:80");
        assert_eq!(l("[::]:80"), "[::1]:80");
        assert_eq!(l("10.1.2.3:80"), "10.1.2.3:80");
        assert_eq!(l("[::1]:80"), "[::1]:80");
        assert_eq!(l("127.0.0.1:0"), "127.0.0.1:0");
    }

    /// The writes of every system say "broken pipe" for a peer that is gone; reads keep "reset".
    #[test]
    fn a_write_to_a_peer_that_is_gone_is_a_broken_pipe_everywhere() {
        for k in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected,
            ErrorKind::BrokenPipe,
        ] {
            assert_eq!(write_kind(&Error::from(k)), Kind::BrokenPipe, "{k:?}");
        }
        assert_eq!(
            kind_of(&Error::from(ErrorKind::ConnectionReset)),
            Kind::Reset
        );
        assert_eq!(write_kind(&Error::from(ErrorKind::TimedOut)), Kind::Timeout);
        let (kind, message) = write_failure(
            "write to",
            "h:1",
            &Error::new(ErrorKind::ConnectionReset, "OS"),
        );
        assert_eq!(kind, Kind::BrokenPipe);
        assert!(
            message.contains("broken pipe") && !message.contains("OS"),
            "{message}"
        );
        // The Windows codes, tested on every system: the mapping of the codes is plain data.
        for code in [10054, 10053, 10058] {
            let kind = kind_of_windows_code(code).map(|k| match k {
                Kind::Reset => Kind::BrokenPipe,
                k => k,
            });
            assert_eq!(kind, Some(Kind::BrokenPipe), "{code}");
        }
    }

    /// The variant of a `net.Error`, by name.
    fn variant(e: &TypedError) -> String {
        let text = format!("{e:?}");
        let rest = text.split("variant: \"").nth(1).expect("a variant");
        rest.split('"').next().unwrap().to_string()
    }

    /// A ticket that waits for its turn ends when the object is closed or when its token is
    /// cancelled, at once (the fallback sleep of the test build is a minute), and when its
    /// deadline passes.
    #[test]
    fn a_queued_operation_wakes_on_close_cancel_and_deadline() {
        let wait_for = |wait: Wait<'_>| {
            let turns = Turns::new();
            let _first = turns.ticket();
            let second = turns.ticket();
            let started = Instant::now();
            let kind = variant(&second.wait_turn(&wait, "read from").unwrap_err());
            assert!(started.elapsed() < Duration::from_secs(30));
            kind
        };
        // Cancelled.
        let token = CancelToken::cancelled();
        let closed = AtomicBool::new(false);
        let wait = Wait {
            token: Some(&token),
            closed: &closed,
            limit: Limit::NONE,
            name: "p",
        };
        assert_eq!(wait_for(wait), "Other");
        // Past its deadline.
        let live = test_token();
        let limit = Limit {
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            timeout: Duration::from_millis(100),
        };
        let wait = Wait {
            token: Some(&live),
            closed: &closed,
            limit,
            name: "p",
        };
        assert_eq!(wait_for(wait), "Timeout");
        // Closed from another thread while it sleeps.
        let turns = Turns::new();
        let _first = turns.ticket();
        let second = turns.ticket();
        let closed = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (turns, closed) = (Arc::clone(&turns), Arc::clone(&closed));
            std::thread::spawn(move || {
                let _ = &turns;
                let wait = Wait {
                    token: None,
                    closed: &closed,
                    limit: Limit::NONE,
                    name: "p",
                };
                variant(&second.wait_turn(&wait, "read from").unwrap_err())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        closed.store(true, Ordering::SeqCst);
        turns.wake();
        assert_eq!(waiter.join().unwrap(), "Closed");
    }

    /// What `close` of a listener does to wake an `accept`: a connection to the listener, on
    /// every system, makes its readiness wait return.
    #[test]
    fn a_connection_to_ourselves_wakes_a_listener_wait() {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let started = Instant::now();
        assert!(!sys::wait_ready(&l, false, Duration::from_millis(20)).unwrap());
        wake_listener(&addr);
        assert!(sys::wait_ready(&l, false, Duration::from_secs(20)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    /// A big take hands out the buffer itself: same allocation, nothing copied.
    #[test]
    fn a_big_take_does_not_copy() {
        let n = 3 * HAND_OUT;
        let mut r = reader(&vec![7u8; n + 10], false);
        let ptr = r.data.as_ptr();
        let out = r.take(n);
        assert_eq!(out.len(), n);
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(r.available(), &[7u8; 10]);
        // With bytes already consumed in front, they are moved out of the way, not copied
        // into a second buffer of the size taken.
        let mut r = reader(&vec![1u8; n + 20], false);
        let _ = r.take(10);
        let out = r.take(n);
        assert_eq!(out.len(), n);
        assert_eq!(r.available().len(), 10);
        // A small take is a copy, and the buffer stays.
        let mut r = reader(b"hello world", false);
        assert_eq!(r.take(5), b"hello");
        assert_eq!(r.available(), b" world");
    }
}
