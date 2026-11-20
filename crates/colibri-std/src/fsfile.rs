//! `fs.File`: a file opened for reading or for writing, read and written piece by piece
//! (DESIGN §6.1).
//!
//! `fs.open(path)` opens for reading, `fs.create(path)` creates or truncates for writing and
//! `fs.open_append(path)` appends, creating the file when it is missing. A handle does one of
//! the two: reading from a handle opened for writing, or the reverse, is an `fs.Error`. Reads
//! and writes go through a buffer inside the handle.
//!
//! Every operation that touches the disk runs on a background thread
//! (`Ctx::run_async_cancellable`), so other coroutines keep running meanwhile; on the main
//! fiber the wait runs the event loop, like an `http` or `process` call.
//!
//! Operations on one handle are served in the order the script issued them. Each one takes a
//! ticket on the interpreter thread when the script calls the method, and its background job
//! waits until every earlier ticket is done: it is never interleaved with another operation on
//! the same handle, and a later call never overtakes an earlier one. Two cases never touch
//! the disk and are done in place, but only when no ticket is outstanding: `write` into buffer
//! space that is still free, and `read_line` of a line that is already in the buffer.
//!
//! A coroutine that is cancelled stops waiting at once. Its operation never runs if it had not
//! begun; one that was running runs to its end (a read has moved the position of the handle),
//! and either way the tickets behind it go on. A read that is stuck (an empty pipe) keeps the
//! turn until it returns, so a `close()` issued after it waits too.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use colibri_runtime::{Bytes, CancelToken, Cap, Ctx, Error, HostType, TypeBuilder, TypedError};

use crate::bytes::utf8_problem;
use crate::fs::{Kind, fail};
use crate::needs;

/// The size of the buffer of a handle, for reading and for writing.
pub(crate) const BUFFER: usize = 64 * 1024;

enum State {
    Closed,
    Reader(BufReader<std::fs::File>),
    Writer(BufWriter<std::fs::File>),
}

struct Inner {
    state: State,
    /// Opened with `fs.open_append`: the position is always the end, so `seek` has no meaning.
    append: bool,
}

/// Which tickets are done. `serving` is the oldest ticket that is not.
struct Turns {
    serving: u64,
    /// Tickets above `serving` that are done already (cancelled before their turn).
    finished: BTreeSet<u64>,
}

/// What a handle shares with the background jobs that work on it.
struct Shared {
    inner: Mutex<Inner>,
    /// The next ticket to hand out; only the interpreter thread takes one.
    next: AtomicU64,
    turns: Mutex<Turns>,
    turn_changed: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new(inner: Inner) -> Shared {
        Shared {
            inner: Mutex::new(inner),
            next: AtomicU64::new(0),
            turns: Mutex::new(Turns {
                serving: 0,
                finished: BTreeSet::new(),
            }),
            turn_changed: Condvar::new(),
        }
    }

    /// Whether every ticket that was handed out is done.
    fn idle(&self) -> bool {
        lock(&self.turns).serving == self.next.load(Ordering::SeqCst)
    }

    fn finish(&self, ticket: u64) {
        let mut guard = lock(&self.turns);
        let turns = &mut *guard;
        if ticket == turns.serving {
            turns.serving += 1;
            while turns.finished.remove(&turns.serving) {
                turns.serving += 1;
            }
        } else {
            turns.finished.insert(ticket);
        }
        drop(guard);
        self.turn_changed.notify_all();
    }

    /// Waits for the turn of `ticket`; `false` when the waiter gave up first.
    fn wait_for(&self, ticket: u64, token: &CancelToken) -> bool {
        let mut turns = lock(&self.turns);
        while turns.serving != ticket {
            if token.is_cancelled() {
                return false;
            }
            turns = self
                .turn_changed
                .wait(turns)
                .unwrap_or_else(PoisonError::into_inner);
        }
        !token.is_cancelled()
    }
}

/// A place in the line of one handle. Dropping it, whether the operation ran, was skipped
/// because its coroutine was cancelled, or panicked, ends the turn.
struct Ticket {
    shared: Arc<Shared>,
    number: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.shared.finish(self.number);
    }
}

/// An open file, from `fs.open`, `fs.create` or `fs.open_append`. When the last reference to it
/// goes away the file is flushed and closed (an error then has nowhere to go: call `close()`,
/// best with `defer`, to see it).
pub(crate) struct File {
    path: Arc<str>,
    shared: Arc<Shared>,
}

/// Runs `f` on the handle now, when no operation is waiting or running on it. For the cases
/// that never touch the disk.
fn inline<R>(shared: &Shared, f: impl FnOnce(&mut Inner) -> Option<R>) -> Option<R> {
    if !shared.idle() {
        return None;
    }
    match shared.inner.try_lock() {
        Ok(mut guard) => f(&mut guard),
        Err(_) => None,
    }
}

fn closed(action: &str, path: &str) -> TypedError {
    Kind::Closed.error(format!("cannot {action} `{path}`: the file is closed"))
}

fn not_for_reading(path: &str) -> TypedError {
    Kind::WrongMode.error(format!(
        "cannot read `{path}`: the file was opened for writing, not for reading \
         (open it with `fs.open` to read it)"
    ))
}

fn not_for_writing(path: &str) -> TypedError {
    Kind::WrongMode.error(format!(
        "cannot write to `{path}`: the file was opened for reading, not for writing \
         (open it with `fs.create` or `fs.open_append` to write)"
    ))
}

/// A line without its `\n` or `\r\n`.
fn strip_eol(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}

fn text_of(line: Vec<u8>, path: &str) -> Result<String, TypedError> {
    String::from_utf8(line).map_err(|e| {
        Kind::InvalidData.error(format!(
            "cannot read a line of `{path}`: the line is not valid UTF-8 ({}); \
             `read` gives the bytes of a file that is not text",
            utf8_problem(&e.utf8_error())
        ))
    })
}

impl Inner {
    fn reader(&mut self, path: &str) -> Result<&mut BufReader<std::fs::File>, TypedError> {
        match &mut self.state {
            State::Reader(r) => Ok(r),
            State::Writer(_) => Err(not_for_reading(path)),
            State::Closed => Err(closed("read", path)),
        }
    }

    fn writer(&mut self, path: &str) -> Result<&mut BufWriter<std::fs::File>, TypedError> {
        match &mut self.state {
            State::Writer(w) => Ok(w),
            State::Reader(_) => Err(not_for_writing(path)),
            State::Closed => Err(closed("write to", path)),
        }
    }

    /// A whole line that is in the buffer already, if there is one.
    fn buffered_line(&mut self, path: &str) -> Option<Result<Option<String>, TypedError>> {
        let State::Reader(r) = &mut self.state else {
            return None;
        };
        let at = r.buffer().iter().position(|&b| b == b'\n')?;
        let mut line = r.buffer()[..=at].to_vec();
        r.consume(at + 1);
        strip_eol(&mut line);
        Some(text_of(line, path).map(Some))
    }

    fn read_line(&mut self, path: &str) -> Result<Option<String>, TypedError> {
        let r = self.reader(path)?;
        let mut line = Vec::new();
        let n = r
            .read_until(b'\n', &mut line)
            .map_err(|e| fail("read", path, &e))?;
        if n == 0 {
            return Ok(None);
        }
        strip_eol(&mut line);
        text_of(line, path).map(Some)
    }

    fn read(&mut self, path: &str, n: u64) -> Result<Bytes, TypedError> {
        let r = self.reader(path)?;
        let mut out = Vec::new();
        r.take(n)
            .read_to_end(&mut out)
            .map_err(|e| fail("read", path, &e))?;
        Ok(Bytes(out))
    }

    fn read_all(&mut self, path: &str) -> Result<Bytes, TypedError> {
        let r = self.reader(path)?;
        let mut out = Vec::new();
        r.read_to_end(&mut out)
            .map_err(|e| fail("read", path, &e))?;
        Ok(Bytes(out))
    }

    /// Bytes that fit in the free space of the buffer, which needs no disk.
    fn buffered_write(&mut self, data: &[u8]) -> Option<Result<(), TypedError>> {
        let State::Writer(w) = &mut self.state else {
            return None;
        };
        if w.buffer().len() + data.len() > w.capacity() {
            return None;
        }
        // It fits, so nothing is flushed and nothing can fail.
        let _ = w.write_all(data);
        Some(Ok(()))
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), TypedError> {
        self.writer(path)?
            .write_all(data)
            .map_err(|e| fail("write to", path, &e))
    }

    fn flush(&mut self, path: &str) -> Result<(), TypedError> {
        // Nothing is buffered for writing in a handle opened for reading.
        if matches!(self.state, State::Reader(_)) {
            return Ok(());
        }
        self.writer(path)?
            .flush()
            .map_err(|e| fail("write to", path, &e))
    }

    fn seek(&mut self, path: &str, to: u64) -> Result<(), TypedError> {
        if self.append {
            return Err(Kind::WrongMode.error(format!(
                "cannot seek in `{path}`: it was opened for appending, so every write goes to the end"
            )));
        }
        let result = match &mut self.state {
            State::Reader(r) => r.seek(SeekFrom::Start(to)),
            State::Writer(w) => w.seek(SeekFrom::Start(to)),
            State::Closed => return Err(closed("seek in", path)),
        };
        result.map(|_| ()).map_err(|e| fail("seek in", path, &e))
    }

    fn position(&mut self, path: &str) -> Result<u64, TypedError> {
        let result = match &mut self.state {
            State::Reader(r) => r.stream_position(),
            State::Writer(w) => {
                let buffered = w.buffer().len() as u64;
                // Where a write goes: the end of the file for an appending handle (whose own
                // position moves only when it writes), where the file is at otherwise.
                let at = if self.append {
                    w.get_ref().metadata().map(|m| m.len())
                } else {
                    w.get_mut().stream_position()
                };
                at.map(|p| p + buffered)
            }
            State::Closed => return Err(closed("look at", path)),
        };
        result.map_err(|e| fail("look at", path, &e))
    }

    /// Flushes and closes. Closing a closed file is fine.
    fn close(&mut self, path: &str) -> Result<(), TypedError> {
        match std::mem::replace(&mut self.state, State::Closed) {
            State::Writer(mut w) => w.flush().map_err(|e| fail("write to", path, &e)),
            State::Reader(_) | State::Closed => Ok(()),
        }
    }
}

impl File {
    fn new(path: &str, state: State, append: bool) -> File {
        File {
            path: Arc::from(path),
            shared: Arc::new(Shared::new(Inner { state, append })),
        }
    }
}

/// Runs `op` on the handle, on a background thread, once every operation issued before it has
/// ended. The ticket is taken here, on the interpreter thread, so the order is the order of
/// the calls in the script.
fn on_handle<T: Send + 'static>(
    cx: &mut Ctx,
    this: &File,
    op: impl FnOnce(&mut Inner, &str) -> Result<T, TypedError> + Send + 'static,
) -> Result<T, TypedError> {
    let path = Arc::clone(&this.path);
    let shared = Arc::clone(&this.shared);
    let ticket = Ticket {
        number: shared.next.fetch_add(1, Ordering::SeqCst),
        shared,
    };
    cx.run_async_cancellable(move |token| {
        let shared = Arc::clone(&ticket.shared);
        // A cancel must wake this thread if it is waiting for its turn. Taking the lock first
        // means the wake cannot slip in between the check and the wait.
        let waker = Arc::clone(&shared);
        token.on_cancel(move || {
            drop(lock(&waker.turns));
            waker.turn_changed.notify_all();
        });
        if !shared.wait_for(ticket.number, token) {
            // The coroutine is gone: nobody will see this.
            return Err(Kind::Other.error("the operation was cancelled before it began"));
        }
        op(&mut lock(&shared.inner), &path)
    })
}

impl HostType for File {
    const NAME: &'static str = "File";

    fn describe(d: &mut TypeBuilder<Self>) {
        d.show(|f| format!("<File `{}`>", f.path));
        // The next line without its `\n` or `\r\n`, or `none` at the end of the file. A line
        // that is not valid UTF-8 is an error (the file is not text: use `read`).
        d.method("read_line", [], |cx: &mut Ctx, this: &File| {
            if let Some(done) = inline(&this.shared, |g| g.buffered_line(&this.path)) {
                return done;
            }
            on_handle(cx, this, |g, path| g.read_line(path))
        });
        // Up to `n` bytes: fewer only at the end of the file, and none at the end.
        d.method("read", ["n"], |cx: &mut Ctx, this: &File, n: i64| {
            let Ok(n) = u64::try_from(n) else {
                return Err(Kind::Other.error(format!(
                    "cannot read `{}`: {n} is not a number of bytes (use 0 or more)",
                    this.path
                )));
            };
            on_handle(cx, this, move |g, path| g.read(path, n))
        });
        // Everything from the current position to the end of the file.
        d.method("read_all", [], |cx: &mut Ctx, this: &File| {
            on_handle(cx, this, |g, path| g.read_all(path))
        });
        d.method(
            "write",
            ["text"],
            |cx: &mut Ctx, this: &File, text: String| {
                if let Some(done) = inline(&this.shared, |g| g.buffered_write(text.as_bytes())) {
                    return done;
                }
                on_handle(cx, this, move |g, path| g.write(path, text.as_bytes()))
            },
        );
        d.method(
            "write_bytes",
            ["data"],
            |cx: &mut Ctx, this: &File, data: Bytes| {
                if let Some(done) = inline(&this.shared, |g| g.buffered_write(&data)) {
                    return done;
                }
                on_handle(cx, this, move |g, path| g.write(path, &data))
            },
        );
        // Pushes what is buffered to the file.
        d.method("flush", [], |cx: &mut Ctx, this: &File| {
            on_handle(cx, this, |g, path| g.flush(path))
        });
        // Moves to `pos` bytes from the start of the file.
        d.method("seek", ["pos"], |cx: &mut Ctx, this: &File, pos: i64| {
            let Ok(pos) = u64::try_from(pos) else {
                return Err(Kind::Other.error(format!(
                    "cannot seek in `{}`: {pos} is not a position (use 0 or more)",
                    this.path
                )));
            };
            on_handle(cx, this, move |g, path| g.seek(path, pos))
        });
        // Bytes from the start of the file: where the next read or write happens.
        d.method("position", [], |cx: &mut Ctx, this: &File| {
            let at = on_handle(cx, this, |g, path| g.position(path))?;
            i64::try_from(at).map_err(|_| Kind::Other.error("the position is too big"))
        });
        // Flushes and closes the file, and reports a flush that failed. Closing twice is fine;
        // any other use of a closed file is an error.
        d.method("close", [], |cx: &mut Ctx, this: &File| {
            on_handle(cx, this, |g, path| g.close(path))
        });
    }
}

fn open_with(
    cx: &mut Ctx,
    path: String,
    action: &'static str,
    options: fn() -> std::fs::OpenOptions,
    make: fn(std::fs::File) -> State,
    append: bool,
) -> Result<File, TypedError> {
    let shown = path.clone();
    let opened = cx.run_async(move || {
        let file = options().open(&path).map_err(|e| fail(action, &path, &e))?;
        let meta = file.metadata().map_err(|e| fail(action, &path, &e))?;
        if meta.is_dir() {
            return Err(
                Kind::IsADirectory.error(format!("cannot {action} `{path}`: is a directory"))
            );
        }
        Ok(file)
    })?;
    Ok(File::new(&shown, make(opened), append))
}

pub(crate) fn register(m: &mut colibri_runtime::HostModule<'_>) -> Result<(), Error> {
    m.register_type::<File>()?;
    // Opens a file to read it, piece by piece, with `read_line`, `read` and `read_all`.
    needs(
        Cap::Fs,
        m.register_fn_named("open", ["path"], |cx: &mut Ctx, path: String| {
            open_with(
                cx,
                path,
                "open",
                || {
                    let mut o = std::fs::OpenOptions::new();
                    o.read(true);
                    o
                },
                |f| State::Reader(BufReader::with_capacity(BUFFER, f)),
                false,
            )
        }),
    )?;
    // Creates a file, or empties an existing one, to write it with `write` and `write_bytes`.
    needs(
        Cap::Fs,
        m.register_fn_named("create", ["path"], |cx: &mut Ctx, path: String| {
            open_with(
                cx,
                path,
                "create",
                || {
                    let mut o = std::fs::OpenOptions::new();
                    o.write(true).create(true).truncate(true);
                    o
                },
                |f| State::Writer(BufWriter::with_capacity(BUFFER, f)),
                false,
            )
        }),
    )?;
    // Opens a file to write at its end, creating it when it is missing.
    needs(
        Cap::Fs,
        m.register_fn_named("open_append", ["path"], |cx: &mut Ctx, path: String| {
            open_with(
                cx,
                path,
                "open",
                || {
                    let mut o = std::fs::OpenOptions::new();
                    o.append(true).create(true);
                    o
                },
                |f| State::Writer(BufWriter::with_capacity(BUFFER, f)),
                true,
            )
        }),
    )?;
    Ok(())
}
