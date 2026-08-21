//! Decode iMessage `attributedBody` typedstream blobs into plaintext.
//!
//! Apple moved message body text out of `message.text` into the `attributedBody`
//! column (a serialized `NSAttributedString` typedstream) — by 2026 essentially
//! every message is blob-only. SQLite cannot decode typedstream, so this tool
//! runs as a preprocessing pass over a *copy* of `chat.db`: for every row whose
//! `text` is empty it extracts the backing string and writes it into a
//! `message_text(rowid, text)` sidecar table. A generic enzyme SQLite source can
//! then read `COALESCE(NULLIF(m.text,''), x.text)` with no engine changes.
//!
//! Usage: imessage-attributedbody <path-to-chat.db-copy>

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

/// Read a typedstream length/count integer at `i`. Values < 0x81 are a single
/// byte; 0x81/0x82/0x83 introduce a little-endian i16/i32/i64. Returns the value
/// and the index just past it.
fn read_len(data: &[u8], i: usize) -> Option<(usize, usize)> {
    let x = *data.get(i)?;
    match x {
        0x81 => {
            let b = data.get(i + 1..i + 3)?;
            Some((u16::from_le_bytes([b[0], b[1]]) as usize, i + 3))
        }
        0x82 => {
            let b = data.get(i + 1..i + 5)?;
            Some((u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize, i + 5))
        }
        0x83 => {
            let b = data.get(i + 1..i + 9)?;
            let value = usize::try_from(u64::from_le_bytes(b.try_into().ok()?)).ok()?;
            Some((value, i + 9))
        }
        _ => Some((x as usize, i + 1)),
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Extract the message body: the backing `NSString` of the `NSAttributedString`
/// is the first char-array (`+` type-encoding, 0x2B) after the `NSString` class
/// marker. Attribute keys/values follow it but we only want the first.
fn extract_text(blob: &[u8]) -> Option<String> {
    let k = find_sub(blob, b"NSString")?;
    let j = blob[k..].iter().position(|&b| b == 0x2B).map(|p| k + p)?;
    let (n, p) = read_len(blob, j + 1)?;
    let end = p.checked_add(n)?;
    let bytes = blob.get(p..end)?;
    let s = String::from_utf8_lossy(bytes).into_owned();
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: imessage-attributedbody <path-to-chat.db-copy>")?;

    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("failed to open {path} read-write"))?;
    if find_sub(path.as_bytes(), b"Library/Messages/chat.db").is_some() {
        bail!("refusing to run against the live Messages DB — pass a copy");
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_text (rowid INTEGER PRIMARY KEY, text TEXT NOT NULL);",
    )?;

    let mut stmt = conn.prepare(
        "SELECT ROWID, attributedBody FROM message \
         WHERE (text IS NULL OR text='') AND attributedBody IS NOT NULL",
    )?;
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut decoded = 0usize;
    let mut failed = 0usize;
    let tx = conn.unchecked_transaction()?;
    {
        let mut ins =
            tx.prepare("INSERT OR REPLACE INTO message_text (rowid, text) VALUES (?1, ?2)")?;
        for (rowid, blob) in &rows {
            match extract_text(blob) {
                Some(text) => {
                    ins.execute(rusqlite::params![rowid, text])?;
                    decoded += 1;
                }
                None => failed += 1,
            }
        }
    }
    tx.commit()?;

    println!(
        "empty-text rows: {}  decoded: {}  undecodable: {}",
        rows.len(),
        decoded,
        failed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_backing_string_from_typedstream() {
        // Synthetic typedstream tail mirroring real layout: NSString class
        // marker, object refs, `+` char-array encoding, single-byte length 5.
        let mut blob = Vec::new();
        blob.extend_from_slice(b"\x84\x08NSString\x01\x94\x84\x01");
        blob.push(0x2B); // '+' char-array type encoding
        blob.push(0x05); // length
        blob.extend_from_slice(b"Hello");
        assert_eq!(extract_text(&blob).as_deref(), Some("Hello"));
    }

    #[test]
    fn short_length_single_byte() {
        assert_eq!(read_len(&[0x05], 0), Some((5, 1)));
    }

    #[test]
    fn i16_length_marker() {
        assert_eq!(read_len(&[0x81, 0xE8, 0x00], 0), Some((232, 3)));
    }

    #[test]
    fn truncated_length_markers_return_none_not_panic() {
        assert_eq!(read_len(&[0x81, 0x01], 0), None); // needs 2 length bytes
        assert_eq!(read_len(&[0x82, 0x01, 0x02], 0), None); // needs 4
        assert_eq!(read_len(&[0x83, 0x01], 0), None); // needs 8
        assert_eq!(read_len(&[], 0), None);
    }

    #[test]
    fn oversized_length_does_not_overflow_or_panic() {
        // A 0x83 length of u64::MAX must not panic on the p+n add; extraction
        // just fails (the slice can't exist) and the row is counted undecodable.
        let mut blob = Vec::new();
        blob.extend_from_slice(b"\x84\x08NSString\x01\x94\x84\x01");
        blob.push(0x2B);
        blob.push(0x83);
        blob.extend_from_slice(&u64::MAX.to_le_bytes());
        blob.extend_from_slice(b"body");
        assert_eq!(extract_text(&blob), None);
    }

    #[test]
    fn length_past_end_returns_none() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"\x84\x08NSString\x01\x94\x84\x01");
        blob.push(0x2B);
        blob.push(0x40); // claims 64 bytes
        blob.extend_from_slice(b"short");
        assert_eq!(extract_text(&blob), None);
    }
}
