//! Local IPC byte streams without exposing platform socket types.
//!
//! Unix endpoints are filesystem paths, including on Linux; Windows endpoints
//! are local named-pipe names without the `\\.\pipe\` prefix, in byte mode.
//!
//! # Endpoint ownership
//!
//! On Unix, the caller must create a private, user-owned parent directory (for
//! example, mode `0700`) before binding. This module neither creates directories
//! nor authenticates peers. Keep the endpoint path and its parent directories
//! unchanged until the listener is dropped: interprocess reclaims the socket by
//! pathname, not inode identity. Binding never overwrites an existing endpoint;
//! stale endpoints require cleanup by their owner, not by this module.

#[cfg(test)]
use interprocess::local_socket::traits::tokio::Stream as _;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions, Name,
    tokio::{Listener as IpcListener, Stream as IpcStream},
    traits::tokio::Listener as _,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn endpoint_name(endpoint: &str) -> io::Result<Name<'_>> {
    if endpoint.is_empty() || endpoint.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint must be nonempty and contain no NUL characters",
        ));
    }
    #[cfg(unix)]
    {
        endpoint.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        endpoint.to_ns_name::<GenericNamespaced>()
    }
}

/// Opaque, full-duplex local IPC byte stream.
///
/// Like interprocess's Tokio local stream, flushing and write shutdown are
/// successful no-ops. Drop the entire stream to disconnect; shutdown does not
/// signal EOF to the peer.
pub struct LocalStream(IpcStream);

impl AsyncRead for LocalStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Local IPC listener supporting independent, simultaneous connections.
///
/// Dropping it reclaims its Unix socket path through interprocess; it does not
/// close already accepted streams. See the module's endpoint ownership rules.
pub struct Listener(IpcListener);

impl Listener {
    /// Bind a Unix filesystem path or a Windows local named-pipe name.
    ///
    /// Requires an entered Tokio runtime with I/O enabled. On Unix, the parent
    /// directory must already exist and be private to the current user. Existing
    /// endpoints cause an error and are never removed to make binding succeed.
    pub fn bind(endpoint: &str) -> io::Result<Self> {
        // Verified in interprocess 2.4.0 and locked 2.4.4: only successful binding
        // arms ReclaimGuard, which survives Tokio conversion and unlinks on drop.
        // https://docs.rs/interprocess/2.4.4/src/interprocess/os/unix/uds_local_socket.rs.html
        ListenerOptions::new()
            .name(endpoint_name(endpoint)?)
            .try_overwrite(false)
            .reclaim_name(true)
            .create_tokio()
            .map(Self)
    }

    /// Accept one connection without taking ownership of the listener.
    pub async fn accept(&self) -> io::Result<LocalStream> {
        self.0.accept().await.map(LocalStream)
    }
}

/// Connect to a Unix filesystem path or a Windows local named-pipe name.
///
/// Requires a Tokio runtime with I/O enabled. Endpoint selection follows the
/// same rules as [`Listener::bind`]; no configuration discovery is performed.
#[cfg(test)]
pub async fn connect(endpoint: &str) -> io::Result<LocalStream> {
    IpcStream::connect(endpoint_name(endpoint)?)
        .await
        .map(LocalStream)
}
