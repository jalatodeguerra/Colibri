//! What a thousand idle blocked reads, a thousand idle accepts and a thousand writes blocked on
//! full buffers cost in threads and in CPU time: the point of waiting for
//! sockets with one poller thread per `Vm`. This is a test file of its own because it counts the
//! threads and measures the CPU time of the whole process, which the other tests of `net` (they
//! run in threads of one process) would pollute. The thread count is read from `/proc/self/task`
//! on Linux, with `proc_pidinfo` on macOS and with a toolhelp snapshot on Windows, and the CPU
//! time with `getrusage` on Unix and `GetProcessTimes` on Windows: no portable call gives them.
//!
//! The file is gated to the three systems the count is written for (the project builds on no
//! other).
#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;

const READERS: usize = 1000;
const LISTENERS: usize = 1000;
const WRITERS: usize = 1000;

/// What keeps the buffers of a blocked write small: the peer asks for a small receive buffer on its
/// listener, which the accepted sockets inherit, and (Linux) a small segment size, which the
/// connecting side takes over, so that the send buffer the system grows to stays small as well
/// (without it a thousand connections took gigabytes of kernel memory before they were full).
/// `WRITE_BYTES` is what a writer is given: far more than the two buffers can take.
const SMALL_BUFFER: usize = 4 * 1024;
const WRITE_BYTES: usize = 4 * 1024 * 1024;

const IDLE_SECONDS: f64 = 3.0;
const LIMIT: Duration = Duration::from_secs(30);

/// How many threads the process may have more than before the test, while a thousand reads
/// wait: the poller, the peer's thread, and the few threads of the pool that the connects used.
/// Before the poller it was one for every waiting read.
const MAY_GROW_BY: usize = 8;

/// How many threads the process has now.
#[cfg(target_os = "linux")]
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("lists the threads")
        .count()
}

/// How many threads the process has now.
#[cfg(target_os = "macos")]
fn threads() -> usize {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` is a valid `proc_taskinfo` of `size` bytes for the call to fill in, and the
    // process asks about itself.
    let (got, info) = unsafe {
        let got = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        (got, info.assume_init())
    };
    assert_eq!(got, size, "proc_pidinfo failed");
    usize::try_from(info.pti_threadnum).expect("a thread count")
}

/// How many threads the process has now.
#[cfg(windows)]
fn threads() -> usize {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    // SAFETY: plain calls of the system; the snapshot handle is checked and closed, and the
    // entry is a valid `THREADENTRY32` with its size set, as `Thread32First` asks.
    unsafe {
        let pid = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        assert_ne!(snapshot, INVALID_HANDLE_VALUE, "no thread snapshot");
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut count = 0;
        let mut more = Thread32First(snapshot, &mut entry);
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                count += 1;
            }
            more = Thread32Next(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        count
    }
}

/// CPU seconds (user and system) the process has used so far.
#[cfg(windows)]
fn cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the four `FILETIME`s are valid for the call to fill in, and the pseudo handle of
    // the current process needs no closing.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "GetProcessTimes failed");
    // Units of 100 ns.
    let secs = |t: FILETIME| {
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7
    };
    secs(kernel) + secs(user)
}

/// CPU seconds (user and system) the process has used so far.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `ru` is a valid `rusage` for the call to fill in.
    let ru = unsafe {
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()), 0);
        ru.assume_init()
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(ru.ru_utime) + secs(ru.ru_stime)
}

/// Unix only: a Windows process has no soft descriptor limit to raise (sockets are not
/// descriptors there).
#[cfg(windows)]
fn raise_descriptor_limit() {}

#[cfg(unix)]
fn raise_descriptor_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid `rlimit` that lives through both calls.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let want = lim.rlim_max.min(10240);
        if want > lim.rlim_cur {
            lim.rlim_cur = want;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

/// The scenarios are one test, one after the other: they count the threads and the CPU time of
/// the whole process, so no other test of this file may run beside them (a second test thread,
/// even one that waits on a lock, would be counted).
#[test]
fn a_thousand_idle_waits_use_a_few_threads_and_no_cpu() {
    raise_descriptor_limit();
    a_thousand_idle_blocked_reads();
    a_thousand_idle_accepts();
    a_thousand_writes_blocked_on_full_buffers();
}

fn a_thousand_idle_blocked_reads() {
    let before = threads();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    // The peer holds every connection open and silent until the script has closed its ends.
    let peer = thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..READERS {
            held.push(listener.accept().unwrap().0);
        }
        for s in &mut held {
            s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
            assert_eq!(s.read(&mut [0u8; 1]).unwrap(), 0);
        }
    });

    let marks = Arc::new(Mutex::new(Vec::<(Instant, f64, usize)>::new()));
    let out = colibri::SharedBuffer::new();
    // Pinned to the tree-walker: this tests the standard library, not the engine, and host
    // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).unwrap();
    let m = Arc::clone(&marks);
    vm.register_fn("mark", move || {
        m.lock()
            .unwrap()
            .push((Instant::now(), cpu_seconds(), threads()));
    })
    .unwrap();
    vm.load_source(
        "app/main.colibri",
        &format!(
            r#"import net

fn reading(c: net.Conn) -> str {{
    match c.read(1) {{
        ok(_) => return "data"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    var conns: List[net.Conn] = []
    var readers: List[Coroutine[str]] = []
    for i in 0..{READERS} {{
        let c = net.connect("{addr}")?
        conns.push(c)
        readers.push(go reading(c))
    }}
    // Every reader is waiting by now, and has been for a moment.
    wait(0.5)
    mark()
    wait({IDLE_SECONDS})
    mark()
    for c in conns {{
        c.close()
    }}
    var closed = 0
    for r in readers {{
        if r.join() == "Closed" {{
            closed += 1
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#
        ),
    )
    .unwrap();
    vm.run_main().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.contents(), format!("closed {READERS}\n"));
    peer.join().unwrap();

    let marks = marks.lock().unwrap().clone();
    let ((t0, c0, n0), (t1, c1, n1)) = (marks[0], marks[1]);
    let cores = (c1 - c0) / t1.duration_since(t0).as_secs_f64();
    println!(
        "{READERS} idle blocked reads: {cores:.3} cores, {before} threads before, {n0} then, {n1} later"
    );
    assert!(
        n0 <= before + MAY_GROW_BY && n1 <= before + MAY_GROW_BY,
        "{READERS} idle blocked reads: {n0} and {n1} threads, from {before}"
    );
    // The reads before the poller used 0.23 of a core in a release build and more in a debug
    // build, in wake-ups alone; now nothing wakes at all while nothing happens.
    assert!(
        cores < 0.05,
        "{READERS} idle blocked reads used {cores:.3} cores"
    );

    // Dropping the `Vm` ends the poller (joined) and the threads of the pool (they end by
    // themselves, so wait for them, with a ceiling).
    drop(vm);
    let began = Instant::now();
    while threads() > before {
        assert!(
            began.elapsed() < LIMIT,
            "{} threads are left from {before} after the Vm was dropped",
            threads()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// A thousand listeners, each with a coroutine in `accept` and no connection coming: one poller
/// thread and no CPU. Before the poller each of them held a pool thread that woke every slice.
/// (A thousand listeners are a thousand descriptors; the other scenario of this file has a
/// thousand connections, two thousand descriptors, and has ended before this one starts.)
fn a_thousand_idle_accepts() {
    let before = threads();
    let marks = Arc::new(Mutex::new(Vec::<(Instant, f64, usize)>::new()));
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).unwrap();
    let m = Arc::clone(&marks);
    vm.register_fn("mark", move || {
        m.lock()
            .unwrap()
            .push((Instant::now(), cpu_seconds(), threads()));
    })
    .unwrap();
    vm.load_source(
        "app/main.colibri",
        &format!(
            r#"import net

fn accepting(l: net.Listener) -> str {{
    match l.accept() {{
        ok(_) => return "connection"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    var listeners: List[net.Listener] = []
    var acceptors: List[Coroutine[str]] = []
    for i in 0..{LISTENERS} {{
        let l = net.listen("127.0.0.1:0")?
        listeners.push(l)
        acceptors.push(go accepting(l))
    }}
    // Every accept is waiting by now, and has been for a moment.
    wait(0.5)
    mark()
    wait({IDLE_SECONDS})
    mark()
    for l in listeners {{
        l.close()
    }}
    var closed = 0
    for a in acceptors {{
        if a.join() == "Closed" {{
            closed += 1
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#
        ),
    )
    .unwrap();
    vm.run_main().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.contents(), format!("closed {LISTENERS}\n"));

    let marks = marks.lock().unwrap().clone();
    let ((t0, c0, n0), (t1, c1, n1)) = (marks[0], marks[1]);
    let cores = (c1 - c0) / t1.duration_since(t0).as_secs_f64();
    println!(
        "{LISTENERS} idle accepts: {cores:.3} cores, {before} threads before, {n0} then, {n1} later"
    );
    assert!(
        n0 <= before + MAY_GROW_BY && n1 <= before + MAY_GROW_BY,
        "{LISTENERS} idle accepts: {n0} and {n1} threads, from {before}"
    );
    assert!(
        cores < 0.05,
        "{LISTENERS} idle accepts used {cores:.3} cores"
    );
    drop(vm);
    let began = Instant::now();
    while threads() > before {
        assert!(
            began.elapsed() < LIMIT,
            "{} threads are left from {before} after the Vm was dropped",
            threads()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// How many writers the blocked-writes scenario runs. A blocked writer parks about one full send
/// buffer of the connection in the kernel, and the buffer size is the system's default: `net`
/// gives a script no way to make it smaller. Linux and Windows take that memory from the general
/// pool, so they run `WRITERS`. macOS takes it from a fixed pool of 2 KiB mbuf clusters
/// (`kern.ipc.nmbclusters`), and a thousand default send buffers (`net.inet.tcp.sendspace`,
/// 128 KiB; auto-sizing does not grow them here, since the peer's window stays small, and
/// `net.inet.tcp.autosndbufmax` is printed only to show that) can exhaust the pool of a small
/// machine: `connect` then fails with "no buffer space available". There the count is at most a
/// quarter of the pool, in send buffers, and never more than `WRITERS`; a pool that allows fewer
/// than `MIN_MAC_WRITERS` fails the test with the numbers instead of running it with a handful.
#[cfg(not(target_os = "macos"))]
fn writer_count() -> usize {
    WRITERS
}

#[cfg(target_os = "macos")]
const MIN_MAC_WRITERS: usize = 100;

#[cfg(target_os = "macos")]
fn writer_count() -> usize {
    fn sysctl(name: &str) -> Option<u64> {
        let out = std::process::Command::new("sysctl")
            .args(["-n", name])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    let clusters = sysctl("kern.ipc.nmbclusters").expect("kern.ipc.nmbclusters");
    let sendspace = sysctl("net.inet.tcp.sendspace").expect("net.inet.tcp.sendspace");
    let autosndbufmax = sysctl("net.inet.tcp.autosndbufmax");
    let pool = clusters * 2048;
    let count = ((pool / 4 / sendspace.max(1)) as usize).min(WRITERS);
    let line = format!(
        "macOS buffer pool: nmbclusters {clusters} ({pool} bytes), sendspace {sendspace}, \
         autosndbufmax {autosndbufmax:?}: {count} writers"
    );
    println!("{line}");
    // Also straight to the error stream, which the test harness does not capture, so that the
    // numbers are in the log of a run that passes as well.
    {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{line}");
    }
    assert!(
        count >= MIN_MAC_WRITERS,
        "the buffer pool allows only {count} blocked writers (nmbclusters {clusters}, \
         sendspace {sendspace}, autosndbufmax {autosndbufmax:?})"
    );
    count
}

/// A thousand writes, each blocked because the peer does not read and the buffers are full: one
/// poller thread and no CPU, and the writers end with `Closed` when their connections are closed
/// (a write that had gone through instead would show as a different end, so the count of
/// `Closed` also says that every write was blocked all along). The peer asks for a small receive
/// buffer on its listener, which the accepted sockets inherit, so that a few megabytes fill a
/// connection; the bytes are one `Bytes` that every writer shares.
fn a_thousand_writes_blocked_on_full_buffers() {
    let writers = writer_count();
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let before = threads();
    let listener = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    listener.set_recv_buffer_size(SMALL_BUFFER).unwrap();
    #[cfg(target_os = "linux")]
    listener.set_tcp_mss(536).unwrap();
    listener
        .bind(&SockAddr::from(
            "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .unwrap();
    listener.listen(1024).unwrap();
    let addr = listener
        .local_addr()
        .unwrap()
        .as_socket()
        .unwrap()
        .to_string();
    let (release, released) = std::sync::mpsc::channel::<()>();
    // The peer holds every connection open and never reads, until the script has ended.
    let peer = thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..writers {
            held.push(listener.accept().unwrap().0);
        }
        let _ = released.recv();
    });

    let marks = Arc::new(Mutex::new(Vec::<(Instant, f64, usize)>::new()));
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).unwrap();
    let m = Arc::clone(&marks);
    vm.register_fn("mark", move || {
        m.lock()
            .unwrap()
            .push((Instant::now(), cpu_seconds(), threads()));
    })
    .unwrap();
    vm.load_source(
        "app/main.colibri",
        &format!(
            r#"import net

fn writing(c: net.Conn, data: Bytes) -> str {{
    match c.write_bytes(data) {{
        ok(_) => return "written"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let block = BytesBuilder()
    for i in 0..1024 {{
        block.push(u8(65))
    }}
    let data = BytesBuilder()
    for i in 0..{blocks} {{
        data.push_bytes(block.to_bytes())
    }}
    let bytes = data.to_bytes()
    var conns: List[net.Conn] = []
    var writers: List[Coroutine[str]] = []
    for i in 0..{writers} {{
        let c = net.connect("{addr}")?
        conns.push(c)
        writers.push(go writing(c, bytes))
    }}
    // Every writer has filled its buffers and is waiting by now, and has been for a moment.
    wait(0.5)
    mark()
    wait({IDLE_SECONDS})
    mark()
    for c in conns {{
        c.close()
    }}
    var closed = 0
    for w in writers {{
        if w.join() == "Closed" {{
            closed += 1
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#,
            blocks = WRITE_BYTES / 1024
        ),
    )
    .unwrap();
    let began = Instant::now();
    vm.run_main().unwrap_or_else(|e| panic!("{e}"));
    println!(
        "{writers} blocked writes: the script took {:?}",
        began.elapsed()
    );
    assert_eq!(out.contents(), format!("closed {writers}\n"));
    release.send(()).unwrap();
    peer.join().unwrap();

    let marks = marks.lock().unwrap().clone();
    let ((t0, c0, n0), (t1, c1, n1)) = (marks[0], marks[1]);
    let cores = (c1 - c0) / t1.duration_since(t0).as_secs_f64();
    println!(
        "{writers} blocked writes: {cores:.3} cores, {before} threads before, {n0} then, {n1} later"
    );
    assert!(
        n0 <= before + MAY_GROW_BY && n1 <= before + MAY_GROW_BY,
        "{writers} blocked writes: {n0} and {n1} threads, from {before}"
    );
    assert!(
        cores < 0.05,
        "{writers} blocked writes used {cores:.3} cores"
    );
    drop(vm);
    let began = Instant::now();
    while threads() > before {
        assert!(
            began.elapsed() < LIMIT,
            "{} threads are left from {before} after the Vm was dropped",
            threads()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
