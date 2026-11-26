//! The wire format of the Debug Adapter Protocol: `Content-Length: n`, a blank line, and n
//! bytes of JSON, as in the language server protocol.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// The longest message that is read (a bigger header is taken for garbage and ends the session).
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Why a message could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The input ended between messages.
    Eof,
    /// The message is unusable but the stream is still in step: the next read starts at the
    /// next message.
    Malformed(String),
    /// The stream cannot be followed any more (it ended inside a message, or failed).
    Fatal(String),
}

/// Reads one message.
pub fn read_message(input: &mut impl BufRead) -> Result<Value, ReadError> {
    let mut length: Option<usize> = None;
    let mut bad_length = false;
    let mut seen_header = false;
    loop {
        let mut line = String::new();
        let mut raw = Vec::new();
        match input.read_until(b'\n', &mut raw) {
            Ok(0) if seen_header => {
                return Err(ReadError::Fatal("input ended inside a header".into()));
            }
            Ok(0) => return Err(ReadError::Eof),
            Ok(_) => {}
            Err(e) => return Err(ReadError::Fatal(format!("cannot read input: {e}"))),
        }
        line.push_str(&String::from_utf8_lossy(&raw));
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        seen_header = true;
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            match value.trim().parse::<usize>() {
                Ok(n) => length = Some(n),
                Err(_) => bad_length = true,
            }
        }
    }
    let Some(length) = length.filter(|_| !bad_length) else {
        return Err(ReadError::Malformed(
            "the header has no valid Content-Length".into(),
        ));
    };
    if length > MAX_BODY {
        return Err(ReadError::Fatal(format!(
            "a message of {length} bytes is too long"
        )));
    }
    let mut body = vec![0u8; length];
    if let Err(e) = input.read_exact(&mut body) {
        return Err(ReadError::Fatal(format!(
            "input ended inside a message: {e}"
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|e| ReadError::Malformed(format!("the body is not JSON: {e}")))
}

/// Writes one message and flushes.
pub fn write_message(out: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_all(bytes: &[u8]) -> Vec<Result<Value, String>> {
        let mut input = io::Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        loop {
            match read_message(&mut input) {
                Ok(v) => out.push(Ok(v)),
                Err(ReadError::Eof) => return out,
                Err(ReadError::Malformed(m)) => out.push(Err(m)),
                Err(ReadError::Fatal(m)) => {
                    out.push(Err(format!("fatal: {m}")));
                    return out;
                }
            }
        }
    }

    #[test]
    fn a_written_message_reads_back() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &json!({"seq": 1, "text": "héllo"})).unwrap();
        let read = read_all(&bytes);
        assert_eq!(read, [Ok(json!({"seq": 1, "text": "héllo"}))]);
    }

    #[test]
    fn a_bad_body_or_header_is_reported_and_the_next_message_reads() {
        let mut bytes = b"Content-Length: 5\r\n\r\n{nope".to_vec();
        bytes.extend(b"Content-Length: abc\r\n\r\n");
        bytes.extend(b"X-Other: 1\r\n\r\n");
        write_message(&mut bytes, &json!({"ok": true})).unwrap();
        let read = read_all(&bytes);
        assert_eq!(read.len(), 4);
        assert!(read[0].as_ref().unwrap_err().contains("not JSON"));
        assert!(read[1].as_ref().unwrap_err().contains("Content-Length"));
        assert!(read[2].as_ref().unwrap_err().contains("Content-Length"));
        assert_eq!(read[3], Ok(json!({"ok": true})));
    }

    #[test]
    fn a_truncated_message_ends_the_stream() {
        let read = read_all(b"Content-Length: 50\r\n\r\n{\"a\":1}");
        assert!(read[0].as_ref().unwrap_err().starts_with("fatal"));
        let read = read_all(b"Content-Length: 5");
        assert!(read[0].as_ref().unwrap_err().starts_with("fatal"));
        assert!(read_all(b"").is_empty());
    }
}
