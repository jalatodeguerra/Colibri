//! A thousand idle keep-alive connections to an `httpd` server cost a few threads and no CPU:
//! the server waits on sockets with the one poller of its `Vm`, its connections are coroutines,
//! and nothing ticks while no handler runs (the sweeper waits on a signal). A test file of its
//! own because it counts the threads and the CPU time of the whole process, which other tests
//! running beside it would pollute. The counts are read as `net_idle_threads.rs` reads them
//! (`/proc/self/task`, `proc_pidinfo`, a toolhelp snapshot; `getrusage`, `GetProcessTimes`).
#![cfg(any(target_os = "linux", target_os = "macos", windows))]

#[path = "support/wire.rs"]
#[allow(dead_code)]
mod wire;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;
use wire::{EVENT, Wire, get, request};

const CONNECTIONS: usize = 1000;
const WAVE: usize = 100;
const IDLE_SECONDS: f64 = 3.0;
const LIMIT: Duration = Duration::from_secs(60);

/// How many threads the process may have more than before the test: the poller, the server's
/// thread and the few that the connects used.
const MAY_GROW_BY: usize = 8;

/// The CPU time the process may use over the idle seconds, as a share of one core.
const MAX_SHARE: f64 = 0.05;

#[cfg(target_os = "linux")]
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("lists the threads")
        .count()
}

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

/// Raises the soft descriptor limit (each connection costs two descriptors in this process).
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

#[cfg(windows)]
fn raise_descriptor_limit() {}

const PROGRAM: &str = r#"import httpd

fn main() -> Result[()] {
    let server = httpd.listen("127.0.0.1:0")?
    publish(server.addr())
    let handler = fn(r: httpd.Request) -> Result[httpd.Response] {
        if r.path == "/__stop" {
            server.stop(5.0)
            return ok(httpd.text(200, "stopping\n"))
        }
        if r.path == "/__stats" {
            return ok(httpd.text(200, "{server.connections()}\n"))
        }
        return ok(httpd.text(200, "hello\n"))
    }
    return server.serve(handler)
}
"#;

#[test]
fn a_thousand_idle_connections_use_a_few_threads_and_no_cpu_and_are_all_served_again() {
    raise_descriptor_limit();
    let threads_before = threads();
    let (addr_tx, addr_rx) = mpsc::channel::<String>();
    let (done_tx, done) = mpsc::channel();
    thread::spawn(move || {
        // Pinned to the tree-walker: this tests the standard library, not the engine, and host
        // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
        let mut vm = colibri::Vm::builder()
            // vm-pin: 2c
            .engine(colibri::EngineKind::Tree)
            .stdout(Box::new(colibri::SharedBuffer::new()))
            .allow(colibri::Cap::Net)
            .allow(colibri::Cap::Os)
            .build();
        colibri_std::install(&mut vm).expect("installs");
        vm.register_fn("publish", move |addr: String| {
            let _ = addr_tx.send(addr);
        })
        .expect("registers publish");
        let result = vm
            .load_source("app/main.colibri", PROGRAM)
            .and_then(|()| vm.run_main())
            .and_then(|()| vm.run_event_loop())
            .map_err(|e| e.render(false));
        let _ = done_tx.send(result);
        // The `Vm` is dropped here: its threads must be gone afterwards.
    });
    let addr = addr_rx.recv_timeout(EVENT).expect("the server's address");

    // The connections, in waves, each with one keep-alive request answered.
    let started = Instant::now();
    let mut clients: Vec<Wire> = Vec::new();
    for _ in 0..CONNECTIONS / WAVE {
        let mut wave: Vec<Wire> = (0..WAVE).map(|_| Wire::connect(&addr)).collect();
        for w in &mut wave {
            assert_eq!(w.call(get("/")).status, 200);
        }
        clients.append(&mut wave);
        assert!(started.elapsed() < LIMIT, "the connections took too long");
    }

    // The server has all of them (and the one that asks).
    let mut asker = Wire::connect(&addr);
    let open: usize = asker.call(get("/__stats")).text().trim().parse().unwrap();
    assert_eq!(open, CONNECTIONS + 1);

    // Idle: the threads stay few and the CPU time stays near nothing.
    let threads_idle = threads();
    let cpu_start = cpu_seconds();
    let t0 = Instant::now();
    thread::sleep(Duration::from_secs_f64(IDLE_SECONDS));
    let seconds = t0.elapsed().as_secs_f64();
    let used = cpu_seconds() - cpu_start;
    assert!(
        threads_idle <= threads_before + MAY_GROW_BY,
        "{threads_idle} threads with {CONNECTIONS} idle connections, {threads_before} before"
    );
    assert!(
        threads() <= threads_before + MAY_GROW_BY,
        "{} threads after the idle seconds",
        threads()
    );
    assert!(
        used < MAX_SHARE * seconds,
        "{used:.3} s of CPU in {seconds:.2} s idle: over {MAX_SHARE} of a core"
    );

    // Every connection makes a second request, and every one is answered.
    for w in &mut clients {
        let r = w.call(get("/"));
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "hello\n");
    }

    // Stop; the run ends, and the `Vm`'s threads go with it.
    let r = asker.call(request("POST", "/__stop", &["Content-Length: 0"], b""));
    assert_eq!(r.status, 200);
    drop(asker);
    drop(clients);
    match done.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => {}
        other => panic!("the server did not end well: {other:?}"),
    }
    let start = Instant::now();
    while threads() > threads_before + 1 {
        assert!(
            start.elapsed() < EVENT,
            "{} threads left after the server ended, {threads_before} before",
            threads()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
