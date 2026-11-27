//! Shared by the test binaries that read the stderr of a server they spawned.

#![allow(dead_code)]

/// What a server child has printed on stderr so far, read on a thread so the pipe never fills.
pub struct Tap(std::sync::Arc<(std::sync::Mutex<String>, std::sync::atomic::AtomicBool)>);

/// Starts reading the piped stderr of `child`.
pub fn tap_stderr(child: &mut std::process::Child) -> Tap {
    use std::io::Read;
    let shared = std::sync::Arc::new((
        std::sync::Mutex::new(String::new()),
        std::sync::atomic::AtomicBool::new(false),
    ));
    let mut err = child.stderr.take().expect("stderr is piped");
    let mine = std::sync::Arc::clone(&shared);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = String::from_utf8_lossy(&buf[..n]);
            mine.0.lock().unwrap().push_str(&chunk);
        }
        mine.1.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Tap(shared)
}

impl Tap {
    /// The stderr so far; once the child has ended, waits a moment for the rest.
    pub fn text(&self) -> String {
        for _ in 0..100 {
            if self.0.1.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.0.0.lock().unwrap().clone()
    }

    /// The address a server announced as `listening on ADDR`. Anything else is a failure to
    /// start, and the failure shows what the server said on stderr.
    pub fn listening_addr(&self, first: &str) -> String {
        match first.trim().strip_prefix("listening on ") {
            Some(addr) => addr.to_string(),
            None => {
                let err = self.text();
                panic!("unexpected first line {first:?}; stderr: {err}")
            }
        }
    }
}

// Damaged programs on macOS. A program built there is signed, and the signature is the last
// thing in the file, after the pack. A test that changes bytes of the pack, or cuts the end of
// the file, must change the *unsigned* layout (the trailer last, as on the other systems) and
// sign the result again, or the kernel refuses the file before the reader ever sees it.

/// The program as the other systems have it: the signature cut off, the trailer the last bytes.
/// A file that is not a thin Mach-O is returned as it is.
pub fn plain(signed: Vec<u8>) -> Vec<u8> {
    use colibri_lang::macho;
    if !macho::is_thin(&signed) {
        return signed;
    }
    let (offset, len) = colibri_lang::pack::payload_range(&mut std::io::Cursor::new(&signed))
        .expect("a readable trailer")
        .expect("a built program has a trailer");
    let end = (offset + len) as usize + colibri_lang::pack::TRAILER_LEN;
    let head = macho::head_len(&signed).expect("a Mach-O head");
    let prepared = macho::prepare(&signed[..head], signed.len() as u64).expect("a signed runner");
    let mut out = signed[..end].to_vec();
    out[..head].copy_from_slice(&prepared.head_with(end as u64));
    out
}

/// Makes `bytes`, a changed copy of `plain` output that was `unsigned_len` bytes long, a file
/// that can be signed: `__LINKEDIT` reaches its (new) end.
pub fn fit(bytes: &mut [u8], unsigned_len: u64) {
    use colibri_lang::macho;
    if !macho::is_thin(bytes) {
        return;
    }
    let head = macho::head_len(bytes).expect("a Mach-O head");
    let prepared = macho::prepare(&bytes[..head], unsigned_len).expect("an unsigned layout");
    let fitted = prepared.head_with(bytes.len() as u64);
    bytes[..head].copy_from_slice(&fitted);
}

/// Signs the file ad hoc on a Mac, so that it can be started; nothing elsewhere.
pub fn sign(path: &std::path::Path) {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("codesign")
            .args(["-s", "-", "-f"])
            .arg(path)
            .output()
            .expect("codesign is on every Mac");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `cmd.output()`, trying again while the system says the executable is busy.
///
/// A test writes an executable (`fs::write`, `fs::copy`) and starts it. Another test thread may
/// `fork` between the open for writing and its close: the child holds a copy of the write
/// descriptor until it reaches `exec` (the standard library marks its own descriptors close on
/// exec, but that only helps once the child gets there), and running the file in that window
/// fails with `ETXTBSY` ("Text file busy"). The window is a few microseconds, so a short,
/// bounded wait is enough; anything else, or a window that does not close, is an error.
pub fn output_of(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match cmd.output() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < give_up =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
    }
}
