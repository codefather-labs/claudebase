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
//! ## What this is not
//!
//! Not a terminal emulator. It scans a rolling tail of recently-rendered bytes
//! for the affordance lines Claude Code prints under a modal ("Enter to
//! confirm", "esc to cancel", …). That is a heuristic, and it is allowed to be:
//! a false positive costs a delayed message, a false negative costs one lost
//! message — while the queue and the retry keep both survivable. The signature
//! list is expected to grow from real usage.

use std::sync::Mutex;

/// How many bytes of recent output to keep. A Claude Code modal repaints the
/// whole dialog, so a few KB always contains the affordance line if one is up.
const TAIL_BYTES: usize = 8192;

/// Substrings that mean "a modal is waiting for a keypress". Matched against
/// output with ANSI escapes stripped and whitespace collapsed, because the TUI
/// interleaves cursor moves inside these strings.
const MODAL_SIGNATURES: &[&str] = &[
    "enter to confirm",
    "esc to cancel",
    "enter to continue",
    "esc to keep",
    "do you want to proceed",
    "yes, and don't ask again",
    "❯ 1.",
];

/// Signatures that mean the child is back at its normal input prompt. Checked
/// only to expire a stale modal verdict when no repaint has happened since.
const PROMPT_SIGNATURES: &[&str] = &["esc to interrupt", "shift+tab to cycle"];

pub struct ModalDetector {
    tail: Mutex<String>,
}

impl ModalDetector {
    pub fn new() -> Self {
        Self {
            tail: Mutex::new(String::new()),
        }
    }

    /// Feed raw child output. Called on the render path, so it stays O(n) with
    /// a bounded buffer and never allocates unboundedly.
    pub fn feed(&self, bytes: &[u8]) {
        let text = normalize(bytes);
        if text.is_empty() {
            return;
        }
        let Ok(mut tail) = self.tail.lock() else {
            return;
        };
        tail.push_str(&text);
        if tail.len() > TAIL_BYTES {
            // Keep the tail; drop the head at a char boundary.
            let cut = tail.len() - TAIL_BYTES;
            let idx = (cut..tail.len())
                .find(|i| tail.is_char_boundary(*i))
                .unwrap_or(tail.len());
            *tail = tail[idx..].to_string();
        }
    }

    /// True when the most recent output looks like a modal awaiting a key.
    ///
    /// A prompt signature appearing AFTER the last modal signature clears the
    /// verdict — that is how the detector recovers once the operator answers a
    /// dialog by hand.
    pub fn modal_is_up(&self) -> bool {
        let Ok(tail) = self.tail.lock() else {
            // Fail closed: if the detector is unavailable we must not inject,
            // because the cost of injecting into a modal is destructive.
            return true;
        };
        let last_modal = MODAL_SIGNATURES
            .iter()
            .filter_map(|sig| tail.rfind(sig))
            .max();
        let Some(modal_at) = last_modal else {
            return false;
        };
        let last_prompt = PROMPT_SIGNATURES
            .iter()
            .filter_map(|sig| tail.rfind(sig))
            .max();
        match last_prompt {
            Some(prompt_at) => prompt_at < modal_at,
            None => true,
        }
    }
}

impl Default for ModalDetector {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn quiet_output_is_not_a_modal() {
        let d = ModalDetector::new();
        d.feed(b"just some text scrolling by\r\n");
        assert!(!d.modal_is_up());
    }

    #[test]
    fn detects_the_dialog_that_ate_a_message_in_the_spike() {
        // Verbatim from docs/qa/evidence/pty-inject/scenario-busy.log context.
        let d = ModalDetector::new();
        d.feed(b"Set up auto mode for your environment?\r\n 1. Set it up\r\n 2. Not now\r\n Enter to confirm \xc2\xb7 Esc to cancel");
        assert!(d.modal_is_up(), "must refuse to inject into this");
    }

    #[test]
    fn detects_signature_split_by_ansi_escapes() {
        let d = ModalDetector::new();
        // The TUI really does interleave cursor moves mid-phrase.
        d.feed(b"Enter\x1b[0m to \x1b[1mconfirm\x1b[0m");
        assert!(d.modal_is_up(), "ANSI between words must not hide the match");
    }

    #[test]
    fn returning_to_the_prompt_clears_the_verdict() {
        let d = ModalDetector::new();
        d.feed(b"Enter to confirm \xc2\xb7 Esc to cancel");
        assert!(d.modal_is_up());
        d.feed(b"\r\n auto mode on (shift+tab to cycle)\r\n");
        assert!(!d.modal_is_up(), "prompt after modal means the dialog is gone");
    }

    #[test]
    fn a_modal_after_a_prompt_wins() {
        let d = ModalDetector::new();
        d.feed(b"shift+tab to cycle");
        d.feed(b"Do you want to proceed? Enter to confirm");
        assert!(d.modal_is_up(), "ordering matters, not mere presence");
    }

    #[test]
    fn tail_is_bounded() {
        let d = ModalDetector::new();
        for _ in 0..50 {
            d.feed(&vec![b'x'; 1024]);
        }
        assert!(d.tail.lock().unwrap().len() <= TAIL_BYTES + 1024);
    }
}
