//! Wire format shared by the private worker channel and, later, the public pipe.
//!
//! One frame is a 32-bit little-endian payload length followed by that many bytes of UTF-8 JSON.
//! Frames are capped at [`MAX_FRAME_BYTES`] so a malformed or hostile length prefix cannot make the
//! process allocate without bound. The private channel is inherited by the worker process; it is not
//! a public endpoint and carries no authentication of its own.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Version of the private and public protocols.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap for one frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// One request from the coordinator to the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// Data root the caller resolved, when it uses user data at all.
    ///
    /// The coordinator refuses a request that names a different root, so a CLI started with another
    /// `--config-dir` cannot make the resident writer act on data the caller did not resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_root: Option<String>,
}

/// One response from the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

/// Typed failure carried inside a response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub message: String,
}

impl Failure {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Failure {
        Failure {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// Why a frame could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// A length prefix larger than the cap.
    TooLarge(usize),
    /// A frame whose prefix does not match its body.
    LengthMismatch { declared: usize, actual: usize },
    /// Truncated prefix or body.
    Incomplete,
    /// Payload is not the expected JSON shape.
    Malformed(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(bytes) => write!(
                formatter,
                "frame of {bytes} bytes exceeds the {MAX_FRAME_BYTES} byte cap"
            ),
            FrameError::LengthMismatch { declared, actual } => {
                write!(formatter, "frame declared {declared} bytes but carried {actual}")
            }
            FrameError::Incomplete => formatter.write_str("incomplete frame"),
            FrameError::Malformed(reason) => write!(formatter, "malformed frame: {reason}"),
        }
    }
}

/// Encode one value into a length-prefixed frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value).map_err(|error| FrameError::Malformed(error.to_string()))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode a complete length-prefixed frame.
pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, FrameError> {
    if frame.len() < 4 {
        return Err(FrameError::Incomplete);
    }
    let declared = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(declared));
    }
    let body = &frame[4..];
    if body.len() != declared {
        return Err(FrameError::LengthMismatch {
            declared,
            actual: body.len(),
        });
    }
    serde_json::from_slice(body).map_err(|error| FrameError::Malformed(error.to_string()))
}

/// Read one frame. Returns `Ok(None)` on a clean end of stream.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {declared} bytes exceeds the cap"),
        ));
    }
    let mut body = vec![0u8; declared];
    reader.read_exact(&mut body)?;
    let mut frame = Vec::with_capacity(declared + 4);
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(&body);
    Ok(Some(frame))
}

/// Write one already encoded frame.
pub fn write_frame<W: Write>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
    writer.write_all(frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let request = Request {
            id: 7,
            method: String::from("read"),
            params: serde_json::json!({"monitor": "mon-aa", "control": "brightness"}),
            storage_root: None,
        };
        let frame = encode_frame(&request).expect("encode");
        assert_eq!(&frame[..4], &((frame.len() - 4) as u32).to_le_bytes());
        let decoded: Request = decode_frame(&frame).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn response_without_result_or_error_omits_both() {
        let response = Response {
            id: 1,
            ok: true,
            result: None,
            error: None,
        };
        let frame = encode_frame(&response).expect("encode");
        let text = String::from_utf8(frame[4..].to_vec()).expect("utf8");
        assert!(!text.contains("result"));
        assert!(!text.contains("error"));
    }

    #[test]
    fn oversized_frames_are_refused() {
        let big = "x".repeat(MAX_FRAME_BYTES);
        let error = encode_frame(&big).expect_err("too large");
        assert!(matches!(error, FrameError::TooLarge(_)));

        let mut frame = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        let error = decode_frame::<Response>(&frame).expect_err("too large");
        assert!(matches!(error, FrameError::TooLarge(_)));
    }

    #[test]
    fn truncated_and_mismatched_frames_are_rejected() {
        assert_eq!(decode_frame::<Response>(&[0, 1]), Err(FrameError::Incomplete));

        let mut frame = 10u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        assert!(matches!(
            decode_frame::<Response>(&frame),
            Err(FrameError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        let mut frame = 3u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"{x}");
        assert!(matches!(
            decode_frame::<Response>(&frame),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn stream_helpers_round_trip_and_stop_cleanly() {
        let request = Request {
            id: 1,
            method: String::from("ping"),
            params: serde_json::Value::Null,
            storage_root: None,
        };
        let frame = encode_frame(&request).expect("encode");
        let mut buffer: Vec<u8> = frame.clone();
        let mut cursor = std::io::Cursor::new(&mut buffer);

        let read = read_frame(&mut cursor).expect("read").expect("frame");
        assert_eq!(read, frame);
        assert_eq!(read_frame(&mut cursor).expect("clean eof"), None);

        let mut sink: Vec<u8> = Vec::new();
        write_frame(&mut sink, &frame).expect("write");
        assert_eq!(sink, frame);
    }
}
