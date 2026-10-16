use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

pub fn read(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let mut body = vec![
        0;
        length.ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length"
        ))?
    ];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn send(value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut output = io::stdout().lock();
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()
}

pub fn reply(id: &Value, result: Value) -> io::Result<()> {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub fn error(id: &Value, code: i64, message: &str) -> io::Result<()> {
    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}
