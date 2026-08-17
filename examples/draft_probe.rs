use claudebase::supervisor::DraftTracker;
fn main() {
    let seqs: Vec<(&str, Vec<u8>)> = vec![
        ("focus-out ESC[O", b"\x1b[O".to_vec()),
        ("focus-in  ESC[I", b"\x1b[I".to_vec()),
        ("sgr mouse M", b"\x1b[<35;10;5M".to_vec()),
        ("sgr mouse m", b"\x1b[<35;11;5m".to_vec()),
        ("all together", b"\x1b[O\x1b[I\x1b[<35;10;5M\x1b[<35;11;5m\x1b[<35;12;6M".to_vec()),
    ];
    for (name, bytes) in seqs {
        let d = DraftTracker::new();
        d.observe_operator_input(&bytes);
        println!("{:20} -> clean = {}", name, d.is_clean());
    }
}
