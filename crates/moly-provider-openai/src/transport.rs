//! Independent bounded JSONL framing and sequential request transport.

use std::time::Duration;

use moly_protocol::{Body, MAX_FRAME_BYTES, Message, ProtocolError, VERSION};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::service::Service;

const REQUEST_QUEUE: usize = 8;
const ERROR_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Transport failure with no input bytes or raw serialization diagnostics.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Underlying stream failed. Never render its diagnostics to the peer.
    #[error("provider stream failed")]
    Io(#[from] std::io::Error),
    /// Invalid bounded frame.
    #[error("{0}")]
    Frame(&'static str),
    /// Peer sent more work than the bounded request queue can hold.
    #[error("provider request queue is full")]
    Overloaded,
}

/// Read one LF-terminated UTF-8 envelope, bounding accumulation before append.
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, Error> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(Error::Frame("partial frame at EOF"))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if count > MAX_FRAME_BYTES - bytes.len() {
            return Err(Error::Frame("frame too large"));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Frame("invalid UTF-8 or JSON"))?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| Error::Frame("invalid UTF-8 or JSON"))?;
    if !value.is_object() {
        return Err(Error::Frame("expected JSON object"));
    }
    let message: Message =
        serde_json::from_value(value).map_err(|_| Error::Frame("invalid envelope"))?;
    if message.version != VERSION {
        return Err(Error::Frame("unsupported version"));
    }
    Ok(Some(message))
}

/// Encode one bounded envelope, excluding the terminating LF from the limit.
pub(crate) fn encode_frame(message: &Message) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| Error::Frame("invalid envelope"))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::Frame("frame too large"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> Result<(), Error> {
    writer.write_all(&encode_frame(message)?).await?;
    writer.flush().await?;
    Ok(())
}

/// Serve one connection, cancelling an in-flight HTTP future as soon as input closes.
pub(crate) async fn serve<R, W>(reader: R, mut writer: W, mut service: Service) -> Result<(), Error>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let cancellation = CancellationToken::new();
    let _cancel_on_exit = cancellation.clone().drop_guard();
    let (requests, mut pending) = mpsc::channel(REQUEST_QUEUE);
    let mut input = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let message = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                message = read_frame(&mut reader) => message?,
            };
            let Some(message) = message else {
                return Ok(());
            };
            if !matches!(message.body, Body::Request { .. }) {
                return Err(Error::Frame("expected request"));
            }
            // Never wait on a full queue: that would hide EOF behind a hung HTTP
            // request. Close overloaded peers instead of accepting unbounded work.
            requests.try_send(message).map_err(|_| Error::Overloaded)?;
        }
    });

    let stopped = tokio::select! {
        biased;
        stopped = &mut input => stopped.map_err(|_| Error::Frame("input reader stopped"))?,
        work = async {
            while let Some(request) = pending.recv().await {
                let response = service.handle(request).await;
                let response = match encode_frame(&response) {
                    Ok(bytes) => bytes,
                    Err(Error::Frame("frame too large")) => {
                        let id = match response.body {
                            Body::Response { id, .. } => Some(id),
                            Body::Error { id, .. } => id,
                            _ => None,
                        };
                        encode_frame(&Message::new(Body::Error {
                            id,
                            error: ProtocolError::new("provider_response_too_large", "Model result exceeds frame limit"),
                        }))?
                    }
                    Err(error) => return Err(error),
                };
                writer.write_all(&response).await?;
                writer.flush().await?;
            }
            // Only the reader completion reports EOF or framing failures; its
            // sender can drop just before the JoinHandle becomes ready.
            std::future::pending::<Result<(), Error>>().await
        } => return work,
    };
    // Dropping the work future above drops reqwest's in-flight request, including
    // body reads. No response, prompt, or tool arguments enter this error path.
    if let Err(error) = stopped {
        let code = if matches!(error, Error::Overloaded) {
            "slow_consumer"
        } else {
            "invalid_frame"
        };
        let fatal = Message::new(Body::Error {
            id: None,
            error: ProtocolError::new(
                code,
                "Invalid, incomplete, oversized, or excessive provider input",
            ),
        });
        let _ = tokio::time::timeout(ERROR_FLUSH_TIMEOUT, write_frame(&mut writer, &fatal)).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../conformance/protocol/framing.rs"]
mod framing_conformance;

#[cfg(test)]
mod tests;
