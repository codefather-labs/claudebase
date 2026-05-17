// Slice 0a spike — interprocess crate concurrent-accept smoke test.
//
// Purpose: validate that the `interprocess` v2 crate correctly handles 2
// concurrent accept-loop connections via its tokio integration on Windows
// named pipes AND Unix domain sockets, WITHOUT `#[cfg]` branches.
//
// If this binary exits 0 on linux-x64, macos-arm64, AND windows-x64 in CI,
// Slice 1a uses `interprocess` as a single cross-platform IPC abstraction.
// If it fails on Windows, Slice 1a switches to platform-arms:
//   - `tokio::net::UnixListener` on Unix
//   - `tokio::net::windows::named_pipe::ServerOptions` on Windows
//
// Protocol: length-prefixed JSON. Each frame is a 4-byte big-endian u32
// length header followed by the UTF-8 JSON body. The server reads
// `{"ping": <N>}` and writes back `{"pong": <N>}`. The client asserts
// the pong matches the ping it sent.
//
// Run: `cargo run --example ipc_concurrent_accept`
// Expected stdout (exit 0): `PASS: 2 concurrent connections round-tripped`
// On any failure: `FAIL: <reason>` and exit 1.

use std::time::Duration;

use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions, ToNsName,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Serialize, Deserialize, Debug)]
struct Ping {
    ping: u32,
}

#[derive(Serialize, Deserialize, Debug)]
struct Pong {
    pong: u32,
}

// Read a single length-prefixed JSON frame from a stream. Returns the raw
// JSON bytes. Errors propagate as `anyhow::Error` so the FAIL path can show
// the exact stage that broke.
async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

// Write a length-prefixed JSON frame.
async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, body: &[u8]) -> anyhow::Result<()> {
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

// Handle a single accepted connection: read one ping frame, write one pong
// frame back with the same number.
async fn handle_connection(mut stream: Stream) -> anyhow::Result<()> {
    let body = read_frame(&mut stream).await?;
    let ping: Ping = serde_json::from_slice(&body)?;
    let pong = Pong { pong: ping.ping };
    let resp = serde_json::to_vec(&pong)?;
    write_frame(&mut stream, &resp).await?;
    Ok(())
}

// Client task: connect, send `{"ping": n}`, expect `{"pong": n}`, return n
// on success.
async fn client_round_trip(name_str: String, n: u32) -> anyhow::Result<u32> {
    let name = name_str.as_str().to_ns_name::<GenericNamespaced>()?;
    let mut stream = Stream::connect(name).await?;
    let req = serde_json::to_vec(&Ping { ping: n })?;
    write_frame(&mut stream, &req).await?;
    let resp = read_frame(&mut stream).await?;
    let pong: Pong = serde_json::from_slice(&resp)?;
    if pong.pong != n {
        anyhow::bail!("expected pong {n}, got {}", pong.pong);
    }
    Ok(n)
}

async fn run_spike() -> anyhow::Result<()> {
    // Unique socket/pipe name per run so concurrent CI matrix entries on the
    // same runner image (unlikely but cheap to guard) cannot collide.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name_str = format!("claudebase-spike-{pid}-{nanos}.sock");
    let name = name_str.as_str().to_ns_name::<GenericNamespaced>()?;

    let listener = ListenerOptions::new().name(name).create_tokio()?;

    // Server task: accept exactly 2 connections, handle each in its own
    // spawned task so they run concurrently.
    let server = tokio::spawn(async move {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let stream = listener.accept().await?;
            handles.push(tokio::spawn(async move {
                handle_connection(stream).await
            }));
        }
        for h in handles {
            h.await??;
        }
        Ok::<(), anyhow::Error>(())
    });

    // Give the listener a moment to enter accept() before clients connect.
    // 50 ms is conservative; on a developer laptop the listener is ready in
    // microseconds, but CI runners under load occasionally need more.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Spawn 2 client tasks in parallel.
    let c1 = tokio::spawn(client_round_trip(name_str.clone(), 1));
    let c2 = tokio::spawn(client_round_trip(name_str.clone(), 2));

    let (r1, r2) = tokio::join!(c1, c2);
    let n1 = r1??;
    let n2 = r2??;
    if !((n1 == 1 && n2 == 2) || (n1 == 2 && n2 == 1)) {
        anyhow::bail!("unexpected round-trip values: {n1}, {n2}");
    }

    // Bound the server wait so a stuck accept loop manifests as a clear
    // timeout rather than a hung CI job.
    tokio::time::timeout(Duration::from_secs(5), server).await???;

    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Wrap the whole spike in a 30 s timeout so the CI job never hangs past
    // its assigned 60 s wallclock.
    let result = tokio::time::timeout(Duration::from_secs(30), run_spike()).await;
    match result {
        Ok(Ok(())) => {
            println!("PASS: 2 concurrent connections round-tripped");
            std::process::exit(0);
        }
        Ok(Err(e)) => {
            println!("FAIL: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            println!("FAIL: spike timed out after 30 s");
            std::process::exit(1);
        }
    }
}
