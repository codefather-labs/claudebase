//! Text an agent or operator pipes into a command, whatever shell sent it.
//!
//! Windows PowerShell does not pipe plain UTF-8 into a native command: it may
//! prefix a byte-order mark, or send UTF-16. A pool worker on the pool machine
//! piped `{"content": …}` into `claudebase pool reply` from PowerShell and got
//! "the result is not JSON … at line 1 column 1" — the BOM was the first
//! character (2026-10-26). Every stdin reader goes through here.

use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Bytes to text: UTF-8 with or without a BOM, or UTF-16 LE/BE with a BOM.
pub fn decode(bytes: &[u8]) -> Result<String> {
    let text = match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => std::str::from_utf8(rest).context("input is not valid UTF-8")?.to_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes)?,
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes)?,
        // UTF-16 LE without a BOM: every other byte of ASCII text is zero. Text
        // never holds a NUL, so a zero second byte cannot be UTF-8 text.
        [b0, 0, ..] if *b0 != 0 => utf16(bytes, u16::from_le_bytes)?,
        _ => std::str::from_utf8(bytes).context("input is not valid UTF-8 or UTF-16")?.to_owned(),
    };
    Ok(strip_bom(&text).to_owned())
}

fn utf16(bytes: &[u8], word: fn([u8; 2]) -> u16) -> Result<String> {
    if bytes.len() % 2 != 0 {
        bail!("input looks like UTF-16 but has an odd number of bytes");
    }
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| word([c[0], c[1]])).collect();
    String::from_utf16(&units).context("input is not valid UTF-16")
}

/// A BOM that survived as a character (a line read as text).
pub fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// All of stdin, decoded.
pub fn read_stdin() -> Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).context("read stdin")?;
    decode(&bytes)
}

/// A file, decoded the same way — the dependable route from any shell.
pub fn read_file(path: &Path) -> Result<String> {
    decode(&std::fs::read(path).with_context(|| format!("read {}", path.display()))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whatever_the_shell_sent_the_text_comes_out_the_same() {
        let json = r#"{"content": "ok ✓"}"#;
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(json.as_bytes());
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain(json.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(json.encode_utf16().flat_map(u16::to_be_bytes)).collect();
        let le_bare: Vec<u8> = json.encode_utf16().flat_map(u16::to_le_bytes).collect();
        for bytes in [json.as_bytes().to_vec(), bom, le, be, le_bare] {
            let text = decode(&bytes).unwrap();
            assert_eq!(text, json);
            assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        }
        assert!(decode(&[0xC3, 0x28]).is_err());
        assert_eq!(strip_bom("\u{feff}cbr_x"), "cbr_x");
    }
}
