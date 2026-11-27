//! A block of status lines, one per task, redrawn in place on a terminal.
//! Pulls run in parallel, so each image gets one line that changes as it
//! goes, rather than a stream of interleaved messages. When stdout is not
//! a terminal, only each task's final line is printed.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Board {
    tty: bool,
    state: Mutex<State>,
}

struct State {
    lines: Vec<String>,
    /// How many lines the last redraw printed.
    drawn: usize,
    last: Option<Instant>,
}

/// Redraws at most this often while tasks report progress.
const EVERY: Duration = Duration::from_millis(100);

impl Default for Board {
    fn default() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }
}

impl Board {
    pub fn new(tty: bool) -> Self {
        Self {
            tty,
            state: Mutex::new(State {
                lines: Vec::new(),
                drawn: 0,
                last: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Adds a line and returns its index.
    pub fn add(&self, text: String) -> usize {
        let mut s = self.lock();
        s.lines.push(text);
        let i = s.lines.len() - 1;
        if self.tty {
            self.redraw(&mut s);
        }
        i
    }

    /// Changes a line, redrawing when enough time has passed.
    pub fn set(&self, i: usize, text: String) {
        let mut s = self.lock();
        if let Some(l) = s.lines.get_mut(i) {
            *l = text;
        }
        if self.tty && s.last.is_none_or(|t| t.elapsed() >= EVERY) {
            self.redraw(&mut s);
        }
    }

    /// Sets a task's final line and shows it now.
    pub fn finish(&self, i: usize, text: String) {
        let mut s = self.lock();
        if let Some(l) = s.lines.get_mut(i) {
            l.clone_from(&text);
        }
        if self.tty {
            self.redraw(&mut s);
        } else {
            println!("{text}");
        }
    }

    fn redraw(&self, s: &mut State) {
        let mut out = std::io::stdout().lock();
        let mut text = String::new();
        if s.drawn > 0 {
            text.push_str(&format!("\x1b[{}A", s.drawn));
        }
        for l in &s.lines {
            text.push_str("\r\x1b[2K");
            text.push_str(l);
            text.push('\n');
        }
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        s.drawn = s.lines.len();
        s.last = Some(Instant::now());
    }

    /// The lines as they stand, for tests.
    pub fn lines(&self) -> Vec<String> {
        self.lock().lines.clone()
    }
}

/// Binary units, as `docker` prints them.
pub fn bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", units[i])
    }
}

/// The short form of an image ID: 12 hex digits after `sha256:`.
pub fn short_id(id: &str) -> &str {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    &hex[..hex.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_keeps_only_final_lines() {
        let b = Board::new(false);
        let i = b.add("a: starting".into());
        b.set(i, "a: 50%".into());
        b.finish(i, "a: done".into());
        assert_eq!(b.lines(), ["a: done"]);
    }

    #[test]
    fn ids_and_units() {
        assert_eq!(short_id("sha256:4f1c2d3e4f5a6b7c8d9e"), "4f1c2d3e4f5a");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(bytes(3 * 1024 * 1024 / 2), "1.5MiB");
    }
}
