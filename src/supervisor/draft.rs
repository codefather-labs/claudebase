//! Exact draft tracking for the operator's input line.
//!
//! ## Why this is not a timer
//!
//! Live finding F-6 (`docs/qa/evidence/pty-inject/scenario-typing-transcript.txt`):
//! injecting while the operator has a half-typed line **silently concatenates**
//! — the submitted prompt became
//! `MK-OP недописанная строка оператораMK-B ответь одним словом: браво`. The
//! operator's unsent draft is swallowed into the inbound message and sent.
//! Nothing warns anyone.
//!
//! The original plan called this a "quiet window" and proposed a timer. A timer
//! is a guess. The supervisor is the ONLY path the operator's keystrokes travel,
//! so it can know the answer exactly: count what was typed, subtract what was
//! erased, reset on submit or line-kill. That is what this does.
//!
//! ## What counts as dirty
//!
//! Only what can actually put text on the line: printable bytes, a paste from
//! the operator's own terminal, and Up/Down (history recall materialises a
//! previous prompt without a single printable byte).
//!
//! Escape sequences are classified, NOT blanket-treated as dirty. Claude Code
//! enables focus reporting (DEC 1004) and any-event mouse tracking
//! (1000/1002/1003/1006) — read off a live transcript, not assumed — so the
//! terminal streams escape bytes on every mouse move and every switch to
//! another window. The first version marked all of them dirty, which closed the
//! gate permanently the moment the operator alt-tabbed to Telegram: messages
//! queued forever and the transport looked dead.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// ASCII control bytes we care about.
const CTRL_C: u8 = 0x03;
const CTRL_U: u8 = 0x15;
const CTRL_W: u8 = 0x17;
const BACKSPACE: u8 = 0x7f;
const BACKSPACE_ALT: u8 = 0x08;
const CR: u8 = b'\r';
const LF: u8 = b'\n';
const ESC: u8 = 0x1b;

#[derive(Default)]
pub struct DraftTracker {
    /// Printable characters currently believed to be on the input line.
    pending: AtomicUsize,
    /// Set when a key that can materialise text without printable bytes was
    /// seen (arrows / history recall). Cleared by submit or line-kill.
    recalled: AtomicBool,
    /// Escaped copy of the bytes that last made the line dirty. Kept for the
    /// stall diagnostic: "the gate is closed" is useless to an operator without
    /// "and this is what closed it".
    last_dirty: std::sync::Mutex<String>,
}

impl DraftTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the operator's input line is believed EMPTY and it is
    /// therefore safe to inject.
    pub fn is_clean(&self) -> bool {
        self.pending.load(Ordering::SeqCst) == 0 && !self.recalled.load(Ordering::SeqCst)
    }

    /// Why the line is considered dirty — for the stall diagnostic.
    pub fn why_dirty(&self) -> String {
        let chars = self.pending.load(Ordering::SeqCst);
        let recalled = self.recalled.load(Ordering::SeqCst);
        let last = self
            .last_dirty
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        format!("chars={chars} recalled={recalled} last_dirty_input={last}")
    }

    fn remember(&self, bytes: &[u8]) {
        if let Ok(mut g) = self.last_dirty.lock() {
            *g = bytes
                .iter()
                .map(|b| match b {
                    0x1b => "<ESC>".to_string(),
                    0x20..=0x7e => (*b as char).to_string(),
                    other => format!("<{other:#04x}>"),
                })
                .collect::<String>()
                .chars()
                .take(60)
                .collect();
        }
    }

    /// Called with every byte the operator types, before it is forwarded to the
    /// child. Cheap and allocation-free: it runs on the keystroke path.
    pub fn observe_operator_input(&self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                CR | LF => self.clear(),
                CTRL_C | CTRL_U => self.clear(),
                CTRL_W => {
                    // Word-erase: we cannot know the word length, so fall back
                    // to the conservative side — assume the line still has
                    // content until an explicit clear. Never assume "clean".
                    self.pending.fetch_max(1, Ordering::SeqCst);
                }
                BACKSPACE | BACKSPACE_ALT => {
                    let _ = self
                        .pending
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                            Some(v.saturating_sub(1))
                        });
                }
                ESC => {
                    // Classify, do not blanket-dirty. Claude Code enables focus
                    // reporting (DEC 1004) and any-event mouse tracking
                    // (1000/1002/1003/1006) — verified from a live transcript —
                    // so the terminal streams escape sequences on every mouse
                    // move and every switch away to another window. Treating
                    // those as "text appeared on the line" closed the gate
                    // permanently: the operator alt-tabbed to Telegram, came
                    // back, and no message ever arrived again.
                    let (len, effect) = classify_escape_sequence(&bytes[i..]);
                    match effect {
                        EscEffect::PutsTextOnLine => {
                            self.recalled.store(true, Ordering::SeqCst);
                            self.remember(&bytes[i..(i + len).min(bytes.len())]);
                        }
                        EscEffect::Inert => {}
                    }
                    i += len;
                    continue;
                }
                // Other C0 controls (tab-complete, ctrl-arrows, …) — not text,
                // but not proof of emptiness either. Ignore.
                0x00..=0x1f => {}
                // UTF-8 continuation byte: part of a character already counted
                // by its leading byte. Counting these too inflated the figure
                // in the stall diagnostic (a 200-character Russian line read as
                // 395) and made backspace under-count, since one backspace
                // erases a character, not a byte.
                0x80..=0xbf => {}
                _ => {
                    self.pending.fetch_add(1, Ordering::SeqCst);
                    self.remember(bytes);
                }
            }
            i += 1;
        }
    }

    /// Called by the injector after IT submits, so its own writes do not leave
    /// the tracker believing the operator has a draft.
    pub fn clear(&self) {
        self.pending.store(0, Ordering::SeqCst);
        self.recalled.store(false, Ordering::SeqCst);
    }
}

/// What an escape sequence means for the input LINE.
#[derive(Debug, PartialEq, Eq)]
enum EscEffect {
    /// Can put text on the line the operator would submit — history recall via
    /// Up/Down, or a paste from their own terminal.
    PutsTextOnLine,
    /// Says nothing about the line: focus in/out, mouse reports, cursor moves.
    Inert,
}

/// Classify the escape sequence starting at `bytes[0] == ESC`, returning its
/// length so the caller can skip it whole.
///
/// Handles the shapes a terminal actually emits into stdin:
///   * CSI  — `ESC [ … final`
///   * SS3  — `ESC O final` (arrows in application-cursor mode)
///   * X10 mouse — `ESC [ M` followed by THREE raw bytes, which must be skipped
///     explicitly or they get counted as typed characters.
fn classify_escape_sequence(bytes: &[u8]) -> (usize, EscEffect) {
    if bytes.len() < 2 {
        return (1, EscEffect::Inert);
    }
    match bytes[1] {
        b'[' => {
            // Legacy X10 mouse report: 3 binary bytes follow the final 'M'.
            if bytes.len() >= 3 && bytes[2] == b'M' {
                return ((3 + 3).min(bytes.len()), EscEffect::Inert);
            }
            let mut i = 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            let final_byte = bytes.get(i).copied().unwrap_or(0);
            let params = &bytes[2..i.min(bytes.len())];
            let len = (i + 1).min(bytes.len());
            let effect = match final_byte {
                // Up / Down: history recall pulls a previous prompt onto the line.
                b'A' | b'B' => EscEffect::PutsTextOnLine,
                // `ESC[200~` opens a paste FROM the operator's terminal; the
                // bytes that follow are their text.
                b'~' if params == b"200" => EscEffect::PutsTextOnLine,
                // Focus in / out (DEC 1004), SGR mouse (final 'M'/'m'), cursor
                // moves, Home/End, function keys — none of them add text.
                _ => EscEffect::Inert,
            };
            (len, effect)
        }
        b'O' => {
            let effect = match bytes.get(2) {
                Some(b'A') | Some(b'B') => EscEffect::PutsTextOnLine,
                _ => EscEffect::Inert,
            };
            (3.min(bytes.len()), effect)
        }
        _ => (2, EscEffect::Inert),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_is_clean() {
        assert!(DraftTracker::new().is_clean());
    }

    #[test]
    fn typing_makes_it_dirty_and_enter_clears_it() {
        let d = DraftTracker::new();
        d.observe_operator_input(b"hello");
        assert!(!d.is_clean(), "typed text must block injection");
        d.observe_operator_input(b"\r");
        assert!(d.is_clean(), "submit must release the line");
    }

    #[test]
    fn backspacing_all_the_way_back_is_clean_again() {
        let d = DraftTracker::new();
        d.observe_operator_input(b"abc");
        d.observe_operator_input(&[BACKSPACE, BACKSPACE, BACKSPACE]);
        assert!(d.is_clean());
    }

    #[test]
    fn backspace_never_underflows() {
        let d = DraftTracker::new();
        d.observe_operator_input(&[BACKSPACE; 5]);
        assert!(d.is_clean());
        d.observe_operator_input(b"x");
        assert!(!d.is_clean(), "counter must not have gone negative");
    }

    #[test]
    fn ctrl_u_and_ctrl_c_clear_the_line() {
        for kill in [CTRL_U, CTRL_C] {
            let d = DraftTracker::new();
            d.observe_operator_input(b"half typed");
            d.observe_operator_input(&[kill]);
            assert!(d.is_clean(), "0x{kill:02x} should clear the draft");
        }
    }

    #[test]
    fn arrow_key_history_recall_counts_as_dirty() {
        for seq in [
            &[ESC, b'[', b'A'][..],  // cursor up, normal mode
            &[ESC, b'[', b'B'][..],  // cursor down
            &[ESC, b'O', b'A'][..],  // cursor up, application mode
        ] {
            let d = DraftTracker::new();
            d.observe_operator_input(seq);
            assert!(!d.is_clean(), "history recall puts text on the line: {seq:?}");
        }
    }

    #[test]
    fn focus_events_do_not_close_the_gate() {
        // DEC 1004 is enabled by Claude Code, so switching to Telegram and back
        // sends these. Treating them as a draft is what broke live delivery.
        let d = DraftTracker::new();
        d.observe_operator_input(&[ESC, b'[', b'O']); // focus out
        d.observe_operator_input(&[ESC, b'[', b'I']); // focus in
        assert!(d.is_clean(), "alt-tabbing must not block injection");
    }

    #[test]
    fn mouse_reports_do_not_close_the_gate() {
        let d = DraftTracker::new();
        // SGR mouse (1006): ESC [ < b ; x ; y M
        d.observe_operator_input(b"\x1b[<35;80;24M");
        d.observe_operator_input(b"\x1b[<35;81;24m");
        assert!(d.is_clean(), "SGR mouse motion must be inert");

        // Legacy X10 mouse (1000): ESC [ M then three RAW bytes, which must be
        // skipped or they count as typed characters.
        let d = DraftTracker::new();
        d.observe_operator_input(&[ESC, b'[', b'M', 32, 40, 40]);
        assert!(d.is_clean(), "X10 mouse payload must not be counted as text");
    }

    #[test]
    fn cursor_movement_is_not_a_draft() {
        let d = DraftTracker::new();
        d.observe_operator_input(&[ESC, b'[', b'C']); // right
        d.observe_operator_input(&[ESC, b'[', b'D']); // left
        d.observe_operator_input(b"\x1b[H");           // home
        assert!(d.is_clean(), "moving the cursor adds no text");
    }

    #[test]
    fn a_paste_from_the_operators_terminal_is_dirty() {
        let d = DraftTracker::new();
        d.observe_operator_input(b"\x1b[200~");
        assert!(!d.is_clean(), "pasted content is text on the line");
    }

    #[test]
    fn a_realistic_idle_stream_leaves_the_gate_open() {
        // What an idle session actually sees: focus changes and mouse motion.
        let d = DraftTracker::new();
        for _ in 0..50 {
            d.observe_operator_input(b"\x1b[<35;10;5M\x1b[<35;11;5m");
        }
        d.observe_operator_input(&[ESC, b'[', b'O']);
        d.observe_operator_input(&[ESC, b'[', b'I']);
        assert!(d.is_clean(), "an idle terminal must stay injectable");
    }

    #[test]
    fn word_erase_stays_conservative() {
        let d = DraftTracker::new();
        d.observe_operator_input(&[CTRL_W]);
        assert!(
            !d.is_clean(),
            "unknown erase width must not be read as an empty line"
        );
    }
}
