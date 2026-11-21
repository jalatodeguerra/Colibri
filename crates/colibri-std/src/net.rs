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
//! *Everything that waits* runs on the interpreter thread, in the coroutine that calls it: reads
//! and writes of a connection, `accept` of a listener, and `connect`. A call makes its system
//! call at once; only when the socket would block does it wait: it registers a wait cell with
//! the `Vm`'s poller (one thread per `Vm`, started by the first wait that needs it and joined
//! when the `Vm` is dropped; it waits for the readiness of every such socket with `epoll`,
//! `kqueue` or the Windows AFD driver, and for deadlines, and sleeps for as long as nothing is
//! due) and suspends the coroutine. The poller only says that a socket is ready (or that the
//! deadline came, or that the object was closed); the coroutine makes its system call itself when
//! it resumes and waits again if the socket would still block. So an idle connection or listener
//! costs no thread and no wake-up, a cancelled call has taken nothing (what a read had read is in
//! the connection's buffer for the next read, what it had not read is still in the kernel, and a
//! connection that was ready for an accept is still in the listener's backlog), and the
//! interpreter thread holds no lock and no reference to the socket while a coroutine is suspended
//! or has given the thread away. (A read that waits holds no room for reading either: it reads
//! into uninitialised spare capacity, so no byte is written but the ones that arrive, and it lets
//! the buffer go before it suspends when the buffer holds nothing; one that holds a half line
//! keeps the half line.) An `accept` makes its system call and returns the connection without
//! suspending in between, so a cancel can neither lose a connection nor leak one.
//!
//! *Connect* looks the host up first when it is a name (a helper thread, because the system's
//! resolver cannot be interrupted; it answers by resolving the wait cell of the call, and the
//! call suspends with the deadline of the whole call), then, for each address, starts a
//! non-blocking connect and waits for the socket to become writable with the attempt's share of
//! the time left. The wait is at most a second long. Whenever it ends, `SO_ERROR` is read first
//! (a failed connect is not reported as an event by every system), then whether the socket is
//! connected at all (a wake proves nothing: the one-second recheck is a timer and no event, and
//! on Windows `getpeername` succeeds on a socket whose connect is still under way, so a connect
//! that is going to be refused looked connected at the recheck); a connect that is neither failed nor
//! done waits again, after a pause of 50 ms when the wake said "ready", so that no system can
//! turn that into a spin. The socket belongs to the call: a cancel ends the wait, and the socket
//! is closed when the call is left, which withdraws a connect that was under way. `listen` (its lookup and its bind never wait for a
//! peer) is the one call that still runs on a helper thread of the runtime.
//!
//! A call that does not have to wait does not suspend; so that a coroutine in a loop of reads and
//! writes (or accepts) against a peer that is always ready cannot keep the others from running,
//! a connection counts what its calls do without any of them suspending and, when
//! `MAX_BURST_CALLS` calls are used up, the next call gives the thread to the other coroutines
//! once; when `MAX_BURST` bytes are used up, the call that used them up does (after it released
//! its reference to the socket). The budget is per connection (and per listener, for accepts, of
//! calls only), so a loop over N connections that are all ready yields every 64 x N calls, not
//! every 64.
//!
//! Timeouts (`set_timeout`, `connect_timeout`) are deadlines that start when the script calls
//! the method, on the interpreter thread.
//!
//! # Cancelling never loses data
//!
//! A read takes bytes out of the buffer only when it returns, and reads the socket only while its
//! coroutine runs, so a coroutine that is cancelled while it waits has taken nothing: the bytes
//! stay for the next read. An accept makes its system call where the coroutine runs, so one that
//! is cancelled while it waits has accepted nothing: the connection stays in the backlog of the
//! listener for the next `accept` (and is reset when the listener is closed, as the system does
//! for every connection that was never accepted).
//!
//! # `close()` never suspends and never fails
//!
//! It sets the closed flag, takes the object's reference to the socket, shuts the socket down,
//! ends the waits that the object has armed with the poller (they resume with `Closed`, and the
//! poller's hold on the socket is gone when `close` returns), wakes everything that waits for a
//! turn, and returns. The socket is reference counted: every operation holds a reference while it
//! makes one system call and lets it go before it suspends, and the descriptor is released when
//! the last one is gone, so a wait never sees a closed or reused descriptor and `close` waits
//! for nobody. The same holds for a listener: its descriptor is released, and its port free,
//! when `close` returns.
//!
//! A `Conn` is full duplex. Reads wait for one turn queue, writes (and `close_write`, which
//! must not cut a write that is still queued) for another, and `close` takes neither: two
//! operations of one kind are never interleaved, and they are served in the order they were
//! called. A read or write that waits for its turn is parked in its queue without a thread until
//! the turn comes, the object is closed, its coroutine is cancelled or its deadline passes: it
//! does not tick. Accepts of a listener are served the same way.
//!
//! # Where nothing else can run
//!
//! A call made from a `Drop` hook or a callback of a host function cannot suspend: it blocks the
//! interpreter thread (`Ctx::can_suspend` is `false`), and the poller wakes it (a `connect`
//! from such a place waits for its lookup and its attempts the same way, and may block as long
//! as they take). A read (or accept) holds its turn until its coroutine has taken the result,
//! and a write is sent by the coroutine that called it, so a read called there while an earlier
//! read of the same connection is outstanding in another coroutine, an accept behind an accept
//! of the same listener, or a write behind a write of the same connection, would wait for a
//! coroutine that can never resume. It returns a `net.Error.Other` at once instead: that is the
//! one case the module can see. `close_write` is the exception, because it has nothing to wait for: behind another
//! coroutine's unfinished write it takes a place in the write order (a ticket and a parked cell
//! whose callback shuts the writing end, run by the drop of the ticket ahead on the interpreter
//! thread), returns `Ok` at once, and the write ahead is never cut. `Ok` from such a place means
//! "scheduled": a shutdown that then fails has nobody to tell, a connection that is closed
//! before its turn skips it, and a write called after it fails as a write after `close_write`
//! does. A write that finds no other write outstanding can still wait in a hook for room that
//! only another coroutine of the same program would make (the one that reads what is written),
//! and that coroutine cannot run while the hook waits. A program must not do that: from a
//! `Drop` hook or a callback, write only what the peer takes without help from this program (as
//! `fs` has the same limit with a pipe that this program reads itself).
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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind, Write};
use std::mem::MaybeUninit;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};

use colibri_runtime::{Bytes, Cap, Ctx, Error, HostType, Incomplete, TypeBuilder, TypedError, Vm};

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

// The poller and its wait cells, which every wait of this module is made with.
mod poll;

/// The longest line `read_line` accepts, without its end of line.
pub(crate) const MAX_LINE: usize = 1 << 20;

/// The longest delimiter `read_until` takes: the check for a delimiter that straddles the limit
/// costs the square of its length, and no protocol has one near this size.
const MAX_DELIM: usize = 1024;

/// The most bytes one `read_exact` buffers.
const MAX_EXACT: usize = 1 << 30;

/// The most one `send` is given, so that a deadline is noticed between sends. (A cancel is seen
/// only where the call suspends, a give-way included: a send that makes progress does not.)
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

/// The most one `read` call on the socket is given room for.
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
/// interrupted: `connect` asks a helper thread, see [`lookup`]).
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

/// The answer of the helper thread of a lookup (see [`lookup`]).
enum Lookup {
    Found(Vec<SocketAddr>),
    Failed,
    /// The helper thread could not be started: the system's reason.
    NoThread(String),
}

/// The system's answer for a name (what the helper thread of a lookup asks).
fn resolve_name(name: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    #[cfg(test)]
    if let Some(answer) = lookup_probe::answer(name, port) {
        return answer;
    }
    (name, port)
        .to_socket_addrs()
        .map(Iterator::collect::<Vec<SocketAddr>>)
}

/// What the unit tests see of the lookup helper: a name can be held (the helper waits until the
/// test lets it go, then answers with an address the test chose, no resolver asked), and the
/// end of the helper's work is reported.
#[cfg(test)]
mod lookup_probe {
    use std::collections::HashMap;
    use std::io;
    use std::net::SocketAddr;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex, PoisonError};

    struct Held {
        go: Receiver<()>,
        answer: SocketAddr,
        done: Sender<()>,
    }

    static HELD: Mutex<Option<HashMap<String, Held>>> = Mutex::new(None);

    /// Holds the lookup of `name` until `go` is sent on; `done` gets a message when the helper
    /// has finished with its answer.
    pub(super) fn hold(name: &str, go: Receiver<()>, answer: SocketAddr, done: Sender<()>) {
        HELD.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(name.to_string(), Held { go, answer, done });
    }

    pub(super) fn answer(name: &str, _port: u16) -> Option<io::Result<Vec<SocketAddr>>> {
        let held = HELD
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()?
            .remove(name)?;
        let _ = held.go.recv();
        let answer = held.answer;
        // Kept for `finished`.
        FINISHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(name.to_string(), held.done);
        Some(Ok(vec![answer]))
    }

    static FINISHED: Mutex<Option<HashMap<String, Sender<()>>>> = Mutex::new(None);

    pub(super) fn finished(name: &str) {
        if let Some(done) = FINISHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|m| m.remove(name))
        {
            let _ = done.send(());
        }
    }
}

/// What a `connect` suspends on: it belongs to no connection or listener, so nothing closes it
/// (the closed flag stays false) and the slot only holds the cell of its wait.
struct ConnectSite {
    name: String,
    never_closed: AtomicBool,
    slots: Arc<Mutex<Slots>>,
    burst: Burst,
}

impl ConnectSite {
    fn new(addr: &str) -> ConnectSite {
        ConnectSite {
            name: addr.to_string(),
            never_closed: AtomicBool::new(false),
            slots: Arc::new(Mutex::new(Slots::default())),
            burst: Burst::default(),
        }
    }
}

impl ConnectSite {
    /// Suspends until `until`, on a timer of the poller and nothing else.
    fn pause(&self, cx: &mut Ctx, until: Instant) -> Result<(), TypedError> {
        let action = "connect to";
        let poller = poller_of(cx, action, &self.name, "")?;
        let end = suspend(cx, move |cell| poller.timer(until, cell));
        self.ended(end, action, None).map(|_| ())
    }
}

impl Waits for ConnectSite {
    fn flag(&self) -> &AtomicBool {
        &self.never_closed
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn slot_table(&self) -> &Arc<Mutex<Slots>> {
        &self.slots
    }
    fn budget(&self) -> &Burst {
        &self.burst
    }
}

/// The error of a connect that ran out of its time.
fn connect_timed_out(addr: &str, timeout: Option<Duration>) -> TypedError {
    Kind::Timeout.error(format!(
        "cannot connect to `{addr}`: no connection within {} seconds (see `net.connect_timeout`)",
        timeout.map_or(0.0, |t| t.as_secs_f64())
    ))
}

/// Resolves the host of `addr` for `connect`: an IP address needs no resolver; a name goes to a
/// helper thread (the system's resolver cannot be interrupted) that stores its answer and
/// resolves the wait cell of this call, which suspends meanwhile with the deadline of the whole
/// call as a timer. A cancel or the deadline ends the wait at once; the helper thread ends when
/// the resolver does and owns nothing the program needs.
fn lookup(
    cx: &mut Ctx,
    site: &ConnectSite,
    addr: &str,
    deadline: Option<Instant>,
    timeout: Option<Duration>,
) -> Result<Vec<SocketAddr>, TypedError> {
    let action = "connect to";
    let (host, port) = split_addr(addr)?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if cannot_be_a_name(host) {
        return Err(dns_error(host));
    }
    let poller = poller_of(cx, action, addr, "")?;
    let slot: Arc<Mutex<Option<Lookup>>> = Arc::new(Mutex::new(None));
    let name = host.to_string();
    let end = suspend(cx, {
        let slot = Arc::clone(&slot);
        move |cell| {
            if let Some(deadline) = deadline {
                poller.timer(deadline, cell)?;
            }
            let (answer, helper_cell) = (Arc::clone(&slot), Arc::clone(cell));
            let spawned = std::thread::Builder::new()
                .name("colibri-net-resolve".into())
                .spawn(move || {
                    let found = resolve_name(&name, port);
                    *answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(match found {
                        Ok(found) => Lookup::Found(found),
                        Err(_) => Lookup::Failed,
                    });
                    helper_cell.fire(poll::Why::Ready);
                    #[cfg(test)]
                    lookup_probe::finished(&name);
                });
            if let Err(e) = spawned {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(Lookup::NoThread(reason(&e)));
                cell.fire(poll::Why::Ready);
            }
            #[cfg(test)]
            arm_probe::armed(cell);
            Ok(())
        }
    });
    match site.ended(end, action, None)? {
        Ended::Ready => {}
        Ended::TimedOut => return Err(connect_timed_out(addr, timeout)),
        Ended::Closed => {
            return Err(
                Kind::Other.error(format!("cannot {action} `{addr}`: the lookup was stopped"))
            );
        }
    }
    let answer = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    match answer {
        Some(Lookup::Found(found)) if !found.is_empty() => Ok(found),
        Some(Lookup::Found(_)) | None => Err(no_address(host)),
        Some(Lookup::Failed) => Err(dns_error(host)),
        Some(Lookup::NoThread(why)) => Err(Kind::Other.error(format!(
            "cannot connect to `{addr}`: cannot start a thread to look up `{host}`: {why}"
        ))),
    }
}

// ---- stopping ----

/// Why a wait ends before its operation has a result.
enum Stop {
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

/// What the error of a stop is made from: the closed flag, the deadline and the name.
struct Wait<'a> {
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
    #[cfg(test)]
    give_way_probe::called();
    if cx.can_suspend() {
        let _ = cx.try_await_completion::<()>(|completer| {
            completer.complete(());
            Ok(())
        });
    }
}

/// What the unit tests see of `give_way`: how many times this thread has called it, and a hook
/// that runs at the moment it is called (before the coroutine gives the thread away).
#[cfg(test)]
mod give_way_probe {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static CALLS: Cell<usize> = const { Cell::new(0) };
        static HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    pub(super) fn called() {
        CALLS.with(|c| c.set(c.get() + 1));
        // Taken out while it runs, so that a hook may itself call things that give way.
        if let Some(mut hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
            HOOK.with(|h| *h.borrow_mut() = Some(hook));
        }
    }

    pub(super) fn calls() -> usize {
        CALLS.with(Cell::get)
    }

    pub(super) fn set_hook(hook: impl FnMut() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn clear_hook() {
        HOOK.with(|h| *h.borrow_mut() = None);
    }
}

/// A test-build hook that runs once in `wait_ready`, on the interpreter thread, right after the
/// wait was armed with the poller and before the coroutine suspends: the other side of a test
/// does what it needs to there (and waits for the cell to be resolved, with a ceiling).
#[cfg(test)]
mod arm_probe {
    use std::cell::RefCell;
    use std::sync::Arc;

    use super::poll::Cell;

    type Hook = Box<dyn FnMut(&Arc<Cell>)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(super) fn armed(cell: &Arc<Cell>) {
        // Taken out while it runs, and put back unless it set another meanwhile.
        if let Some(mut hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook(cell);
            HOOK.with(|h| {
                let mut slot = h.borrow_mut();
                if slot.is_none() {
                    *slot = Some(hook);
                }
            });
        }
    }

    /// A hook for the next arm only.
    pub(super) fn set_hook(hook: impl FnOnce(&Arc<Cell>) + 'static) {
        let mut hook = Some(hook);
        set_hook_for_every_arm(move |cell| {
            if let Some(hook) = hook.take() {
                hook(cell);
            }
        });
    }

    /// A hook for every arm, until the thread's next `set_hook*` replaces it.
    pub(super) fn set_hook_for_every_arm(hook: impl FnMut(&Arc<Cell>) + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
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

    /// The error of a stop. `sent` is `(done, total)` for a write.
    fn stop_error(&self, stop: Stop, action: &str, sent: Option<(usize, usize)>) -> TypedError {
        let note = sent_note(sent);
        match stop {
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
}

/// The order in which operations of one kind (reads, writes, accepts) on one object are served:
/// the order their calls were made. A ticket is taken on the interpreter thread when the method
/// is called, and the operation waits (suspended) for its turn, so which
/// thread happens to start first decides nothing. A ticket that is dropped without having had its turn (the
/// coroutine was cancelled, the operation stopped while it waited, its job never started) is
/// skipped, so it neither runs later nor holds up the ones behind it.
struct Turns {
    state: Mutex<TurnState>,
}

struct TurnState {
    next: u64,
    serving: u64,
    skipped: BTreeSet<u64>,
    /// The wait cells of tickets that wait for their turn (see `Ticket::park`), by ticket.
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
        self.turns.park_at(self.id, cell);
    }
}

impl Turns {
    /// `Ticket::park` for the ticket `id`, which a callback of `cell` may own (see
    /// `Shared::schedule_shutdown`): only the queue and the id are used here.
    fn park_at(self: &Arc<Self>, id: u64, cell: &Arc<poll::Cell>) {
        let mut st = self.lock();
        if st.serving == id {
            drop(st);
            cell.fire(poll::Why::Turn);
            return;
        }
        let turns = Arc::downgrade(self);
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
    /// The deadline of the attempt came.
    TimedOut,
    /// The wait for the connection ended for another reason (the caller was cancelled, the
    /// poller stopped): the error to give.
    Failed(TypedError),
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

/// How a connect attempt started.
enum Started {
    Connected(TcpStream),
    /// Under way: the socket is to be waited for (writable), and `SO_ERROR` read.
    Pending(Socket),
}

/// Starts a connect to one address without blocking: looks at the deadline, makes the socket and
/// calls `connect`. Nothing is waited for here.
fn start_connect(addr: SocketAddr, deadline: Option<Instant>) -> Result<Started, Attempt> {
    // `listen("0.0.0.0:0")` followed by `connect(listener.addr())` is natural, and must work
    // everywhere: Windows refuses to connect to an unspecified address.
    let addr = loopback_for(addr);
    // Looked at before the socket exists: a call that is late does nothing.
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Attempt::TimedOut);
    }
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(Attempt::Io)?;
    sock.set_nonblocking(true).map_err(Attempt::Io)?;
    #[cfg(target_vendor = "apple")]
    let _ = sock.set_nosigpipe(true);
    let _ = sock.set_tcp_nodelay(true);
    match sock.connect(&SockAddr::from(addr)) {
        // Done at once, by what `connect` says; the same check as after a wait confirms it.
        Ok(()) => match is_connected(&sock) {
            Ok(true) => finished_connect(sock).map(Started::Connected),
            Ok(false) => Ok(Started::Pending(sock)),
            Err(e) => Err(Attempt::Io(e)),
        },
        Err(e) if in_progress(&e) => Ok(Started::Pending(sock)),
        Err(e) => Err(Attempt::Io(e)),
    }
}

/// How long a connect that is waited for may go without looking at `SO_ERROR`: some systems do
/// not report a failed connect as an event (old Windows, and a refusal that is still in progress
/// on macOS may not), so a wait is at most this long. One wake a second for a connect in flight,
/// never for an idle connection.
const CONNECT_RECHECK: Duration = Duration::from_secs(1);

/// Whether the connect of `sock` has completed, which is not the same as the socket being
/// readable or writable, and not to be read from `SO_ERROR` alone (it is 0 while the connect is
/// still under way). On Unix a socket that is not connected has no peer (`getpeername` fails
/// with `ENOTCONN`), and one that is connected has.
#[cfg(unix)]
fn is_connected(sock: &Socket) -> io::Result<bool> {
    match sock.peer_addr() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotConnected => Ok(false),
        Err(e) => Err(e),
    }
}

/// Windows differs: `getpeername` succeeds on a socket whose non-blocking connect is still under
/// way (it gives back the address that was passed to `connect`), so a peer address proves
/// nothing, and a connect that is going to be refused stays in that state for about two seconds
/// while the SYN is sent again. `SO_CONNECT_TIME` is what says: the seconds the socket has been
/// connected, or `0xFFFFFFFF` while it is not. Microsoft documents it for connection-oriented
/// sockets; that it answers for a plain non-blocking `connect` is what the Windows CI of this
/// project confirmed on the refused-connect tests. A socket that is not connected may also make
/// the call fail with `WSAENOTCONN`, which means "not yet" as well, and is no error of the connect.
#[cfg(windows)]
fn is_connected(sock: &Socket) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_CONNECT_TIME, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, getsockopt,
    };
    let mut seconds: u32 = 0;
    let mut len = size_of::<u32>() as i32;
    // SAFETY: `seconds` is a valid `u32` for the call to write, `len` is its size and a valid
    // `i32` to read and write, and the socket is open.
    let r = unsafe {
        getsockopt(
            sock.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_CONNECT_TIME,
            std::ptr::from_mut(&mut seconds).cast::<u8>(),
            &mut len,
        )
    };
    if r == SOCKET_ERROR {
        // SAFETY: a plain call that reads the thread's last Winsock error.
        let code = unsafe { WSAGetLastError() };
        // WSAENOTCONN: not connected (yet), not a failure of the connect.
        return if code == 10057 {
            Ok(false)
        } else {
            Err(io::Error::from_raw_os_error(code))
        };
    }
    Ok(seconds != u32::MAX)
}

/// How long a connect that is waited for pauses after a wake that said "ready" but found it
/// neither failed nor connected (a state that the systems do not seem to have; if one had it,
/// a one-shot wait that is ready at once would otherwise spin): a timer, never the socket, so
/// that such a connect costs twenty wakes a second at most, and the deadline ends it.
const CONNECT_PAUSE: Duration = Duration::from_millis(50);

/// The socket of a connect that has ended, as a stream, or the error it ended with.
fn finished_connect(sock: Socket) -> Result<TcpStream, Attempt> {
    if let Some(e) = sock.take_error().map_err(Attempt::Io)? {
        return Err(Attempt::Io(e));
    }
    Ok(sock.into())
}

/// Waits (suspended) for a connect that `start_connect` left under way: until the socket is
/// writable, a failure shows in `SO_ERROR`, or `deadline` (the attempt's) comes. The wait ends at
/// least every `CONNECT_RECHECK`, and what the socket says is looked at before the deadline is,
/// whatever woke the call. The socket is the call's own: a cancel ends the wait, the poller lets
/// go of it, and it is closed when this call is left, which withdraws a connect that was under
/// way. Nothing connects in the background.
fn finish_connect(
    cx: &mut Ctx,
    site: &ConnectSite,
    sock: Arc<Socket>,
    deadline: Option<Instant>,
) -> Result<TcpStream, Attempt> {
    loop {
        let recheck = Instant::now() + CONNECT_RECHECK;
        let limit = Limit {
            deadline: Some(deadline.map_or(recheck, |d| d.min(recheck))),
            timeout: Duration::ZERO,
        };
        let woke = match site.wait_ready(
            cx,
            Arc::clone(&sock),
            poll::Dir::Write,
            limit,
            "connect to",
            None,
        ) {
            Ok(woke @ (Ended::Ready | Ended::TimedOut)) => woke,
            Ok(Ended::Closed) => {
                return Err(Attempt::Failed(Kind::Other.error(format!(
                    "cannot connect to `{}`: the wait was stopped",
                    site.name
                ))));
            }
            Err(e) => return Err(Attempt::Failed(e)),
        };
        // What a wake proves: nothing by itself (the recheck is a timer, a "ready" is "try
        // again"). What the socket says does: a failure shows in `SO_ERROR` (read first), and a
        // connect that has completed is told by `is_connected`, not by the wake. Anything else
        // is a connect still under way, and the wait is armed again.
        if let Some(e) = sock.take_error().map_err(Attempt::Io)? {
            return Err(Attempt::Io(e));
        }
        if is_connected(&sock).map_err(Attempt::Io)? {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Attempt::TimedOut);
        }
        if matches!(woke, Ended::Ready) {
            // "Ready" with the connect neither failed nor done: not to be spun on.
            let until = Instant::now() + CONNECT_PAUSE;
            site.pause(cx, deadline.map_or(until, |d| d.min(until)))
                .map_err(Attempt::Failed)?;
        }
    }
    // The poller's reference went with the resolved cell, before this call resumed.
    let sock = match Arc::try_unwrap(sock) {
        Ok(sock) => sock,
        // Never so by what is said above; if it were, the socket is duplicated, and a system
        // that cannot (out of descriptors) is an error of the connect and not a panic.
        Err(shared) => shared.try_clone().map_err(Attempt::Io)?,
    };
    Ok(sock.into())
}

// ---- connections ----

/// What was read from the socket and not yet handed to the script.
struct Reader {
    data: Vec<u8>,
    pos: usize,
    /// Bytes after `pos` known to hold no `\n`.
    scanned: usize,
    /// What `read_until` scans for: the delimiter of the last such call, and how many bytes after
    /// `pos` hold no start of it (so the next look begins there, not at `pos`).
    delim: Vec<u8>,
    delim_clear: usize,
    eof: bool,
}

/// Where a read gets its bytes from: the socket (`recv` into room that holds nothing yet, so the
/// room is never zero-filled and only the bytes that arrive are ever written), or a test's
/// source.
///
/// # Safety
///
/// An implementation must have written (initialised) the first `n` bytes of `buf` when it
/// returns `Ok(n)`, and must not return an `n` above `buf.len()`: `Reader::read_from` takes those
/// bytes as part of its data without looking.
unsafe trait Recv {
    /// Writes up to `buf.len()` bytes at the start of `buf` and returns how many; the bytes
    /// after those are not touched.
    fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize>;
}

// SAFETY: `SockRef::recv` is `recv(2)`, which writes the bytes it returns the count of.
unsafe impl Recv for &TcpStream {
    fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
        SockRef::from(*self).recv(buf)
    }
}

impl Reader {
    fn new() -> Reader {
        Reader {
            data: Vec::new(),
            pos: 0,
            scanned: 0,
            delim: Vec::new(),
            delim_clear: 0,
            eof: false,
        }
    }

    fn available(&self) -> &[u8] {
        &self.data[self.pos..]
    }

    /// Hands out the first `n` bytes of what is buffered. A small piece is copied; a big one
    /// is the buffer itself, so that a big `read_exact` never holds the data twice.
    fn take(&mut self, n: usize) -> Vec<u8> {
        self.scanned = 0;
        self.delim_clear = 0;
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

    /// Lets go of the buffer when it holds nothing that has not been taken: a read that waits
    /// (and a connection that is idle) holds no memory for reading, resident or virtual. The
    /// next read makes room again, which is an allocation and no write. A buffer that holds
    /// bytes keeps them, with the room it has: they are what a later read goes on from.
    fn release(&mut self) {
        if self.pos == self.data.len() {
            self.data = Vec::new();
            self.pos = 0;
        }
    }

    /// The end of one read call (`read`, `read_line`, `read_exact`).
    fn end_call(&mut self) {
        self.release();
    }

    /// Reads once from `src` into the room of the buffer: up to one `READ_STEP` (or
    /// `READ_UNKNOWN` when `want` is 0). The room is the spare capacity of `data` itself, handed
    /// to the system as it is (`recv` into uninitialised bytes), so no byte is written but the
    /// ones that arrive: a peer that sends single bytes costs a copy of one byte per read, and
    /// a read that finds nothing touches no page. Gives `Err(bytes)` when memory for that many
    /// cannot be had, and otherwise the result of the read (the number of bytes appended; 0 is
    /// the end of the stream).
    fn read_from(&mut self, src: &mut impl Recv, want: usize) -> Result<io::Result<usize>, usize> {
        let chunk = if want == 0 {
            READ_UNKNOWN
        } else {
            want.min(READ_STEP)
        };
        self.make_room(want, chunk)
            .map_err(|_| self.step(want).saturating_add(chunk).min(want.max(chunk)))?;
        let room = &mut self.data.spare_capacity_mut()[..chunk];
        Ok(match src.recv(room) {
            Ok(n) => {
                assert!(
                    n <= chunk,
                    "a read returned more bytes than the room it was given"
                );
                // SAFETY: `recv` wrote the first `n` bytes of `room`, which is the spare
                // capacity of `data` after its `len` bytes, and `n <= chunk` is within it.
                unsafe { self.data.set_len(self.data.len() + n) };
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
    /// Set when a shutdown of the writing end was scheduled behind a write (see
    /// `schedule_shutdown`): a write that a hook makes after that must fail as one after
    /// `close_write` does, and cannot wait behind the shutdown for the write ahead of it.
    shutdown_scheduled: AtomicBool,
    /// Test builds: how many sends the write loop has begun (a test that closes from another
    /// thread asks whether the writer was in a send).
    #[cfg(test)]
    send_attempts: AtomicUsize,
    /// Nanoseconds; 0 means no timeout.
    timeout_ns: AtomicU64,
    /// The longest line `read_line` takes (`set_line_limit`), read at each look of a call.
    line_limit: AtomicUsize,
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

impl Drop for Shared {
    /// The parked cells of the queues (a shutdown scheduled by `close_write` owns a ticket, which
    /// owns its queue) are let go of when the connection goes away, so that nothing keeps a
    /// queue alive in a cycle with itself.
    fn drop(&mut self) {
        self.reads.wake();
        self.writes.wake();
    }
}

/// A connection, from `net.connect` or `listener.accept()`. When the last reference to it goes
/// away the socket is closed.
pub(crate) struct Conn {
    sh: Arc<Shared>,
}

impl Conn {
    /// Wraps a socket that `prepare` or `start_connect` / `finish_connect` has made ready.
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
                shutdown_scheduled: AtomicBool::new(false),
                #[cfg(test)]
                send_attempts: AtomicUsize::new(0),
                timeout_ns: AtomicU64::new(0),
                line_limit: AtomicUsize::new(MAX_LINE),
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

/// What a call that suspends needs of the object it is on, which a connection and a listener
/// both have: the closed flag, the name for messages, the slots that `close` resolves, and the
/// burst budget that a suspension starts over. The suspensions themselves are the same.
trait Waits {
    fn flag(&self) -> &AtomicBool;
    fn label(&self) -> &str;
    fn slot_table(&self) -> &Arc<Mutex<Slots>>;
    fn budget(&self) -> &Burst;

    fn wait_of(&self, limit: Limit) -> Wait<'_> {
        Wait {
            closed: self.flag(),
            limit,
            name: self.label(),
        }
    }

    /// What the end of a suspension means for the call that suspended.
    fn ended(
        &self,
        end: Result<poll::Why, Incomplete>,
        action: &str,
        sent: Option<(usize, usize)>,
    ) -> Result<Ended, TypedError> {
        self.budget().reset();
        let note = sent_note(sent);
        let other = |what: &str| {
            Kind::Other.error(format!("cannot {action} `{}`: {what}{note}", self.label()))
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
        self.wait_of(limit).check_open(action)?;
        if ticket.has_turn() {
            return Ok(());
        }
        let poller = poller_of(cx, action, self.label(), "")?;
        let end = suspend(cx, |cell| {
            ticket.park(cell);
            if let Some(deadline) = limit.deadline {
                poller.timer(deadline, cell)?;
            }
            // Stored, then looked at: a close that came before the cell was parked has woken
            // nobody (`Turns::wake` took an empty queue).
            if self.flag().load(Ordering::SeqCst) {
                cell.fire(poll::Why::Closed);
            }
            Ok(())
        });
        match self.ended(end, action, None)? {
            Ended::Ready => Ok(()),
            Ended::TimedOut => Err(self.wait_of(limit).stop_error(Stop::Timeout, action, None)),
            Ended::Closed => Err(self
                .wait_of(Limit::NONE)
                .stop_error(Stop::Closed, action, None)),
        }
    }

    /// Suspends until `sock` is ready in direction `dir` (spuriously too), the deadline of `limit`
    /// comes, or the connection is closed. `sock` is a reference of this call's own and is given
    /// to the poller: nothing of it is held here while the coroutine is suspended.
    fn wait_ready<S: poll::Source>(
        &self,
        cx: &mut Ctx,
        sock: Arc<S>,
        dir: poll::Dir,
        limit: Limit,
        action: &str,
        sent: Option<(usize, usize)>,
    ) -> Result<Ended, TypedError> {
        let poller = poller_of(cx, action, self.label(), &sent_note(sent))?;
        let end = suspend(cx, move |cell| {
            // So that a close finds the wait: stored, then the flag is looked at.
            let leave = {
                let slots = Arc::downgrade(self.slot_table());
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
            *lock_slots(self.slot_table()).of(dir) = Some(Arc::clone(cell));
            if cell.is_resolved() {
                // Resolved between the two lines above: the clean-up ran before the store.
                *lock_slots(self.slot_table()).of(dir) = None;
                return Ok(());
            }
            if self.flag().load(Ordering::SeqCst) {
                cell.fire(poll::Why::Closed);
                return Ok(());
            }
            let armed = poller.arm(&sock, dir, limit.deadline, cell);
            #[cfg(test)]
            arm_probe::armed(cell);
            armed
        });
        self.ended(end, action, sent)
    }
}

impl Waits for Shared {
    fn flag(&self) -> &AtomicBool {
        &self.closed
    }
    fn label(&self) -> &str {
        &self.peer
    }
    fn slot_table(&self) -> &Arc<Mutex<Slots>> {
        &self.slots
    }
    fn budget(&self) -> &Burst {
        &self.burst
    }
}

impl Shared {
    /// The deadline a call made now has. Taken on the interpreter thread, when the method is
    /// called, so waiting for earlier calls counts against it.
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

    fn wait(&self, limit: Limit) -> Wait<'_> {
        Wait {
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

    /// A write that cannot suspend (a `Drop` hook, a callback) blocks the interpreter thread, and
    /// a write that another coroutine has begun is sent by that coroutine when it runs again: it
    /// would wait for something that cannot happen while the thread is blocked. It is refused at
    /// once instead, as a read is. (`close_write` is not refused: it is scheduled.)
    fn refuse_behind_a_write(&self, cx: &mut Ctx, action: &str) -> Result<(), TypedError> {
        if !cx.can_suspend()
            && !self.closed.load(Ordering::SeqCst)
            && self.shutdown_scheduled.load(Ordering::SeqCst)
        {
            // Behind a scheduled shutdown the write would fail anyway: it fails now, with the
            // error of a write after `close_write`.
            let (kind, message) =
                write_failure(action, &self.peer, &io::Error::from(ErrorKind::BrokenPipe));
            return Err(kind.error(message));
        }
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
        self.wait(limit).stop_error(Stop::Timeout, action, sent)
    }

    fn closed_error(&self, action: &str, sent: Option<(usize, usize)>) -> TypedError {
        self.wait(Limit::NONE)
            .stop_error(Stop::Closed, action, sent)
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
        // A wake that said "ready" is owed its try, whatever the clock says by the time this
        // coroutine runs again (the thread may have been busy past the deadline meanwhile).
        let mut owed = false;
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
            if tried && !final_try && !owed && limit.late() {
                return Err(self.timeout_error(limit, action, None));
            }
            owed = false;
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
                        // No reference to the socket while the coroutine is away: a close that
                        // runs meanwhile must release the descriptor.
                        drop(sock);
                        give_way(cx);
                    }
                }
                Ok(Err(e)) if e.kind() == ErrorKind::Interrupted => {}
                Ok(Err(e)) if e.kind() == ErrorKind::WouldBlock => {
                    if final_try || limit.late() {
                        return Err(self.timeout_error(limit, action, None));
                    }
                    // A waiting read holds no room for reading (see `Reader::release`).
                    self.lock_reader().release();
                    match self.wait_ready(cx, sock, poll::Dir::Read, limit, action, None)? {
                        Ended::Ready => owed = true,
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
        self.wait(limit).check_open(action)?;
        self.await_turn(cx, ticket, limit, action)?;
        if self.write_broken.load(Ordering::SeqCst) {
            return Err(Kind::BrokenPipe.error(format!(
                "cannot write to `{}`: an earlier write stopped part way, so the stream is out of step (close the connection; reading still works)",
                self.peer
            )));
        }
        let mut tried = false;
        let mut final_try = false;
        // As in `read_call`: a wake that said "ready" is owed its try.
        let mut owed = false;
        while *done < total {
            if self.closed.load(Ordering::SeqCst) {
                return Err(self.closed_error(action, Some((*done, total))));
            }
            // Between every two sends, so a deadline does not wait for the end of a big write.
            // The first send is tried whatever the deadline says.
            if tried && !final_try && !owed && limit.late() {
                return Err(self.timeout_error(limit, action, Some((*done, total))));
            }
            owed = false;
            tried = true;
            let Some(sock) = self.sock.get() else {
                return Err(self.closed_error(action, Some((*done, total))));
            };
            let end = total.min(*done + SEND_CHUNK);
            let failed = |e: &io::Error| {
                let (kind, message) = write_failure(action, &self.peer, e);
                kind.error(format!("{message}{}", sent_note(Some((*done, total)))))
            };
            #[cfg(test)]
            self.send_attempts.fetch_add(1, Ordering::SeqCst);
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
                        drop(sock);
                        give_way(cx);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if final_try || limit.late() {
                        return Err(self.timeout_error(limit, action, Some((*done, total))));
                    }
                    // No room at all: the wait ends when the socket says "try again", which is
                    // not a promise of room for the rest (a short send is followed by another
                    // send, not by a wait).
                    match self.wait_ready(
                        cx,
                        sock,
                        poll::Dir::Write,
                        limit,
                        action,
                        Some((*done, total)),
                    )? {
                        Ended::Ready => owed = true,
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

    /// Takes a place in the write order for the shutdown of the writing end, and returns: the
    /// shutdown is made when the turn comes, by whoever passes the turn on (the drop of the
    /// ticket ahead, on the interpreter thread), and a closed connection skips it. This is what
    /// `close_write` does where it cannot wait for the writes ahead; a shutdown that fails then
    /// has nobody to tell (`NotConnected` is ignored anyway). The queue is the one thing the
    /// parked cell keeps alive; the connection is only reached through a weak reference, so a
    /// connection that goes away with the shutdown still scheduled is not kept alive by it.
    fn schedule_shutdown(self: &Arc<Self>) {
        self.shutdown_scheduled.store(true, Ordering::SeqCst);
        let ticket = self.writes.ticket();
        let (turns, id) = (Arc::clone(&self.writes), ticket.id);
        let sh = Arc::downgrade(self);
        let cell = poll::Cell::new(move |why| {
            // Dropped at the end: the turn goes on to the next write, after the shutdown.
            let _ticket = ticket;
            if why == poll::Why::Turn
                && let Some(sh) = sh.upgrade()
                && !sh.closed.load(Ordering::SeqCst)
            {
                let _ = sh.shut_write();
            }
        });
        turns.park_at(id, &cell);
    }

    /// Shuts the writing end down, whoever is ahead of it in the queue having finished.
    fn shut_write(&self) -> Result<(), TypedError> {
        let action = "close the writing end of";
        self.wait(Limit::NONE).check_open(action)?;
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

/// Looks for the next line in the buffer. A line may be `limit` bytes long, not counting its
/// `\n` or `\r\n`.
fn find_line(r: &mut Reader, peer: &str, limit: usize) -> Result<Line, TypedError> {
    let start = r.pos;
    let from = start + r.scanned;
    if let Some(at) = r.data[from..].iter().position(|&b| b == b'\n') {
        let end = from + at;
        let mut content = end - start;
        if content > 0 && r.data[end - 1] == b'\r' {
            content -= 1;
        }
        if content > limit {
            return Err(too_long(peer, limit));
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
        if pending > limit {
            return Err(too_long(peer, limit));
        }
        return Ok(Line::Tail(pending));
    }
    // A line of exactly `limit` bytes may be waiting for its `\n` behind a `\r`.
    if pending > limit + 1 || (pending == limit + 1 && r.data.last() != Some(&b'\r')) {
        return Err(too_long(peer, limit));
    }
    Ok(Line::More)
}

fn too_long(peer: &str, limit: usize) -> TypedError {
    Kind::LineTooLong.error(format!(
        "cannot read a line from `{peer}`: it is longer than {limit} bytes without an end of line (close the connection, raise the limit with `set_line_limit`, or read the data with `read`)"
    ))
}

/// How much a read for a line or a delimited piece asks for: no more than can still matter, so
/// that a peer cannot make a call read past the limit and the end of the line (or the delimiter)
/// by a whole read. `bound` is that many bytes, and the read is for what is left to the larger of
/// it and `READ_UNKNOWN` less what is `pending`: a tight limit does not make the reads smaller
/// than the usual ones (a call near its limit would otherwise cost a system call for each record
/// of a stream of short ones). From the size of an ordinary read up the answer is 0 ("unknown":
/// the usual read of `READ_UNKNOWN` bytes), so a limit of a mebibyte reads as before until it is
/// nearly reached; below that it is the exact number (at least 1).
fn want_within(bound: usize, pending: usize) -> usize {
    let room = bound.max(READ_UNKNOWN).saturating_sub(pending);
    if room >= READ_UNKNOWN { 0 } else { room.max(1) }
}

fn line_probe(r: &mut Reader, peer: &str, limit: usize) -> Probe {
    match find_line(r, peer, limit) {
        // A line of `limit` bytes with `\r\n` is the longest that can still be taken: with two
        // bytes more and no end the call fails.
        Ok(Line::More) => Probe::Pending(want_within(limit + 2, r.available().len())),
        Ok(_) => Probe::Ready,
        Err(e) => Probe::Failed(e),
    }
}

fn take_line(r: &mut Reader, peer: &str, limit: usize) -> Result<Option<String>, TypedError> {
    match find_line(r, peer, limit)? {
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

/// Where the next `delim` is in the buffer (see `find_delim`).
enum Found {
    /// Its end is not there yet.
    More,
    /// The stream ended with nothing pending.
    End,
    /// The stream ended after this many bytes without the delimiter.
    Tail(usize),
    /// `content` bytes before the delimiter; `consumed` bytes with it.
    Full { content: usize, consumed: usize },
}

/// Looks for the next `delim` in the buffer, at a start no later than `max` bytes after the
/// first unread byte (`max` counts the bytes before the delimiter). Only the bytes that arrived
/// since the last look are scanned, backing up by the length of the delimiter less one (`r.delim`
/// and `r.delim_clear` say what the last look covered).
///
/// It is `LineTooLong` as soon as the bytes can no longer be followed by a delimiter that starts
/// within `max`: a match that starts after it, or `max + 1` bytes of which no tail is the
/// beginning of the delimiter (a delimiter of one byte: the first byte past `max`). A tail that
/// still is the beginning of the delimiter waits for the rest of it, which is how a delimiter
/// that straddles the limit is found.
fn find_delim(r: &mut Reader, delim: &[u8], max: usize, peer: &str) -> Result<Found, TypedError> {
    if r.delim != delim {
        r.delim = delim.to_vec();
        r.delim_clear = 0;
    }
    let dl = delim.len();
    let start = r.pos;
    let from = start + r.delim_clear;
    if let Some(at) = memchr::memmem::find(&r.data[from..], delim) {
        let content = r.delim_clear + at;
        if content > max {
            return Err(too_long_until(peer, max));
        }
        return Ok(Found::Full {
            content,
            consumed: content + dl,
        });
    }
    let pending = r.data.len() - start;
    r.delim_clear = r.delim_clear.max(pending.saturating_sub(dl - 1));
    if r.eof {
        if pending == 0 {
            return Ok(Found::End);
        }
        if pending > max {
            return Err(too_long_until(peer, max));
        }
        return Ok(Found::Tail(pending));
    }
    if pending > max {
        // A start in `pending - dl + 1 ..= max` whose bytes so far are the beginning of `delim`
        // may still be completed.
        let lowest = (pending + 1).saturating_sub(dl);
        let open = (lowest..=max).any(|s| {
            let tail = &r.data[start + s..start + pending];
            delim.starts_with(tail)
        });
        if !open {
            return Err(too_long_until(peer, max));
        }
    }
    Ok(Found::More)
}

fn too_long_until(peer: &str, max: usize) -> TypedError {
    Kind::LineTooLong.error(format!(
        "cannot read from `{peer}`: more than {max} bytes without the delimiter (close the connection, or raise the limit given to `read_until`)"
    ))
}

fn delim_probe(r: &mut Reader, delim: &[u8], max: usize, peer: &str) -> Probe {
    match find_delim(r, delim, max, peer) {
        // `max + delim.len()` pending bytes without a delimiter are too long whatever follows.
        Ok(Found::More) => Probe::Pending(want_within(max + delim.len(), r.available().len())),
        Ok(Found::Tail(have)) => Probe::Failed(Kind::UnexpectedEnd.error(format!(
            "cannot read from `{peer}`: the stream ended after {have} bytes without the delimiter ({})",
            if have == 1 {
                "the 1 byte stays readable with `read`".to_string()
            } else {
                format!("the {have} bytes stay readable with `read`")
            }
        ))),
        Ok(_) => Probe::Ready,
        Err(e) => Probe::Failed(e),
    }
}

fn take_until(
    r: &mut Reader,
    delim: &[u8],
    max: usize,
    peer: &str,
) -> Result<Option<Bytes>, TypedError> {
    match find_delim(r, delim, max, peer)? {
        Found::Full { content, consumed } => {
            let mut piece = r.take(consumed);
            piece.truncate(content);
            Ok(Some(Bytes(piece)))
        }
        Found::End => Ok(None),
        Found::Tail(_) | Found::More => Err(Kind::Other.error(format!(
            "cannot read from `{peer}`: the buffered data is gone (a bug in `net`)"
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
            // The limit is read at each look, so a `set_line_limit` made while this call waits
            // applies from its next look.
            let (a, b) = (Arc::clone(&this.sh), Arc::clone(&this.sh));
            on_read(
                cx,
                this,
                move |r| line_probe(r, &a.peer, a.line_limit.load(Ordering::SeqCst)),
                move |r| take_line(r, &b.peer, b.line_limit.load(Ordering::SeqCst)),
            )
        });
        // The bytes before the next `delim`, which is taken off the stream with them; `none`
        // at the end of the stream with nothing pending; `LineTooLong` when `max` bytes
        // (counted before the delimiter) are passed without it.
        d.method(
            "read_until",
            ["delim", "max"],
            |cx: &mut Ctx, this: &Conn, delim: Bytes, max: i64| {
                if delim.0.is_empty() {
                    return Err(Kind::Other.error(format!(
                        "cannot read from `{}`: the delimiter is empty (give at least one byte)",
                        this.sh.peer
                    )));
                }
                if delim.0.len() > MAX_DELIM {
                    return Err(Kind::Other.error(format!(
                        "cannot read from `{}`: the delimiter has {} bytes, and at most {MAX_DELIM} are allowed",
                        this.sh.peer,
                        delim.0.len()
                    )));
                }
                let max = match usize::try_from(max) {
                    Ok(m) if m <= MAX_EXACT => m,
                    _ => {
                        return Err(Kind::Other.error(format!(
                            "cannot read from `{}`: {max} is not a limit for the bytes before the delimiter (use 0 to {MAX_EXACT})",
                            this.sh.peer
                        )));
                    }
                };
                let a = Arc::new(delim.0);
                let (p1, p2) = (this.sh.peer.clone(), this.sh.peer.clone());
                let a2 = Arc::clone(&a);
                on_read(
                    cx,
                    this,
                    move |r| delim_probe(r, &a, max, &p1),
                    move |r| take_until(r, &a2, max, &p2),
                )
            },
        );
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
        // connection ends with `Closed`. Closing twice is fine. Never waits for anything and
        // never fails.
        d.method("close", [], |this: &Conn| this.sh.close());
        // Tells the peer that nothing more comes (it reads the end of the stream) and keeps
        // reading possible. It takes its turn behind the writes called before it, so a write
        // that is still on its way is sent whole first; with none queued it is done at once. From
        // a hook or callback behind another coroutine's write it is scheduled and returns `Ok` at
        // once ("scheduled": a shutdown error then has nowhere to go).
        d.method("close_write", [], |cx: &mut Ctx, this: &Conn| {
            let sh = Arc::clone(&this.sh);
            let action = "close the writing end of";
            sh.begin_call(cx);
            sh.wait(Limit::NONE).check_open(action)?;
            if let Some(_turn) = sh.writes.immediate() {
                return sh.shut_write();
            }
            if !cx.can_suspend() {
                // A hook or callback cannot wait for the writes ahead (they are sent by the
                // coroutines that began them): the shutdown takes its place in the order and
                // happens when its turn comes.
                sh.schedule_shutdown();
                return Ok(());
            }
            let ticket = sh.writes.ticket();
            let limit = sh.limit();
            sh.await_turn(cx, &ticket, limit, action)?;
            // Whether the writes before it were sent whole, cut short or failed, the writing end
            // is shut: it is the one way left to tell the peer that no more comes (a cut write
            // left a half message, which the peer should not wait on).
            sh.shut_write()
        });
        // The most bytes `read_line` takes before its end of line; read at each look of a call.
        d.method("set_line_limit", ["bytes"], |cx: &mut Ctx, this: &Conn, bytes: i64| {
            match usize::try_from(bytes) {
                Ok(n) if (1..=MAX_EXACT).contains(&n) => {
                    this.sh.line_limit.store(n, Ordering::SeqCst);
                }
                _ => cx.raise(format!(
                    "net.Conn.set_line_limit: {bytes} is not a line limit (use 1 to {MAX_EXACT} bytes)"
                )),
            }
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
    /// The wait cell that the accept armed with the poller is stored in (the read slot), so that
    /// `close` can resolve it.
    slots: Arc<Mutex<Slots>>,
    /// What the listener has accepted without any call having suspended: a loop of accepts that
    /// find a connection every time (a full backlog) gives the thread away like a loop of reads.
    burst: Burst,
    addr: String,
}

/// A listening socket, from `net.listen`.
pub(crate) struct Listener {
    sh: Arc<ListenShared>,
}

impl Waits for ListenShared {
    fn flag(&self) -> &AtomicBool {
        &self.closed
    }
    fn label(&self) -> &str {
        &self.addr
    }
    fn slot_table(&self) -> &Arc<Mutex<Slots>> {
        &self.slots
    }
    fn budget(&self) -> &Burst {
        &self.burst
    }
}

impl ListenShared {
    fn new(sock: TcpListener, addr: String) -> Arc<ListenShared> {
        Arc::new(ListenShared {
            sock: Handle::new(sock),
            closed: AtomicBool::new(false),
            accepts: Turns::new(),
            slots: Arc::new(Mutex::new(Slots::default())),
            burst: Burst::default(),
            addr,
        })
    }

    /// A call on this listener begins: after a burst of accepts that did not wait it gives the
    /// thread to the other coroutines once, before it takes its place (nothing is held then, so
    /// a cancel there loses nothing).
    fn begin_call(&self, cx: &mut Ctx) {
        if self.burst.begin_call() {
            give_way(cx);
        }
    }

    /// Accepts the next connection on the interpreter thread, behind the accepts called before
    /// it: the system call is made at once, and only when no connection is waiting does the call
    /// arm the listener with the poller and suspend. Nothing is held across a suspension (no
    /// reference to the listener), and between the system call that returns a connection and the
    /// return of this function nothing suspends, so a cancel can neither lose nor leak one.
    fn accept_call(
        self: &Arc<Self>,
        cx: &mut Ctx,
        ticket: Ticket,
    ) -> Result<TcpStream, TypedError> {
        let action = "accept on";
        self.wait_of(Limit::NONE).check_open(action)?;
        self.await_turn(cx, &ticket, Limit::NONE, action)?;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Err(closed_error(&self.addr, action));
            }
            let Some(listener) = self.sock.get() else {
                return Err(closed_error(&self.addr, action));
            };
            match listener.accept() {
                Ok((stream, _)) => {
                    drop(listener);
                    if let Err(e) = prepare(&stream) {
                        // The connection cannot be handed over (it would be a blocking socket
                        // that stalls the interpreter thread), so it is closed here,
                        // explicitly, and this accept reports why. The peer sees the end of
                        // the stream at once; the connection is not left half open.
                        let _ = stream.shutdown(Shutdown::Both);
                        drop(stream);
                        return Err(fail(action, &self.addr, &e));
                    }
                    return Ok(stream);
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        match self.wait_ready(
                            cx,
                            listener,
                            poll::Dir::Read,
                            Limit::NONE,
                            action,
                            None,
                        )? {
                            // "Try again": the listener says it may have a connection.
                            Ended::Ready | Ended::TimedOut => {}
                            Ended::Closed => return Err(closed_error(&self.addr, action)),
                        }
                    }
                    // A connection that was reset before it was accepted: the next one.
                    ErrorKind::Interrupted
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset => {}
                    _ => {
                        // Closing shuts the listener down first, which can make `accept` fail.
                        self.wait_of(Limit::NONE).check_open(action)?;
                        return Err(fail(action, &self.addr, &e));
                    }
                },
            }
        }
    }

    /// Stops listening: marks the listener closed, ends the accept that waits (its cell is
    /// resolved with `Closed`, and the poller's hold on the socket goes under its lock before
    /// this returns), and lets go of the socket. Never suspends, never fails. No operation holds
    /// a reference to the socket while it is suspended, so the descriptor is released, and the
    /// port free, when this returns (but for the instant of a system call that another thread
    /// is making: none, on the interpreter thread).
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let listener = self.sock.take();
        if let Some(listener) = &listener {
            // Ends the listening at once where the system lets a shutdown do that (Linux).
            let _ = SockRef::from(&**listener).shutdown(Shutdown::Read);
        }
        let cell = lock_slots(&self.slots).read.take();
        let panic = cell.and_then(|cell| poll::fire_all(vec![(cell, poll::Why::Closed)]));
        self.accepts.wake();
        drop(listener);
        if let Some(panic) = panic {
            std::panic::resume_unwind(panic);
        }
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
            sh.begin_call(cx);
            // As for a read: an accept that blocks the interpreter thread cannot wait for a
            // coroutine that holds its turn until it resumes.
            if !cx.can_suspend() && !sh.closed.load(Ordering::SeqCst) && sh.accepts.busy() {
                return Err(Kind::Other.error(format!(
                    "cannot accept on `{}`: an accept on this listener is already waiting in another coroutine, and it cannot finish while a `Drop` hook or a callback waits (accept in the coroutine, or before that accept starts)",
                    sh.addr
                )));
            }
            let ticket = sh.accepts.ticket();
            let stream = sh.accept_call(cx, ticket)?;
            Ok::<Conn, TypedError>(Conn::new(stream))
        });
        // Stops listening: the port is free again, and an `accept` that waits ends with
        // `Closed`. Connections already accepted are not touched; connections that still wait
        // in the system's backlog (nobody accepted them) are reset or see the end of the stream,
        // as the system does when a listening socket is closed. Closing twice is fine.
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
/// whole call (the lookup and every attempt); without one the system's own limit applies. It
/// runs on the interpreter thread: the lookup and each attempt suspend the coroutine, and where
/// nothing can suspend (a `Drop` hook, a callback) the call blocks the thread on the same
/// completions.
fn connect(cx: &mut Ctx, addr: String, timeout: Option<Duration>) -> Result<Conn, TypedError> {
    // Counted from the call.
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let site = ConnectSite::new(&addr);
    let addrs = lookup(cx, &site, &addr, deadline, timeout)?;
    let mut first = None;
    let count = addrs.len();
    for (i, a) in addrs.into_iter().enumerate() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(connect_timed_out(&addr, timeout));
        }
        // Each address gets a fair share of what is left, so that one that does not answer
        // cannot use the whole budget while the next one would have connected at once.
        let attempt = attempt_deadline(Instant::now(), deadline, count - i);
        let result = match start_connect(a, attempt) {
            Ok(Started::Connected(stream)) => Ok(stream),
            Ok(Started::Pending(sock)) => finish_connect(cx, &site, Arc::new(sock), attempt),
            Err(e) => Err(e),
        };
        match result {
            Ok(stream) => return Ok(Conn::new(stream)),
            Err(Attempt::TimedOut) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(connect_timed_out(&addr, timeout));
                }
                first = first.or(Some(io::Error::from(ErrorKind::TimedOut)));
            }
            Err(Attempt::Failed(e)) => return Err(e),
            Err(Attempt::Io(e)) => first = first.or(Some(e)),
        }
    }
    let e = first.expect("an address list is never empty");
    Err(fail("connect to", &addr, &e))
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
    use std::io::{Error, ErrorKind, Read};
    use std::sync::mpsc;

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
            let name = format!("{host}:80");
            let e = with_cx(move |cx| {
                let site = ConnectSite::new(&name);
                lookup(cx, &site, &name, None, None).unwrap_err()
            });
            assert_eq!(variant(&e), "Dns", "{host}");
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
        match line_probe(r, "peer", MAX_LINE) {
            Probe::Failed(e) => Err(e),
            Probe::Pending(_) => panic!("not ready"),
            Probe::Ready => take_line(r, "peer", MAX_LINE),
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
        assert!(matches!(
            line_probe(&mut r, "p", MAX_LINE),
            Probe::Pending(_)
        ));
        let mut r = reader(&line(MAX_LINE, b"y"), false);
        assert!(matches!(
            line_probe(&mut r, "p", MAX_LINE),
            Probe::Failed(_)
        ));
        let mut r = reader(&line(MAX_LINE + 1, b""), false);
        assert!(matches!(
            line_probe(&mut r, "p", MAX_LINE),
            Probe::Failed(_)
        ));
        // A last line without an end of line.
        let mut r = reader(&line(MAX_LINE, b""), true);
        assert_eq!(line_of(&mut r).unwrap().unwrap().len(), MAX_LINE);
        let mut r = reader(&line(MAX_LINE + 1, b""), true);
        assert!(line_of(&mut r).is_err());
    }

    // ---- `read_until` and the line limit ----

    /// What a `read_until` look gives on `r`: the bytes before the delimiter (`Some`, or `None`
    /// at the end), or the name of the error, or "pending".
    fn until(r: &mut Reader, delim: &[u8], max: usize) -> Result<Option<Vec<u8>>, String> {
        match delim_probe(r, delim, max, "peer") {
            Probe::Failed(e) => Err(variant(&e)),
            Probe::Pending(_) => Err("pending".to_string()),
            Probe::Ready => Ok(take_until(r, delim, max, "peer").unwrap().map(|b| b.0)),
        }
    }

    /// A delimiter of one or more bytes is found wherever the stream is cut into arrivals: the
    /// pieces come one, two, three bytes at a time and whole, and the bytes after a delimiter are
    /// the start of the next call. The cases have delimiters that begin inside themselves and
    /// inside the data (`aab` in `xaaab`), a `\r` before `\r\n`, and a `\n` inside the data.
    #[test]
    fn a_delimiter_is_found_however_the_stream_is_cut_into_reads() {
        // The delimiter, the stream, and the pieces that calls one after the other give.
        type Case = (&'static [u8], &'static [u8], &'static [&'static [u8]]);
        let cases: [Case; 8] = [
            (b"\r\n", b"a\nb\r\n\r\nc", &[b"a\nb", b""]),
            (b"\r\n", b"a\r\r\nbb\r\n", &[b"a\r", b"bb"]),
            (b"\n", b"ab\ncd\n", &[b"ab", b"cd"]),
            (b"--==", b"x-y--=-=--==z--==", &[b"x-y--=-=", b"z"]),
            (b"aab", b"xaaabaab", &[b"xa", b""]),
            (b"ab", b"aab", &[b"a"]),
            (b"\x00", b"\x00\x00a\x00", &[b"", b"", b"a"]),
            (b"abcabd", b"abcabcabdz", &[b"abc"]),
        ];
        for (delim, stream, expected) in cases {
            for piece in [1, 2, 3, stream.len()] {
                let mut r = reader(b"", false);
                let mut got = Vec::new();
                for chunk in stream.chunks(piece) {
                    r.data.extend_from_slice(chunk);
                    // Every call that can end does, before the next bytes arrive.
                    while got.len() < expected.len() {
                        match until(&mut r, delim, 100) {
                            Ok(Some(v)) => got.push(v),
                            Err(e) if e == "pending" => break,
                            other => panic!("{delim:?} {stream:?} {piece}: {other:?}"),
                        }
                    }
                }
                let expected: Vec<Vec<u8>> = expected.iter().map(|e| e.to_vec()).collect();
                assert_eq!(got, expected, "{delim:?} in {stream:?}, {piece} at a time");
            }
        }
    }

    /// A look scans the bytes that came since the last one, backing up by the length of the
    /// delimiter less one: the part already searched is not searched again, so a stream that
    /// arrives a byte at a time costs a linear amount and not a quadratic one. What the last look
    /// covered is `delim_clear` (the number of bytes after the first unread one that hold no
    /// start of the delimiter), and the next look begins there.
    #[test]
    fn a_look_for_a_delimiter_covers_only_the_new_bytes() {
        for delim in [&b"\n"[..], b"\r\n", b"abcd"] {
            let mut r = reader(b"", false);
            for n in 1..=3000usize {
                r.data.push(b'x');
                assert!(until(&mut r, delim, 1 << 20).is_err());
                assert_eq!(
                    r.delim_clear,
                    n.saturating_sub(delim.len() - 1),
                    "{delim:?} after {n} bytes: the next look would start earlier"
                );
            }
            // Taking bytes starts the count again (the positions are relative to the first unread
            // byte), and so does another delimiter (the bytes were searched for the first).
            r.take(10);
            assert_eq!(r.delim_clear, 0);
            r.data.push(b'x');
            assert!(until(&mut r, delim, 1 << 20).is_err());
            assert!(r.delim_clear > 0);
            r.delim_clear = 5;
            let _ = until(&mut r, b"zz", 1 << 20);
            assert_eq!(r.delim, b"zz");
            assert_eq!(r.delim_clear, r.available().len() - 1);
        }
    }

    /// What one look for a delimiter covered says nothing to a look for another delimiter, nor to
    /// a `read_line` (its `scanned` is for `\n` alone): a `read_until` that is cancelled and
    /// followed by a `read_until` of another delimiter, or by a `read_line`, or the other way
    /// round, finds what is there.
    #[test]
    fn what_one_look_covered_says_nothing_to_another_kind_of_call() {
        let mut r = reader(b"aaaaaab", false);
        assert!(until(&mut r, b"zz", 100).is_err());
        // `ab` starts at 5: a look that took the cleared bytes of the search for `zz` (6 of them)
        // as its own would start after it.
        assert_eq!(until(&mut r, b"ab", 100), Ok(Some(b"aaaaa".to_vec())));
        let mut r = reader(b"abc\r", false);
        assert!(matches!(
            line_probe(&mut r, "p", MAX_LINE),
            Probe::Pending(_)
        ));
        assert_eq!(r.scanned, 4);
        assert_eq!(until(&mut r, b"c", 100), Ok(Some(b"ab".to_vec())));
        let mut r = reader(b"abc\r\nd", false);
        assert!(until(&mut r, b"zz", 100).is_err());
        assert_eq!(line_of(&mut r).unwrap(), Some("abc".to_string()));
    }

    /// `max` counts the bytes before the delimiter. With a delimiter of one byte the call fails
    /// when the byte after `max` bytes is not the delimiter (`max + 1` bytes and no delimiter),
    /// not at `max`; a delimiter of two bytes whose first byte is the byte after `max` waits for
    /// its second (the delimiter straddles the limit and the content is `max` bytes).
    #[test]
    fn read_until_fails_at_one_byte_more_than_max_and_not_at_max() {
        const MAX: usize = 16;
        let x = |n: usize, tail: &[u8]| {
            let mut v = vec![b'x'; n];
            v.extend_from_slice(tail);
            v
        };
        let look = |data: &[u8], delim: &[u8], eof: bool| {
            let mut r = reader(data, eof);
            let got = until(&mut r, delim, MAX);
            let kept = r.available().len();
            (got, kept)
        };
        let content = Ok(Some(vec![b'x'; MAX]));
        let pending = Err("pending".to_string());
        let long = Err("LineTooLong".to_string());
        for delim in [&b"\n"[..], b"\r\n", b"--"] {
            // The delimiter right after `max` bytes: fine. (`max - 1` too.)
            assert_eq!(look(&x(MAX, delim), delim, false).0, content, "{delim:?}");
            assert_eq!(
                look(&x(MAX - 1, delim), delim, false).0,
                Ok(Some(vec![b'x'; MAX - 1]))
            );
            // `max` bytes and nothing more yet: it may still come.
            assert_eq!(look(&x(MAX, b""), delim, false).0, pending, "{delim:?}");
            // One byte more, and it is not the delimiter (nor the start of one): too long, and the
            // bytes stay.
            assert_eq!(
                look(&x(MAX + 1, b""), delim, false),
                (long.clone(), MAX + 1)
            );
            // The delimiter after `max + 1` bytes starts too late.
            assert_eq!(look(&x(MAX + 1, delim), delim, false).0, long, "{delim:?}");
        }
        // A delimiter that straddles the limit: its first byte is the byte after `max` bytes. It
        // waits for the rest, completes, and the content is `max` bytes.
        assert_eq!(look(&x(MAX, b"\r"), b"\r\n", false).0, pending);
        assert_eq!(look(&x(MAX, b"\r\n"), b"\r\n", false).0, content);
        assert_eq!(look(&x(MAX, b"-"), b"--", false).0, pending);
        // ... and fails when what follows is not the rest of it.
        assert_eq!(
            look(&x(MAX, b"\rx"), b"\r\n", false),
            (long.clone(), MAX + 2)
        );
        assert_eq!(look(&x(MAX, b"-x"), b"--", false).0, long);
        // A delimiter of three bytes, cut after one and after two.
        assert_eq!(look(&x(MAX, b"ab"), b"abc", false).0, pending);
        assert_eq!(look(&x(MAX - 1, b"xab"), b"abc", false).0, pending);
        assert_eq!(look(&x(MAX, b"abx"), b"abc", false).0, long);
        // A start that is not at the end of the bytes (`xab` ... a start at `max`) and one past it.
        assert_eq!(look(&x(MAX, b"abc"), b"abc", false).0, content);
        assert_eq!(look(&x(MAX + 1, b"abc"), b"abc", false).0, long);
        // Nothing before the delimiter, and a `max` of 0.
        let mut r = reader(b"\r\n", false);
        assert_eq!(until(&mut r, b"\r\n", 0), Ok(Some(vec![])));
        let mut r = reader(b"x", false);
        assert_eq!(until(&mut r, b"\r\n", 0).unwrap_err(), "LineTooLong");
        let mut r = reader(b"\r", false);
        assert_eq!(until(&mut r, b"\r\n", 0).unwrap_err(), "pending");
        // The bytes that arrive later are looked at again from the start of the unread part:
        // too long when a later arrival shows it.
        let mut r = reader(&x(MAX, b"\r"), false);
        assert_eq!(until(&mut r, b"\r\n", MAX).unwrap_err(), "pending");
        r.data.push(b'y');
        assert_eq!(until(&mut r, b"\r\n", MAX).unwrap_err(), "LineTooLong");
        assert_eq!(r.available().len(), MAX + 2);
    }

    /// At the end of the stream: nothing pending is `None`; bytes pending are `UnexpectedEnd` (and
    /// stay, for `read`), unless there are more than `max` of them, which is `LineTooLong` as it
    /// is before the end.
    #[test]
    fn read_until_at_the_end_of_the_stream() {
        let mut r = reader(b"", true);
        assert_eq!(until(&mut r, b"\r\n", 16), Ok(None));
        let mut r = reader(b"abc\r", true);
        assert_eq!(until(&mut r, b"\r\n", 16).unwrap_err(), "UnexpectedEnd");
        assert_eq!(r.available(), b"abc\r");
        // Again: the same, the bytes are still there.
        assert_eq!(until(&mut r, b"\r\n", 16).unwrap_err(), "UnexpectedEnd");
        // Exactly `max` bytes pending: not too long. One more: too long.
        let mut r = reader(&[b'x'; 16], true);
        assert_eq!(until(&mut r, b"\r\n", 16).unwrap_err(), "UnexpectedEnd");
        let mut r = reader(&[b'x'; 17], true);
        assert_eq!(until(&mut r, b"\r\n", 16).unwrap_err(), "LineTooLong");
        assert_eq!(r.available().len(), 17);
        // A delimiter that was there comes before the end.
        let mut r = reader(b"ab\r\ncd", true);
        assert_eq!(until(&mut r, b"\r\n", 16), Ok(Some(b"ab".to_vec())));
        assert_eq!(until(&mut r, b"\r\n", 16).unwrap_err(), "UnexpectedEnd");
        assert_eq!(r.available(), b"cd");
    }

    /// The limit of `read_line` is a parameter of the look: a line of `limit` bytes is fine with
    /// either end of line, one more byte is too long, and a limit that is changed between two
    /// looks of one read counts at the second look.
    #[test]
    fn the_line_limit_is_the_one_given_at_each_look() {
        let line = |n: usize, end: &[u8]| {
            let mut v = vec![b'x'; n];
            v.extend_from_slice(end);
            v
        };
        for end in [&b"\n"[..], b"\r\n"] {
            let mut r = reader(&line(16, end), false);
            assert_eq!(
                take_line(&mut r, "p", 16).unwrap().unwrap().len(),
                16,
                "{end:?}"
            );
            let mut r = reader(&line(17, end), false);
            assert!(matches!(line_probe(&mut r, "p", 16), Probe::Failed(_)));
            assert_eq!(r.available().len(), 17 + end.len());
        }
        let mut r = reader(&line(16, b""), false);
        assert!(matches!(line_probe(&mut r, "p", 16), Probe::Pending(_)));
        assert!(matches!(line_probe(&mut r, "p", 15), Probe::Failed(_)));
        let mut r = reader(&line(16, b""), false);
        assert!(matches!(line_probe(&mut r, "p", 16), Probe::Pending(_)));
        r.data.extend_from_slice(&line(30, b"\n"));
        assert!(matches!(line_probe(&mut r, "p", 100), Probe::Ready));
        assert!(matches!(line_probe(&mut r, "p", 16), Probe::Failed(_)));
    }

    /// `read_line` of a line that is not text takes the line off the stream, and the line after
    /// it is still there.
    #[test]
    fn a_line_that_is_not_text_is_taken_off_the_stream() {
        let mut r = reader(b"\xff\xfe\nok\n", false);
        let e = take_line(&mut r, "peer", MAX_LINE).unwrap_err();
        assert_eq!(variant(&e), "InvalidData");
        assert!(
            e.message().contains("taken off the stream"),
            "{}",
            e.message()
        );
        assert_eq!(
            take_line(&mut r, "peer", MAX_LINE).unwrap().as_deref(),
            Some("ok")
        );
    }

    /// A connect whose deadline has passed ends at once with the right reason and does not touch
    /// the network at all (the address is a loopback one with a listener that must see nothing).
    /// Runs on every system: this is what covers bounding a connect where a connect cannot be
    /// kept pending on loopback. (A connect whose coroutine is cancelled never runs again: the
    /// cancel is seen where the call suspends, see `a_cancelled_connect_leaves_no_socket`.)
    #[test]
    fn a_connect_that_is_late_stops_before_it_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();

        let started = Instant::now();
        let past = Instant::now();
        let r = start_connect(addr, Some(past));
        assert!(matches!(r, Err(Attempt::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(5));

        // Nobody connected.
        let e = listener.accept().expect_err("no connection was made");
        assert_eq!(e.kind(), ErrorKind::WouldBlock);

        // And with time left, the same call does connect.
        let r = start_connect(addr, Some(Instant::now() + Duration::from_secs(60)));
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

    /// A listener whose accept queue is full (smallest backlog; nobody accepts) and the
    /// connections that filled it, so that a connect to it stays under way: the SYN is dropped
    /// and retried. Linux and macOS; Windows answers an overflowing backlog with a reset.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn full_listener() -> (Socket, SocketAddr, Vec<TcpStream>) {
        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        sock.bind(&SockAddr::from(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        ))
        .unwrap();
        sock.listen(1).unwrap();
        let addr = sock.local_addr().unwrap().as_socket().unwrap();
        let mut clients = Vec::new();
        loop {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
                Ok(c) => clients.push(c),
                Err(e) => {
                    assert_eq!(e.kind(), ErrorKind::TimedOut, "the queue did not fill up");
                    break;
                }
            }
            assert!(clients.len() < 400, "the accept queue does not fill up");
        }
        (sock, addr, clients)
    }

    /// What a connect can be left under way against, and what keeps it so. Linux and macOS: a
    /// listener whose accept queue is full (the SYN is dropped and retried). Windows answers an
    /// overflowing backlog with a reset, but keeps a connect that is refused pending for about
    /// two seconds, so there it is a port that nothing is bound to (bound and let go of).
    #[cfg(windows)]
    fn pending_target() -> (SocketAddr, ()) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        (l.local_addr().unwrap(), ())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn pending_target() -> (SocketAddr, (Socket, Vec<TcpStream>)) {
        let (listener, addr, clients) = full_listener();
        (addr, (listener, clients))
    }

    /// A connect that is cancelled while it waits for the connection leaves nothing behind: the
    /// poller's entry is gone, and nothing holds the socket once the call has been left, so no
    /// connect goes on in the background. The hook cancels the wait where it was just armed (the
    /// connection cannot finish yet). Where the call is cancelled for real, the coroutine
    /// unwinds and drops the same things. On Windows the connect is one to a port that nothing
    /// is bound to, which that system keeps pending for about two seconds: if it is refused at
    /// once instead, the test says that its precondition did not hold.
    #[test]
    fn a_cancelled_connect_leaves_no_socket() {
        let (addr, _keep) = pending_target();
        let (error, weak_dead, counts, hook_ran) = with_cx(move |cx| {
            let site = ConnectSite::new("test");
            let sock = match start_connect(addr, None) {
                Ok(Started::Pending(sock)) => sock,
                Ok(Started::Connected(_)) => {
                    panic!("precondition: the connect finished at once, it was not left under way")
                }
                Err(Attempt::Io(e)) => {
                    panic!("precondition: the connect was refused at once, not left under way: {e}")
                }
                Err(_) => panic!("precondition: the connect did not start"),
            };
            let sock = Arc::new(sock);
            let weak = Arc::downgrade(&sock);
            let ran = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&ran);
            arm_probe::set_hook(move |cell| {
                flag.store(true, Ordering::SeqCst);
                cell.fire(poll::Why::Cancelled);
            });
            let error = match finish_connect(cx, &site, sock, None) {
                Err(Attempt::Failed(e)) => e.message().to_string(),
                Err(Attempt::Io(e)) => panic!(
                    "precondition: the connect ended with an error before the wait began: {e}"
                ),
                Err(Attempt::TimedOut) => panic!("another end"),
                Ok(_) => panic!("connected"),
            };
            let poller = cx
                .state_mut::<poll::NetState>()
                .unwrap()
                .poller()
                .expect("the poller started");
            (
                error,
                weak.strong_count() == 0,
                poller.counts(),
                ran.load(Ordering::SeqCst),
            )
        });
        assert!(hook_ran, "the wait was never armed: nothing was cancelled");
        assert!(error.contains("cancelled"), "{error}");
        assert!(weak_dead, "the socket was still held");
        assert_eq!(counts, (0, 0));
    }

    /// A connect that is cancelled while its name is being looked up ends at once, leaves nothing
    /// armed (the timer of its deadline is gone from the poller), and the late answer of the
    /// helper thread is dropped harmlessly: the helper finishes, resolves a cell that is
    /// resolved already, and nothing is connected to. The hook cancels where the lookup was
    /// armed, and the name is held (no resolver, no network) until after the cancel; then the
    /// test lets the helper go and waits for it to report that it is done, with a ceiling.
    #[test]
    fn a_connect_cancelled_during_its_lookup_ends_and_the_late_answer_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let name = "held.lookup.test";
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        lookup_probe::hold(name, gate, listener.local_addr().unwrap(), done);
        let (error, counts, hook_ran) = with_cx(move |cx| {
            let ran = std::rc::Rc::new(std::cell::Cell::new(false));
            let flag = std::rc::Rc::clone(&ran);
            arm_probe::set_hook(move |cell| {
                flag.set(true);
                cell.fire(poll::Why::Cancelled);
            });
            let error = match connect(cx, format!("{name}:{port}"), Some(Duration::from_secs(60))) {
                Err(e) => e.message().to_string(),
                Ok(_) => panic!("connected"),
            };
            let poller = cx
                .state_mut::<poll::NetState>()
                .unwrap()
                .poller()
                .expect("the poller started");
            (error, poller.counts(), ran.get())
        });
        assert!(hook_ran, "the lookup was never armed");
        assert!(error.contains("cancelled"), "{error}");
        assert_eq!(counts, (0, 0), "something is left armed");
        // The helper has not answered yet (it is held): let it, and wait until it has finished.
        assert!(finished.try_recv().is_err(), "the helper was not held");
        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(30))
            .expect("the helper thread did not finish its late answer");
        // Nothing connected to the listener.
        assert!(listener.accept().is_err(), "a connection was made");
    }

    /// A wake proves nothing by itself: a connect that is under way (Linux and macOS: the queue of
    /// the listener is full; Windows: a port that nothing is bound to, which that system keeps
    /// pending for about two seconds) is not concluded on the one-second recheck, which is a timer
    /// and no event, nor on a "ready" wake that finds it neither failed nor connected (forced by
    /// the hook): the wait is armed again. Each of the two wakes is followed by a second arm, where
    /// the hook cancels. After the forced "ready" the second arm comes no sooner than
    /// `CONNECT_PAUSE`, measured between the two arms: a lower bound that a timer cannot break, so
    /// the test cannot fail for being slow, only for a missing pause. A connect that ends before
    /// the second arm fails the precondition, and the test says so.
    #[test]
    fn a_wake_that_finds_the_connect_neither_failed_nor_done_arms_again() {
        for forced_ready in [false, true] {
            let (addr, _keep) = pending_target();
            let (arms, error, gap) = with_cx(move |cx| {
                let site = ConnectSite::new("test");
                let sock = match start_connect(addr, None) {
                    Ok(Started::Pending(sock)) => sock,
                    Ok(Started::Connected(_)) => {
                        panic!(
                            "precondition: the connect finished at once, it was not left under way"
                        )
                    }
                    Err(Attempt::Io(e)) => panic!(
                        "precondition: the connect was refused at once, not left under way: {e}"
                    ),
                    Err(_) => panic!("precondition: the connect did not start"),
                };
                let arms = std::rc::Rc::new(std::cell::Cell::new(0));
                let first = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
                let second = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
                let (counted, t1, t2) = (
                    std::rc::Rc::clone(&arms),
                    std::rc::Rc::clone(&first),
                    std::rc::Rc::clone(&second),
                );
                arm_probe::set_hook_for_every_arm(move |cell| {
                    counted.set(counted.get() + 1);
                    if counted.get() == 1 {
                        t1.set(Some(Instant::now()));
                        // The first wake: a "ready" (forced), or none (the recheck comes).
                        if forced_ready {
                            cell.fire(poll::Why::Ready);
                        }
                    } else {
                        if counted.get() == 2 {
                            t2.set(Some(Instant::now()));
                        }
                        cell.fire(poll::Why::Cancelled);
                    }
                });
                let error = match finish_connect(cx, &site, Arc::new(sock), None) {
                    Err(Attempt::Failed(e)) => e.message().to_string(),
                    Err(Attempt::Io(e)) => panic!(
                        "precondition: the connect ended before the second arm (forced_ready \
                         {forced_ready}): {e}"
                    ),
                    Err(Attempt::TimedOut) => panic!("another end"),
                    Ok(_) => panic!("concluded that the connect was done"),
                };
                let gap = match (first.get(), second.get()) {
                    (Some(a), Some(b)) => b.duration_since(a),
                    _ => Duration::ZERO,
                };
                (arms.get(), error, gap)
            });
            assert_eq!(arms, 2, "forced_ready {forced_ready}");
            assert!(error.contains("cancelled"), "{error}");
            if forced_ready {
                assert!(
                    gap >= CONNECT_PAUSE,
                    "the second arm came {gap:?} after the first, before the pause of \
                     {CONNECT_PAUSE:?} was over"
                );
            }
        }
    }

    /// A connect that was under way (the queue of the listener was full, so its SYN was dropped)
    /// finishes through the wait when the queue is emptied: the system sends the SYN again, the
    /// socket becomes writable (or the one-second look at `SO_ERROR` finds it connected) and the
    /// call returns the connected stream, which the listener accepts. The test waits for that
    /// event, not for a time.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_connect_that_was_under_way_finishes_when_the_queue_drains() {
        let (listener, addr, clients) = full_listener();
        let stream = with_cx(move |cx| {
            let site = ConnectSite::new("test");
            let Ok(Started::Pending(sock)) = start_connect(addr, None) else {
                panic!("the connect to a full queue was not left under way");
            };
            // Emptied after the wait has begun: on this thread, from the hook that runs where
            // the wait was armed.
            let listener = Arc::new(listener);
            let draining = Arc::clone(&listener);
            arm_probe::set_hook(move |_| {
                draining.set_nonblocking(true).unwrap();
                while draining.accept().is_ok() {}
                drop(clients);
            });
            let stream = finish_connect(cx, &site, Arc::new(sock), None)
                .unwrap_or_else(|_| panic!("the connect did not finish"));
            (stream, listener)
        });
        assert!(stream.0.peer_addr().is_ok());
    }

    /// A connect that is waited for ends when the connection is made, and the stream it gives is
    /// the connected one (the listener sees it).
    #[test]
    fn a_pending_connect_is_finished_by_the_wait() {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        l.set_nonblocking(false).unwrap();
        let addr = l.local_addr().unwrap();
        let stream = with_cx(move |cx| {
            let site = ConnectSite::new("test");
            match start_connect(addr, Some(Instant::now() + Duration::from_secs(60))) {
                Ok(Started::Connected(s)) => s,
                Ok(Started::Pending(sock)) => finish_connect(cx, &site, Arc::new(sock), None)
                    .unwrap_or_else(|_| panic!("a connect that waits")),
                Err(_) => panic!("the connect failed"),
            }
        });
        let (accepted, _) = l.accept().unwrap();
        assert_eq!(accepted.peer_addr().unwrap(), stream.local_addr().unwrap());
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

    /// A ticket that parks a cell: the turn comes, a close, a cancel and a deadline each
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

    /// The size of the buffers of `conn_pair_fixed`.
    const BUFFER: usize = 64 * 1024;

    /// `conn_pair` with buffers of a fixed size, so that the system's automatic sizing cannot
    /// make room in a full pipe later on (macOS does, on loopback): the peer's receive buffer is
    /// set before the connection, the connection's send buffer after it.
    fn conn_pair_fixed() -> (Conn, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        peer.set_recv_buffer_size(BUFFER).unwrap();
        peer.connect(&SockAddr::from(l.local_addr().unwrap()))
            .unwrap();
        let peer = TcpStream::from(peer);
        let (s, _) = l.accept().unwrap();
        SockRef::from(&s).set_send_buffer_size(BUFFER).unwrap();
        prepare(&s).unwrap();
        (Conn::new(s), peer)
    }

    /// Writes raw into `sock` until it would block.
    fn fill_raw(sock: &TcpStream) {
        let chunk = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            match (&*sock).write(&chunk) {
                Ok(n) => total += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) => panic!("filling the send buffer: {e}"),
            }
            assert!(total < 1 << 30, "the send buffer never filled");
        }
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
        // Pinned to the tree-walker: this tests the standard library, not the engine, and host
        // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
        let mut vm = Vm::builder()
            .engine(colibri_runtime::EngineKind::Tree)
            .stdout(Box::new(out))
            .allow(Cap::Net)
            .build();
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
                        |r| line_probe(r, "p", MAX_LINE),
                        |r| take_line(r, "p", MAX_LINE),
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

    /// An accept takes a connection that arrived before it was called without suspending, and
    /// one that arrives while it waits, in the order they came; the waiting one gets its
    /// connection and not another.
    #[test]
    fn an_accept_takes_the_connections_in_order_whether_it_waits_or_not() {
        let (sh, addr) = listen_shared();
        let first = TcpStream::connect(addr).unwrap();
        let first_addr = first.local_addr().unwrap();
        // Connected before the accept is called, and the loopback handshake is over once
        // `connect` has returned: the accept finds it, or (on macOS, a moment later) waits for it.
        let got = with_cx(move |cx| {
            let one = sh.accept_call(cx, sh.accepts.ticket()).unwrap();
            let poller = cx
                .state_mut::<poll::NetState>()
                .unwrap()
                .poller()
                .expect("the poller starts");
            let second = std::thread::spawn(move || {
                // Not before the second accept waits (armed, and holding no reference of its own).
                let began = Instant::now();
                while poller.counts() != (1, 1) {
                    assert!(began.elapsed() < Duration::from_secs(30), "never waited");
                    std::thread::yield_now();
                }
                let c = TcpStream::connect(addr).unwrap();
                (c.local_addr().unwrap(), c)
            });
            let two = sh.accept_call(cx, sh.accepts.ticket()).unwrap();
            let (second_addr, _client) = second.join().unwrap();
            (
                one.peer_addr().unwrap(),
                two.peer_addr().unwrap(),
                second_addr,
            )
        });
        assert_eq!(got.0, first_addr);
        assert_eq!(got.1, got.2);
        drop(first);
    }

    /// `close` of a listener that an accept waits on ends the accept with `Closed`, and the
    /// descriptor is released by the time `close` returns: nothing holds the listener while the
    /// coroutine is suspended, and the poller's hold goes under its lock. (The closer runs on
    /// another thread here, which the product never does; it waits until the accept has armed
    /// and let go of its own reference.)
    #[test]
    fn close_releases_the_listener_of_an_accept_that_waits() {
        for _ in 0..20 {
            let (sh, _addr) = listen_shared();
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
                        let began = Instant::now();
                        while poller.counts() != (1, 1) || weak.strong_count() != 2 {
                            assert!(
                                began.elapsed() < Duration::from_secs(30),
                                "the accept never waited"
                            );
                            std::thread::yield_now();
                        }
                        sh.close();
                        weak.strong_count() == 0
                    })
                };
                let error = sh
                    .accept_call(cx, sh.accepts.ticket())
                    .map(|_| ())
                    .map_err(|e| variant(&e));
                (error, closer.join().unwrap())
            });
            assert_eq!(error, Err("Closed".to_string()));
            assert!(released, "the listener was still held when close returned");
        }
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

    // SAFETY: writes the first `n` bytes of the room before it returns `n`.
    unsafe impl Recv for Pieces {
        fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
            let n = self.0.min(buf.len());
            for b in &mut buf[..n] {
                b.write(0xAB);
            }
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

    /// The room a read is handed is never written before the system call: the test fills the
    /// capacity of the buffer with a pattern, and the source checks that every byte it is given
    /// still has it (a zero-fill of the room, for each read or for each wake, would show) and
    /// writes only the bytes that arrive. A peer that sends single bytes thus costs a copy of one
    /// byte per read, and the bytes after the data are as they were at the end.
    #[test]
    fn a_trickle_of_single_bytes_zero_fills_nothing() {
        /// `(piece size, start of the filled capacity, its end)`.
        struct Checked(usize, usize, usize);
        // SAFETY: writes the first `n` bytes of the room before it returns `n`.
        unsafe impl Recv for Checked {
            fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
                // The room must lie in the region the test filled, or it is not read at all.
                let (from, to) = (buf.as_ptr() as usize, buf.as_ptr() as usize + buf.len());
                assert!(
                    from >= self.1 && to <= self.2,
                    "the room lies outside the capacity the test filled: the buffer moved"
                );
                for b in buf.iter() {
                    // SAFETY: the test wrote the pattern to all of the capacity, and the room
                    // is inside it (checked above).
                    assert_eq!(
                        unsafe { b.assume_init() },
                        0xCD,
                        "the room was written before the read"
                    );
                }
                let n = self.0.min(buf.len());
                for b in &mut buf[..n] {
                    b.write(0xAB);
                }
                Ok(n)
            }
        }
        for want in [4 * GROW_STEP, 0] {
            let mut r = reader(b"", false);
            r.data.reserve_exact(8 * GROW_STEP);
            let capacity = r.data.capacity();
            for b in r.data.spare_capacity_mut() {
                b.write(0xCD);
            }
            let start = r.data.as_ptr() as usize;
            let mut src = Checked(1, start, start + capacity);
            for i in 1..=5000 {
                let more = if want == 0 { 0 } else { want - (i - 1) };
                r.read_from(&mut src, more).unwrap().unwrap();
                assert_eq!(r.data.len(), i);
            }
            assert_eq!(
                r.data.capacity(),
                capacity,
                "want {want}: it was reallocated"
            );
            assert!(r.data.iter().all(|&b| b == 0xAB));
            for b in &r.data.spare_capacity_mut()[..READ_STEP] {
                // SAFETY: as above.
                assert_eq!(unsafe { b.assume_init() }, 0xCD, "want {want}");
            }
        }
    }

    /// A connection that has read holds no buffer for reading while it is idle: after a read has
    /// returned (here a line) the `Reader` has none, and a second read makes it again and lets
    /// it go again. A call that fails or is unwound lets go of what it does not need too; the
    /// half line that is left is kept.
    #[test]
    fn a_reader_holds_no_buffer_after_a_read_has_returned() {
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
                        |r| line_probe(r, "p", MAX_LINE),
                        |r| take_line(r, "p", MAX_LINE),
                    )
                    .unwrap()
            });
            assert_eq!(line.as_deref(), Some(expected));
            assert_eq!(
                sh.reader.lock().unwrap().data.capacity(),
                0,
                "after {expected}"
            );
        }
        // A call that fails (here: a read that times out half way through a line) keeps the
        // half line for the next read.
        peer.write_all(b"three").unwrap();
        let shared = Arc::clone(&sh);
        let failed = with_cx(move |cx| {
            // The five bytes are in the buffer before the call that times out is made (a call
            // that waits for them and takes none), so the result does not depend on how fast
            // the system delivers them.
            let ticket = shared.reads.ticket();
            shared
                .read_call(
                    cx,
                    ticket,
                    close_test_limit(),
                    |r| match r.available().len() {
                        5.. => Probe::Ready,
                        have => Probe::Pending(5 - have),
                    },
                    |r| Ok(r.take(0)),
                )
                .unwrap();
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
                    |r| line_probe(r, "p", MAX_LINE),
                    |r| take_line(r, "p", MAX_LINE),
                )
                .map(|_| ())
                .map_err(|e| variant(&e))
        });
        assert_eq!(failed, Err("Timeout".to_string()));
        assert_eq!(sh.reader.lock().unwrap().available(), b"three");
    }

    /// A read that waits holds no room for reading: while its wait is armed (the hook runs where
    /// it was just armed, and cancels it) the buffer of the connection has no capacity, which is
    /// neither resident nor virtual memory. A thousand waiting reads held 64 MB, and 10,000 held
    /// 640 MB, when each kept a buffer across its wait.
    #[test]
    fn a_waiting_read_holds_no_buffer() {
        let (c, _peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let (held, ran) = with_cx(move |cx| {
            let seen = Arc::new(Mutex::new(None));
            let probe = Arc::clone(&seen);
            let shared = Arc::clone(&sh);
            arm_probe::set_hook(move |cell| {
                *probe.lock().unwrap() = Some(shared.reader.lock().unwrap().data.capacity());
                cell.fire(poll::Why::Cancelled);
            });
            let ticket = sh.reads.ticket();
            let _ = sh.read_call(
                cx,
                ticket,
                Limit::NONE,
                |r| line_probe(r, "p", MAX_LINE),
                |r| take_line(r, "p", MAX_LINE),
            );
            let seen = *seen.lock().unwrap();
            (seen, sh.reader.lock().unwrap().data.capacity())
        });
        assert_eq!(held, Some(0), "the read kept a buffer across its wait");
        assert_eq!(ran, 0);
    }

    /// A `read_until` call on `sh`, behind its turn, with the probe and take of the module.
    fn until_call(
        cx: &mut Ctx,
        sh: &Arc<Shared>,
        delim: &'static [u8],
        max: usize,
        limit: Limit,
    ) -> Result<Option<Bytes>, TypedError> {
        let ticket = sh.reads.ticket();
        sh.read_call(
            cx,
            ticket,
            limit,
            move |r| delim_probe(r, delim, max, "p"),
            move |r| take_until(r, delim, max, "p"),
        )
    }

    /// A `read_until` that waits with nothing buffered holds no buffer (like `read_line`); one
    /// that waits with the start of a match buffered keeps its bytes, and with them the room they
    /// have, and a cancel of that wait leaves them for the next call: the call after it, with the
    /// rest of the delimiter, gives all of it. (The hook runs where the wait was just armed and
    /// cancels it.)
    #[test]
    fn a_read_until_that_waits_keeps_what_it_holds_and_a_cancel_loses_nothing() {
        let (c, mut peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let (empty, partial, left, after) = with_cx(move |cx| {
            let seen = Arc::new(Mutex::new(None));
            let probe = Arc::clone(&seen);
            let shared = Arc::clone(&sh);
            arm_probe::set_hook(move |cell| {
                *probe.lock().unwrap() = Some(shared.reader.lock().unwrap().data.capacity());
                cell.fire(poll::Why::Cancelled);
            });
            let first = until_call(cx, &sh, b"\r\n", 100, Limit::NONE);
            assert!(first.is_err(), "the wait was cancelled");
            let empty = seen.lock().unwrap().take();
            // Half a delimiter arrives; the read waits again with it buffered. The order is the
            // slowest one a system can have, and the test makes it so: the call finds nothing and
            // waits (the first arm), the bytes are sent only then, the wake reads them, they are
            // half a match, and the read waits again (the second arm), which is where the buffer
            // is looked at. Whether the system delivers before the first try or after it decides
            // nothing.
            let shared = Arc::clone(&sh);
            let probe = Arc::clone(&seen);
            let mut sender = peer.try_clone().unwrap();
            let mut arms = 0;
            arm_probe::set_hook_for_every_arm(move |cell| {
                arms += 1;
                if arms == 1 {
                    assert!(shared.reader.lock().unwrap().available().is_empty());
                    sender.write_all(b"ab\r").unwrap();
                    return;
                }
                let r = shared.reader.lock().unwrap();
                *probe.lock().unwrap() = Some(r.data.capacity());
                assert_eq!(r.available(), b"ab\r", "the bytes were let go of");
                drop(r);
                cell.fire(poll::Why::Cancelled);
            });
            let second = until_call(cx, &sh, b"\r\n", 100, close_test_limit());
            arm_probe::set_hook_for_every_arm(|_| {});
            assert!(second.is_err(), "the wait was cancelled");
            let partial = seen.lock().unwrap().take();
            let left = sh.reader.lock().unwrap().available().to_vec();
            peer.write_all(b"\nrest").unwrap();
            let after = until_call(cx, &sh, b"\r\n", 100, Limit::NONE)
                .map_err(|e| variant(&e))
                .map(|b| b.map(|b| b.0));
            (empty, partial, left, after)
        });
        assert_eq!(empty, Some(0), "the read kept a buffer across its wait");
        assert!(partial.unwrap() > 0, "the half match has no room");
        assert_eq!(left, b"ab\r");
        assert_eq!(after, Ok(Some(b"ab".to_vec())));
        assert_eq!(c.sh.reader.lock().unwrap().available(), b"rest");
    }

    /// A delimiter that is buffered is taken without a wait: the call arms nothing (the hook for
    /// every arm counts them), and the bytes after it are for the next call.
    #[test]
    fn a_buffered_delimiter_is_taken_without_a_wait() {
        let (c, mut peer) = conn_pair();
        peer.write_all(b"one\r\ntwo\r\nthree").unwrap();
        let (armed, got) = with_cx(move |cx| {
            // The bytes are in the buffer before the calls under test are made (a call that waits
            // for all fifteen and takes none), so that a system that delivers late cannot make
            // them wait.
            let ticket = c.sh.reads.ticket();
            c.sh.read_call(
                cx,
                ticket,
                close_test_limit(),
                |r| match r.available().len() {
                    15.. => Probe::Ready,
                    have => Probe::Pending(15 - have),
                },
                |r| Ok(r.take(0)),
            )
            .unwrap();
            let armed = Arc::new(Mutex::new(0usize));
            let count = Arc::clone(&armed);
            arm_probe::set_hook_for_every_arm(move |cell| {
                *count.lock().unwrap() += 1;
                cell.fire(poll::Why::Cancelled);
            });
            let mut got = Vec::new();
            for _ in 0..2 {
                let line = on_read(
                    cx,
                    &c,
                    |r| delim_probe(r, b"\r\n", 100, "p"),
                    |r| take_until(r, b"\r\n", 100, "p"),
                );
                got.push(line.map(|b| b.map(|b| b.0)).map_err(|e| variant(&e)));
            }
            let armed = *armed.lock().unwrap();
            arm_probe::set_hook_for_every_arm(|_| {});
            (armed, got)
        });
        assert_eq!(got, [Ok(Some(b"one".to_vec())), Ok(Some(b"two".to_vec()))]);
        assert_eq!(armed, 0);
    }

    /// The deadline of a call counts from the call and the call ends with `Timeout`, with the
    /// half match still readable. The bytes are in the buffer before the call that times out is
    /// made (a call without a deadline waits for all five and takes the first), so the result
    /// does not depend on how fast the system delivers them; only the deadline has to pass.
    #[test]
    fn a_read_until_that_times_out_keeps_the_bytes() {
        let (c, mut peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        peer.write_all(b"half\r").unwrap();
        let shared = Arc::clone(&sh);
        let failed = with_cx(move |cx| {
            let ticket = shared.reads.ticket();
            let first = shared
                .read_call(
                    cx,
                    ticket,
                    Limit::NONE,
                    |r| match r.available().len() {
                        5.. => Probe::Ready,
                        have => Probe::Pending(5 - have),
                    },
                    |r| Ok(r.take(1)),
                )
                .unwrap();
            assert_eq!(first, b"h");
            let limit = Limit {
                deadline: Some(Instant::now() + Duration::from_millis(150)),
                timeout: Duration::from_millis(150),
            };
            until_call(cx, &shared, b"\r\n", 100, limit)
                .map(|_| ())
                .map_err(|e| variant(&e))
        });
        assert_eq!(failed, Err("Timeout".to_string()));
        assert_eq!(sh.reader.lock().unwrap().available(), b"alf\r");
    }

    /// A wake that said "ready" is owed its try for a `read_until` as for a `read_line` (see
    /// `a_ready_wake_that_resumes_after_the_deadline_still_gets_its_read_unit`): the data arrives
    /// before the deadline, the thread is held until after it, and the call gets its bytes.
    #[test]
    fn a_ready_wake_that_resumes_after_the_deadline_still_gets_its_read_until() {
        let (c, mut peer) = conn_pair_fixed();
        let sh = Arc::clone(&c.sh);
        let _keep = peer.try_clone().unwrap();
        let result = with_cx(move |cx| {
            let limit = Limit {
                deadline: Some(Instant::now() + Duration::from_millis(1500)),
                timeout: Duration::from_millis(1500),
            };
            let deadline = limit.deadline.unwrap();
            arm_probe::set_hook(move |cell| {
                peer.write_all(b"line\r\n").unwrap();
                let began = Instant::now();
                while !cell.is_resolved() {
                    assert!(began.elapsed() < Duration::from_secs(20), "no wake");
                    std::thread::yield_now();
                }
                while Instant::now() < deadline + Duration::from_millis(100) {
                    std::hint::spin_loop();
                }
            });
            until_call(cx, &sh, b"\r\n", 100, limit)
                .map(|b| b.map(|b| b.0))
                .map_err(|e| variant(&e))
        });
        assert_eq!(result, Ok(Some(b"line".to_vec())));
    }

    /// A source that has endless bytes and records how much room each read was given.
    struct Endless(Vec<usize>);

    // SAFETY: writes the whole room before it returns its length.
    unsafe impl Recv for Endless {
        fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
            self.0.push(buf.len());
            for b in &mut buf[..] {
                b.write(b'x');
            }
            Ok(buf.len())
        }
    }

    /// Reads from an endless source until `probe` stops asking for more; gives its answer, the
    /// bytes held then, and the sizes of the reads.
    fn read_until_done(
        r: &mut Reader,
        probe: impl Fn(&mut Reader) -> Probe,
    ) -> (Result<(), String>, usize, Vec<usize>) {
        let mut src = Endless(Vec::new());
        let answer = loop {
            match probe(r) {
                Probe::Pending(want) => {
                    r.read_from(&mut src, want).unwrap().unwrap();
                }
                Probe::Ready => break Ok(()),
                Probe::Failed(e) => break Err(variant(&e)),
            }
        };
        (answer, r.available().len(), src.0)
    }

    /// A call that fails with `LineTooLong` has read no more than the larger of the usual read
    /// (32 KiB) and the limit with the end of the line (`read_line`: the limit and `\r\n`;
    /// `read_until`: `max` and the delimiter): the read that would pass that is not asked for. The
    /// reads are never bigger than 32 KiB, and the first one is one at every limit (the size of a
    /// read does not change for the default).
    #[test]
    fn a_call_that_fails_as_too_long_reads_no_more_than_its_bound() {
        for (max, delim) in [
            (0, &b"\n"[..]),
            (4096, b"\r\n"),
            (16, b"--=="),
            (40_000, b"\r\n"),
            (100_000, b"\0"),
        ] {
            let mut r = reader(b"", false);
            let (answer, held, reads) =
                read_until_done(&mut r, |r| delim_probe(r, delim, max, "p"));
            assert_eq!(answer, Err("LineTooLong".to_string()), "{max}");
            let bound = (max + delim.len()).max(READ_UNKNOWN);
            assert_eq!(held, bound, "max {max}: the bytes read");
            assert!(reads.iter().all(|&n| n <= READ_UNKNOWN), "{reads:?}");
            assert_eq!(reads[0], READ_UNKNOWN);
        }
        for limit in [1, 16, 4096, 40_000, MAX_LINE] {
            let mut r = reader(b"", false);
            let (answer, held, reads) = read_until_done(&mut r, |r| line_probe(r, "p", limit));
            assert_eq!(answer, Err("LineTooLong".to_string()), "{limit}");
            assert_eq!(
                held,
                (limit + 2).max(READ_UNKNOWN),
                "limit {limit}: the bytes read"
            );
            assert!(reads.iter().all(|&n| n <= READ_UNKNOWN), "{reads:?}");
            assert_eq!(reads[0], READ_UNKNOWN);
        }
        // With bytes already buffered the next read asks for what is left to the bound.
        let mut r = reader(&[b'x'; 10], false);
        let (answer, held, reads) = read_until_done(&mut r, |r| delim_probe(r, b"\r\n", 16, "p"));
        assert_eq!(
            (answer, held),
            (Err("LineTooLong".to_string()), READ_UNKNOWN)
        );
        assert_eq!(reads, [READ_UNKNOWN - 10]);
        let mut r = reader(&[b'x'; 40_000], false);
        let (answer, held, reads) =
            read_until_done(&mut r, |r| delim_probe(r, b"\r\n", 40_000, "p"));
        assert_eq!(
            (answer, held, reads),
            (Err("LineTooLong".to_string()), 40_002, vec![2])
        );
    }

    /// A source of records of eight bytes (`1234567\n`), as many as the room takes.
    struct Records(Vec<usize>);

    // SAFETY: writes the whole room before it returns its length.
    unsafe impl Recv for Records {
        fn recv(&mut self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
            self.0.push(buf.len());
            for (i, b) in buf.iter_mut().enumerate() {
                b.write(if i % 8 == 7 {
                    b'\n'
                } else {
                    b'0' + (i % 8) as u8
                });
            }
            Ok(buf.len())
        }
    }

    /// A small `max` does not make the reads small: 4,096 records of eight bytes (32 KiB) that
    /// are read with `read_until("\n", 16)`, and with `read_line` at a limit of 16, take one read
    /// of 32 KiB, not one read for each record. (A read that asks only for what is left to the
    /// bound would be one per record.)
    #[test]
    fn a_small_max_next_to_pipelined_data_takes_one_read() {
        for until in [true, false] {
            let mut r = reader(b"", false);
            let mut src = Records(Vec::new());
            for _ in 0..4096 {
                loop {
                    let probe = if until {
                        delim_probe(&mut r, b"\n", 16, "p")
                    } else {
                        line_probe(&mut r, "p", 16)
                    };
                    match probe {
                        Probe::Pending(want) => {
                            r.read_from(&mut src, want).unwrap().unwrap();
                        }
                        Probe::Ready => break,
                        Probe::Failed(e) => panic!("{}", variant(&e)),
                    }
                }
                if until {
                    assert_eq!(
                        take_until(&mut r, b"\n", 16, "p").unwrap().unwrap().0.len(),
                        7
                    );
                } else {
                    assert_eq!(take_line(&mut r, "p", 16).unwrap().unwrap().len(), 7);
                }
            }
            assert_eq!(src.0, [READ_UNKNOWN], "until {until}");
        }
    }

    /// The same through the socket: a peer that sends 60,000 bytes with no delimiter makes
    /// `read_until(.., 40_000)` fail with at most 40,002 bytes read, and a `read_line` at a limit of
    /// 40,000 with at most 40,002 (the rest is still in the system). A limit under 32 KiB would not
    /// show the bound: the usual read is that big.
    #[test]
    fn a_peer_that_sends_too_much_is_not_read_past_the_bound() {
        for (until, bound) in [(true, 40_002), (false, 40_002)] {
            let (c, mut peer) = conn_pair();
            peer.write_all(&[b'x'; 60_000]).unwrap();
            let sh = Arc::clone(&c.sh);
            let shared = Arc::clone(&sh);
            let error = with_cx(move |cx| {
                if until {
                    until_call(cx, &shared, b"\r\n", 40_000, Limit::NONE)
                        .map(|_| ())
                        .map_err(|e| variant(&e))
                } else {
                    let ticket = shared.reads.ticket();
                    shared
                        .read_call(
                            cx,
                            ticket,
                            Limit::NONE,
                            |r| line_probe(r, "p", 40_000),
                            |r| take_line(r, "p", 40_000),
                        )
                        .map(|_| ())
                        .map_err(|e| variant(&e))
                }
            });
            assert_eq!(error, Err("LineTooLong".to_string()));
            let held = sh.reader.lock().unwrap().available().len();
            assert!(held <= bound, "until {until}: {held} bytes read");
        }
    }

    /// `close` of a connection that a `read_until` waits on ends it with `Closed` and releases
    /// the descriptor by the time `close` returns, as it does for a `read_line`; also with half a
    /// match buffered (what is buffered of a closed connection is for no one).
    #[test]
    fn close_releases_the_socket_of_a_connection_that_a_read_until_waits_on() {
        for partial in [false, true] {
            for _ in 0..10 {
                let (c, mut peer) = conn_pair();
                if partial {
                    peer.write_all(b"ab\r").unwrap();
                }
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
                            // Waiting: armed, and the connection's and the poller's references
                            // are the two left (see the `read_line` test).
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
                    let error = until_call(cx, &sh, b"\r\n", 100, close_test_limit())
                        .map(|_| ())
                        .map_err(|e| variant(&e));
                    (error, closer.join().unwrap())
                });
                assert_eq!(error, Err("Closed".to_string()), "partial {partial}");
                assert!(released, "the socket was still held when close returned");
            }
        }
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
                    |r| line_probe(r, "p", MAX_LINE),
                    |r| take_line(r, "p", MAX_LINE),
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

    /// The deadline of the waiting reads of the tests of `close`: far above the closer's own limit
    /// of 30 s and never what a passing test depends on (a `close` that works ends the read with
    /// `Closed` in milliseconds), so that a `close` that does not end the read fails the test on
    /// its assertion and does not hang the run.
    fn close_test_limit() -> Limit {
        Limit {
            deadline: Some(Instant::now() + Duration::from_secs(60)),
            timeout: Duration::from_secs(60),
        }
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
                        close_test_limit(),
                        |r| line_probe(r, "p", MAX_LINE),
                        |r| take_line(r, "p", MAX_LINE),
                    )
                    .map(|_| ())
                    .map_err(|e| variant(&e));
                (error, closer.join().unwrap())
            });
            assert_eq!(error, Err("Closed".to_string()));
            assert!(released, "the socket was still held when close returned");
        }
    }

    /// The same for a write: `close` of a connection that a write waits on (the peer reads
    /// nothing, so the write fills the buffers and waits for room) releases the descriptor by the
    /// time it returns. This needs the write slot's wake in `close`: the poller's hold on the
    /// socket goes with the resolved cell, and without that nothing resolves it before `close`
    /// returns.
    ///
    /// The closer runs on another thread, which the product never does (`close` runs on the
    /// interpreter thread, where the writer is suspended). Where a write wait can end `Ready` on
    /// its own while the pipe fills (macOS, Windows), the writer may be in its next send when the
    /// closer's `close` returns, and that round says nothing: the writer's send attempts are
    /// counted, and a round in which one was made after the closer had looked is void and is
    /// done again. Twenty valid rounds are required, within a ceiling.
    #[test]
    fn close_releases_the_socket_of_a_connection_that_a_write_waits_on() {
        let (mut valid, mut rounds) = (0, 0);
        while valid < 20 {
            rounds += 1;
            assert!(
                rounds <= 200,
                "{valid} valid rounds in {} rounds",
                rounds - 1
            );
            let (c, peer) = conn_pair_fixed();
            let sh = Arc::clone(&c.sh);
            let weak = Arc::downgrade(&sh.sock.get().expect("open"));
            let (error, closed, attempts_after) = with_cx(move |cx| {
                let poller = cx
                    .state_mut::<poll::NetState>()
                    .unwrap()
                    .poller()
                    .expect("the poller starts");
                let closer = {
                    let sh = Arc::clone(&sh);
                    std::thread::spawn(move || {
                        let began = Instant::now();
                        let armed = || poller.counts() == (1, 1) && weak.strong_count() == 2;
                        loop {
                            while !armed() {
                                assert!(
                                    began.elapsed() < Duration::from_secs(30),
                                    "the write never waited"
                                );
                                std::thread::yield_now();
                            }
                            // Read, and then the state looked at once more: a send that began
                            // before the read holds a reference, and one after it counts.
                            let before = sh.send_attempts.load(Ordering::SeqCst);
                            if armed() {
                                sh.close();
                                return (before, weak.strong_count() == 0);
                            }
                        }
                    })
                };
                let ticket = sh.writes.ticket();
                let error = sh
                    .write_call(cx, ticket, Limit::NONE, &vec![1u8; 64 << 20])
                    .map_err(|e| variant(&e));
                let (before, released) = closer.join().unwrap();
                let after = sh.send_attempts.load(Ordering::SeqCst);
                (error, (before, released), after)
            });
            drop(peer);
            assert_eq!(error, Err("Closed".to_string()));
            let (before, released) = closed;
            if before != attempts_after {
                // The writer began a send after the closer looked: not what is tested.
                continue;
            }
            valid += 1;
            assert!(released, "the socket was still held when close returned");
        }
    }

    /// A deadline in the past by the time the coroutine runs again does not take away the try that
    /// a `Ready` wake is owed, for a write: the socket is full, so a write of one byte waits at
    /// once; the hook (it runs where the wait was just armed) has the peer read, waits for the
    /// poller to say "ready", and then keeps the thread busy past the deadline. With the try
    /// owed the byte is written; without it the call times out. The hook waits for conditions,
    /// not for a guessed time, so being slow cannot fail the test (a poller that was starved past
    /// the deadline makes it a timeout wake, which has its own final try).
    #[test]
    fn a_ready_wake_that_resumes_after_the_deadline_still_gets_its_write() {
        let (c, mut peer) = conn_pair_fixed();
        let sh = Arc::clone(&c.sh);
        let sock = sh.sock.get().expect("open");
        fill_raw(&sock);
        drop(sock);
        // The hook owns `peer` and drops it when it has run: this one keeps the connection open.
        let _keep = peer.try_clone().unwrap();
        let result = with_cx(move |cx| {
            let limit = Limit {
                deadline: Some(Instant::now() + Duration::from_millis(1500)),
                timeout: Duration::from_millis(1500),
            };
            let deadline = limit.deadline.unwrap();
            arm_probe::set_hook(move |cell| {
                // Room for far more than a byte.
                peer.set_read_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                let mut got = 0;
                let mut buf = [0u8; 16 * 1024];
                while got < 64 * 1024 {
                    got += peer.read(&mut buf).unwrap();
                }
                let began = Instant::now();
                while !cell.is_resolved() {
                    assert!(began.elapsed() < Duration::from_secs(20), "no wake");
                    std::thread::yield_now();
                }
                while Instant::now() < deadline + Duration::from_millis(100) {
                    std::hint::spin_loop();
                }
            });
            let ticket = sh.writes.ticket();
            sh.write_call(cx, ticket, limit, b"x")
                .map_err(|e| variant(&e))
        });
        assert_eq!(result, Ok(()));
    }

    /// The same for a read.
    #[test]
    fn a_ready_wake_that_resumes_after_the_deadline_still_gets_its_read_unit() {
        let (c, mut peer) = conn_pair_fixed();
        let sh = Arc::clone(&c.sh);
        let _keep = peer.try_clone().unwrap();
        let result = with_cx(move |cx| {
            let limit = Limit {
                deadline: Some(Instant::now() + Duration::from_millis(1500)),
                timeout: Duration::from_millis(1500),
            };
            let deadline = limit.deadline.unwrap();
            arm_probe::set_hook(move |cell| {
                peer.write_all(b"line\n").unwrap();
                let began = Instant::now();
                while !cell.is_resolved() {
                    assert!(began.elapsed() < Duration::from_secs(20), "no wake");
                    std::thread::yield_now();
                }
                while Instant::now() < deadline + Duration::from_millis(100) {
                    std::hint::spin_loop();
                }
            });
            let ticket = sh.reads.ticket();
            sh.read_call(
                cx,
                ticket,
                limit,
                |r| line_probe(r, "p", MAX_LINE),
                |r| take_line(r, "p", MAX_LINE),
            )
            .map_err(|e| variant(&e))
        });
        assert_eq!(result, Ok(Some("line".to_string())));
    }

    /// Makes the next successful read or write of `sh` use the burst budget up.
    fn nearly_spent(sh: &Shared) {
        sh.burst.reset();
        assert!(!sh.burst.spend(MAX_BURST - 1));
    }

    /// The byte half of the burst budget yields at its call sites: a read and a write that use
    /// the bytes up give the thread away once, inside the call. (The counter is the test build's
    /// `give_way_probe`; the budget is made nearly spent by hand, so that no timing or buffer size
    /// decides whether a burst of a mebibyte happened.)
    #[test]
    fn the_byte_budget_gives_way_inside_a_read_and_inside_a_write() {
        let (c, mut peer) = conn_pair();
        peer.write_all(&vec![7u8; 100_000]).unwrap();
        let sh = Arc::clone(&c.sh);
        let (read, written) = with_cx(move |cx| {
            let before = give_way_probe::calls();
            nearly_spent(&sh);
            let ticket = sh.reads.ticket();
            let got = sh
                .read_call(
                    cx,
                    ticket,
                    Limit::NONE,
                    |r| {
                        if r.available().len() >= 100_000 {
                            Probe::Ready
                        } else {
                            Probe::Pending(100_000 - r.available().len())
                        }
                    },
                    |r| Ok(r.take(100_000)),
                )
                .unwrap();
            let read = (got.len(), give_way_probe::calls() - before);
            let before = give_way_probe::calls();
            nearly_spent(&sh);
            let ticket = sh.writes.ticket();
            sh.write_call(cx, ticket, Limit::NONE, &vec![9u8; 100_000])
                .unwrap();
            (read, give_way_probe::calls() - before)
        });
        assert_eq!(read, (100_000, 1), "the read gave way once");
        assert_eq!(written, 1, "the write gave way once");
        let mut got = vec![0u8; 100_000];
        peer.read_exact(&mut got).unwrap();
        assert!(got.iter().all(|&b| b == 9));
    }

    /// A close that runs while a read or a write has given the thread away after its burst
    /// releases the descriptor before `close` returns: the call holds no reference to the socket
    /// while it is away. The hook of `give_way_probe` is the other coroutine: it closes at the
    /// moment of the yield, on this thread, and looks at who still holds the socket.
    #[test]
    fn a_close_during_a_give_way_releases_the_socket_of_a_read_and_of_a_write() {
        for writing in [false, true] {
            let (c, mut peer) = conn_pair();
            let sh = Arc::clone(&c.sh);
            let weak = Arc::downgrade(&sh.sock.get().expect("open"));
            let drain = writing.then(|| {
                let mut peer = peer.try_clone().unwrap();
                std::thread::spawn(move || {
                    let mut all = Vec::new();
                    peer.read_to_end(&mut all).unwrap();
                    all.len()
                })
            });
            if !writing {
                peer.write_all(&vec![7u8; 100_000]).unwrap();
            }
            let (outcome, released) = with_cx(move |cx| {
                let released = Arc::new(Mutex::new(None));
                {
                    let (sh, weak, released) =
                        (Arc::clone(&sh), weak.clone(), Arc::clone(&released));
                    give_way_probe::set_hook(move || {
                        sh.close();
                        *released.lock().unwrap() = Some(weak.strong_count() == 0);
                    });
                }
                nearly_spent(&sh);
                let outcome = if writing {
                    let ticket = sh.writes.ticket();
                    // More than one chunk, so that the loop is still going at the yield.
                    sh.write_call(cx, ticket, Limit::NONE, &vec![9u8; SEND_CHUNK + 1000])
                } else {
                    let ticket = sh.reads.ticket();
                    sh.read_call(
                        cx,
                        ticket,
                        Limit::NONE,
                        |r| {
                            if r.available().len() >= 100_000 {
                                Probe::Ready
                            } else {
                                Probe::Pending(100_000 - r.available().len())
                            }
                        },
                        |r| Ok(r.take(100_000).len()).map(|_| ()),
                    )
                }
                .map_err(|e| format!("{e:?}"));
                give_way_probe::clear_hook();
                let released = *released.lock().unwrap();
                (outcome, released)
            });
            let text = outcome.expect_err("the call ends: the connection was closed");
            assert!(text.contains("Closed"), "writing {writing}: {text}");
            if writing {
                assert!(
                    text.contains("bytes were sent"),
                    "the writer says how much it had sent: {text}"
                );
            }
            assert_eq!(
                released,
                Some(true),
                "writing {writing}: the socket was held while the call was away"
            );
            if let Some(drain) = drain {
                drop(c);
                // The peer got what was sent, cut short, and then the end.
                let got = drain.join().unwrap();
                assert!(got > 0 && got < SEND_CHUNK + 1000, "{got}");
            }
        }
    }

    /// A peer that has seen the end of the stream: the read returns 0 (and does not wait for it
    /// longer than the test's patience), or has not yet: a non-blocking read would block.
    fn peer_sees_the_end(peer: &mut TcpStream) -> bool {
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        matches!(peer.read(&mut [0u8; 1]), Ok(0))
    }

    fn peer_has_not_seen_the_end(peer: &mut TcpStream) -> bool {
        peer.set_nonblocking(true).unwrap();
        let blocked =
            matches!(peer.read(&mut [0u8; 1]), Err(e) if e.kind() == ErrorKind::WouldBlock);
        peer.set_nonblocking(false).unwrap();
        blocked
    }

    /// Shutdowns that are scheduled behind a ticket wait for it, run in the order they were
    /// scheduled once it is dropped, and leave the queue empty.
    #[test]
    fn scheduled_shutdowns_wait_for_the_ticket_ahead_and_leave_the_queue_empty() {
        let (c, mut peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let ahead = sh.writes.ticket();
        sh.schedule_shutdown();
        sh.schedule_shutdown();
        assert!(peer_has_not_seen_the_end(&mut peer), "shut before its turn");
        assert!(sh.writes.busy());
        drop(ahead);
        assert!(
            !sh.writes.busy(),
            "both shutdowns ran and passed the turn on"
        );
        assert!(peer_sees_the_end(&mut peer));
    }

    /// A shutdown that is scheduled on a connection that is closed before its turn comes does
    /// nothing, and still passes the turn on.
    #[test]
    fn a_scheduled_shutdown_on_a_closed_connection_passes_the_turn_on() {
        let (c, _peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let ahead = sh.writes.ticket();
        sh.schedule_shutdown();
        sh.close();
        drop(ahead);
        assert!(!sh.writes.busy());
    }

    /// A connection that goes away with a shutdown scheduled does not leave the queue's cell and
    /// the ticket in it to keep each other alive: the cell's ticket holds the queue, and the queue
    /// holds the cell. The queue is held by the ticket ahead and by this test, and by no one else
    /// once the connection is gone.
    #[test]
    fn a_connection_dropped_with_a_scheduled_shutdown_leaves_nothing_behind() {
        let (c, _peer) = conn_pair();
        let sh = Arc::clone(&c.sh);
        let turns = Arc::clone(&sh.writes);
        let ahead = sh.writes.ticket();
        sh.schedule_shutdown();
        drop(sh);
        drop(c);
        // `turns` and `ahead`; the scheduled ticket is gone with its cell.
        assert_eq!(Arc::strong_count(&turns), 2);
        drop(ahead);
        assert!(!turns.busy());
        assert_eq!(Arc::strong_count(&turns), 1);
    }
}
