//! Injection-gate behaviour — the part of the PTY transport that can destroy
//! operator state if it is wrong.
//!
//! The unit tests inside `src/supervisor/` cover the detectors in isolation.
//! These tests drive the real `Injector` against a fake PTY writer and assert
//! the two rules that came out of live findings in
//! `docs/qa/evidence/pty-inject/`:
//!
//! * **F-6** — never write while the operator has a half-typed line, because
//!   the TUI silently concatenates the draft with the injected text and
//!   submits both.
//! * **F-3** — never write while a modal is up, because the message is
//!   swallowed whole and the submit key answers the dialog (a stray CR once
//!   confirmed "Yes, use my browser" on the operator's desktop).
//!
//! Both rules must HOLD the message, not drop it: delivery is delayed, never
//! lost.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use claudebase::supervisor::{DraftTracker, InboundMessage, Injector, ModalDetector};

/// A `Write` that appends into a shared buffer, standing in for the PTY master.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Harness {
    buf: Arc<Mutex<Vec<u8>>>,
    draft: Arc<DraftTracker>,
    modal: Arc<ModalDetector>,
    done: Arc<AtomicBool>,
    tx: mpsc::Sender<InboundMessage>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(SharedBuf(buf.clone()))));
        let draft = Arc::new(DraftTracker::new());
        let modal = Arc::new(ModalDetector::new());
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<InboundMessage>();
        let injector = Injector::new(writer, draft.clone(), modal.clone(), done.clone());
        let handle = std::thread::spawn(move || injector.run(rx));
        Self {
            buf,
            draft,
            modal,
            done,
            tx,
            handle: Some(handle),
        }
    }

    fn send(&self, id: &str, body: &str) {
        self.tx
            .send(InboundMessage {
                message_id: id.to_string(),
                body: body.to_string(),
            })
            .expect("injector alive");
    }

    fn written(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).to_string()
    }

    /// Wait until `pred` holds over the written bytes, or give up.
    fn wait_for(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred(&self.written()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.done
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn delivers_on_a_clean_line_with_paste_framing_and_a_separate_submit() {
    let h = Harness::start();
    h.send("m1", "hello from telegram");

    assert!(
        h.wait_for(Duration::from_secs(3), |w| w.contains("hello from telegram")),
        "a clean line must accept the message"
    );
    let out = h.written();
    assert!(out.contains("\x1b[200~"), "must use bracketed paste (F-1)");
    assert!(out.contains("\x1b[201~"), "must close bracketed paste");

    assert!(
        h.wait_for(Duration::from_secs(3), |w| w.ends_with('\r')),
        "submit key must eventually be written (F-2)"
    );
    // F-2: the CR is a separate write AFTER the paste, never inside it.
    let out = h.written();
    let paste_end = out.find("\x1b[201~").expect("paste terminator");
    assert!(
        out[..paste_end].find('\r').is_none(),
        "no CR may appear inside the pasted block"
    );
}

#[test]
fn holds_the_message_while_the_operator_is_mid_line_then_releases_it() {
    let h = Harness::start();
    h.draft.observe_operator_input(b"half-typed draft");

    h.send("m1", "INBOUND-WHILE-TYPING");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !h.written().contains("INBOUND-WHILE-TYPING"),
        "F-6: injecting over a draft concatenates it into the operator's prompt"
    );

    // Operator submits their own line — the draft is gone, delivery may proceed.
    h.draft.observe_operator_input(b"\r");
    assert!(
        h.wait_for(Duration::from_secs(3), |w| w.contains("INBOUND-WHILE-TYPING")),
        "message must be delivered once the line is clear, not dropped"
    );
}

#[test]
fn holds_the_message_while_a_modal_is_up_then_releases_it() {
    let h = Harness::start();
    // Verbatim shape of the dialog that ate a message during the spike.
    h.modal
        .feed(b"Set up auto mode for your environment?\r\n 1. Set it up\r\n Enter to confirm - Esc to cancel");

    h.send("m1", "INBOUND-DURING-MODAL");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !h.written().contains("INBOUND-DURING-MODAL"),
        "F-3: a modal swallows the message and the submit key answers the dialog"
    );

    // Operator dismisses the dialog; the TUI repaints its normal prompt.
    h.modal.feed(b"\r\n auto mode on (shift+tab to cycle)\r\n");
    assert!(
        h.wait_for(Duration::from_secs(3), |w| w.contains("INBOUND-DURING-MODAL")),
        "message must survive the modal and land afterwards"
    );
}

#[test]
fn messages_queued_behind_the_gate_are_coalesced_into_one_prompt() {
    let h = Harness::start();
    h.draft.observe_operator_input(b"typing");

    h.send("m1", "FIRST");
    h.send("m2", "SECOND");
    h.send("m3", "THIRD");
    std::thread::sleep(Duration::from_millis(400));
    h.draft.observe_operator_input(b"\r");

    assert!(
        h.wait_for(Duration::from_secs(3), |w| w.contains("THIRD")),
        "queued messages must be delivered"
    );
    let out = h.written();
    assert!(out.contains("FIRST") && out.contains("SECOND"));
    assert_eq!(
        out.matches("\x1b[200~").count(),
        1,
        "a burst must cost one prompt, not one per message"
    );
}

#[test]
fn duplicate_message_ids_are_injected_once() {
    let h = Harness::start();
    h.send("same-id", "DEDUP-BODY");
    assert!(h.wait_for(Duration::from_secs(3), |w| w.contains("DEDUP-BODY")));

    // A daemon reconnect can replay the same message; it must not be pasted twice.
    h.send("same-id", "DEDUP-BODY");
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        h.written().matches("DEDUP-BODY").count(),
        1,
        "reconnect replay must not duplicate the message"
    );
}
