//! Slice 0 spike — inject text into a child TUI's input by owning its PTY.
//!
//! Two modes:
//!
//! * `--selftest` — spawns `cat` (no TTY of our own required), injects, and
//!   asserts the injected bytes come back on the master. Verifies the PTY
//!   plumbing itself, automatable in CI. Exits non-zero on failure.
//!
//! * default — spawns the real command (`claude` unless `--cmd` says
//!   otherwise), proxies the operator's terminal both ways, and injects the
//!   payload after `--delay-ms`. This is the half a human has to watch:
//!   whether the TUI renders the block, whether it submits, and what it does
//!   to a half-typed line.
//!
//! Everything the spike does is timestamped into `--log` so the observation
//! is a file, not a memory of what the screen looked like.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// How the payload is framed on the wire into the PTY master.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Bytes as-is, exactly as if typed character by character.
    Raw,
    /// Wrapped in bracketed-paste markers. A TUI that enabled bracketed
    /// paste (DECSET 2004) treats the span as one atomic paste instead of
    /// N keystrokes — which is how a multi-line block avoids being
    /// interpreted as N separate submissions.
    Paste,
}

/// What (if anything) is appended after the payload to make the TUI act on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Submit {
    Cr,
    Lf,
    None,
}

/// One scripted action against the PTY master. A scenario is a list of these,
/// which is what lets the spike simulate an operator typing: bytes we write
/// and bytes a human types travel the exact same path into the child's input
/// queue, so `Type` is a faithful stand-in for a keyboard.
#[derive(Clone, Debug)]
enum Step {
    /// Raw keystrokes, no paste framing, no submit — "operator is typing".
    Type(String),
    /// Bracketed-paste block — "daemon injects an inbound message".
    Paste(String),
    /// The submit key on its own (F-2: must be a separate, later write).
    Submit,
    Sleep(u64),
    /// Marker written only to the spike log, to timestamp phases.
    Note(&'static str),
}

/// Which risk from Slice 3 this run is probing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    /// Single injection into an idle prompt (the Slice 0 baseline).
    Plain,
    /// Inject WHILE the model is generating: does the message queue, or is
    /// it dropped?
    Busy,
    /// Inject on top of a half-typed operator line: is the operator's text
    /// preserved, destroyed, or silently merged into the submitted message?
    Typing,
    /// Multi-line block: does it submit per-line (bad) or stay atomic?
    Multiline,
    /// Operator types a partial line and holds it for 25 s without submitting,
    /// then submits. Used to drive the SUPERVISOR (not `claude` directly) so
    /// the draft-gate can be observed live: a daemon message sent during the
    /// hold must not be injected until the line clears.
    Hold,
}

struct Args {
    cmd: String,
    cmd_args: Vec<String>,
    scenario: Scenario,
    mode: Mode,
    submit: Submit,
    delay_ms: u64,
    /// Pause between the end of the payload and the submit key. Zero means
    /// "same write". Live finding 2026-08-16: a CR written immediately after
    /// `ESC[201~` does NOT submit — the TUI is still digesting the paste — so
    /// the submit has to be a separate, later write.
    submit_delay_ms: u64,
    repeat: u32,
    interval_ms: u64,
    text: String,
    log: String,
    selftest: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            cmd: "claude".to_string(),
            cmd_args: Vec::new(),
            scenario: Scenario::Plain,
            mode: Mode::Paste,
            submit: Submit::Cr,
            delay_ms: 8_000,
            submit_delay_ms: 300,
            repeat: 1,
            interval_ms: 3_000,
            // Shaped like the frame the supervisor will actually inject: an
            // explicit envelope that marks the content as external and
            // non-authoritative. The wording matters — this is the only thing
            // separating an operator message from an instruction once
            // --dangerously-skip-permissions is default-on (plan risk R-6).
            text: "<channel source=\"telegram\" user=\"codefather_dev\" thread=\"telegram:0\" \
                   message_id=\"spike-1\">\nраз два три четыре\n</channel>"
                .to_string(),
            log: "/tmp/pty-inject-spike.log".to_string(),
            selftest: false,
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .with_context(|| format!("{arg} needs a value"))
        };
        match arg {
            "--selftest" => a.selftest = true,
            "--cmd" => a.cmd = next(&mut i)?,
            "--scenario" => {
                a.scenario = match next(&mut i)?.as_str() {
                    "plain" => Scenario::Plain,
                    "busy" => Scenario::Busy,
                    "typing" => Scenario::Typing,
                    "multiline" => Scenario::Multiline,
                    "hold" => Scenario::Hold,
                    other => anyhow::bail!("--scenario must be plain|busy|typing|multiline|hold, got {other}"),
                }
            }
            "--mode" => {
                a.mode = match next(&mut i)?.as_str() {
                    "raw" => Mode::Raw,
                    "paste" => Mode::Paste,
                    other => anyhow::bail!("--mode must be raw|paste, got {other}"),
                }
            }
            "--submit" => {
                a.submit = match next(&mut i)?.as_str() {
                    "cr" => Submit::Cr,
                    "lf" => Submit::Lf,
                    "none" => Submit::None,
                    other => anyhow::bail!("--submit must be cr|lf|none, got {other}"),
                }
            }
            "--delay-ms" => a.delay_ms = next(&mut i)?.parse()?,
            "--submit-delay-ms" => a.submit_delay_ms = next(&mut i)?.parse()?,
            "--repeat" => a.repeat = next(&mut i)?.parse()?,
            "--interval-ms" => a.interval_ms = next(&mut i)?.parse()?,
            "--text" => a.text = next(&mut i)?,
            "--log" => a.log = next(&mut i)?,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            // Everything after `--` is forwarded to the child verbatim.
            "--" => {
                a.cmd_args = argv[i + 1..].to_vec();
                break;
            }
            other => anyhow::bail!("unknown flag {other} (try --help)"),
        }
        i += 1;
    }
    Ok(a)
}

fn print_help() {
    eprintln!(
        "pty-inject-spike — Slice 0 gate for the PTY transport\n\
         \n\
         USAGE:\n  \
           pty-inject-spike [--cmd claude] [--mode paste|raw] [--submit cr|lf|none]\n  \
                            [--delay-ms 8000] [--repeat 1] [--interval-ms 3000]\n  \
                            [--text '...'] [--log /tmp/pty-inject-spike.log] [-- <child args>]\n  \
           pty-inject-spike --selftest\n\
         \n\
         SELFTEST spawns `cat` and asserts the injected bytes round-trip. No\n\
         controlling terminal required — safe to run from CI or an agent shell.\n\
         \n\
         The default mode needs a human watching the screen: it answers whether\n\
         the TUI accepts the paste, whether it submits, and what an injection\n\
         does to a half-typed line."
    );
}

/// Millisecond wall-clock stamp for the log. Deliberately not chrono — the
/// spike has no business pulling a date library for six log lines.
fn stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

struct Logger(Mutex<std::fs::File>);

impl Logger {
    fn open(path: &str) -> Result<Self> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open log {path}"))?;
        Ok(Self(Mutex::new(f)))
    }
    fn log(&self, line: &str) {
        if let Ok(mut f) = self.0.lock() {
            let _ = writeln!(f, "[{}] {}", stamp(), line);
            let _ = f.flush();
        }
    }
}

/// Frame the payload per `mode`/`submit`.
///
/// Bracketed paste is DECSET 2004: the terminal (here: us, the writer)
/// brackets pasted content so the application can tell paste from typing.
/// If the child never enabled 2004 it sees the markers as literal escape
/// junk — which is itself a finding worth logging, not a crash.
fn frame(text: &str, mode: Mode) -> Vec<u8> {
    let mut out = Vec::new();
    match mode {
        Mode::Raw => out.extend_from_slice(text.as_bytes()),
        Mode::Paste => {
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
        }
    }
    out
}

/// The submit key, written as its own later payload (see `submit_delay_ms`).
fn submit_bytes(submit: Submit) -> &'static [u8] {
    match submit {
        Submit::Cr => b"\r",
        Submit::Lf => b"\n",
        Submit::None => b"",
    }
}

/// Build the scripted step list for a scenario.
///
/// Markers are deliberately ugly ASCII (`MK-A`, `MK-OP`, ...) so they survive
/// the TUI's own line-wrapping and can be grepped out of a stripped
/// transcript without false positives.
fn script(scenario: Scenario, submit_delay: u64) -> Vec<Step> {
    match scenario {
        Scenario::Plain => vec![
            Step::Note("plain: inject into idle prompt"),
            Step::Paste("MK-A ответь ровно одним словом: альфа".into()),
            Step::Sleep(submit_delay),
            Step::Submit,
        ],
        // First give the model something slow enough that the second message
        // lands mid-generation, then inject while it is still talking.
        Scenario::Busy => vec![
            Step::Note("busy: start a long generation"),
            Step::Paste("MK-A перечисли числа от 1 до 40, по одному в строке, без комментариев".into()),
            Step::Sleep(submit_delay),
            Step::Submit,
            Step::Sleep(2_500),
            Step::Note("busy: inject WHILE generating"),
            Step::Paste("MK-B ответь ровно одним словом: браво".into()),
            Step::Sleep(submit_delay),
            Step::Submit,
        ],
        // The operator has typed a half-line and not submitted it. Then an
        // inbound message arrives. This is the case the "quiet window"
        // heuristic exists for — the question is what it actually costs.
        Scenario::Typing => vec![
            Step::Note("typing: operator types a partial line, no submit"),
            Step::Type("MK-OP недописанная строка оператора".into()),
            Step::Sleep(1_500),
            Step::Note("typing: inject on top of it"),
            Step::Paste("MK-B ответь ровно одним словом: браво".into()),
            Step::Sleep(submit_delay),
            Step::Submit,
        ],
        Scenario::Hold => vec![
            Step::Note("hold: operator starts a line and does not submit"),
            Step::Type("MK-DRAFT черновик оператора".into()),
            Step::Sleep(25_000),
            Step::Note("hold: operator submits, draft gate should open"),
            Step::Submit,
        ],
        Scenario::Multiline => vec![
            Step::Note("multiline: 3-line block, one paste"),
            Step::Paste(
                "MK-A строка один\nстрока два\nстрока три — ответь одним словом: чарли".into(),
            ),
            Step::Sleep(submit_delay),
            Step::Submit,
        ],
    }
}

// ---------------------------------------------------------------------------
// Unix terminal handling (raw mode + window size)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod term {
    use anyhow::Result;

    /// Saved termios of the real terminal, restored on drop so a panic or an
    /// early exit never leaves the operator's shell in raw mode.
    pub struct RawGuard {
        fd: i32,
        saved: libc::termios,
        active: bool,
    }

    impl RawGuard {
        pub fn enter() -> Result<Option<Self>> {
            let fd = libc::STDIN_FILENO;
            if unsafe { libc::isatty(fd) } != 1 {
                // No controlling TTY — question 5 of the spike. Not an error:
                // the production supervisor must degrade to "no injection"
                // rather than refuse to launch.
                return Ok(None);
            }
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                anyhow::bail!("tcgetattr failed: {}", std::io::Error::last_os_error());
            }
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                anyhow::bail!("tcsetattr failed: {}", std::io::Error::last_os_error());
            }
            Ok(Some(Self {
                fd,
                saved,
                active: true,
            }))
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            if self.active {
                unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
                self.active = false;
            }
        }
    }

    /// Current terminal size, or None when stdin is not a TTY.
    pub fn win_size() -> Option<(u16, u16)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc != 0 || ws.ws_row == 0 || ws.ws_col == 0 {
            return None;
        }
        Some((ws.ws_row, ws.ws_col))
    }
}

#[cfg(not(unix))]
mod term {
    use anyhow::Result;
    pub struct RawGuard;
    impl RawGuard {
        pub fn enter() -> Result<Option<Self>> {
            Ok(None)
        }
    }
    pub fn win_size() -> Option<(u16, u16)> {
        None
    }
}

// ---------------------------------------------------------------------------
// Selftest — PTY plumbing only, no TUI, no human
// ---------------------------------------------------------------------------

fn selftest(log: &Logger) -> Result<()> {
    log.log("selftest: start (child=cat)");
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // `cat` echoes its stdin back to stdout. Through a PTY the line
    // discipline ALSO echoes, so a successful round-trip proves the master
    // write reached the slave's input queue — which is exactly the property
    // the whole transport rests on.
    let cmd = CommandBuilder::new("cat");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    let needle = "claudebase-pty-selftest-marker";
    writer.write_all(format!("{needle}\r").as_bytes())?;
    writer.flush()?;
    log.log(&format!("selftest: wrote marker ({} bytes)", needle.len() + 1));

    // Read with a deadline; `cat` never exits on its own, so we stop as soon
    // as the marker shows up.
    let found = Arc::new(AtomicBool::new(false));
    let found_w = found.clone();
    let handle = std::thread::spawn(move || {
        let mut acc = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if acc.contains("claudebase-pty-selftest-marker") {
                        found_w.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        acc
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !found.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }

    let ok = found.load(Ordering::SeqCst);
    // Killing `cat` closes the slave side, which ends the reader thread.
    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    let seen = handle.join().unwrap_or_default();

    log.log(&format!(
        "selftest: marker_found={ok} bytes_seen={} sample={:?}",
        seen.len(),
        seen.chars().take(120).collect::<String>()
    ));

    if !ok {
        anyhow::bail!("selftest FAILED — injected marker never came back off the PTY master");
    }
    println!("selftest OK — PTY master write reached the child's input queue");
    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive run — the half a human watches
// ---------------------------------------------------------------------------

fn interactive(a: &Args, log: &Logger) -> Result<()> {
    let (rows, cols) = term::win_size().unwrap_or((24, 80));
    log.log(&format!(
        "run: cmd={} args={:?} mode={:?} submit={:?} delay_ms={} repeat={} size={}x{}",
        a.cmd, a.cmd_args, a.mode, a.submit, a.delay_ms, a.repeat, rows, cols
    ));

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&a.cmd);
    for arg in &a.cmd_args {
        cmd.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // Marks the session for anything the child spawns — the production
    // supervisor will put the real agent id / session token here.
    cmd.env("CLAUDEBASE_PTY_SPIKE", "1");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn {}", a.cmd))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    // `dyn MasterPty + Send` is not Sync, so the Arc alone is not Send —
    // wrap in a Mutex to hand it to the resize thread.
    let master = Arc::new(Mutex::new(pair.master));

    // Raw mode LAST, so any error above still prints legibly. The guard
    // restores the terminal on every exit path.
    let _raw = term::RawGuard::enter()?;
    if _raw.is_none() {
        log.log("run: stdin is NOT a tty — proxying anyway, injection still exercised (spike question 5)");
    }

    let done = Arc::new(AtomicBool::new(false));

    // child stdout/stderr -> our stdout
    let done_r = done.clone();
    let pump_out = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut out = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        done_r.store(true, Ordering::SeqCst);
    });

    // our stdin -> child. Kept as a detached thread: a blocking read on a
    // real terminal cannot be interrupted portably, so the process exits
    // while this thread is parked on read(2) and the OS reaps it.
    let writer_in = writer.clone();
    let done_in = done.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin();
        loop {
            if done_in.load(Ordering::SeqCst) {
                break;
            }
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let Ok(mut w) = writer_in.lock() else { break };
                    if w.write_all(&buf[..n]).is_err() || w.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // window-size follower: cheap poll instead of a SIGWINCH handler, which
    // would need signal-safe plumbing for no benefit in a spike.
    let master_rs = master.clone();
    let done_rs = done.clone();
    std::thread::spawn(move || {
        let mut last = (rows, cols);
        while !done_rs.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(cur) = term::win_size() {
                if cur != last {
                    if let Ok(m) = master_rs.lock() {
                        let _ = m.resize(PtySize {
                            rows: cur.0,
                            cols: cur.1,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    last = cur;
                }
            }
        }
    });

    // the scripted injector
    //
    // `--text` still wins for one-off manual probes; a scenario replaces the
    // whole script when asked for.
    let steps: Vec<Step> = if a.scenario == Scenario::Plain && a.text != Args::default().text {
        vec![
            Step::Paste(a.text.clone()),
            Step::Sleep(a.submit_delay_ms),
            Step::Submit,
        ]
    } else {
        script(a.scenario, a.submit_delay_ms)
    };

    let mode = a.mode;
    let submit = submit_bytes(a.submit).to_vec();
    let writer_inj = writer.clone();
    let done_inj = done.clone();
    let (delay, repeat, interval) = (a.delay_ms, a.repeat, a.interval_ms);
    let log_path = a.log.clone();
    let scenario = a.scenario;
    std::thread::spawn(move || {
        let logger = Logger::open(&log_path).ok();
        let say = |l: &Option<Logger>, s: &str| {
            if let Some(l) = l {
                l.log(s)
            }
        };
        std::thread::sleep(Duration::from_millis(delay));
        say(&logger, &format!("scenario {scenario:?}: begin"));
        for round in 0..repeat.max(1) {
            for step in &steps {
                if done_inj.load(Ordering::SeqCst) {
                    return;
                }
                match step {
                    Step::Note(n) => say(&logger, &format!("note: {n}")),
                    Step::Sleep(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                    Step::Type(t) => {
                        let Ok(mut w) = writer_inj.lock() else { return };
                        let _ = w.write_all(t.as_bytes());
                        let _ = w.flush();
                        say(&logger, &format!("type: {} bytes", t.len()));
                    }
                    Step::Paste(t) => {
                        let payload = frame(t, mode);
                        let Ok(mut w) = writer_inj.lock() else { return };
                        let _ = w.write_all(&payload);
                        let _ = w.flush();
                        say(
                            &logger,
                            &format!("paste: mode={mode:?} {} bytes", payload.len()),
                        );
                    }
                    Step::Submit => {
                        if submit.is_empty() {
                            continue;
                        }
                        let Ok(mut w) = writer_inj.lock() else { return };
                        let _ = w.write_all(&submit);
                        let _ = w.flush();
                        say(&logger, "submit: written");
                    }
                }
            }
            say(&logger, &format!("scenario {scenario:?}: round {} done", round + 1));
            std::thread::sleep(Duration::from_millis(interval));
        }
    });

    let status = child.wait()?;
    done.store(true, Ordering::SeqCst);
    let _ = pump_out.join();
    log.log(&format!("run: child exited status={status:?}"));

    // Raw mode is dropped here; print AFTER so the message lands on a sane
    // terminal.
    drop(_raw);
    eprintln!("\r\npty-inject-spike: child exited ({status:?}); log at {}", a.log);
    Ok(())
}

fn main() -> Result<()> {
    let a = parse_args()?;
    let log = Logger::open(&a.log)?;
    if a.selftest {
        selftest(&log)
    } else {
        interactive(&a, &log)
    }
}
