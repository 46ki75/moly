//! Private MPP framing, independently exercised by the shared wire conformance suite.
use moly_protocol::{MAX_FRAME_BYTES, Message, VERSION};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("stream I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid protocol frame: {0}")]
    Frame(&'static str),
}

pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, Error> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(Error::Frame("partial frame at EOF"))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if frame.len() + count > MAX_FRAME_BYTES {
            return Err(Error::Frame("frame too large"));
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            let value: Value = serde_json::from_slice(&frame)
                .map_err(|_| Error::Frame("invalid UTF-8 or JSON"))?;
            if !value.is_object() {
                return Err(Error::Frame("expected JSON object"));
            }
            let message: Message =
                serde_json::from_value(value).map_err(|_| Error::Frame("invalid envelope"))?;
            if message.version != VERSION {
                return Err(Error::Frame("unsupported version"));
            }
            return Ok(Some(message));
        }
    }
}

pub(crate) fn encode_frame(message: &Message) -> Result<Vec<u8>, Error> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|_| Error::Frame("cannot encode envelope"))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::Frame("frame too large"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}
