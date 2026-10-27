//! Model downloads shared by the ASR backends: one HTTP client policy and a
//! copy loop that can say how far it has got.
//!
//! The models are large (whisper medium ~1.5 GB, parakeet ~640 MB) and the
//! installers fetch them through `claudebase daemon warmup --asr`. On a slow
//! line that is many minutes, and it used to be many minutes of nothing at all:
//! the copy loop printed no progress, the installers discarded the command's
//! output, and an operator watching `downloading the whisper model ...` for
//! eighteen minutes reasonably concluded it had hung and interrupted it.

use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Print download progress to stdout. Off by default: the daemon also
/// downloads (on the first voice note) and its stdout may be a terminal that
/// belongs to someone else's TUI. `claudebase daemon warmup` turns it on.
/// Stdout rather than stderr so the Windows installer can show it — under
/// `$ErrorActionPreference = 'Stop'` a redirected stderr line is an error.
pub static REPORT_PROGRESS: AtomicBool = AtomicBool::new(false);

/// How long a download may go without receiving a single byte before it is
/// abandoned. Every backend keeps or can redo what it already has, so failing
/// a stalled connection quickly and retrying beats sitting on it: this was 15
/// minutes for whisper and unbounded for parakeet, which is what a real hang
/// looks like.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Between progress lines when stdout is not a terminal (an installer run over
/// ssh or by an agent pipes it): lines, since a pipe cannot redraw.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// Between redraws of the bar on a terminal.
const REDRAW_EVERY: Duration = Duration::from_millis(250);
const BAR_WIDTH: usize = 24;

/// The client every model download uses. reqwest's blocking `timeout` bounds
/// the wait for the response headers and each read of the body separately —
/// it is not a deadline on the whole transfer (reqwest 0.12
/// `blocking/client.rs` keeps it out of the inner client;
/// `blocking/response.rs` applies it per `read`), so it is the stall timeout
/// here and a slow but moving download is never cut off.
pub fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(STALL_TIMEOUT)
        .build()
        .context("build http client for model download")
}

/// `io::copy` that reports progress while [`REPORT_PROGRESS`] is set.
/// `already` is what a resumed download had before this call; `total` is the
/// full size when known. Returns the bytes copied by this call.
pub fn copy_with_progress(
    label: &str,
    from: &mut impl Read,
    to: &mut impl Write,
    already: u64,
    total: Option<u64>,
) -> io::Result<u64> {
    let report = REPORT_PROGRESS.load(Ordering::Relaxed);
    let mut bar = report.then(|| Redraw::new(io::stdout().is_terminal()));
    if report && !bar.as_ref().is_some_and(|b| b.tty) {
        let size = total.map(|t| format!(" of {}", mb(t))).unwrap_or_default();
        if already > 0 {
            println!("{label}: resuming at {}{size}", mb(already));
        } else {
            println!("{label}: downloading{size}");
        }
    }
    let start = Instant::now();
    let mut last = start;
    let mut copied = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        copied += n as u64;
        match bar.as_mut() {
            Some(b) if b.tty && last.elapsed() >= REDRAW_EVERY => {
                last = Instant::now();
                b.draw(&bar_line(label, already + copied, total, copied, start.elapsed()));
            }
            Some(b) if !b.tty && last.elapsed() >= REPORT_EVERY => {
                last = Instant::now();
                println!("{}", progress_line(label, already + copied, total, copied, start.elapsed()));
                let _ = io::stdout().flush();
            }
            _ => {}
        }
    }
    match bar.as_mut() {
        Some(b) if b.tty => {
            b.draw(&bar_line(label, already + copied, total, copied, start.elapsed()));
            println!();
        }
        Some(_) => println!("{}", progress_line(label, already + copied, total, copied, start.elapsed())),
        None => {}
    }
    Ok(copied)
}

/// One line on a terminal, rewritten in place: `\r` and padding over what
/// the last draw left, no ANSI codes (a legacy Windows console prints them).
pub struct Redraw {
    pub tty: bool,
    width: usize,
}

impl Redraw {
    pub fn new(tty: bool) -> Self {
        Self { tty, width: 0 }
    }

    pub fn draw(&mut self, line: &str) {
        let len = line.chars().count();
        let pad = self.width.saturating_sub(len);
        print!("\r{line}{}", " ".repeat(pad));
        self.width = len;
        let _ = io::stdout().flush();
    }
}

/// `whisper model [#########---------------] 41%  640 / 1533 MB  1.4 MB/s  ~11 min`
/// — the terminal form of [`progress_line`]. Without a known size: no bar.
pub fn bar_line(label: &str, done: u64, total: Option<u64>, copied_now: u64, elapsed: Duration) -> String {
    let rate = copied_now as f64 / elapsed.as_secs_f64().max(0.001);
    let speed = if copied_now > 0 { format!("  {:.1} MB/s", rate / 1e6) } else { String::new() };
    let Some(total) = total.filter(|t| *t > 0) else {
        return format!("{label}  {}{speed}", mb(done));
    };
    let frac = (done as f64 / total as f64).clamp(0.0, 1.0);
    let filled = (frac * BAR_WIDTH as f64).round() as usize;
    let mut line = format!(
        "{label} [{}{}] {:>3.0}%  {} / {}{speed}",
        "#".repeat(filled),
        "-".repeat(BAR_WIDTH - filled),
        frac * 100.0,
        done / 1_000_000,
        mb(total)
    );
    if rate > 0.0 && done < total {
        line.push_str(&format!("  ~{}", eta((total - done) as f64 / rate)));
    }
    line
}

/// `whisper model: 640 MB of 1533 MB (41%), 1.4 MB/s, ~11 min left`
pub fn progress_line(
    label: &str,
    done: u64,
    total: Option<u64>,
    copied_now: u64,
    elapsed: Duration,
) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    let rate = copied_now as f64 / secs;
    let mut line = format!("{label}: {}", mb(done));
    if let Some(total) = total.filter(|t| *t > 0) {
        let pct = (done as f64 / total as f64 * 100.0).min(100.0);
        line.push_str(&format!(" of {} ({pct:.0}%)", mb(total)));
        if rate > 0.0 && done < total {
            let left = (total - done) as f64 / rate;
            line.push_str(&format!(", {:.1} MB/s, ~{} left", rate / 1e6, eta(left)));
            return line;
        }
    }
    line.push_str(&format!(", {:.1} MB/s", rate / 1e6));
    line
}

fn mb(bytes: u64) -> String {
    format!("{} MB", bytes / 1_000_000)
}

fn eta(secs: f64) -> String {
    if secs < 90.0 {
        format!("{} s", secs.round() as u64)
    } else {
        format!("{} min", (secs / 60.0).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_reads_like_a_person_wrote_it() {
        let line = progress_line(
            "whisper model",
            640_000_000,
            Some(1_533_000_000),
            140_000_000,
            Duration::from_secs(100),
        );
        assert_eq!(line, "whisper model: 640 MB of 1533 MB (42%), 1.4 MB/s, ~11 min left");
        // Unknown size: no percentage, no estimate.
        let line = progress_line("parakeet model", 5_000_000, None, 5_000_000, Duration::from_secs(5));
        assert_eq!(line, "parakeet model: 5 MB, 1.0 MB/s");
        // Done: no estimate.
        let line = progress_line("m", 10_000_000, Some(10_000_000), 10_000_000, Duration::from_secs(10));
        assert_eq!(line, "m: 10 MB of 10 MB (100%), 1.0 MB/s");
    }

    #[test]
    fn the_bar_fills_with_the_download() {
        let line = bar_line("whisper model", 640_000_000, Some(1_533_000_000), 140_000_000, Duration::from_secs(100));
        assert_eq!(line, "whisper model [##########--------------]  42%  640 / 1533 MB  1.4 MB/s  ~11 min");
        let line = bar_line("m", 10_000_000, Some(10_000_000), 10_000_000, Duration::from_secs(10));
        assert_eq!(line, "m [########################] 100%  10 / 10 MB  1.0 MB/s");
        assert_eq!(bar_line("p", 5_000_000, None, 0, Duration::from_secs(1)), "p  5 MB");
    }

    #[test]
    fn copy_reports_nothing_unless_asked_and_copies_everything() {
        let data = vec![7u8; 1_000_000];
        let mut out = Vec::new();
        let n = copy_with_progress("x", &mut data.as_slice(), &mut out, 0, Some(1_000_000)).unwrap();
        assert_eq!(n, 1_000_000);
        assert_eq!(out, data);
    }
}
