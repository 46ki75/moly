//! SDK-private byte transport, bounded JSONL, and full-duplex RPC.
//! No Server implementation is imported; common behavior is checked by conformance.
pub(crate) mod local;

use moly_protocol::{Body, MAX_FRAME_BYTES, Message, ProtocolError, VERSION};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

/// Recoverable wire or remote semantic error. Payloads are deliberately excluded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying stream failed.
    #[error("stream I/O failed")]
    Io(#[from] std::io::Error),
    /// Invalid framing, encoding, version, or envelope.
    #[error("invalid protocol frame: {0}")]
    Frame(&'static str),
    /// Peer closed or its bounded queues were exhausted.
    #[error("connection closed")]
    Closed,
    /// Too many concurrent outstanding requests.
    #[error("outstanding request limit reached")]
    Busy,
    /// Remote operation rejected the request.
    #[error("{0}")]
    Remote(#[from] ProtocolError),
}
/// Read exactly one LF-delimited object without unbounded buffering.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Message>, Error> {
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
/// Encode one bounded frame; only the connection's writer task writes it.
pub fn encode_frame(message: &Message) -> Result<Vec<u8>, Error> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|_| Error::Frame("cannot encode envelope"))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::Frame("frame too large"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

const QUEUE: usize = 128;
type Pending = HashMap<u64, oneshot::Sender<Result<Value, Error>>>;
struct Write {
    bytes: Vec<u8>,
    flushed: Option<oneshot::Sender<()>>,
}
struct Inner {
    writer: mpsc::Sender<Write>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    closed: CancellationToken,
}
/// Cloneable, full-duplex wire peer with independent request correlation.
#[derive(Clone)]
pub struct Peer(Arc<Inner>);
/// Uncorrelated messages for the application dispatcher.
#[derive(Debug)]
pub enum Incoming {
    /// Dispatch concurrently; responses may arrive out of order.
    Request {
        /// Correlation ID.
        id: u64,
        /// Semantic operation.
        method: String,
        /// Typed by the application.
        params: Value,
    },
    /// Unsolicited notification.
    Event {
        /// Event stream name.
        event: String,
        /// Typed by the application.
        params: Value,
    },
}
/// Bounded application inbox. Dropping it closes the connection.
pub struct Inbox {
    receiver: mpsc::Receiver<Incoming>,
    peer: Peer,
}
impl Inbox {
    /// Receive the next request/event; None means disconnection.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.receiver.recv().await
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        self.peer.close();
    }
}

struct PendingGuard {
    peer: Peer,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.peer
            .0
            .pending
            .lock()
            .expect("pending lock not poisoned")
            .remove(&self.id);
    }
}
impl Peer {
    /// Start one reader and exactly one writer for a connected byte stream.
    pub fn spawn<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream: S) -> (Self, Inbox) {
        let (reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Write>(QUEUE);
        let (incoming_tx, incoming_rx) = mpsc::channel(QUEUE);
        let peer = Self(Arc::new(Inner {
            writer: tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: CancellationToken::new(),
        }));
        let writer_peer = peer.clone();
        tokio::spawn(async move {
            loop {
                let write = tokio::select! {
                    biased;
                    _ = writer_peer.closed() => break,
                    value = rx.recv() => match value { Some(value) => value, None => break },
                };
                let result = tokio::select! {
                    _ = writer_peer.closed() => break,
                    result = async { writer.write_all(&write.bytes).await?; writer.flush().await } => result,
                };
                if result.is_err() {
                    break;
                }
                if let Some(ack) = write.flushed {
                    let _ = ack.send(());
                }
            }
            writer_peer.close();
            let _ = writer.shutdown().await;
        });
        let reader_peer = peer.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            loop {
                let frame = tokio::select! {
                    _ = reader_peer.closed() => break,
                    frame = read_frame(&mut reader) => frame,
                };
                let message = match frame {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(_) => {
                        reader_peer
                            .fatal("invalid_frame", "malformed, oversized, or incomplete frame")
                            .await;
                        break;
                    }
                };
                match message.body {
                    Body::Response { id, result } => reader_peer.complete(id, Ok(result)),
                    Body::Error {
                        id: Some(id),
                        error,
                    } => reader_peer.complete(id, Err(Error::Remote(error))),
                    Body::Error { id: None, .. } => break,
                    Body::Request { id, method, params } => {
                        if incoming_tx
                            .try_send(Incoming::Request { id, method, params })
                            .is_err()
                        {
                            reader_peer
                                .fatal("slow_consumer", "application inbox exhausted")
                                .await;
                            break;
                        }
                    }
                    Body::Event { event, params } => {
                        if incoming_tx
                            .try_send(Incoming::Event { event, params })
                            .is_err()
                        {
                            reader_peer
                                .fatal("slow_consumer", "application inbox exhausted")
                                .await;
                            break;
                        }
                    }
                }
            }
            reader_peer.close();
        });
        (
            peer.clone(),
            Inbox {
                receiver: incoming_rx,
                peer,
            },
        )
    }
    fn complete(&self, id: u64, value: Result<Value, Error>) {
        if let Some(tx) = self
            .0
            .pending
            .lock()
            .expect("pending lock not poisoned")
            .remove(&id)
        {
            let _ = tx.send(value);
        }
        // Late replies to cancelled requests are harmless, never new requests.
    }
    async fn send(&self, body: Body) -> Result<(), Error> {
        let bytes = encode_frame(&Message::new(body))?;
        tokio::select! {
            biased;
            _ = self.closed() => Err(Error::Closed),
            result = self.0.writer.send(Write { bytes, flushed: None }) => result.map_err(|_| Error::Closed),
        }
    }
    async fn fatal(&self, code: &str, description: &str) {
        let (tx, rx) = oneshot::channel();
        if let Ok(bytes) = encode_frame(&Message::new(Body::Error {
            id: None,
            error: ProtocolError::new(code, description),
        })) {
            // A malicious peer must not keep a decoder alive by refusing to read.
            let send = async {
                self.0
                    .writer
                    .send(Write {
                        bytes,
                        flushed: Some(tx),
                    })
                    .await
                    .map_err(|_| Error::Closed)?;
                rx.await.map_err(|_| Error::Closed)
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), send).await;
        }
    }
    /// Send without waiting for any other request to complete.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self
            .0
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::Busy)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.0.pending.lock().expect("pending lock not poisoned");
            if pending.len() >= QUEUE {
                return Err(Error::Busy);
            }
            pending.insert(id, tx);
        }
        let _guard = PendingGuard {
            peer: self.clone(),
            id,
        };
        self.send(Body::Request {
            id,
            method: method.into(),
            params,
        })
        .await?;
        rx.await.map_err(|_| Error::Closed)?
    }
    /// Respond to an inbound request.
    pub async fn respond(
        &self,
        id: u64,
        result: Result<Value, ProtocolError>,
    ) -> Result<(), Error> {
        self.send(match result {
            Ok(result) => Body::Response { id, result },
            Err(error) => Body::Error {
                id: Some(id),
                error,
            },
        })
        .await
    }
    /// Test peer notification used by duplex conformance; Clients send commands.
    #[cfg(test)]
    pub async fn event(&self, event: &str, params: Value) -> Result<(), Error> {
        self.send(Body::Event {
            event: event.into(),
            params,
        })
        .await
    }
    /// End this connection only, failing outstanding requests promptly.
    pub fn close(&self) {
        self.0.closed.cancel();
        for (_, tx) in self
            .0
            .pending
            .lock()
            .expect("pending lock not poisoned")
            .drain()
        {
            let _ = tx.send(Err(Error::Closed));
        }
    }
    /// Wait for closure without polling.
    pub async fn closed(&self) {
        self.0.closed.cancelled().await;
    }
}
