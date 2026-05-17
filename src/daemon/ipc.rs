//! Length-prefixed JSON frame codec for daemon IPC (Slice 1a).
//!
//! Wire format: 4-byte big-endian u32 length header followed by a UTF-8
//! JSON body. The 16 MiB cap protects the daemon process from
//! buffer-exhaustion DoS by a misbehaving (or hostile) local client.
//!
//! Lifted from `spikes/ipc_concurrent_accept/src/main.rs:44-63` (the
//! Slice 0a spike that proved the codec works under concurrent accept
//! over `interprocess` v2 + tokio). The only change is raising the
//! 1 MiB cap to 16 MiB — production traffic includes embeddings and
//! full chunk text which can exceed 1 MiB per frame.
//!
//! INVARIANT (async discipline): these helpers operate on generic
//! tokio AsyncRead / AsyncWrite — they hold NO mutex, touch NO global
//! state, and are safe to call from any tokio task without violating
//! the PDFIUM / ENCODER / OCR_ENGINE mutex-vs-await rule in main.rs.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum frame size accepted by the daemon. Frames larger than this
/// are rejected as a likely-malformed (or hostile) input. 16 MiB is
/// generous enough for full-document chunk replies + embeddings; the
/// MCP protocol itself never approaches this size.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Read a single length-prefixed JSON frame from `reader`. Returns the
/// raw body bytes (not yet JSON-parsed — callers parse with `serde_json`
/// to decouple wire layer from message layer).
///
/// Errors:
/// - I/O error on the length or body read (caller treats EOF as
///   "connection closed cleanly")
/// - frame larger than `MAX_FRAME_SIZE` (rejected before allocating)
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {len} bytes (max {MAX_FRAME_SIZE})");
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

/// Write a length-prefixed JSON frame to `writer`. Caller passes
/// already-serialized JSON bytes; this function adds the 4-byte length
/// prefix and flushes.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> anyhow::Result<()> {
    if body.len() > MAX_FRAME_SIZE {
        anyhow::bail!(
            "outbound frame too large: {} bytes (max {MAX_FRAME_SIZE})",
            body.len()
        );
    }
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}
