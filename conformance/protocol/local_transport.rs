//! Hermetic local IPC tests using private temporary endpoint directories.

use crate::transport::{
    Inbox, Incoming, Peer,
    local::{Listener, connect},
};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{error::Error as StdError, future::Future, io, time::Duration};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type BoxError = Box<dyn StdError + Send + Sync>;
type TestResult = Result<(), BoxError>;

struct Endpoint {
    // Own the directory for at least as long as every listener and connection.
    _directory: TempDir,
    name: String,
}

impl Endpoint {
    fn new() -> Result<Self, BoxError> {
        #[cfg(unix)]
        // macOS's default temporary path can exceed the Unix socket path limit.
        let directory = tempfile::Builder::new()
            .prefix("moly-ipc-")
            // Tempdirs otherwise inherit umask-based, usually world-readable modes.
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")?;
        #[cfg(windows)]
        let directory = tempfile::Builder::new().prefix("moly-ipc-").tempdir()?;
        #[cfg(unix)]
        let name = directory
            .path()
            .join("endpoint.sock")
            .to_str()
            .ok_or_else(|| io::Error::other("test endpoint must be UTF-8"))?
            .to_owned();
        #[cfg(windows)]
        let name = directory
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("test pipe name must be UTF-8"))?
            .to_owned();
        Ok(Self {
            _directory: directory,
            name,
        })
    }
}

async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), future).await?
}

async fn echo(client: &Peer, server: &Peer, inbox: &mut Inbox, value: Value) -> TestResult {
    let expected = value.clone();
    let respond = async {
        let Some(Incoming::Request { id, method, params }) = inbox.recv().await else {
            return Err(io::Error::other("expected local request").into());
        };
        assert_eq!(method, "echo");
        server.respond(id, Ok(params)).await?;
        Ok::<(), BoxError>(())
    };
    let (result, ()) = tokio::try_join!(
        async { Ok::<Value, BoxError>(client.request("echo", value).await?) },
        respond,
    )?;
    assert_eq!(result, expected);
    Ok(())
}

#[tokio::test]
async fn local_stream_transfers_binary_bytes_in_both_directions() -> TestResult {
    bounded(async {
        let endpoint = Endpoint::new()?;
        let listener = Listener::bind(&endpoint.name)?;
        let (mut client, mut server) =
            tokio::try_join!(connect(&endpoint.name), listener.accept())?;
        // Accepted connections must remain usable after the listener is dropped.
        drop(listener);
        let request = b"bytes\0with\nLF\xffand\xc3\xa9";
        let reply = b"reverse\nbytes\0\xfe";
        let client_io = async {
            for chunk in request.chunks(3) {
                client.write_all(chunk).await?;
            }
            client.flush().await?;
            let mut received = vec![0; reply.len()];
            client.read_exact(&mut received).await?;
            assert_eq!(received, reply);
            // The documented interprocess shutdown is a no-op, not half-close.
            client.shutdown().await?;
            client.write_all(b"still open").await?;
            Ok::<(), io::Error>(())
        };
        let server_io = async {
            let mut received = vec![0; request.len()];
            server.read_exact(&mut received).await?;
            assert_eq!(received, request);
            server.write_all(reply).await?;
            server.flush().await?;
            let mut after_shutdown = [0; 10];
            server.read_exact(&mut after_shutdown).await?;
            assert_eq!(&after_shutdown, b"still open");
            Ok::<(), io::Error>(())
        };
        tokio::try_join!(client_io, server_io)?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn listener_accepts_multiple_independent_full_duplex_connections() -> TestResult {
    bounded(async {
        let endpoint = Endpoint::new()?;
        let listener = Listener::bind(&endpoint.name)?;
        let (a, b) = tokio::try_join!(connect(&endpoint.name), listener.accept())?;
        let (first, _first_inbox) = Peer::spawn(a);
        let (first_server, mut first_server_inbox) = Peer::spawn(b);
        let (a, b) = tokio::try_join!(connect(&endpoint.name), listener.accept())?;
        let (second, mut second_inbox) = Peer::spawn(a);
        let (second_server, mut second_server_inbox) = Peer::spawn(b);

        tokio::try_join!(
            echo(
                &first,
                &first_server,
                &mut first_server_inbox,
                json!({"connection": 1})
            ),
            echo(
                &second,
                &second_server,
                &mut second_server_inbox,
                json!({"connection": 2})
            ),
        )?;
        first.close();
        first_server.closed().await;
        echo(
            &second,
            &second_server,
            &mut second_server_inbox,
            json!("unaffected"),
        )
        .await?;

        // A disconnect must not end the accept loop or another connection.
        let (a, b) = tokio::try_join!(connect(&endpoint.name), listener.accept())?;
        let (third, mut third_inbox) = Peer::spawn(a);
        let (third_server, mut third_server_inbox) = Peer::spawn(b);
        drop(listener);
        tokio::try_join!(
            echo(
                &second,
                &second_server,
                &mut second_server_inbox,
                json!("listener dropped")
            ),
            echo(&third, &third_server, &mut third_server_inbox, json!(3)),
        )?;
        // Reverse requests use exactly the same transport and connection.
        echo(&second_server, &second, &mut second_inbox, json!("reverse")).await?;
        echo(
            &third_server,
            &third,
            &mut third_inbox,
            json!("reverse third"),
        )
        .await?;
        second.close();
        third.close();
        second_server.closed().await;
        third_server.closed().await;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn empty_and_nul_endpoints_return_invalid_input() -> TestResult {
    for endpoint in ["", "invalid\0endpoint"] {
        match Listener::bind(endpoint) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => return Err(io::Error::other("invalid endpoint bound successfully").into()),
        }
        match connect(endpoint).await {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => return Err(io::Error::other("invalid endpoint connected successfully").into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bind_collision_preserves_the_existing_listener_and_drop_reclaims_its_path() -> TestResult {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    bounded(async {
        let endpoint = Endpoint::new()?;
        let parent = std::path::Path::new(&endpoint.name)
            .parent()
            .ok_or_else(|| io::Error::other("endpoint needs a parent"))?;
        assert_eq!(
            std::fs::metadata(parent)?.permissions().mode() & 0o777,
            0o700
        );
        let listener = Listener::bind(&endpoint.name)?;
        let before = std::fs::symlink_metadata(&endpoint.name)?;
        assert!(before.file_type().is_socket());
        assert!(Listener::bind(&endpoint.name).is_err());
        let after = std::fs::symlink_metadata(&endpoint.name)?;
        assert_eq!(before.ino(), after.ino());
        let (mut client, mut server) =
            tokio::try_join!(connect(&endpoint.name), listener.accept())?;
        client.write_all(b"original").await?;
        let mut bytes = [0; 8];
        server.read_exact(&mut bytes).await?;
        assert_eq!(&bytes, b"original");
        drop(listener);
        assert!(!std::path::Path::new(&endpoint.name).exists());
        // Cleanup happened even though accepted streams are still alive.
        drop((client, server));
        let rebound = Listener::bind(&endpoint.name)?;
        drop(rebound);
        assert!(!std::path::Path::new(&endpoint.name).exists());
        Ok(())
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bind_does_not_remove_regular_files_symlinks_or_stale_sockets() -> TestResult {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};

    let endpoint = Endpoint::new()?;
    std::fs::write(&endpoint.name, b"owned by someone else")?;
    assert!(Listener::bind(&endpoint.name).is_err());
    assert_eq!(std::fs::read(&endpoint.name)?, b"owned by someone else");
    std::fs::remove_file(&endpoint.name)?;

    let target = endpoint._directory.path().join("target");
    std::fs::write(&target, b"keep target")?;
    symlink(&target, &endpoint.name)?;
    assert!(Listener::bind(&endpoint.name).is_err());
    assert!(
        std::fs::symlink_metadata(&endpoint.name)?
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target)?, b"keep target");
    std::fs::remove_file(&endpoint.name)?;

    // Simulate an abandoned socket without leaking a listener or using any
    // platform socket API. Move it out of reach of the verified drop cleanup.
    let listener = Listener::bind(&endpoint.name)?;
    let parked = endpoint._directory.path().join("parked.sock");
    std::fs::rename(&endpoint.name, &parked)?;
    drop(listener);
    std::fs::rename(&parked, &endpoint.name)?;
    let before = std::fs::symlink_metadata(&endpoint.name)?;
    assert!(before.file_type().is_socket());
    assert!(Listener::bind(&endpoint.name).is_err());
    assert_eq!(
        before.ino(),
        std::fs::symlink_metadata(&endpoint.name)?.ino()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn binding_does_not_create_missing_parent_directories() -> TestResult {
    let endpoint = Endpoint::new()?;
    let parent = endpoint._directory.path().join("missing");
    let path = parent.join("endpoint.sock");
    let name = path
        .to_str()
        .ok_or_else(|| io::Error::other("endpoint must be UTF-8"))?;
    assert!(Listener::bind(name).is_err());
    assert!(!parent.exists());
    Ok(())
}
