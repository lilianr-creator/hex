//! Chrome's native messaging host: hands the extension the running app's
//! presence endpoint and token, so pairing needs no copy-paste. Chrome starts
//! this process and frames each message as a little-endian `u32` length
//! followed by UTF-8 JSON; stdout carries nothing else.

use std::io::{Read, Write};

use color_eyre::eyre::{Result, bail};

/// A pairing answer is a few hundred bytes; anything larger is not ours.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    while let Some(message) = read_message(&mut input)? {
        let reply = reply(&message);
        write_message(&mut output, &reply)?;
        output.flush()?;
    }
    Ok(())
}

/// Every message is a request for the pairing, so a hello is all it takes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn reply(_message: &[u8]) -> serde_json::Value {
    match crate::local_api::presence_setup() {
        Ok((endpoint, token)) => serde_json::json!({
            "ok": true,
            "endpoint": endpoint,
            "token": token,
            "version": crate::VERSION,
        }),
        Err(error) => serde_json::json!({
            "ok": false,
            "error": format!("{error:#}"),
        }),
    }
}

fn read_message(input: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 4];
    match input.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        bail!("native message is too large");
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(output: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    output.write_all(&(body.len() as u32).to_le_bytes())?;
    output.write_all(&body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_messages_round_trip() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &serde_json::json!({"type": "hello"})).unwrap();
        let mut input = buffer.as_slice();
        let message = read_message(&mut input).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(value["type"], "hello");
        assert!(read_message(&mut input).unwrap().is_none());
    }

    #[test]
    fn an_oversized_length_is_refused() {
        let bytes = u32::MAX.to_le_bytes();
        let mut input = bytes.as_slice();
        assert!(read_message(&mut input).is_err());
    }
}
