//! Read the plain text in modern Apple Notes `ZICNOTEDATA.ZDATA` blobs.
//! The payload is gzip-compressed protobuf. The note text is field 2 of
//! field 3 of field 2; the other fields carry formatting and attachments.

use std::io::Read;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Error};

const MAX_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

pub(super) fn register(connection: &Connection) -> Result<()> {
    connection
        .create_scalar_function(
            "enzyme_apple_notes_text",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| match ctx.get_raw(0) {
                ValueRef::Null => Ok(None),
                ValueRef::Blob(blob) => decode(blob).map(Some).map_err(function_error),
                other => Err(Error::InvalidFunctionParameterType(0, other.data_type())),
            },
        )
        .context("failed to register Apple Notes text decoder")
}

fn function_error(error: anyhow::Error) -> Error {
    Error::UserFunctionError(Box::new(std::io::Error::other(error.to_string())))
}

fn decode(blob: &[u8]) -> Result<String> {
    if !blob.starts_with(&[0x1f, 0x8b]) {
        bail!("Apple Notes body is not gzip data (possibly encrypted)");
    }
    let mut bytes = Vec::new();
    GzDecoder::new(blob)
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to decompress Apple Notes body")?;
    if bytes.len() as u64 > MAX_DECOMPRESSED_BYTES {
        bail!("Apple Notes body exceeds the 32 MiB decompression limit");
    }
    let document = field_bytes(&bytes, 2)?.ok_or_else(|| anyhow!("missing Notes document"))?;
    let note = field_bytes(document, 3)?.ok_or_else(|| anyhow!("missing Notes note"))?;
    let text = field_bytes(note, 2)?.ok_or_else(|| anyhow!("missing Notes text"))?;
    Ok(std::str::from_utf8(text)
        .context("Apple Notes text is not UTF-8")?
        .to_owned())
}

fn field_bytes(input: &[u8], wanted: u64) -> Result<Option<&[u8]>> {
    let mut offset = 0;
    let mut found = None;
    while offset < input.len() {
        let tag = varint(input, &mut offset)?;
        let field = tag >> 3;
        let wire = tag & 7;
        if field == 0 {
            bail!("invalid protobuf field zero");
        }
        match wire {
            0 => {
                varint(input, &mut offset)?;
            }
            1 => skip(input, &mut offset, 8)?,
            2 => {
                let size = usize::try_from(varint(input, &mut offset)?)
                    .context("protobuf field is too large")?;
                let start = offset;
                skip(input, &mut offset, size)?;
                if field == wanted {
                    if found.is_some() {
                        bail!("duplicate Apple Notes text field {wanted}");
                    }
                    found = Some(&input[start..offset]);
                }
            }
            5 => skip(input, &mut offset, 4)?,
            _ => bail!("unsupported protobuf wire type {wire}"),
        }
    }
    Ok(found)
}

fn skip(input: &[u8], offset: &mut usize, size: usize) -> Result<()> {
    *offset = offset
        .checked_add(size)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| anyhow!("truncated Apple Notes protobuf"))?;
    Ok(())
}

fn varint(input: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *input
            .get(*offset)
            .ok_or_else(|| anyhow!("truncated Apple Notes protobuf varint"))?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            bail!("Apple Notes protobuf varint overflows u64");
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("Apple Notes protobuf varint is too long")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    fn field(number: u8, bytes: &[u8]) -> Vec<u8> {
        assert!(bytes.len() < 128);
        let mut result = vec![(number << 3) | 2, bytes.len() as u8];
        result.extend_from_slice(bytes);
        result
    }

    #[test]
    fn decodes_text_and_rejects_non_gzip_payloads() {
        let note = field(2, "Whole note\nSecond line".as_bytes());
        let document = field(3, &note);
        let payload = field(2, &document);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).unwrap();
        let blob = encoder.finish().unwrap();
        assert_eq!(decode(&blob).unwrap(), "Whole note\nSecond line");
        assert!(
            decode(b"encrypted")
                .unwrap_err()
                .to_string()
                .contains("not gzip")
        );
    }

    #[test]
    fn scalar_function_returns_text_and_null() {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection).unwrap();
        let null: Option<String> = connection
            .query_row("SELECT enzyme_apple_notes_text(NULL)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(null, None);

        let note = field(2, b"Complete body");
        let document = field(3, &note);
        let payload = field(2, &document);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).unwrap();
        let blob = encoder.finish().unwrap();
        let text: String = connection
            .query_row(
                "SELECT enzyme_apple_notes_text(?1)",
                rusqlite::params![blob],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text, "Complete body");
    }
}
