//! Live probe: what the screen shows around `/clear` (pool worker clear).
//! cargo run --example clear_probe -- <dir>
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn dump(p: &Arc<Mutex<vt100::Parser>>, label: &str) {
    let s = p.lock().unwrap().screen().contents();
    let rows: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    println!("==== {label} ({} non-blank rows)", rows.len());
    for r in rows.iter().rev().take(12).rev() {
        println!("| {r}");
    }
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 30, cols: 110, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(&dir);
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let parser = Arc::new(Mutex::new(vt100::Parser::new(30, 110, 0)));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut w = pair.master.take_writer().unwrap();
    let p2 = parser.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 { break; }
            p2.lock().unwrap().process(&buf[..n]);
        }
    });
    std::thread::sleep(Duration::from_secs(8));
    dump(&parser, "startup");
    w.write_all(b"\x1b[200~say only the word PROBE-ONE\x1b[201~").unwrap();
    w.flush().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    w.write_all(b"\r").unwrap();
    std::thread::sleep(Duration::from_secs(12));
    dump(&parser, "after a turn");
    let mode = std::env::args().nth(2).unwrap_or_else(|| "type".into());
    if mode == "paste" {
        w.write_all(b"\x1b[200~/clear\x1b[201~").unwrap();
    } else {
        w.write_all(b"/clear").unwrap();
    }
    w.flush().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    dump(&parser, "typed /clear");
    w.write_all(b"\r").unwrap();
    w.flush().unwrap();
    for i in 0..8 {
        std::thread::sleep(Duration::from_millis(500));
        dump(&parser, &format!("after enter +{}ms", (i + 1) * 500));
    }
    let _ = child.kill();
}
