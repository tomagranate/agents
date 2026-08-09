use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct Shared {
    message: Mutex<String>,
    done: AtomicBool,
}

pub struct Activity {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    interactive: bool,
    started: Instant,
    finished: bool,
}

impl Activity {
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let interactive = io::stderr().is_terminal();
        let shared = Arc::new(Shared {
            message: Mutex::new(message.clone()),
            done: AtomicBool::new(false),
        });
        let worker = interactive.then(|| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || draw_spinner(&shared))
        });
        if !interactive {
            eprintln!("{message}...");
        }
        Self {
            shared,
            worker,
            interactive,
            started: Instant::now(),
            finished: false,
        }
    }

    pub fn set_message(&self, message: impl Into<String>) {
        let message = message.into();
        if let Ok(mut current) = self.shared.message.lock() {
            *current = message.clone();
        }
        if !self.interactive {
            eprintln!("{message}...");
        }
    }

    pub fn finish(mut self, message: impl AsRef<str>) {
        self.stop();
        let elapsed = format_elapsed(self.started.elapsed());
        eprintln!("✓ {} ({elapsed})", message.as_ref());
        self.finished = true;
    }

    fn stop(&mut self) {
        self.shared.done.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if self.interactive {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if !self.finished {
            self.stop();
        }
    }
}

fn draw_spinner(shared: &Shared) {
    let mut frame = 0;
    while !shared.done.load(Ordering::Acquire) {
        let message = shared
            .message
            .lock()
            .map(|message| message.clone())
            .unwrap_or_else(|_| "Working".to_owned());
        eprint!("\r\x1b[2K{} {message}", FRAMES[frame % FRAMES.len()]);
        let _ = io::stderr().flush();
        frame += 1;
        thread::sleep(Duration::from_millis(80));
    }
}

fn format_elapsed(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        format!("{} ms", duration.as_millis())
    } else {
        format!(
            "{}.{:01} s",
            duration.as_secs(),
            duration.subsec_millis() / 100
        )
    }
}
