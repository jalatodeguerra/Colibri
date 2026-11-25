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
