//! Modal-dialog detector over the child's output stream.
//!
//! ## Why this exists
//!
//! Live finding F-3, reproduced twice
//! (`docs/qa/evidence/pty-inject/scenario-busy.log`): when Claude Code raises a
//! modal, an injected message is swallowed **entirely** — the marker text
//! appears zero times in the transcript — and the submit key *answers the
//! dialog*. In the very first spike run a stray CR confirmed "Yes, use my
//! browser" and launched a browser on the operator's desktop.
//!
//! So a modal is not merely a bad moment to type: it is a moment when typing is
//! destructive. Injection is gated on this detector, and the message stays
//! queued rather than being written and lost.
//!
//! ## How it decides
//!
//! It keeps a terminal emulator (`vt100`) fed with everything the child prints
//! and looks at the screen AS IT IS NOW: a modal is up when one of Claude Code's
//! dialog affordance lines ("Enter to confirm", "esc to cancel", …) is visible
//! in the bottom rows. Dialogs are drawn there, under the conversation, so
//! the same words inside an answer further up do not count.
//!
//! It used to scan a rolling tail of output bytes and wait for a prompt line
//! ("shift+tab to cycle") printed AFTER the dialog to clear the verdict. A
//! dialog that Claude Code erased without repainting that line — the folder
//! trust question, answered by hand at startup — left the verdict stuck for
//! good: on the Windows pool machine three of four workers withheld every job
//! they were given (2026-10-26). The screen has no such history.
//!
//! A false positive costs a delayed message, a false negative one lost
//! message — the queue and the retry keep both survivable. The signature list
//! is expected to grow from real usage.

use std::sync::Mutex;

/// Substrings that mean "a modal is waiting for a keypress". Matched against
/// the bottom rows of the screen, lowercased, whitespace collapsed.
const MODAL_SIGNATURES: &[&str] = &[
    "enter to confirm",
    "esc to cancel",
    "enter to continue",
    "esc to keep",
    "do you want to proceed",
    "yes, and don't ask again",
    "❯ 1.",
];

/// How many non-blank rows, counted from the bottom, a dialog can occupy below
/// the conversation and still be seen: the option list plus its affordance
/// line. The conversation above is never inspected.
const BOTTOM_ROWS: usize = 8;

pub struct ModalDetector {
    screen: Mutex<vt100::Parser>,
}

impl ModalDetector {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            screen: Mutex::new(vt100::Parser::new(rows.max(1), cols.max(1), 0)),
        }
    }

    /// Feed raw child output. Called on the render path.
    pub fn feed(&self, bytes: &[u8]) {
        if let Ok(mut p) = self.screen.lock() {
            p.process(bytes);
        }
    }

    /// The child's terminal changed size; the emulator follows.
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Ok(mut p) = self.screen.lock() {
            p.set_size(rows.max(1), cols.max(1));
        }
    }

    /// True when a dialog's affordance line is on screen now.
    pub fn modal_is_up(&self) -> bool {
        let Ok(p) = self.screen.lock() else {
            // Fail closed: if the detector is unavailable we must not inject,
            // because the cost of injecting into a modal is destructive.
            return true;
        };
        let (_, cols) = p.screen().size();
        let rows: Vec<String> = p.screen().rows(0, cols).collect();
        let bottom: Vec<&String> = rows.iter().rev().filter(|r| !r.trim().is_empty()).take(BOTTOM_ROWS).collect();
        let text = normalize(bottom.iter().rev().map(|r| r.as_str()).collect::<Vec<_>>().join("\n").as_bytes());
        MODAL_SIGNATURES.iter().any(|sig| text.contains(sig))
    }
}

/// A startup question `claudebase run --no-input` answers by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownDialog {
    /// "Quick safety check: Is this a project you created or one you trust?"
    /// The cursor starts on "No, exit" — Enter alone would close Claude Code.
    FolderTrust { yes_selected: bool },
}

impl ModalDetector {
    /// The known dialog on screen now, if any, and where its cursor is.
    pub fn known_dialog(&self) -> Option<KnownDialog> {
        let p = self.screen.lock().ok()?;
        let (_, cols) = p.screen().size();
        let rows: Vec<String> = p.screen().rows(0, cols).map(|r| normalize(r.as_bytes())).collect();
        let all = rows.join("\n");
        if all.contains("quick safety check") && all.contains("yes, i trust this folder") {
            let yes_selected = rows.iter().any(|r| r.contains('❯') && r.contains("yes, i trust this folder"));
            return Some(KnownDialog::FolderTrust { yes_selected });
        }
        None
    }

    /// The bottom of the screen as text, for a log line about a dialog nobody answers.
    pub fn bottom_text(&self) -> String {
        let Ok(p) = self.screen.lock() else { return String::new() };
        let (_, cols) = p.screen().size();
        let rows: Vec<String> = p.screen().rows(0, cols).collect();
        let bottom: Vec<&String> = rows.iter().rev().filter(|r| !r.trim().is_empty()).take(BOTTOM_ROWS).collect();
        normalize(bottom.iter().rev().map(|r| r.as_str()).collect::<Vec<_>>().join(" | ").as_bytes())
    }
}

/// A horizontal rule of the input box: a row of nothing but box-drawing dashes.
fn is_rule(row: &str) -> bool {
    let t = row.trim();
    t.chars().count() >= 8 && t.chars().all(|c| matches!(c, '─' | '━' | '-'))
}

/// The text of a prompt row (`❯ text` or `> text`), if it is one.
fn prompt_text(row: &str) -> Option<String> {
    let t = row.trim_start();
    let rest = t.strip_prefix('❯').or_else(|| t.strip_prefix('>'))?;
    Some(rest.trim().to_owned())
}

/// The input box: the rows between the last two rules. Returns the index of
/// its top rule and the text typed into it.
fn input_box(rows: &[String]) -> Option<(usize, String)> {
    let rules: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| is_rule(r)).map(|(i, _)| i).collect();
    let [.., top, bottom] = rules.as_slice() else { return None };
    let inside: Vec<String> = rows[top + 1..*bottom].iter().filter(|r| !r.trim().is_empty()).cloned().collect();
    let text = match inside.first() {
        None => String::new(),
        Some(first) => {
            let mut t = prompt_text(first).unwrap_or_else(|| first.trim().to_owned());
            for more in &inside[1..] {
                t.push(' ');
                t.push_str(more.trim());
            }
            t
        }
    };
    Some((*top, text))
}

impl ModalDetector {
    fn rows(&self) -> Vec<String> {
        let Ok(p) = self.screen.lock() else { return Vec::new() };
        let (_, cols) = p.screen().size();
        p.screen().rows(0, cols).collect()
    }

    /// What is typed in Claude Code's input box now; `None` when no input box
    /// is on screen (a dialog, startup).
    pub fn input_text(&self) -> Option<String> {
        input_box(&self.rows()).map(|(_, t)| t)
    }

    /// The screen of a session that `/clear` has just emptied: an empty input
    /// box, and above it nothing of a conversation but the `/clear` itself
    /// (Claude Code 2.1.310 redraws its banner, then `❯ /clear`). Measured
    /// with `examples/clear_probe.rs`.
    pub fn shows_cleared_session(&self) -> bool {
        let rows = self.rows();
        let Some((top, text)) = input_box(&rows) else { return false };
        if !text.is_empty() {
            return false;
        }
        let above: Vec<&String> = rows[..top].iter().filter(|r| !r.trim().is_empty()).collect();
        let prompts: Vec<String> = above.iter().filter_map(|r| prompt_text(r)).collect();
        prompts == ["/clear"]
            && above.last().and_then(|r| prompt_text(r)).as_deref() == Some("/clear")
            && !above.iter().any(|r| r.contains('●'))
    }
}

impl Default for ModalDetector {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// Strip ANSI/OSC escapes, lowercase, and collapse whitespace runs to single
/// spaces. The TUI splits words across cursor-positioning sequences, so naive
/// substring matching on raw bytes finds nothing.
fn normalize(bytes: &[u8]) -> String {
    let raw = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut last_was_space = false;

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') | Some('?') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || c2 == '\x1b' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c.is_control() || c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        last_was_space = false;
        out.extend(c.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> ModalDetector {
        ModalDetector::new(24, 80)
    }

    #[test]
    fn quiet_output_is_not_a_modal() {
        let d = detector();
        d.feed(b"just some text scrolling by\r\n");
        assert!(!d.modal_is_up());
    }

    #[test]
    fn detects_the_dialog_that_ate_a_message_in_the_spike() {
        let d = detector();
        d.feed(b"Set up auto mode for your environment?\r\n 1. Set it up\r\n 2. Not now\r\n Enter to confirm \xc2\xb7 Esc to cancel");
        assert!(d.modal_is_up(), "must refuse to inject into this");
    }

    #[test]
    fn detects_signature_split_by_ansi_escapes() {
        let d = detector();
        d.feed(b"Enter\x1b[0m to \x1b[1mconfirm\x1b[0m");
        assert!(d.modal_is_up(), "ANSI between words must not hide the match");
    }

    #[test]
    fn a_dialog_erased_without_a_prompt_repaint_is_gone() {
        // The pool-machine case: the trust dialog is answered, Claude Code
        // clears those lines and draws its input box — no "shift+tab to cycle".
        let d = detector();
        d.feed(b"Do you trust the files in this folder?\r\n\xe2\x9d\xaf 1. Yes, proceed\r\n  2. No, exit\r\n Enter to confirm \xc2\xb7 Esc to exit");
        assert!(d.modal_is_up());
        d.feed(b"\x1b[2J\x1b[H\r\n> \r\n");
        assert!(!d.modal_is_up(), "an erased dialog is not up, whatever was printed before");
    }

    #[test]
    fn the_same_words_in_the_conversation_do_not_count() {
        let d = detector();
        d.feed(b"the dialog says: Enter to confirm, Esc to cancel\r\n");
        for i in 0..12 {
            d.feed(format!("more conversation {i}\r\n").as_bytes());
        }
        d.feed(b"> \r\n  bypass permissions on (shift+tab to cycle)");
        assert!(!d.modal_is_up(), "words above the input box are conversation, not a dialog");
    }

    #[test]
    fn a_dialog_after_the_prompt_wins() {
        let d = detector();
        d.feed(b"> \r\n  shift+tab to cycle\r\n");
        d.feed(b"Do you want to proceed?\r\n Enter to confirm");
        assert!(d.modal_is_up());
    }

    /// Verbatim from claude 2.1.310 in an untrusted directory.
    const TRUST: &[u8] = "Accessing workspace:\r\n /tmp/x\r\n Quick safety check: Is this a project you created or one you trust?\r\n Claude Code'll be able to read, edit, and execute files here.\r\n \u{276f} 1. No, exit\r\n   2. Yes, I trust this folder\r\n Enter to confirm \u{b7} Esc to cancel".as_bytes();

    #[test]
    fn recognises_the_trust_question_and_where_its_cursor_is() {
        let d = detector();
        d.feed(TRUST);
        assert_eq!(d.known_dialog(), Some(KnownDialog::FolderTrust { yes_selected: false }));
        assert!(d.modal_is_up());
        d.feed("\x1b[2J\x1b[HQuick safety check:\r\n   1. No, exit\r\n \u{276f} 2. Yes, I trust this folder\r\n Enter to confirm".as_bytes());
        assert_eq!(d.known_dialog(), Some(KnownDialog::FolderTrust { yes_selected: true }));
        d.feed(b"\x1b[2J\x1b[H> ");
        assert_eq!(d.known_dialog(), None);
    }

    /// Screens from `examples/clear_probe.rs` against claude 2.1.310.
    const RULE: &str = "──────────────────────────────────────────────────";
    fn screen(rows: &[&str]) -> Vec<u8> {
        format!("\x1b[2J\x1b[H{}", rows.join("\r\n")).into_bytes()
    }

    #[test]
    fn tells_a_cleared_session_from_the_moments_around_it() {
        let banner = [" ▐▛███▜▌   Claude Code v2.1.310", "▝▜█████▛▘  Opus 5.5 · Claude Max", "  ▘▘ ▝▝    ~/project"];
        let d = ModalDetector::new(30, 110);
        let mut conversation = banner.to_vec();
        conversation.extend(["❯ say only the word PROBE-ONE", "● PROBE-ONE", "✻ Churned for 1s", RULE, "❯ ", RULE, "  ⏵⏵ auto mode on (shift+tab to cycle)"]);
        d.feed(&screen(&conversation));
        assert_eq!(d.input_text().as_deref(), Some(""));
        assert!(!d.shows_cleared_session(), "a conversation is on screen");

        let mut typed = banner.to_vec();
        typed.extend(["❯ say only the word PROBE-ONE", "● PROBE-ONE", "  ❯ /clear        Start a new session with empty context", RULE, "❯ /clear", RULE]);
        d.feed(&screen(&typed));
        assert_eq!(d.input_text().as_deref(), Some("/clear"));
        assert!(!d.shows_cleared_session(), "typed, not yet run");
        assert!(!d.modal_is_up(), "the command menu is not a dialog");

        let mut cleared = banner.to_vec();
        cleared.extend(["❯ /clear", RULE, "❯ ", RULE, "  ⏵⏵ auto mode on (shift+tab to cycle)"]);
        d.feed(&screen(&cleared));
        assert!(d.shows_cleared_session());

        // The next conversation after an earlier clear is not a fresh clear.
        let mut later = banner.to_vec();
        later.extend(["❯ /clear", "❯ next question", "● an answer", RULE, "❯ ", RULE]);
        d.feed(&screen(&later));
        assert!(!d.shows_cleared_session());
    }

    #[test]
    fn follows_a_resize() {
        let d = detector();
        d.resize(40, 120);
        d.feed(b"Enter to confirm");
        assert!(d.modal_is_up());
    }
}
