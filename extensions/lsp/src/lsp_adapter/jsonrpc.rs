//! LSP message framing: `Content-Length: N\r\n\r\n<json>` over byte streams.
//!
//! This is the LSP transport (distinct from MCP's newline-delimited framing).
//! Hand-rolled on `serde_json` — no `lsp-types` dependency for the MVP.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};

use serde_json::Value;

/// Serialise a JSON value as a framed LSP message and write it to `out`.
///
/// # Errors
///
/// Returns an `std::io::Error` if writing fails.
pub fn write_message<W: Write + ?Sized>(out: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// Read one framed LSP message from `reader`.
///
/// Returns `Ok(None)` on clean EOF (the server closed its pipe).
///
/// # Errors
///
/// Returns an `std::io::Error` on malformed headers or a body read failure.
pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut body = vec![0u8; len];
    std::io::Read::read_exact(reader, &mut body)?;
    let value = serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use serde_json::json;

    use super::{read_message, write_message};

    #[test]
    fn round_trips_a_message_through_a_buffer() {
        let msg = json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} });
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &msg).unwrap();

        let mut reader = BufReader::new(buf.as_slice());
        let got = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn clean_eof_returns_none() {
        let empty: &[u8] = b"";
        let mut reader = BufReader::new(empty);
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn parses_lowercase_content_length_header() {
        let body = b"{\"id\":1}";
        let mut framed = format!("content-length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(body);
        let mut reader = BufReader::new(framed.as_slice());
        let got = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(got, json!({ "id": 1 }));
    }

    #[test]
    fn reads_two_back_to_back_messages() {
        let a = json!({ "id": 1 });
        let b = json!({ "id": 2 });
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &a).unwrap();
        write_message(&mut buf, &b).unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), a);
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), b);
    }
}
