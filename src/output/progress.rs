use std::{
    io::{self, IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::cli::OutputFormat;

pub struct ProgressIndicator {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    active: bool,
}

impl ProgressIndicator {
    pub fn start(format: &OutputFormat, message: impl Into<String>) -> Self {
        if !should_show_progress(format, io::stderr().is_terminal()) {
            return Self {
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
                active: false,
            };
        }

        let message = message.into();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let frames = ["-", "\\", "|", "/"];
            let mut idx = 0usize;
            while thread_running.load(Ordering::Relaxed) {
                let _ = write!(io::stderr(), "\r{} {}", frames[idx % frames.len()], message);
                let _ = io::stderr().flush();
                idx = idx.wrapping_add(1);
                thread::sleep(Duration::from_millis(120));
            }
            let _ = write!(io::stderr(), "\r{}\r", " ".repeat(message.len() + 4));
            let _ = io::stderr().flush();
        });
        Self {
            running,
            handle: Some(handle),
            active: true,
        }
    }

    pub fn finish(mut self, message: impl AsRef<str>) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.active {
            let message = message.as_ref();
            if !message.is_empty() {
                let _ = writeln!(io::stderr(), "{message}");
            }
        }
    }
}

fn should_show_progress(format: &OutputFormat, stderr_is_terminal: bool) -> bool {
    *format == OutputFormat::Text && stderr_is_terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_indicator_is_enabled_only_for_text_tty_output() {
        assert!(should_show_progress(&OutputFormat::Text, true));
        assert!(!should_show_progress(&OutputFormat::Text, false));
        assert!(!should_show_progress(&OutputFormat::Json, true));
        assert!(!should_show_progress(&OutputFormat::CompactJson, true));
        assert!(!should_show_progress(&OutputFormat::Jsonl, true));
    }
}
