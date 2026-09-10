use crate::result::ResultSet;
use crate::wire;
use crate::{Error, Result};

/// Transport-agnostic request/response codec for one connection.
///
/// Kept synchronous and buffer-based so it is easy to unit test and so each
/// frontend (text, MySQL, HTTP) can own its framing without depending on
/// async I/O. The async connection loop drives buffering and writes.
pub trait Protocol: Send {
    /// Decodes one request from the front of `input`, returning the SQL text
    /// and the number of bytes consumed. `None` means more bytes are needed.
    fn decode_request(&mut self, input: &[u8]) -> Result<Option<(String, usize)>>;

    /// Appends a successful statement batch, including any trailing marker.
    fn encode_success(&self, results: &[ResultSet], out: &mut Vec<u8>);

    /// Appends a failed statement, including any trailing marker.
    fn encode_failure(&self, message: &str, out: &mut Vec<u8>);
}

/// Default binary protocol: `[u32 len][sql utf8]` requests, length-prefixed
/// frame responses.
pub struct TextProtocol;

impl Protocol for TextProtocol {
    fn decode_request(&mut self, input: &[u8]) -> Result<Option<(String, usize)>> {
        if input.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(input[0..4].try_into().unwrap()) as usize;
        if input.len() < 4 + len {
            return Ok(None);
        }
        let sql = String::from_utf8(input[4..4 + len].to_vec())
            .map_err(|e| Error::Runtime(format!("invalid utf8 request: {e}")))?;
        Ok(Some((sql, 4 + len)))
    }

    fn encode_success(&self, results: &[ResultSet], out: &mut Vec<u8>) {
        for rs in results {
            out.extend_from_slice(&wire::encode_result_frame(rs));
        }
        out.extend_from_slice(&wire::encode_done_frame());
    }

    fn encode_failure(&self, message: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(&wire::encode_error_frame(message));
        out.extend_from_slice(&wire::encode_done_frame());
    }
}
