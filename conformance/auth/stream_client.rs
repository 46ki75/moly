//! Deliberately small independent JSONL peer for adversarial Client replies.
//! No executable or SDK transport implementation is linked or source-included.
use super::TestError;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{tokio::Stream, traits::tokio::Stream as _};
use moly_client::protocol::{MAX_FRAME_BYTES, SERVER_VERSION};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) struct StreamClient {
    stream: BufReader<Stream>,
    next_id: u64,
}

impl StreamClient {
    pub(super) async fn connect(endpoint: &str) -> Result<Self, TestError> {
        #[cfg(unix)]
        let name = endpoint.to_fs_name::<GenericFilePath>()?;
        #[cfg(windows)]
        let name = endpoint.to_ns_name::<GenericNamespaced>()?;
        let mut client = Self {
            stream: BufReader::new(Stream::connect(name).await?),
            next_id: 1,
        };
        let id = client
            .send("initialize", json!({"protocol_version":SERVER_VERSION}))
            .await?;
        let message = client.result(id).await?;
        assert_eq!(message["result"]["role"], "server");
        assert_eq!(message["result"]["protocol_version"], SERVER_VERSION);
        Ok(client)
    }

    async fn write(&mut self, message: Value) -> Result<(), TestError> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        self.stream.get_mut().write_all(&bytes).await?;
        self.stream.get_mut().flush().await?;
        Ok(())
    }

    pub(super) async fn send(&mut self, method: &str, params: Value) -> Result<u64, TestError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"version":1,"type":"request","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    pub(super) async fn next(&mut self) -> Result<Value, TestError> {
        let mut line = String::new();
        assert_ne!(
            self.stream.read_line(&mut line).await?,
            0,
            "unexpected Client EOF"
        );
        assert!(line.ends_with('\n') && line.len() <= MAX_FRAME_BYTES + 1);
        let message: Value = serde_json::from_str(&line)?;
        assert_eq!(message["version"], 1);
        Ok(message)
    }

    pub(super) async fn result(&mut self, id: u64) -> Result<Value, TestError> {
        let message = self.next().await?;
        assert!(matches!(
            message["type"].as_str(),
            Some("response" | "error")
        ));
        assert_eq!(message["id"], id);
        Ok(message)
    }

    pub(super) async fn reply(&mut self, id: u64, result: Value) -> Result<(), TestError> {
        self.write(json!({"version":1,"type":"response","id":id,"result":result}))
            .await
    }
}
