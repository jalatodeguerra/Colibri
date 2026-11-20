//! `net`: TCP sockets behind the `net` capability (DESIGN §13.1).
//!
//! `net.listen(addr)` gives a `net.Listener`, `net.connect(addr)` (or `net.connect_timeout`) a
//! `net.Conn`, and `listener.accept()` another one: the two ends of a connection are the same
//! type. Every address is `"host:port"` (`"127.0.0.1:8080"`, `"[::1]:0"`, `"localhost:9000"`);
//! port `0` asks the system for a free port, and `addr()` says which one it picked.
//!
//! # How waiting works (the same on Linux, macOS and Windows)
//!
//! Every socket (listener, accepted and connected streams) is non-blocking.
//!
//! *Reads and writes of a connection* run on the interpreter thread, in the coroutine that calls
//! them. A call makes its system call at once; only when the socket would block does it wait: it
//! registers a wait cell with the `Vm`'s poller (one thread per `Vm`, started by the first wait
//! that needs it and joined when the `Vm` is dropped; it waits for the readiness of every such
//! socket with `epoll`, `kqueue` or the Windows AFD driver, and for deadlines, and sleeps for as
//! long as nothing is due) and suspends the coroutine. The poller only says that a socket is
//! ready (or that the deadline came, or that the connection was closed); the coroutine makes its
//! system call itself when it resumes and waits again if the socket would still block. So an
//! idle connection costs no thread and no wake-up, a cancelled call has taken nothing (what it
//! had read is in the connection's buffer for the next read, and what it had not read is still
//! in the kernel), and the interpreter thread never holds the buffer or a reference to the
//! socket while a coroutine is suspended. A call that does not have to wait does not suspend; so
//! that a coroutine in a loop of reads and writes against a peer that is always ready cannot
//! keep the others from running, a connection counts what its calls do without any of them
//! suspending and, when `MAX_BURST` bytes or `MAX_BURST_CALLS` calls are used up, the next call
//! gives the thread to the other coroutines once.
//!
//! *Accept and connect* (and `listen`) still run on a background thread (`Ctx::try_run_async_cancellable`)
//! which waits for readiness with `poll(2)` (Unix) or `WSAPoll` (Windows) for at most one slice
//! (`SLICE`, 50 ms) and only when the socket is ready makes its system call. Between slices it
//! looks at three things: whether its coroutine was cancelled (or the `Vm` ended), whether the
//! object was closed, and whether the deadline has passed, so a cancelled operation stops within
//! a slice and no such thread outlives its `Vm` by more than a slice. No socket is ever left in a
//! state where a timed-out call made it unusable (Windows documents that for `SO_RCVTIMEO`,
//! which is why the sockets carry no kernel timeouts).
//!
//! Timeouts (`set_timeout`, `connect_timeout`) are deadlines that start when the script calls
//! the method, on the interpreter thread.
//!
//! # Cancelling never loses data
//!
//! A read takes bytes out of the buffer only when it returns, and reads the socket only while its
//! coroutine runs, so a coroutine that is cancelled while it waits has taken nothing: the bytes
//! stay for the next read. An accept that finished for a coroutine that was cancelled meanwhile
//! puts the connection into a queue on the listener, and the next `accept` returns it first.
//!
//! # `close()` never suspends and never fails
//!
//! It sets the closed flag, takes the object's reference to the socket, shuts the socket down,
//! ends the waits that the object has armed with the poller (they resume with `Closed`, and the
//! poller's hold on the socket is gone when `close` returns), wakes everything that waits for a
//! turn, and returns. The socket is reference counted: every operation holds a reference while it
//! makes one system call and lets it go before it suspends, and the descriptor is released when
//! the last one is gone, so a wait never sees a closed or reused descriptor and `close` waits
//! for nobody. (An accept still holds one for up to one slice, and the connection that a listener
//! makes to itself to wake an accept is started and not waited for either; its socket is kept in
//! the listener's shared state, which every waiting accept holds, so it is not closed before the
//! accept has left.)
//!
//! A `Conn` is full duplex. Reads wait for one turn queue, writes (and `close_write`, which
//! must not cut a write that is still queued) for another, and `close` takes neither: two
//! operations of one kind are never interleaved, and they are served in the order they were
//! called. A read or write that waits for its turn is parked in its queue without a thread until
//! the turn comes, the object is closed, its coroutine is cancelled or its deadline passes: it
//! does not tick. (An accept still sleeps on a condition variable for its turn.)
//!
//! # Where nothing else can run
//!
//! A call made from a `Drop` hook or a callback of a host function cannot suspend: it blocks the
//! interpreter thread (`Ctx::can_suspend` is `false`), and the poller wakes it. A read (or
//! accept) holds its turn until its coroutine has taken the result, and a write is sent by the
//! coroutine that called it, so one that is called there while an earlier read, write or accept
//! of the same object is outstanding in another coroutine would wait for a coroutine that can
//! never resume. It returns a `net.Error.Other` at once instead: that is the one case the module
//! can see (`close_write` too, when it would have to wait for a write). A write that finds no
//! other write outstanding can still wait in a hook for room that only another coroutine of the
//! same program would make (the one that reads what is written), and that coroutine cannot run
//! while the hook waits. A program must not do that: from a `Drop` hook or a callback, write only
//! what the peer takes without help from this program (as `fs` has the same limit with a pipe
//! that this program reads itself).
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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};

use colibri_runtime::{
    Bytes, CancelToken, Cap, Ctx, Error, HostType, Incomplete, TypeBuilder, TypedError, Vm,
};

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
        /// `read_line` got a line that is not UTF-8 text (the line is gone from the stream, as
        /// with `fs.File`: a program that expects binary data reads it with `read`).
        InvalidData,
        /// Anything else, with the system's own text (also: out of descriptors or memory, a
        /// thread that cannot be started for an operation or for a name lookup, a read or accept
        /// refused because it could never finish). The one variant whose text is not always
        /// Colibri's own.
        Other,
    }
);

// The poller and its wait cells, which the reads and writes of a connection wait with. (Accept
// and connect still wait on threads of their own until their steps.)
mod poll;

/// How long a worker waits for readiness before it looks at its stop conditions again.
const SLICE: Duration = Duration::from_millis(50);

/// The longest line `read_line` accepts, without its end of line.
pub(crate) const MAX_LINE: usize = 1 << 20;

/// The most bytes one `read_exact` buffers.
const MAX_EXACT: usize = 1 << 30;

/// The most one `send` is given, so that a cancel or a deadline is noticed between sends.
const SEND_CHUNK: usize = 256 * 1024;

/// How many bytes of reads and writes a connection does on the interpreter thread without any
/// call on it having suspended, before the next call gives the thread to the other coroutines
/// once (see `Burst`). A read or write that never has to wait (a peer that is always ready, a
/// buffer that always has the bytes) would otherwise keep every other coroutine from running.
/// One mebibyte is a few copies at memory speed, well under a millisecond.
const MAX_BURST: usize = 1 << 20;

/// How many calls on a connection may follow one another without any of them suspending before
/// the next one gives the thread to the other coroutines once. The bytes cap does not bound a
/// loop of tiny reads and writes, and a call costs a few microseconds, so sixty-four of them are
/// still far below a millisecond.
const MAX_BURST_CALLS: usize = 64;

/// What `listen` asks the system for: connections that wait to be accepted.
const BACKLOG: i32 = 1024;

/// The least a `read_exact` buffer grows by while the bytes it waits for are still arriving.
const GROW_STEP: usize = 1 << 20;

/// The most one `read` call on the socket is given room for, and the size of the buffer that the
/// reads go through (see `Reader::stage`).
const READ_STEP: usize = 64 * 1024;

/// What a read of unknown size (a line, `read`) is given room for.
const READ_UNKNOWN: usize = 32 * 1024;

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

/// The variant of `net.Error` a Windows socket error code is when it comes from `bind` (so from
/// `listen`), where it differs from `kind_of_windows_code`. `WSAEACCES` there means that the port
/// is not free for this socket: another socket holds it with `SO_EXCLUSIVEADDRUSE` (also when
/// that one is bound to the wildcard address and this one asks for a specific address of it),
/// or the system has reserved it (an excluded port range). It is not a lack of rights on the
/// user's part, as `EACCES` of a port below 1024 is on Unix, so it is `AddrInUse`, the variant
/// every system gives a second listener on a port. Never used for a connect, where `WSAEACCES`
/// is a real refusal (a broadcast address, a firewall).
#[cfg_attr(not(windows), allow(dead_code))]
fn kind_of_windows_bind_code(code: i32) -> Option<Kind> {
    match code {
        // WSAEACCES
        10013 => Some(Kind::AddrInUse),
        _ => kind_of_windows_code(code),
    }
}

/// The variant of `net.Error` a failed `bind` is (see `kind_of_windows_bind_code`).
fn kind_of_bind(e: &io::Error) -> Kind {
    #[cfg(windows)]
    if let Some(kind) = e.raw_os_error().and_then(kind_of_windows_bind_code) {
        return kind;
    }
    kind_of(e)
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
        Kind::AddrInUse if listening => (
            "address already in use",
            " (another socket has or reserves that port: stop it, or listen on port 0 for a free one)",
        ),
        // From a connect (Windows says this when it has no free local port left): advice about
        // listening would be wrong.
        Kind::AddrInUse => (
            "address already in use",
            " (no local port is free for the connection: close the ones that are not needed)",
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
    failure_as(kind_of(e), action, what, e)
}

fn failure_as(kind: Kind, action: &str, what: &str, e: &io::Error) -> (Kind, String) {
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

/// Whether `host` cannot be a name in the DNS, whatever the resolver says: a label of more than
/// 63 bytes, or a name of more than 253 (a trailing dot, the root, is not counted). The lookup is
/// not made for such a name, so it costs no round trip to a resolver, and the answer is the
/// same on every system. Only for an ASCII host: the length of a Unicode label in UTF-8 is not
/// its length in the DNS (a label of 32 letters `ü` is 64 bytes and about 37 characters as
/// punycode), and a system that converts such names itself must be left to do it.
fn cannot_be_a_name(host: &str) -> bool {
    if !host.is_ascii() {
        return false;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    name.len() > 253 || name.split('.').any(|label| label.len() > 63)
}

fn no_address(host: &str) -> TypedError {
    Kind::Dns.error(format!("cannot find the address of `{host}`: it has none"))
}

/// Splits `host:port` and resolves the host, waiting for the resolver (it cannot be
/// interrupted: `connect` uses [`resolve_cancellable`]).
fn resolve(addr: &str) -> Result<Vec<SocketAddr>, TypedError> {
    let (host, port) = split_addr(addr)?;
    if cannot_be_a_name(host) {
        return Err(dns_error(host));
    }
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
    if cannot_be_a_name(host) {
        return Err(Resolved::Failed(dns_error(host)));
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

    /// Whether the deadline has passed.
    fn late(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

/// What a worker looks at between slices.
struct Wait<'a> {
    token: Option<&'a CancelToken>,
    closed: &'a AtomicBool,
    limit: Limit,
    /// What to call the thing in messages: the peer or listening address.
    name: &'a str,
}

/// The pool could not start a thread for an operation: `reason` is the system's.
fn no_thread(action: &str, name: &str, reason: &str) -> TypedError {
    Kind::Other.error(format!("cannot {action} `{name}`: {reason}"))
}

/// The error of a wait that needed the poller and could not get one. A poller that the system
/// would not make is an error of the system (out of descriptors gets the text that `describe`
/// gives it); a thread that cannot be started is `Other`, as for the helper threads.
/// `note` is what a write adds when some of its data is out already (see `sent_note`).
fn poller_error(action: &str, name: &str, e: &poll::StartError, note: &str) -> TypedError {
    match e {
        poll::StartError::System(e) => {
            let (kind, message) = failure(action, name, e);
            kind.error(format!("{message}{note}"))
        }
        poll::StartError::Thread(e) => no_thread(
            action,
            name,
            &format!(
                "cannot start the thread that waits for sockets: {}{note}",
                reason(e)
            ),
        ),
    }
}

/// The poller of this `Vm`, started if it is not running.
fn poller_of(
    cx: &mut Ctx,
    action: &str,
    name: &str,
    note: &str,
) -> Result<Arc<poll::Poller>, TypedError> {
    match cx.state_mut::<poll::NetState>() {
        Some(state) => state
            .poller()
            .map_err(|e| poller_error(action, name, &e, note)),
        None => Err(Kind::Other.error(format!(
            "cannot {action} `{name}`: the net module is not set up in this Vm{note}"
        ))),
    }
}

/// Suspends the calling coroutine until the wait cell that `arrange` is given is resolved, and
/// gives the reason. `arrange` registers the cell wherever it is to be resolved from (the poller,
/// a queue of turns, the object's slot); it runs before the coroutine suspends and must register
/// what it can, or resolve the cell itself. A cancel of the coroutine resolves the cell too (its
/// registrations go with it), and a cell that is registered nowhere and never resolved is
/// reported by the runtime as a wait that ended without an answer, not as a hang.
fn suspend(
    cx: &mut Ctx,
    arrange: impl FnOnce(&Arc<poll::Cell>) -> io::Result<()>,
) -> Result<poll::Why, Incomplete> {
    cx.try_await_completion::<poll::Why>(move |completer| {
        let token = Arc::clone(completer.token());
        let cell = poll::Cell::new(move |why| completer.complete(why));
        let weak = Arc::downgrade(&cell);
        // At once if the token is cancelled already; `arrange` then finds a resolved cell and
        // registers nothing.
        token.on_cancel(move || {
            if let Some(cell) = weak.upgrade() {
                cell.fire(poll::Why::Cancelled);
            }
        });
        arrange(&cell).map_err(|e| e.to_string())
    })
}

/// Gives the interpreter thread to the other coroutines once: a call that is only a scheduling
/// point (the completion is made inside `start`). Where nothing else can run it does nothing.
fn give_way(cx: &mut Ctx) {
    if cx.can_suspend() {
        let _ = cx.try_await_completion::<()>(|completer| {
            completer.complete(());
            Ok(())
        });
    }
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
                "cannot {action} `{}`: did not finish within {} seconds (see `set_timeout`; the connection is still usable)",
                self.name,
                self.limit.timeout.as_secs_f64()
            )),
            Stop::Timeout => Kind::Timeout.error(format!(
                "cannot {action} `{}`: did not finish within {} seconds (see `set_timeout`){note}",
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
    /// The wait cells of tickets that wait for their turn without sleeping on the condition
    /// variable (see `Ticket::park`), by ticket. Until the reads, writes and accepts move to the
    /// poller nothing but the tests parks.
    parked: BTreeMap<u64, Arc<poll::Cell>>,
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
                parked: BTreeMap::new(),
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
    ///
    /// The parked cells are resolved with `Why::Closed` (each looks at the flag and at its
    /// deadline to find out whether that is what happened). Every cell is resolved even if the
    /// callback of one of them panics; the first such panic is then passed on to the caller.
    fn wake(&self) {
        let parked = std::mem::take(&mut self.lock().parked);
        self.advanced.notify_all();
        let cells = parked
            .into_values()
            .map(|cell| (cell, poll::Why::Closed))
            .collect();
        if let Some(panic) = poll::fire_all(cells) {
            std::panic::resume_unwind(panic);
        }
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

impl Ticket {
    /// Whether it is this ticket's turn now.
    fn has_turn(&self) -> bool {
        self.turns.lock().serving == self.id
    }

    /// Waits for this ticket's turn without a thread: `cell` is resolved with `Why::Turn` when
    /// the turn comes (at once if it is here), or with whatever else ends the wait first (a
    /// close through `Turns::wake`, a deadline given to the poller with the same cell, a cancel).
    /// A cell that ends another way is taken out of the queue. Does nothing if the cell is
    /// resolved already.
    fn park(&self, cell: &Arc<poll::Cell>) {
        let mut st = self.turns.lock();
        if st.serving == self.id {
            drop(st);
            cell.fire(poll::Why::Turn);
            return;
        }
        let turns = Arc::downgrade(&self.turns);
        let id = self.id;
        let leave = move || {
            if let Some(turns) = turns.upgrade() {
                turns.lock().parked.remove(&id);
            }
        };
        // The cell takes only its own lock here; `fire` never holds it while it runs `leave`.
        if cell.on_resolve(leave) {
            st.parked.insert(id, Arc::clone(cell));
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
            let serving = st.serving;
            let next = st.parked.remove(&serving);
            drop(st);
            self.turns.advanced.notify_all();
            if let Some(cell) = next {
                // A callback that panics is the dropper's panic, unless the dropper is already
                // unwinding: a second panic then would abort the process.
                let fired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cell.fire(poll::Why::Turn)
                }));
                if let Err(panic) = fired
                    && !std::thread::panicking()
                {
                    std::panic::resume_unwind(panic);
                }
            }
        } else {
            st.skipped.insert(self.id);
            // A ticket that is dropped while its cell is parked never has its turn.
            st.parked.remove(&self.id);
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

// ---- the waits of an object ----

/// The wait cells that are armed with the poller for an object: at most one per direction, as
/// the queues of turns allow one read and one write at a time. `close` resolves them.
#[derive(Default)]
struct Slots {
    read: Option<Arc<poll::Cell>>,
    write: Option<Arc<poll::Cell>>,
}

impl Slots {
    fn of(&mut self, dir: poll::Dir) -> &mut Option<Arc<poll::Cell>> {
        match dir {
            poll::Dir::Read => &mut self.read,
            poll::Dir::Write => &mut self.write,
        }
    }
}

fn lock_slots(slots: &Mutex<Slots>) -> MutexGuard<'_, Slots> {
    slots.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How much a connection has done on the interpreter thread since one of its calls last
/// suspended. A call that never has to wait does not suspend, so a coroutine in a loop of reads
/// and writes against a peer that is always ready would never give the thread back: when the
/// bytes (`MAX_BURST`) or the calls (`MAX_BURST_CALLS`) are used up, the next call gives the
/// thread to the others once (`give_way`). Anything that really suspends starts the count over.
#[derive(Default)]
struct Burst {
    bytes: AtomicUsize,
    calls: AtomicUsize,
}

impl Burst {
    fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.calls.store(0, Ordering::Relaxed);
    }

    /// A call begins: `true` (and the count starts over) when the calls of the budget are used up.
    fn begin_call(&self) -> bool {
        let before = self.calls.fetch_add(1, Ordering::Relaxed);
        if before >= MAX_BURST_CALLS {
            self.reset();
            return true;
        }
        false
    }

    /// `n` bytes were read or written: `true` (and the count starts over) when that used the
    /// bytes up.
    fn spend(&self, n: usize) -> bool {
        if self.bytes.fetch_add(n, Ordering::Relaxed).saturating_add(n) >= MAX_BURST {
            self.reset();
            return true;
        }
        false
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
    /// The buffer the socket is read into before the bytes are appended to `data`. It is zeroed
    /// once, when it is made, and lives for one read call (`end_call`), so a peer that sends single bytes costs a copy of
    /// one byte per read and not a zero-fill of the room that was offered (which, done into
    /// `data` itself, is a `memset` of up to a mebibyte for every byte that arrives).
    stage: Vec<u8>,
    /// How many bytes `stage` has had zero-filled in all (for the test of the above).
    #[cfg(test)]
    zero_filled: usize,
}

impl Reader {
    fn new() -> Reader {
        Reader {
            data: Vec::new(),
            pos: 0,
            scanned: 0,
            eof: false,
            stage: Vec::new(),
            #[cfg(test)]
            zero_filled: 0,
        }
    }

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

    /// How much more room a `read_exact` that waits for `want` more bytes asks for now.
    fn step(&self, want: usize) -> usize {
        want.min(self.data.len().max(GROW_STEP))
    }

    /// Makes room for the next read of up to `chunk` bytes. `want` is how many more bytes a
    /// `read_exact` waits for (0 when unknown). The room grows with what has arrived, not with
    /// what is asked for: a peer that announces a gigabyte and sends one byte holds one step of
    /// memory, not a gigabyte. Nothing is reserved while the spare room still takes the next
    /// read; when it does not, the buffer grows by a step as big as everything received so far
    /// (at least one mebibyte) and one read more, never past `want`, so that a request ends with
    /// a capacity of exactly the bytes asked for. The buffer thus stays within about twice the
    /// bytes received plus a step, and the number of reallocations of a read of `n` bytes is
    /// about `log2(n / 1 MiB)` plus a few, whatever the size of the pieces it arrives in.
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
            if self.data.capacity() - self.data.len() < chunk {
                self.data
                    .try_reserve_exact(want.min(self.step(want).saturating_add(chunk)))?;
            }
            Ok(())
        } else {
            self.data.try_reserve(chunk)
        }
    }

    /// The end of one read call (`read`, `read_line`, `read_exact`): lets go of the buffer the
    /// socket was read into. It is made again, and zero-filled again, by the next call that has
    /// to read from the socket.
    fn end_call(&mut self) {
        self.stage = Vec::new();
    }

    /// Reads once from `src` into the buffer: room for up to one `READ_STEP` (or `READ_UNKNOWN`
    /// when `want` is 0). Gives `Err(bytes)` when memory for that many cannot be had, and
    /// otherwise the result of the read (the number of bytes appended; 0 is the end of the
    /// stream).
    fn read_from(&mut self, src: &mut impl Read, want: usize) -> Result<io::Result<usize>, usize> {
        let chunk = if want == 0 {
            READ_UNKNOWN
        } else {
            want.min(READ_STEP)
        };
        self.make_room(want, chunk)
            .map_err(|_| self.step(want).saturating_add(chunk).min(want.max(chunk)))?;
        if self.stage.len() < READ_STEP {
            #[cfg(test)]
            {
                self.zero_filled += READ_STEP - self.stage.len();
            }
            self.stage.resize(READ_STEP, 0);
        }
        Ok(match src.read(&mut self.stage[..chunk]) {
            Ok(n) => {
                // The room is there (`make_room`), so this does not reallocate.
                self.data.extend_from_slice(&self.stage[..n]);
                Ok(n)
            }
            Err(e) => Err(e),
        })
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
    /// The waits that are armed with the poller for this connection, so that `close` can end them.
    slots: Arc<Mutex<Slots>>,
    /// How much the connection has done on the interpreter thread since a call on it suspended.
    burst: Burst,
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
                reader: Mutex::new(Reader::new()),
                reads: Turns::new(),
                writes: Turns::new(),
                slots: Arc::new(Mutex::new(Slots::default())),
                burst: Burst::default(),
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

    fn lock_reader(&self) -> MutexGuard<'_, Reader> {
        self.reader.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A call on this connection begins. When the connection has done a burst without any call
    /// suspending (`MAX_BURST` bytes, `MAX_BURST_CALLS` calls), this one first gives the thread to
    /// the other coroutines once.
    fn begin_call(&self, cx: &mut Ctx) {
        if self.burst.begin_call() {
            give_way(cx);
        }
    }

    /// A write or `close_write` that cannot suspend (a `Drop` hook, a callback) blocks the
    /// interpreter thread, and a write that another coroutine has begun is sent by that coroutine
    /// when it runs again: it would wait for something that cannot happen while the thread is
    /// blocked. It is refused at once instead, as a read is.
    fn refuse_behind_a_write(&self, cx: &mut Ctx, action: &str) -> Result<(), TypedError> {
        if !cx.can_suspend() && !self.closed.load(Ordering::SeqCst) && self.writes.busy() {
            return Err(Kind::Other.error(format!(
                "cannot {action} `{}`: a write of this connection is already waiting in another coroutine, and it cannot finish while a `Drop` hook or a callback waits (write in the coroutine, or before that write starts)",
                self.peer
            )));
        }
        Ok(())
    }

    /// The error of a timeout, for a call that had sent `sent` (`(done, total)`) if it is a write.
    fn timeout_error(
        &self,
        limit: Limit,
        action: &str,
        sent: Option<(usize, usize)>,
    ) -> TypedError {
        self.wait(None, limit)
            .stop_error(Stop::Timeout, action, sent)
    }

    fn closed_error(&self, action: &str, sent: Option<(usize, usize)>) -> TypedError {
        self.wait(None, Limit::NONE)
            .stop_error(Stop::Closed, action, sent)
    }

    /// What the end of a suspension means for the call that suspended.
    fn ended(
        &self,
        end: Result<poll::Why, Incomplete>,
        action: &str,
        sent: Option<(usize, usize)>,
    ) -> Result<Ended, TypedError> {
        self.burst.reset();
        let note = sent_note(sent);
        let other = |what: &str| {
            Kind::Other.error(format!("cannot {action} `{}`: {what}{note}", self.peer))
        };
        match end {
            Ok(poll::Why::Ready | poll::Why::Turn) => Ok(Ended::Ready),
            Ok(poll::Why::TimedOut) => Ok(Ended::TimedOut),
            Ok(poll::Why::Closed) => Ok(Ended::Closed),
            Ok(poll::Why::Cancelled) => Err(other("its caller was cancelled")),
            Ok(poll::Why::Failed) => Err(other(
                "the thread that waits for sockets has stopped, so the wait cannot go on",
            )),
            Err(Incomplete::NotStarted(why)) => {
                Err(other(&format!("cannot wait for the socket: {why}")))
            }
            Err(Incomplete::Dropped(_)) => Err(other(
                "the wait for the socket ended without an answer (the thread that waits for sockets is gone)",
            )),
        }
    }

    /// Waits for the turn of `ticket` without a thread: the coroutine suspends, and the queue, a
    /// close or the deadline resumes it. Does not suspend when the turn is here.
    fn await_turn(
        &self,
        cx: &mut Ctx,
        ticket: &Ticket,
        limit: Limit,
        action: &str,
    ) -> Result<(), TypedError> {
        self.wait(None, limit).check_open(action)?;
        if ticket.has_turn() {
            return Ok(());
        }
        let poller = poller_of(cx, action, &self.peer, "")?;
        let end = suspend(cx, |cell| {
            ticket.park(cell);
            if let Some(deadline) = limit.deadline {
                poller.timer(deadline, cell)?;
            }
            // Stored, then looked at: a close that came before the cell was parked has woken
            // nobody (`Turns::wake` took an empty queue).
            if self.closed.load(Ordering::SeqCst) {
                cell.fire(poll::Why::Closed);
            }
            Ok(())
        });
        match self.ended(end, action, None)? {
            Ended::Ready => Ok(()),
            Ended::TimedOut => Err(self.timeout_error(limit, action, None)),
            Ended::Closed => Err(self.closed_error(action, None)),
        }
    }

    /// Suspends until `sock` is ready in direction `dir` (spuriously too), the deadline of `limit`
    /// comes, or the connection is closed. `sock` is a reference of this call's own and is given
    /// to the poller: nothing of it is held here while the coroutine is suspended.
    fn wait_ready(
        &self,
        cx: &mut Ctx,
        sock: Arc<TcpStream>,
        dir: poll::Dir,
        limit: Limit,
        action: &str,
        sent: Option<(usize, usize)>,
    ) -> Result<Ended, TypedError> {
        let poller = poller_of(cx, action, &self.peer, &sent_note(sent))?;
        let end = suspend(cx, move |cell| {
            // So that a close finds the wait: stored, then the flag is looked at.
            let leave = {
                let slots = Arc::downgrade(&self.slots);
                let me = Arc::downgrade(cell);
                move || {
                    if let Some(slots) = slots.upgrade() {
                        let mut slots = lock_slots(&slots);
                        let slot = slots.of(dir);
                        if slot
                            .as_ref()
                            .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), me.as_ptr()))
                        {
                            *slot = None;
                        }
                    }
                }
            };
            if !cell.on_resolve(leave) {
                return Ok(());
            }
            *lock_slots(&self.slots).of(dir) = Some(Arc::clone(cell));
            if cell.is_resolved() {
                // Resolved between the two lines above: the clean-up ran before the store.
                *lock_slots(&self.slots).of(dir) = None;
                return Ok(());
            }
            if self.closed.load(Ordering::SeqCst) {
                cell.fire(poll::Why::Closed);
                return Ok(());
            }
            poller.arm(&sock, dir, limit.deadline, cell)
        });
        self.ended(end, action, sent)
    }

    /// Waits for the turn of `ticket`, then reads on the interpreter thread until `probe` says
    /// the buffer has what the call needs, and `take`s it. The socket is read where the call runs,
    /// so a coroutine that is cancelled while it waits has taken nothing: what has arrived stays
    /// in the buffer (and in the kernel) for the next read. `reader` and a reference to the
    /// socket are never held across a suspension, and the buffer that the socket is read into is
    /// let go on every way out, an unwinding cancel included.
    fn read_call<R>(
        &self,
        cx: &mut Ctx,
        ticket: Ticket,
        limit: Limit,
        probe: impl Fn(&mut Reader) -> Probe,
        take: impl Fn(&mut Reader) -> Result<R, TypedError>,
    ) -> Result<R, TypedError> {
        let action = "read from";
        let _end = EndCall(self);
        self.await_turn(cx, &ticket, limit, action)?;
        let mut tried = false;
        // After the deadline came one more try is made (data that arrived at the last moment is
        // not a timeout), and only then does the call time out.
        let mut final_try = false;
        loop {
            // Closed first: what is buffered of a connection that was closed is not for anyone.
            if self.closed.load(Ordering::SeqCst) {
                return Err(self.closed_error(action, None));
            }
            let want = {
                let mut r = self.lock_reader();
                match probe(&mut r) {
                    Probe::Ready => return take(&mut r),
                    Probe::Failed(e) => return Err(e),
                    Probe::Pending(want) => want,
                }
            };
            if tried && !final_try && limit.late() {
                return Err(self.timeout_error(limit, action, None));
            }
            let Some(sock) = self.sock.get() else {
                return Err(self.closed_error(action, None));
            };
            let read = {
                let mut r = self.lock_reader();
                let read = r.read_from(&mut &*sock, want);
                if let Ok(Ok(0)) = read {
                    r.eof = true;
                }
                read
            };
            tried = true;
            match read {
                Err(asked) => {
                    return Err(Kind::Other.error(format!(
                        "cannot {action} `{}`: not enough memory for {asked} more bytes",
                        self.peer
                    )));
                }
                Ok(Ok(n)) => {
                    final_try = false;
                    if self.burst.spend(n) {
                        give_way(cx);
                    }
                }
                Ok(Err(e)) if e.kind() == ErrorKind::Interrupted => {}
                Ok(Err(e)) if e.kind() == ErrorKind::WouldBlock => {
                    if final_try || limit.late() {
                        return Err(self.timeout_error(limit, action, None));
                    }
                    match self.wait_ready(cx, sock, poll::Dir::Read, limit, action, None)? {
                        Ended::Ready => {}
                        Ended::TimedOut => final_try = true,
                        Ended::Closed => return Err(self.closed_error(action, None)),
                    }
                }
                Ok(Err(e)) => {
                    // A closed connection reports the shutdown as an error of the socket.
                    if self.closed.load(Ordering::SeqCst) {
                        return Err(self.closed_error(action, None));
                    }
                    return Err(fail(action, &self.peer, &e));
                }
            }
        }
    }

    /// Sends `data`, behind the writes called before it. A write that stops with some of its
    /// data sent and some not (cancelled, out of time, failed) leaves the stream out of step,
    /// so it marks the write side broken: later writes fail with `BrokenPipe` and say why. A
    /// write that stopped before it sent anything changes nothing. The mark is made by a guard,
    /// so a cancel that unwinds the call makes it too.
    fn write_call(
        &self,
        cx: &mut Ctx,
        ticket: Ticket,
        limit: Limit,
        data: &[u8],
    ) -> Result<(), TypedError> {
        let mut sent = SentGuard {
            broken: &self.write_broken,
            done: 0,
            total: data.len(),
            ok: false,
        };
        let result = self.send_all(cx, &ticket, limit, data, &mut sent.done);
        sent.ok = result.is_ok();
        result
    }

    fn send_all(
        &self,
        cx: &mut Ctx,
        ticket: &Ticket,
        limit: Limit,
        data: &[u8],
        done: &mut usize,
    ) -> Result<(), TypedError> {
        let action = "write to";
        let total = data.len();
        self.wait(None, limit).check_open(action)?;
        self.await_turn(cx, ticket, limit, action)?;
        if self.write_broken.load(Ordering::SeqCst) {
            return Err(Kind::BrokenPipe.error(format!(
                "cannot write to `{}`: an earlier write stopped part way, so the stream is out of step (close the connection; reading still works)",
                self.peer
            )));
        }
        let mut tried = false;
        let mut final_try = false;
        while *done < total {
            if self.closed.load(Ordering::SeqCst) {
                return Err(self.closed_error(action, Some((*done, total))));
            }
            // Between every two sends, so a deadline does not wait for the end of a big write.
            // The first send is tried whatever the deadline says.
            if tried && !final_try && limit.late() {
                return Err(self.timeout_error(limit, action, Some((*done, total))));
            }
            tried = true;
            let Some(sock) = self.sock.get() else {
                return Err(self.closed_error(action, Some((*done, total))));
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
                Ok(n) => {
                    *done += n;
                    final_try = false;
                    if self.burst.spend(n) {
                        give_way(cx);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if final_try || limit.late() {
                        return Err(self.timeout_error(limit, action, Some((*done, total))));
                    }
                    // Room for no byte at all, or for fewer than were wanted: the wait ends when
                    // the socket says "try again", which is not a promise of room for the rest.
                    match self.wait_ready(
                        cx,
                        sock,
                        poll::Dir::Write,
                        limit,
                        action,
                        Some((*done, total)),
                    )? {
                        Ended::Ready => {}
                        Ended::TimedOut => final_try = true,
                        Ended::Closed => {
                            return Err(self.closed_error(action, Some((*done, total))));
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    if self.closed.load(Ordering::SeqCst) {
                        return Err(self.closed_error(action, Some((*done, total))));
                    }
                    return Err(failed(&e));
                }
            }
        }
        Ok(())
    }

    /// Shuts the writing end down, whoever is ahead of it in the queue having finished.
    fn shut_write(&self) -> Result<(), TypedError> {
        let action = "close the writing end of";
        self.wait(None, Limit::NONE).check_open(action)?;
        if let Some(sock) = self.sock.get() {
            match sock.shutdown(Shutdown::Write) {
                Ok(()) => {}
                // The peer is gone already: there is nothing left to tell it.
                Err(e) if e.kind() == ErrorKind::NotConnected => {}
                Err(e) => return Err(fail(action, &self.peer, &e)),
            }
        }
        Ok(())
    }

    /// Closes the connection: marks it closed, shuts the socket down, lets go of the socket,
    /// ends the waits that are armed with the poller (their cells are resolved with `Closed`, and
    /// the poller's hold on the socket goes under its lock before this returns) and wakes everything
    /// that waits for a turn. It returns at once and never fails. The descriptor is released by the
    /// time it returns, because no operation holds a reference to the socket while it is
    /// suspended.
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(sock) = self.sock.take() {
            let _ = sock.shutdown(Shutdown::Both);
        }
        let cells = {
            let mut slots = lock_slots(&self.slots);
            [slots.read.take(), slots.write.take()]
        };
        let cells = cells
            .into_iter()
            .flatten()
            .map(|cell| (cell, poll::Why::Closed))
            .collect();
        let panic = poll::fire_all(cells);
        self.reads.wake();
        self.writes.wake();
        if let Some(panic) = panic {
            std::panic::resume_unwind(panic);
        }
    }
}

/// How a suspension of a call ended, as the call sees it.
enum Ended {
    Ready,
    TimedOut,
    Closed,
}

/// Lets go of the buffer the socket is read into, when a read call is over, however it is over
/// (the buffer is for the length of one call: a connection that is idle holds none, 1,000 of them
/// would hold 64 MB, and one call over a trickle of single bytes still zero-fills it only once).
struct EndCall<'a>(&'a Shared);

impl Drop for EndCall<'_> {
    fn drop(&mut self) {
        self.0.lock_reader().end_call();
    }
}

/// Marks the write side broken when a write ends with some of its data sent and some not,
/// whether it returned an error or was unwound by a cancel.
struct SentGuard<'a> {
    broken: &'a AtomicBool,
    done: usize,
    total: usize,
    ok: bool,
}

impl Drop for SentGuard<'_> {
    fn drop(&mut self) {
        if !self.ok && self.done > 0 && self.done < self.total {
            self.broken.store(true, Ordering::SeqCst);
        }
    }
}

/// Runs a read of `this` on the interpreter thread: at once when the buffer has what it needs
/// and no earlier read waits; otherwise behind the reads called before it, suspending whenever
/// the socket has nothing to read. `probe` says whether the buffer is enough, `take` takes the
/// result out of it.
fn on_read<R>(
    cx: &mut Ctx,
    this: &Conn,
    probe: impl Fn(&mut Reader) -> Probe,
    take: impl Fn(&mut Reader) -> Result<R, TypedError>,
) -> Result<R, TypedError> {
    let sh = Arc::clone(&this.sh);
    sh.begin_call(cx);
    if !sh.closed.load(Ordering::SeqCst)
        && let Some(_turn) = sh.reads.immediate()
    {
        let mut r = sh.lock_reader();
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
    sh.read_call(cx, ticket, limit, probe, take)
}

/// Runs a write of `this` on the interpreter thread, behind the writes called before it,
/// suspending whenever the socket takes no more.
fn on_write(cx: &mut Ctx, this: &Conn, data: Bytes) -> Result<(), TypedError> {
    let sh = Arc::clone(&this.sh);
    sh.begin_call(cx);
    sh.refuse_behind_a_write(cx, "write to")?;
    let ticket = sh.writes.ticket();
    let limit = sh.limit();
    sh.write_call(cx, ticket, limit, &data)
}

/// "1 byte", "2 bytes".
fn count_of_bytes(n: usize) -> String {
    if n == 1 {
        "1 byte".to_string()
    } else {
        format!("{n} bytes")
    }
}

fn bad_count(this: &str, what: &str, n: i64) -> TypedError {
    Kind::Other.error(format!(
        "cannot {what} `{this}`: {n} is not a number of bytes (use 0 or more)"
    ))
}

fn text_of(line: Vec<u8>, peer: &str) -> Result<String, TypedError> {
    String::from_utf8(line).map_err(|e| {
        Kind::InvalidData.error(format!(
            "cannot read a line from `{peer}`: the line is not valid UTF-8 ({}); the line has been taken off the stream (read data that is not text with `read` or `read_exact`, not `read_line`)",
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
                            "cannot read {} from `{peer}`: the stream ended after {have} ({})",
                            count_of_bytes(n),
                            match have {
                                0 => "nothing is left to read".to_string(),
                                1 => "the 1 byte stays readable with `read`".to_string(),
                                _ => format!("the {have} bytes stay readable with `read`"),
                            }
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
        // reading possible. It takes its turn behind the writes called before it, so a write
        // that is still on its way is sent whole first; with none queued it is done at once.
        d.method("close_write", [], |cx: &mut Ctx, this: &Conn| {
            let sh = Arc::clone(&this.sh);
            let action = "close the writing end of";
            sh.begin_call(cx);
            sh.wait(None, Limit::NONE).check_open(action)?;
            if let Some(_turn) = sh.writes.immediate() {
                return sh.shut_write();
            }
            sh.refuse_behind_a_write(cx, action)?;
            let ticket = sh.writes.ticket();
            let limit = sh.limit();
            sh.await_turn(cx, &ticket, limit, action)?;
            // Whether the writes before it were sent whole, cut short or failed, the writing end
            // is shut: it is the one way left to tell the peer that no more comes (a cut write
            // left a half message, which the peer should not wait on).
            sh.shut_write()
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
    /// The socket of the connection that `close` made to wake an accept. It is kept, not waited
    /// for: a socket that is closed while its handshake is still in flight withdraws the
    /// connection (macOS), so it lives as long as this state does, and an accept that waits holds
    /// a reference to the state, so it outlives the wake-up it is for.
    wake: Mutex<Option<Socket>>,
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
    fn new(sock: TcpListener, addr: String) -> Arc<ListenShared> {
        Arc::new(ListenShared {
            sock: Handle::new(sock),
            closed: AtomicBool::new(false),
            accepts: Turns::new(),
            pending: Mutex::new(VecDeque::new()),
            wake: Mutex::new(None),
            addr,
        })
    }

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
            let wake = wake_listener(&self.addr);
            *self.wake.lock().unwrap_or_else(PoisonError::into_inner) = wake;
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
/// interface), without waiting for the connection to complete. Returns the socket of the
/// connection, which the caller must keep until the accept it is for has left: a socket that is
/// closed while it still waits for the answer to its first packet takes the connection with it.
/// Linux hands loopback packets on inside the `connect` call itself, so there the handshake is
/// over when the call returns; macOS queues them for another thread, and the listener then only
/// ever sees a connection that was withdrawn (it never becomes readable). Not waiting is also
/// what lets `close` return at once when the accept queue is full and the handshake cannot end.
fn wake_listener(addr: &str) -> Option<Socket> {
    let addr = addr.parse::<SocketAddr>().map(loopback_for).ok()?;
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP)).ok()?;
    sock.set_nonblocking(true).ok()?;
    // Started, not finished: `EINPROGRESS` is the usual answer, and the system goes on.
    let _ = sock.connect(&SockAddr::from(addr));
    Some(sock)
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
    let what = addr.clone();
    cx.try_run_async_cancellable(move |_| {
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
                        sh: ListenShared::new(l, shown),
                    });
                }
                Err(e) => first = first.or(Some(e)),
            }
        }
        let e = first.expect("an address list is never empty");
        // The kind of a failed bind: a port that is not free is `AddrInUse` on every system.
        let (kind, message) = failure_as(kind_of_bind(&e), "listen on", &addr, &e);
        Err(kind.error(message))
    })
    .map_err(|why| no_thread("listen on", &what, &why))?
}

/// When the attempt at the next address must end: its share of the time left until `deadline`,
/// which is all of it for the last address. `remaining` counts the addresses not tried yet,
/// this one included.
fn attempt_deadline(now: Instant, deadline: Option<Instant>, remaining: usize) -> Option<Instant> {
    let deadline = deadline?;
    let share = u32::try_from(remaining.max(1)).unwrap_or(u32::MAX);
    Some(now + deadline.saturating_duration_since(now) / share)
}

/// Opens a connection to `addr`, trying every address it resolves to. `timeout` limits the
/// whole call (the lookup and every attempt); without one the system's own limit applies.
fn connect(cx: &mut Ctx, addr: String, timeout: Option<Duration>) -> Result<Conn, TypedError> {
    // Counted from the call, on this thread, not from when a worker starts.
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let target = addr.clone();
    let stream = cx.try_run_async_cancellable(move |token| {
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
        let count = addrs.len();
        for (i, a) in addrs.into_iter().enumerate() {
            patience.check().map_err(&halted)?;
            // Each address gets a fair share of what is left, so that one that does not answer
            // cannot use the whole budget while the next one would have connected at once.
            let attempt = Patience {
                token,
                deadline: attempt_deadline(Instant::now(), deadline, count - i),
            };
            match connect_one(a, attempt) {
                Ok(s) => return Ok(s),
                Err(Attempt::Halt(Halt::TimedOut)) if patience.check().is_ok() => {
                    first = first.or(Some(io::Error::from(ErrorKind::TimedOut)));
                }
                Err(Attempt::Halt(h)) => return Err(halted(h)),
                Err(Attempt::Io(e)) => first = first.or(Some(e)),
            }
        }
        let e = first.expect("an address list is never empty");
        Err(fail("connect to", &addr, &e))
    })
    .map_err(|why| no_thread("connect to", &target, &why))??;
    Ok(Conn::new(stream))
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    // The poller thread is made by the first wait that needs it, not here.
    vm.insert_state(poll::NetState::default());
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

    /// `WSAEACCES` from a bind is `AddrInUse`, and from anything else it stays `PermissionDenied`.
    /// Run on every system: the function takes the raw code.
    #[test]
    fn a_windows_access_error_from_bind_is_an_address_in_use() {
        assert_eq!(kind_of_windows_bind_code(10013), Some(Kind::AddrInUse));
        assert_eq!(kind_of_windows_code(10013), Some(Kind::PermissionDenied));
        // The other codes are the ones every call has.
        assert_eq!(kind_of_windows_bind_code(10048), Some(Kind::AddrInUse));
        assert_eq!(
            kind_of_windows_bind_code(10049),
            Some(Kind::AddrNotAvailable)
        );
        assert_eq!(kind_of_windows_bind_code(10061), Some(Kind::Refused));
        assert_eq!(kind_of_windows_bind_code(10035), None);
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
        // The advice to listen on port 0 is for a listen only.
        let in_use = |action: &str| {
            fail(action, "h:1", &Error::from(ErrorKind::AddrInUse))
                .message()
                .to_string()
        };
        assert!(in_use("listen on").contains("listen on port 0"));
        assert!(
            !in_use("connect to").contains("port 0"),
            "{}",
            in_use("connect to")
        );
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

    /// A name that cannot exist is a `Dns` failure without a lookup: the resolver is not asked
    /// (the check comes before `to_socket_addrs`), so this test needs no network and takes no time.
    #[test]
    fn a_name_that_cannot_exist_is_not_looked_up() {
        let label = "x".repeat(64);
        let long = format!("{}.com", "a.".repeat(130));
        for host in [format!("{label}.invalid"), format!("{label}."), long] {
            assert!(cannot_be_a_name(&host), "{host}");
            let started = Instant::now();
            let e = resolve(&format!("{host}:80")).unwrap_err();
            assert_eq!(variant(&e), "Dns", "{}", e.message());
            assert!(started.elapsed() < Duration::from_secs(5));
            let token = CancelToken::default();
            let patience = Patience {
                token: &token,
                deadline: None,
            };
            match resolve_cancellable(&format!("{host}:80"), patience) {
                Err(Resolved::Failed(e)) => assert_eq!(variant(&e), "Dns"),
                _ => panic!("{host} was not refused"),
            }
        }
        // 63 bytes in a label, 253 in a name, and a trailing dot are the limits, not beyond them.
        assert!(!cannot_be_a_name(&format!("{}.com", "x".repeat(63))));
        assert!(!cannot_be_a_name("localhost"));
        // A Unicode name is not judged by its bytes: 32 `ü` are 64 bytes but one short label.
        assert!(!cannot_be_a_name(&"ü".repeat(32)));
        assert!(!cannot_be_a_name(&format!("{}.de", "ü".repeat(100))));
        // An ASCII one still is.
        assert!(cannot_be_a_name(&"u".repeat(64)));
        assert!(!cannot_be_a_name("example.com."));
        assert!(!cannot_be_a_name("1.2.3"));
        let max = format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(61));
        assert_eq!(max.len(), 253);
        assert!(!cannot_be_a_name(&max));
        assert!(cannot_be_a_name(&format!("{max}c")));
    }

    fn reader(bytes: &[u8], eof: bool) -> Reader {
        Reader {
            data: bytes.to_vec(),
            eof,
            ..Reader::new()
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

    /// `read_line` of a line that is not text takes the line off the stream, and the line after
    /// it is still there.
    #[test]
    fn a_line_that_is_not_text_is_taken_off_the_stream() {
        let mut r = reader(b"\xff\xfe\nok\n", false);
        let e = take_line(&mut r, "peer").unwrap_err();
        assert_eq!(variant(&e), "InvalidData");
        assert!(
            e.message().contains("taken off the stream"),
            "{}",
            e.message()
        );
        assert_eq!(take_line(&mut r, "peer").unwrap().as_deref(), Some("ok"));
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
        // The connect returns when the client's side of the handshake is done. The listener
        // has the connection at once on some systems and a moment later on others, so the
        // accept (which does not block here) is asked again until it has it.
        let began = Instant::now();
        loop {
            match listener.accept() {
                Ok(_) => break,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        began.elapsed() < Duration::from_secs(30),
                        "the connection never reached the listener"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
    }

    /// Each address left to try gets its share of the time left, the last one all of it.
    #[test]
    fn each_address_gets_a_fair_share_of_the_time_left() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(12);
        let share = |remaining| attempt_deadline(now, Some(deadline), remaining).unwrap() - now;
        assert_eq!(share(1), Duration::from_secs(12));
        assert_eq!(share(2), Duration::from_secs(6));
        assert_eq!(share(3), Duration::from_secs(4));
        // After the first of three took its 4 seconds, 8 are left for two: 4 each.
        let later = now + Duration::from_secs(4);
        assert_eq!(
            attempt_deadline(later, Some(deadline), 2).unwrap() - later,
            Duration::from_secs(4)
        );
        // No deadline, no share; a deadline that has passed leaves nothing, and no count of
        // zero divides by zero.
        assert!(attempt_deadline(now, None, 3).is_none());
        let past = attempt_deadline(deadline + Duration::from_secs(1), Some(deadline), 2);
        assert_eq!(past, Some(deadline + Duration::from_secs(1)));
        assert_eq!(share(0), Duration::from_secs(12));
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

    /// The twin of `a_queued_operation_wakes_on_close_cancel_and_deadline` for a ticket that
    /// parks a cell instead of sleeping: the turn comes, a close, a cancel and a deadline each
    /// end the wait, with their own reason, at once and with no sleep in the test; and a cell
    /// that ended another way leaves the queue, so it neither fires later nor holds up the
    /// tickets behind it.
    #[test]
    fn a_parked_ticket_wakes_on_its_turn_close_cancel_and_deadline() {
        use poll::{Cell, Why};
        use std::sync::mpsc::{self, Receiver};
        let ceiling = Duration::from_secs(30);
        let cell = || -> (Arc<Cell>, Receiver<Why>) {
            let (tx, rx) = mpsc::channel();
            (
                Cell::new(move |why| {
                    let _ = tx.send(why);
                }),
                rx,
            )
        };
        let parked = |turns: &Turns| turns.lock().parked.len();

        // Its turn: parks while the ticket ahead is held, and is told when that one is dropped.
        let turns = Turns::new();
        let first = turns.ticket();
        let second = turns.ticket();
        let (c, rx) = cell();
        second.park(&c);
        assert_eq!(parked(&turns), 1);
        assert!(rx.try_recv().is_err());
        drop(first);
        assert_eq!(rx.recv_timeout(ceiling).unwrap(), Why::Turn);
        assert_eq!(parked(&turns), 0);
        // A ticket that has its turn already is told at once.
        let (c, rx) = cell();
        second.park(&c);
        assert_eq!(rx.recv_timeout(ceiling).unwrap(), Why::Turn);
        drop(second);

        // Closed: `wake` resolves every parked cell.
        let turns = Turns::new();
        let _first = turns.ticket();
        let (second, third) = (turns.ticket(), turns.ticket());
        let ((c2, rx2), (c3, rx3)) = (cell(), cell());
        second.park(&c2);
        third.park(&c3);
        turns.wake();
        assert_eq!(rx2.recv_timeout(ceiling).unwrap(), Why::Closed);
        assert_eq!(rx3.recv_timeout(ceiling).unwrap(), Why::Closed);
        assert_eq!(parked(&turns), 0);

        // Cancelled: the cell leaves the queue; the ticket (dropped by the unwinding caller) is
        // skipped, and the one behind it gets the turn when the first is dropped.
        let turns = Turns::new();
        let first = turns.ticket();
        let (second, third) = (turns.ticket(), turns.ticket());
        let ((c2, rx2), (c3, rx3)) = (cell(), cell());
        second.park(&c2);
        third.park(&c3);
        assert!(c2.fire(Why::Cancelled));
        assert_eq!(rx2.recv_timeout(ceiling).unwrap(), Why::Cancelled);
        assert_eq!(parked(&turns), 1);
        drop(second);
        drop(first);
        assert_eq!(rx3.recv_timeout(ceiling).unwrap(), Why::Turn);
        assert_eq!(parked(&turns), 0);
        assert!(rx2.try_recv().is_err(), "the cancelled cell fired again");

        // Its deadline: the poller's timer resolves the cell, which leaves the queue.
        let poller = poll::Poller::start().unwrap();
        let turns = Turns::new();
        let _first = turns.ticket();
        let second = turns.ticket();
        let (c, rx) = cell();
        let deadline = Instant::now() + Duration::from_millis(50);
        second.park(&c);
        poller.timer(deadline, &c).unwrap();
        assert_eq!(rx.recv_timeout(ceiling).unwrap(), Why::TimedOut);
        assert!(Instant::now() >= deadline);
        assert_eq!(parked(&turns), 0);
        assert_eq!(poller.counts(), (0, 0));

        // The turn arrives first: the deadline's timer is taken out of the poller.
        let turns = Turns::new();
        let first = turns.ticket();
        let second = turns.ticket();
        let (c, rx) = cell();
        second.park(&c);
        poller
            .timer(Instant::now() + Duration::from_secs(3600), &c)
            .unwrap();
        assert_eq!(poller.counts(), (0, 1));
        drop(first);
        assert_eq!(rx.recv_timeout(ceiling).unwrap(), Why::Turn);
        assert_eq!(poller.counts(), (0, 0));
        drop(second);

        // A parked ticket that is dropped never gets its turn, and does not hold up the next.
        let turns = Turns::new();
        let first = turns.ticket();
        let (second, third) = (turns.ticket(), turns.ticket());
        let ((c2, rx2), (c3, rx3)) = (cell(), cell());
        second.park(&c2);
        third.park(&c3);
        drop(second);
        drop(first);
        assert_eq!(rx3.recv_timeout(ceiling).unwrap(), Why::Turn);
        assert!(rx2.try_recv().is_err());
        // Nothing of the dropped ticket is left in the queue.
        assert_eq!(parked(&turns), 0);
        drop(c2);
    }

    /// A callback that panics does not keep the other parked cells from being resolved when a
    /// close wakes them, and does not abort the process when the ticket that hands on the turn is
    /// dropped by an unwinding thread.
    #[test]
    fn a_panicking_callback_of_a_parked_cell_is_contained() {
        use poll::{Cell, Why};
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::mpsc;
        let ceiling = Duration::from_secs(30);
        let panicking = || Cell::new(|_| panic!("the callback panics"));
        let quiet = || {
            let (tx, rx) = mpsc::channel();
            (
                Cell::new(move |why| {
                    let _ = tx.send(why);
                }),
                rx,
            )
        };

        // A close: the first cell panics, the other two are resolved all the same, and the
        // panic reaches the caller of `wake`.
        let turns = Turns::new();
        let _first = turns.ticket();
        let (second, third, fourth) = (turns.ticket(), turns.ticket(), turns.ticket());
        let (c3, rx3) = quiet();
        let (c4, rx4) = quiet();
        second.park(&panicking());
        third.park(&c3);
        fourth.park(&c4);
        assert!(catch_unwind(AssertUnwindSafe(|| turns.wake())).is_err());
        assert_eq!(rx3.recv_timeout(ceiling).unwrap(), Why::Closed);
        assert_eq!(rx4.recv_timeout(ceiling).unwrap(), Why::Closed);

        // A turn handed on to a callback that panics: a panic of the dropper when it is not
        // unwinding already...
        let turns = Turns::new();
        let first = turns.ticket();
        let second = turns.ticket();
        let bomb = panicking();
        second.park(&bomb);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(first))).is_err());
        drop(second);

        // ...and nothing but the first panic when it is (a second one would abort here).
        let turns = Turns::new();
        let first = turns.ticket();
        let second = turns.ticket();
        let bomb = panicking();
        second.park(&bomb);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _held = first;
            panic!("the thread is unwinding");
        }));
        assert!(outcome.is_err());
        drop(second);
    }

    /// A poller that cannot be made is the system's error (the text for running out of
    /// descriptors is Colibri's own); a thread that cannot be started is `Other` and says so.
    #[test]
    fn a_poller_that_cannot_be_had_is_reported_as_an_error_of_the_wait() {
        // The system's code for "too many open files" differs by system; the text must not.
        #[cfg(unix)]
        let emfile = libc::EMFILE;
        // WSAEMFILE
        #[cfg(windows)]
        let emfile = 10024;
        let e = poll::StartError::System(io::Error::from_raw_os_error(emfile));
        let text = format!("{:?}", poller_error("read from", "peer", &e, ""));
        assert!(text.contains("too many open files"), "{text}");
        assert_eq!(variant(&poller_error("read from", "peer", &e, "")), "Other");
        let e = poll::StartError::Thread(io::Error::other("no more threads"));
        let error = poller_error("read from", "peer", &e, "");
        assert_eq!(variant(&error), "Other");
        let text = format!("{error:?}");
        assert!(
            text.contains(
                "cannot read from `peer`: cannot start the thread that waits for sockets"
            ),
            "{text}"
        );
    }

    /// A connected pair: the `Conn` of the server side and the peer's stream.
    fn conn_pair() -> (Conn, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        prepare(&s).unwrap();
        (Conn::new(s), peer)
    }

    /// Runs `f` inside a host function of a `Vm` that has the net module, on the main fiber (where
    /// a wait drives the event loop), and gives what it returns: the way the unit tests get a
    /// `Ctx` to call the interpreter-thread read and write loops with.
    fn with_cx<T: Send + 'static>(f: impl FnOnce(&mut Ctx) -> T + Send + 'static) -> T {
        type Job = Box<dyn FnOnce(&mut Ctx) + Send>;
        let job: Arc<Mutex<Option<Job>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        *job.lock().unwrap() = Some(Box::new(move |cx| {
            let _ = tx.send(f(cx));
        }));
        let out = colibri_runtime::SharedBuffer::new();
        let mut vm = Vm::builder().stdout(Box::new(out)).allow(Cap::Net).build();
        register(&mut vm).unwrap();
        vm.std_module("probe")
            .register_fn_named("run", [], move |cx: &mut Ctx| {
                let job = job.lock().unwrap().take().expect("run once");
                job(cx);
            })
            .unwrap();
        vm.load_source(
            "app/main.colibri",
            "import probe\nfn main() { probe.run() }\n",
        )
        .unwrap();
        vm.run_main().unwrap();
        rx.recv().expect("the probe ran")
    }

    /// A read that stops at the first line leaves the rest in the buffer: the next read finds it
    /// there, in order, whatever the socket does meanwhile. (That a cancelled read takes nothing
    /// is `a_read_that_finished_before_its_coroutine_was_cancelled_loses_no_data` in
    /// `tests/net.rs`; the read happens where the call runs, so there is no window to test here.)
    #[test]
    fn a_read_leaves_the_bytes_it_did_not_take_for_the_next_read() {
        let (c, mut peer) = conn_pair();
        peer.write_all(b"hello\nworld\n").unwrap();
        let sh = Arc::clone(&c.sh);
        let lines = with_cx(move |cx| {
            let mut lines = Vec::new();
            for _ in 0..2 {
                let ticket = sh.reads.ticket();
                let line = sh
                    .read_call(
                        cx,
                        ticket,
                        Limit::NONE,
                        |r| line_probe(r, "p"),
                        |r| take_line(r, "p"),
                    )
                    .unwrap();
                lines.push(line);
            }
            lines
        });
        assert_eq!(
            lines,
            [Some("hello".to_string()), Some("world".to_string())]
        );
    }

    /// What `close` of a listener does to wake an `accept`: a connection to the listener, on
    /// every system, makes its readiness wait return.
    #[test]
    fn a_connection_to_ourselves_wakes_a_listener_wait() {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let started = Instant::now();
        assert!(!sys::wait_ready(&l, false, Duration::from_millis(20)).unwrap());
        // Kept open while the wait lasts, as `close` keeps it (macOS withdraws the connection
        // of a socket that is closed before its handshake is over).
        let _wake = wake_listener(&addr);
        assert!(sys::wait_ready(&l, false, Duration::from_secs(20)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    /// `close` of a listener whose accept queue is full does not wait for the connection it makes
    /// to wake an accept (that handshake cannot end: the system drops the SYN of a full queue).
    /// It waited 200 ms for it once. Five listeners are closed and the quickest is measured, so a
    /// slow moment of the machine cannot fail the test (it would have to hit all five), while the
    /// waiting version is never under 200 ms.
    #[test]
    fn closing_a_listener_with_a_full_accept_queue_does_not_wait() {
        let mut quickest = Duration::MAX;
        for _ in 0..5 {
            let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
            sock.bind(&SockAddr::from(
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ))
            .unwrap();
            sock.listen(1).unwrap();
            let addr = sock.local_addr().unwrap().as_socket().unwrap();
            let sh = ListenShared::new(sock.into(), addr.to_string());
            // Connect until one does not finish (or is refused: then nothing more can be done).
            let mut clients = Vec::new();
            while clients.len() < 400 {
                match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                    Ok(c) => clients.push(c),
                    Err(_) => break,
                }
            }
            let started = Instant::now();
            sh.close();
            quickest = quickest.min(started.elapsed());
            drop(clients);
        }
        assert!(
            quickest < Duration::from_millis(100),
            "close took {quickest:?} at the quickest"
        );
    }

    /// The listener of a test, and its address.
    fn listen_shared() -> (Arc<ListenShared>, SocketAddr) {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = l.local_addr().unwrap();
        (ListenShared::new(l, addr.to_string()), addr)
    }

    /// A connection that was accepted for a coroutine that was cancelled before it took it is in
    /// the listener's own queue, the next accept returns it first, and `close` closes the ones
    /// that are still queued: the client sees the end of the stream while the listener object
    /// is still alive (so it is `close`, and not the drop of the listener, that did it).
    #[test]
    fn an_accepted_connection_that_was_not_taken_is_kept_and_closed_with_the_listener() {
        let (sh, addr) = listen_shared();
        let live = test_token();
        let queued = |sh: &Arc<ListenShared>| sh.pending.lock().unwrap().len();

        // Kept: the accept finished (here, on this thread, so there is no race), its caller
        // never took the connection, and the next accept gets it before any new one.
        let first = TcpStream::connect(addr).unwrap();
        let accepted = sh.accept(sh.accepts.ticket(), &live).ok().unwrap();
        drop(accepted);
        assert_eq!(queued(&sh), 1);
        let second = TcpStream::connect(addr).unwrap();
        let again = sh.accept(sh.accepts.ticket(), &live).ok().unwrap();
        assert_eq!(queued(&sh), 0, "the kept one is served first");
        assert_eq!(
            again.stream.as_ref().unwrap().peer_addr().unwrap(),
            first.local_addr().unwrap()
        );
        let kept = again.into_stream();
        // The second connection is accepted, too, so that the next one is the third.
        let taken = sh
            .accept(sh.accepts.ticket(), &live)
            .ok()
            .unwrap()
            .into_stream();
        assert_eq!(taken.peer_addr().unwrap(), second.local_addr().unwrap());
        drop((kept, taken, first, second));

        // Closed with the listener: queue a connection, then close.
        let client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        drop(sh.accept(sh.accepts.ticket(), &live).ok().unwrap());
        assert_eq!(queued(&sh), 1);
        // Alive in the queue: nothing to read, and not an end of the stream.
        client.set_nonblocking(true).unwrap();
        let e = (&client).read(&mut [0u8; 1]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::WouldBlock);
        client.set_nonblocking(false).unwrap();
        sh.close();
        assert_eq!(queued(&sh), 0, "close does not clear the queue");
        // `sh` is alive still: the end of the stream comes from `close`.
        assert_eq!((&client).read(&mut [0u8; 1]).unwrap(), 0);
        drop(sh);
    }

    /// A connection of a closed listener is not queued either: a caller that was cancelled
    /// after `close` leaves nothing behind.
    #[test]
    fn a_connection_accepted_for_a_gone_caller_is_not_kept_by_a_closed_listener() {
        let (sh, addr) = listen_shared();
        let live = test_token();
        let client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let accepted = sh.accept(sh.accepts.ticket(), &live).ok().unwrap();
        sh.close();
        drop(accepted);
        assert_eq!(sh.pending.lock().unwrap().len(), 0);
        assert_eq!((&client).read(&mut [0u8; 1]).unwrap(), 0);
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

    /// `read_exact` of a gigabyte asks the system for one step of memory when one byte has
    /// arrived, and the buffer then grows with what comes in (at most twice what has arrived
    /// plus a step), never to the size that was asked for.
    #[test]
    fn read_exact_reserves_with_what_has_arrived() {
        let mut r = reader(b"x", false);
        r.make_room(MAX_EXACT - 1, GROW_STEP).unwrap();
        assert!(
            r.data.capacity() <= 2 * GROW_STEP + 1,
            "{}",
            r.data.capacity()
        );
        assert!(r.data.capacity() >= GROW_STEP);
        // Grows by what has arrived, a doubling each time, so that a big read stays linear.
        let mut r = reader(&vec![0u8; 8 * GROW_STEP], false);
        r.make_room(MAX_EXACT, GROW_STEP).unwrap();
        let cap = r.data.capacity();
        assert!((16 * GROW_STEP..=17 * GROW_STEP).contains(&cap), "{cap}");
        // A small request is exact: nothing is reserved beyond it.
        let mut r = reader(b"x", false);
        r.make_room(100, 100).unwrap();
        assert!(r.data.capacity() >= 101 && r.data.capacity() < GROW_STEP);
    }

    /// A peer that sends `piece` bytes at a time (or as many as the room takes, if less).
    struct Pieces(usize);

    impl Read for Pieces {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.min(buf.len());
            buf[..n].fill(0xAB);
            Ok(n)
        }
    }

    /// The buffer of a `read_exact` reallocates a logarithmic number of times whatever the size
    /// of the pieces the bytes come in, and ends with a capacity of exactly the bytes asked for.
    /// (It was one reallocation per fill until half of the bytes had arrived.)
    #[test]
    fn read_exact_reallocates_a_logarithmic_number_of_times() {
        let mib = GROW_STEP;
        let big = 64 * mib + 5;
        for n in [mib - 1, mib, mib + 1, 3 * mib + 7, big] {
            for piece in [1, 1_500, 16 * 1024, 64 * 1024, 300_000] {
                // A byte at a time is for the sizes a test can count to.
                if n / piece > 4_000_000 {
                    continue;
                }
                let mut r = reader(b"", false);
                let mut src = Pieces(piece);
                let mut changes = 0;
                while r.available().len() < n {
                    let want = n - r.available().len();
                    let before = r.data.capacity();
                    r.read_from(&mut src, want).unwrap().unwrap();
                    changes += usize::from(r.data.capacity() != before);
                    assert!(r.data.capacity() <= n, "n {n}, piece {piece}");
                }
                let bound = 4 + (n / mib + 1).ilog2() as usize;
                assert!(
                    changes <= bound,
                    "n {n}, piece {piece}: {changes} capacity changes, bound {bound}"
                );
                assert_eq!(r.data.len(), n, "n {n}, piece {piece}");
                assert_eq!(r.data.capacity(), n, "n {n}, piece {piece}");
                assert!(r.available().iter().all(|&b| b == 0xAB));
            }
        }
    }

    /// A peer that sends single bytes costs a copy of one byte per read: the buffer the socket is
    /// read into is zero-filled once, not the room offered for each read (which was up to a
    /// mebibyte of `memset` for every byte that arrived).
    #[test]
    fn a_trickle_of_single_bytes_zero_fills_nothing_after_the_first_read() {
        for want in [4 * GROW_STEP, 0] {
            let mut r = reader(b"", false);
            let mut src = Pieces(1);
            for i in 1..=5000 {
                let more = if want == 0 { 0 } else { want - (i - 1) };
                r.read_from(&mut src, more).unwrap().unwrap();
                assert_eq!(r.data.len(), i);
            }
            // Over one call (the lifetime of the buffer) it is zero-filled once, however many
            // reads the call makes.
            assert_eq!(r.zero_filled, READ_STEP, "want {want}");
            // Nor does a read hand the system more room than one step.
            assert_eq!(r.stage.len(), READ_STEP);
            // The next call makes it again: the cost is one step for each call, not for each read.
            r.end_call();
            for _ in 0..100 {
                r.read_from(&mut src, 0).unwrap().unwrap();
            }
            assert_eq!(r.zero_filled, 2 * READ_STEP, "want {want}");
        }
    }

    /// A connection that has read holds no buffer for reading while it is idle: after a read has
    /// returned (here a line) the `Reader` has none, and a second read makes it again and lets
    /// it go again; a call that fails or is unwound lets go too.
    #[test]
    fn a_reader_holds_no_stage_buffer_after_a_read_has_returned() {
        let (c, mut peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        for expected in ["one", "two"] {
            peer.write_all(format!("{expected}\n").as_bytes()).unwrap();
            let shared = Arc::clone(&sh);
            let line = with_cx(move |cx| {
                let ticket = shared.reads.ticket();
                shared
                    .read_call(
                        cx,
                        ticket,
                        Limit::NONE,
                        |r| line_probe(r, "p"),
                        |r| take_line(r, "p"),
                    )
                    .unwrap()
            });
            assert_eq!(line.as_deref(), Some(expected));
            assert_eq!(
                sh.reader.lock().unwrap().stage.capacity(),
                0,
                "after {expected}"
            );
        }
        // It was made for each of the two calls.
        assert_eq!(sh.reader.lock().unwrap().zero_filled, 2 * READ_STEP);
        // And a call that fails (here: a read that times out half way through a line) lets go too.
        peer.write_all(b"three").unwrap();
        let shared = Arc::clone(&sh);
        let failed = with_cx(move |cx| {
            let ticket = shared.reads.ticket();
            let limit = Limit {
                deadline: Some(Instant::now() + Duration::from_millis(150)),
                timeout: Duration::from_millis(150),
            };
            shared
                .read_call(
                    cx,
                    ticket,
                    limit,
                    |r| line_probe(r, "p"),
                    |r| take_line(r, "p"),
                )
                .map(|_| ())
                .map_err(|e| variant(&e))
        });
        assert_eq!(failed, Err("Timeout".to_string()));
        assert_eq!(sh.reader.lock().unwrap().stage.capacity(), 0);
        // The half line is still there for the next read.
        assert_eq!(sh.reader.lock().unwrap().available(), b"three");
    }

    /// A wait that ends because the poller stopped (or broke) is an error of `net` that says so,
    /// never the runtime's own text for a completer that was dropped, and the connection can be
    /// read again afterwards (the next wait starts a poller).
    #[test]
    fn a_wait_whose_poller_stops_is_a_net_error_and_the_connection_goes_on() {
        let (c, mut peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let (first, second) = with_cx(move |cx| {
            let poller = cx
                .state_mut::<poll::NetState>()
                .unwrap()
                .poller()
                .expect("the poller starts");
            let stopper = std::thread::spawn(move || {
                // Stops it when the read has armed its wait (nothing else waits on it).
                let began = Instant::now();
                while poller.counts() != (1, 1) {
                    assert!(
                        began.elapsed() < Duration::from_secs(30),
                        "the read never waited"
                    );
                    std::thread::yield_now();
                }
                poller.stop();
            });
            let read = |cx: &mut Ctx| {
                let ticket = sh.reads.ticket();
                sh.read_call(
                    cx,
                    ticket,
                    Limit::NONE,
                    |r| line_probe(r, "p"),
                    |r| take_line(r, "p"),
                )
            };
            let first = read(cx).map_err(|e| format!("{e:?}"));
            stopper.join().unwrap();
            peer.write_all(b"again\n").unwrap();
            let second = read(cx).map_err(|e| format!("{e:?}"));
            (first, second)
        });
        let text = first.expect_err("the wait was cut off");
        assert!(text.contains("Other"), "{text}");
        assert!(
            text.contains("cannot read from") && text.contains("the thread that waits for sockets"),
            "{text}"
        );
        assert!(!text.contains("ended without a result"), "{text}");
        assert_eq!(second.unwrap().as_deref(), Some("again"));
    }

    /// The ways a wait can end without a socket being ready are `net` errors in `net`'s words.
    #[test]
    fn the_ends_of_a_wait_that_are_not_readiness_have_net_wording() {
        let (c, _peer) = conn_pair();
        let sh = &c.sh;
        let text = |end| match sh.ended(end, "write to", Some((3, 10))) {
            Err(e) => format!("{e:?}"),
            Ok(_) => panic!("not an error"),
        };
        let dropped = text(Err(Incomplete::Dropped(
            "the operation ended without a result".into(),
        )));
        assert!(!dropped.contains("without a result"), "{dropped}");
        assert!(
            dropped.contains("the wait for the socket ended without an answer"),
            "{dropped}"
        );
        assert!(dropped.contains("3 of 10 bytes were sent"), "{dropped}");
        let refused = text(Err(Incomplete::NotStarted("the poller has stopped".into())));
        assert!(
            refused.contains("cannot wait for the socket: the poller has stopped"),
            "{refused}"
        );
        let failed = text(Ok(poll::Why::Failed));
        assert!(
            failed.contains("the thread that waits for sockets has stopped"),
            "{failed}"
        );
        for e in [dropped, refused, failed] {
            assert_eq!(
                e.split("variant: \"").nth(1).unwrap().split('"').next(),
                Some("Other")
            );
        }
    }

    /// The accounting of the burst budget: calls and bytes, each ending the burst once, and the
    /// count starting over afterwards (and whenever a call suspends).
    #[test]
    fn a_burst_is_cut_after_its_calls_or_its_bytes() {
        let burst = Burst::default();
        for _ in 0..MAX_BURST_CALLS {
            assert!(!burst.begin_call());
        }
        assert!(
            burst.begin_call(),
            "the call after the last one in the budget"
        );
        for _ in 0..MAX_BURST_CALLS {
            assert!(!burst.begin_call(), "the count started over");
        }
        burst.reset();
        assert!(!burst.spend(MAX_BURST - 1));
        assert!(burst.spend(1), "the bytes are used up");
        assert!(!burst.spend(1), "and start over");
        // A suspension starts the count over.
        for _ in 0..MAX_BURST_CALLS - 1 {
            burst.begin_call();
        }
        burst.reset();
        assert!(!burst.begin_call());
    }

    /// `close` of a connection that a read waits on with the poller ends the wait with `Closed`,
    /// and the descriptor is released by the time `close` returns: nothing holds the socket
    /// while a coroutine is suspended, and the poller's hold goes under its lock.
    #[test]
    fn close_releases_the_socket_of_a_connection_that_a_read_waits_on() {
        for _ in 0..20 {
            let (c, _peer) = conn_pair();
            let sh = Arc::clone(&c.sh);
            let weak = Arc::downgrade(&sh.sock.get().expect("open"));
            let (error, released) = with_cx(move |cx| {
                let poller = cx
                    .state_mut::<poll::NetState>()
                    .unwrap()
                    .poller()
                    .expect("the poller starts");
                let closer = {
                    let sh = Arc::clone(&sh);
                    std::thread::spawn(move || {
                        // Armed, and the reader's own reference (it is given to the poller
                        // and let go when the call suspends) gone: the connection's and the
                        // poller's are the two left. A close from another thread before that
                        // would meet the reader's reference, which is not what is tested (on
                        // the interpreter thread a close always comes after the suspension).
                        let began = Instant::now();
                        while poller.counts() != (1, 1) || weak.strong_count() != 2 {
                            assert!(
                                began.elapsed() < Duration::from_secs(30),
                                "the read never waited"
                            );
                            std::thread::yield_now();
                        }
                        sh.close();
                        weak.strong_count() == 0
                    })
                };
                let ticket = sh.reads.ticket();
                let error = sh
                    .read_call(
                        cx,
                        ticket,
                        Limit::NONE,
                        |r| line_probe(r, "p"),
                        |r| take_line(r, "p"),
                    )
                    .map(|_| ())
                    .map_err(|e| variant(&e));
                (error, closer.join().unwrap())
            });
            assert_eq!(error, Err("Closed".to_string()));
            assert!(released, "the socket was still held when close returned");
        }
    }
}
