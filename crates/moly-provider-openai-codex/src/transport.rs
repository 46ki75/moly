//! Bounded independent Provider v2 JSONL and scoped reverse host requests.

use std::time::Duration;

use moly_protocol::auth::{CredentialReplace, InteractionRequest, InteractionResponse};
use moly_protocol::{Body, MAX_FRAME_BYTES, Message, ProtocolError, VERSION};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::Error;
use crate::service::Service;

const QUEUE: usize = 8;
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    #[error("Invalid provider frame")]
    Frame,
    #[error("Provider stream failed")]
    Io,
}

pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, TransportError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| TransportError::Io)?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(TransportError::Frame)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if count > MAX_FRAME_BYTES - bytes.len() {
            return Err(TransportError::Frame);
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| TransportError::Frame)?;
    if !value.is_object() {
        return Err(TransportError::Frame);
    }
    let message: Message = serde_json::from_value(value).map_err(|_| TransportError::Frame)?;
    if message.version != VERSION {
        return Err(TransportError::Frame);
    }
    Ok(Some(message))
}

pub(crate) fn encode_frame(message: &Message) -> Result<Vec<u8>, TransportError> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| TransportError::Frame)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(TransportError::Frame);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

struct HostCall {
    method: &'static str,
    params: Value,
    reply: oneshot::Sender<Result<Value, Error>>,
}

/// Capability exists only for the currently running operation. It never exposes a secret key.
#[derive(Clone)]
pub(crate) struct Host {
    calls: mpsc::Sender<HostCall>,
}

impl Host {
    async fn call<T: Serialize>(&self, method: &'static str, params: T) -> Result<Value, Error> {
        let params = serde_json::to_value(params).map_err(|_| Error::Internal)?;
        let (reply, answer) = oneshot::channel();
        self.calls
            .send(HostCall {
                method,
                params,
                reply,
            })
            .await
            .map_err(|_| Error::Host)?;
        answer.await.map_err(|_| Error::Host)?
    }

    pub(crate) async fn replace(&self, credential: Option<String>) -> Result<(), Error> {
        if credential
            .as_ref()
            .is_some_and(|value| value.len() > 64 * 1024)
        {
            return Err(Error::TooLarge);
        }
        let result = self
            .call("host.credential.replace", CredentialReplace { credential })
            .await?;
        if !result.is_null() {
            return Err(Error::Host);
        }
        Ok(())
    }

    pub(crate) async fn interact(
        &self,
        request: InteractionRequest,
    ) -> Result<InteractionResponse, Error> {
        let result = self.call("host.interact", request).await?;
        if !result.is_object() {
            return Err(Error::Host);
        }
        serde_json::from_value(result).map_err(|_| Error::Host)
    }
}

struct Task<T>(JoinHandle<T>);
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type PendingHost = Option<(u64, oneshot::Sender<Result<Value, Error>>)>;

fn enqueue(writer: &mpsc::Sender<Message>, message: Message) -> Result<(), TransportError> {
    writer.try_send(message).map_err(|_| TransportError::Io)
}

fn correlate(message: Message, pending: &mut PendingHost) -> Result<(), TransportError> {
    let (id, result) = match message.body {
        Body::Response { id, result } => (id, Ok(result)),
        Body::Error {
            id: Some(id),
            error,
        } => {
            // Semantic unavailability is not a malformed host protocol. Preserve
            // only this known category, never the host's diagnostic text.
            let error = match error.code.as_str() {
                "interaction_unavailable" | "auth_interaction_unavailable" => Error::Interaction,
                _ => Error::Host,
            };
            (id, Err(error))
        }
        _ => return Err(TransportError::Frame),
    };
    let Some((expected, reply)) = pending.take() else {
        return Err(TransportError::Frame);
    };
    if id == 0 || id != expected {
        return Err(TransportError::Frame);
    }
    reply.send(result).map_err(|_| TransportError::Frame)
}

/// The reader keeps running during HTTP and output writes. EOF drops the active future,
/// closing the loopback listener and abandoning in-flight network I/O without retries.
pub(crate) async fn serve<R, W>(
    reader: R,
    writer: W,
    mut service: Service,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (input_tx, mut input_rx) = mpsc::channel(QUEUE);
    let mut input = Task(tokio::spawn(async move {
        let mut reader = reader;
        while let Some(message) = read_frame(&mut reader).await? {
            input_tx
                .try_send(message)
                .map_err(|_| TransportError::Frame)?;
        }
        Ok::<_, TransportError>(())
    }));
    let (output_tx, mut output_rx) = mpsc::channel::<Message>(QUEUE);
    let mut output = Task(tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = output_rx.recv().await {
            let bytes = encode_frame(&message)?;
            tokio::time::timeout(WRITE_TIMEOUT, async {
                writer.write_all(&bytes).await?;
                writer.flush().await
            })
            .await
            .map_err(|_| TransportError::Io)?
            .map_err(|_| TransportError::Io)?;
        }
        Ok::<_, TransportError>(())
    }));
    let mut last_server_id = 0_u64;
    let mut last_host_id = 0_u64;
    let mut output_finished = false;
    let outcome = async {
        loop {
            let request = tokio::select! {
                biased;
                stopped = &mut input.0 => return stopped.map_err(|_| TransportError::Io)?,
                _ = &mut output.0 => { output_finished = true; return Err(TransportError::Io); }
                request = input_rx.recv() => request.ok_or(TransportError::Io)?,
            };
            let Body::Request { id, .. } = &request.body else { return Err(TransportError::Frame); };
            if *id == 0 || *id <= last_server_id { return Err(TransportError::Frame); }
            last_server_id = *id;
            let (calls, mut host_calls) = mpsc::channel(1);
            let host = Host { calls };
            let work = service.handle(request, host);
            tokio::pin!(work);
            let mut pending: PendingHost = None;
            let mut count = 0_u8;
            loop {
                tokio::select! {
                    biased;
                    stopped = &mut input.0 => return stopped.map_err(|_| TransportError::Io)?,
                    _ = &mut output.0 => { output_finished = true; return Err(TransportError::Io); }
                    message = input_rx.recv() => correlate(message.ok_or(TransportError::Io)?, &mut pending)?,
                    Some(call) = host_calls.recv() => {
                        if pending.is_some() || count >= 64 { return Err(TransportError::Frame); }
                        count += 1;
                        last_host_id = last_host_id.checked_add(1).ok_or(TransportError::Frame)?;
                        enqueue(&output_tx, Message::new(Body::Request {
                            id: last_host_id, method: call.method.into(), params: call.params,
                        }))?;
                        pending = Some((last_host_id, call.reply));
                    }
                    response = &mut work => {
                        if pending.is_some() { return Err(TransportError::Frame); }
                        let response = if encode_frame(&response).is_err() {
                            Message::new(Body::Error {
                                id: Some(last_server_id), error: Error::TooLarge.protocol(),
                            })
                        } else { response };
                        enqueue(&output_tx, response)?;
                        break;
                    }
                }
            }
        }
    }.await;
    if outcome.is_err() {
        // Best effort only, and static: never echo malformed input or host diagnostics.
        let _ = enqueue(
            &output_tx,
            Message::new(Body::Error {
                id: None,
                error: ProtocolError::new(
                    "invalid_frame",
                    "Invalid provider input or host protocol",
                ),
            }),
        );
        drop(output_tx);
        if !output_finished {
            let _ = tokio::time::timeout(WRITE_TIMEOUT, &mut output.0).await;
        }
    }
    // On clean EOF there is no operation whose response may be committed. Dropping
    // both pumps here also cancels a blocked writer and avoids detached work.
    outcome
}

#[cfg(test)]
mod tests;
