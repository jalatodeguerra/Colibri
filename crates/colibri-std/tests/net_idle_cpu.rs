//! What a thousand idle blocked reads cost. This is a test file of its own because it measures
//! the CPU time of the whole process, which the other tests of `net` (they run in threads of one
//! process) would pollute. The process's CPU time is read with `getrusage` on Unix and with
//! `GetProcessTimes` on Windows: no portable call gives it.

mod common;
use common::vm_pending;

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;

const READERS: usize = 1000;
const IDLE_SECONDS: f64 = 3.0;

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

#[test]
fn a_thousand_idle_blocked_reads_use_a_fraction_of_a_core() {
    vm_pending(
        "a_thousand_idle_blocked_reads_use_a_fraction_of_a_core",
        || {
            raise_descriptor_limit();
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

            let marks = Arc::new(Mutex::new(Vec::<(Instant, f64)>::new()));
            let out = colibri::SharedBuffer::new();
            let mut vm = colibri::Vm::builder()
                .stdout(Box::new(out.clone()))
                .allow(colibri::Cap::Net)
                .build();
            colibri_std::install(&mut vm).unwrap();
            let m = Arc::clone(&marks);
            vm.register_fn("mark", move || {
                m.lock().unwrap().push((Instant::now(), cpu_seconds()));
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
    let mut conns: List[net.Conn] = []
    let mut readers: List[Coroutine[str]] = []
    for i in 0..{READERS} {{
        let c = net.connect("{addr}")?
        conns.push(c)
        readers.push(go reading(c))
    }}
    // Every reader is on its worker by now, and has been waiting for a moment.
    wait(0.5)
    mark()
    wait({IDLE_SECONDS})
    mark()
    for c in conns {{
        c.close()
    }}
    let mut closed = 0
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

            let marks = marks.lock().unwrap();
            let ((t0, c0), (t1, c1)) = (marks[0], marks[1]);
            let cores = (c1 - c0) / t1.duration_since(t0).as_secs_f64();
            println!("{READERS} idle blocked reads: {cores:.3} cores");
            // Before the fix 1,000 idle reads used 1.9 cores in a debug build and 0.32 in a release
            // build (every 50 ms slice zero-filled a buffer, and queued operations woke every 10 ms);
            // now about 0.23 in both, which is the wake-ups themselves. The bound is far above that,
            // for a slow machine, and below what the old code used in a debug build.
            assert!(
                cores < 0.9,
                "{READERS} idle blocked reads used {cores:.3} cores"
            );
        },
    )
}
