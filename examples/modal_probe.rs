use claudebase::supervisor::ModalDetector;
fn main() {
    for path in [
        "docs/qa/evidence/pty-inject/run3-transcript-stripped.txt",
        "docs/qa/evidence/pty-transport-e2e/roundtrip-transcript.txt",
        "docs/qa/evidence/pty-transport-e2e/agent-to-agent-transcript.txt",
    ] {
        let Ok(body) = std::fs::read(path) else { continue };
        let d = ModalDetector::new();
        // feed in chunks, like the real pump
        for chunk in body.chunks(4096) { d.feed(chunk); }
        println!("{:60} modal_is_up = {}", path, d.modal_is_up());
    }
}
