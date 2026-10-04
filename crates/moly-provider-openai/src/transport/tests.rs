use super::*;
use moly_protocol::model::PROVIDER_VERSION;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;

fn request(id: u64, method: &str, params: Value) -> Message {
    Message::new(Body::Request {
        id,
        method: method.into(),
        params,
    })
}

fn step(id: u64, endpoint: String) -> Message {
    request(
        id,
        "provider.step",
        json!({
            "options": {"model_endpoint": endpoint, "model": "mock"},
            "credential": "private-credential",
            "context": {
                "session_id": "00000000-0000-4000-8000-000000000001",
                "run_id": "00000000-0000-4000-8000-000000000002",
                "model_call_id": "00000000-0000-4000-8000-000000000003",
                "call_kind": "primary"
            },
            "messages": [{"kind": "user", "text": "private-prompt"}],
            "tools": []
        }),
    )
}

async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Message, TestError> {
    tokio::time::timeout(Duration::from_secs(5), read_frame(reader))
        .await??
        .ok_or_else(|| "expected response".into())
}

async fn accept_request(listener: &TcpListener) -> Result<TcpStream, TestError> {
    let (stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(stream);
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err("unexpected mock HTTP EOF".into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; length.ok_or("missing content length")?];
    reader.read_exact(&mut body).await?;
    Ok(reader.into_inner())
}

async fn complete(stream: &mut TcpStream) -> TestResult {
    let body = br#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn split_coalesced_requests_have_ordered_correlated_replies() -> TestResult {
    let (client, server) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(server);
    let service = Service::new()?;
    let server = tokio::spawn(serve(BufReader::new(read), write, service));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    let mut bytes = encode_frame(&request(
        7,
        "initialize",
        json!({"protocol_version": PROVIDER_VERSION}),
    ))?;
    bytes.extend(encode_frame(&request(
        8,
        "provider.validate",
        json!({"model_endpoint": "http://localhost/exact", "model": "mock"}),
    ))?);
    bytes.extend(encode_frame(&request(9, "future.method", Value::Null))?);
    for chunk in bytes.chunks(3) {
        write.write_all(chunk).await?;
    }
    assert!(
        matches!(read_reply(&mut read).await?.body, Body::Response { id: 7, result } if result["role"] == "model_provider")
    );
    assert!(matches!(
        read_reply(&mut read).await?.body,
        Body::Response {
            id: 8,
            result: Value::Null
        }
    ));
    assert!(
        matches!(read_reply(&mut read).await?.body, Body::Error { id: Some(9), error } if error.code == "unknown_method")
    );
    write.shutdown().await?;
    tokio::time::timeout(Duration::from_secs(1), server).await???;
    Ok(())
}

#[tokio::test]
async fn invalid_input_gets_redacted_fatal_error_and_closes() -> TestResult {
    for bytes in [
        b"private-invalid-json\n".as_slice(),
        b"\xff\n",
        b"{\"version\":2,\"type\":\"request\",\"id\":1,\"method\":\"initialize\"}\n",
        b"{\"version\":1,\"type\":\"event\",\"event\":\"private-event\",\"params\":null}\n",
        b"{\"version\":1,\"type\":\"response\",\"id\":1,\"result\":null}\n",
        b"{\"private-partial\":",
    ] {
        let (client, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let server = tokio::spawn(serve(BufReader::new(read), write, Service::new()?));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write.write_all(bytes).await?;
        if !bytes.ends_with(b"\n") {
            write.shutdown().await?;
        }
        let response = read_reply(&mut read).await?;
        assert!(!serde_json::to_string(&response)?.contains("private"));
        assert!(
            matches!(response.body, Body::Error { id: None, error } if error.code == "invalid_frame")
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), server)
                .await??
                .is_err()
        );
        assert!(read_frame(&mut read).await?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn eof_and_malformed_input_abort_an_in_flight_http_request() -> TestResult {
    for malformed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/", listener.local_addr()?);
        let (client, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let server = tokio::spawn(serve(BufReader::new(read), write, Service::new()?));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write_frame(
            &mut write,
            &request(
                1,
                "initialize",
                json!({"protocol_version": PROVIDER_VERSION}),
            ),
        )
        .await?;
        read_reply(&mut read).await?;
        write_frame(&mut write, &step(2, endpoint)).await?;
        let mut http =
            tokio::time::timeout(Duration::from_secs(5), accept_request(&listener)).await??;
        http.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
            .await?;
        if malformed {
            write.write_all(b"private-malformed-frame\n").await?;
            assert!(
                matches!(read_reply(&mut read).await?.body, Body::Error { id: None, error } if error.code == "invalid_frame")
            );
        } else {
            write.shutdown().await?;
        }
        let result = tokio::time::timeout(Duration::from_secs(1), server).await??;
        assert_eq!(result.is_err(), malformed);
        assert!(read_frame(&mut read).await?.is_none());
        let mut byte = [0];
        match tokio::time::timeout(Duration::from_secs(1), http.read(&mut byte)).await? {
            Ok(0) => {}
            // Dropping a socket with unread response bytes may send TCP RST.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            _ => return Err("HTTP request was not disconnected".into()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn requests_are_not_executed_concurrently() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/", listener.local_addr()?);
    let (client, server) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(server);
    let server = tokio::spawn(serve(BufReader::new(read), write, Service::new()?));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_frame(
        &mut write,
        &request(
            1,
            "initialize",
            json!({"protocol_version": PROVIDER_VERSION}),
        ),
    )
    .await?;
    read_reply(&mut read).await?;
    write_frame(&mut write, &step(2, endpoint.clone())).await?;
    let mut first =
        tokio::time::timeout(Duration::from_secs(5), accept_request(&listener)).await??;
    write_frame(&mut write, &step(3, endpoint)).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    complete(&mut first).await?;
    assert!(matches!(
        read_reply(&mut read).await?.body,
        Body::Response { id: 2, .. }
    ));
    let mut second =
        tokio::time::timeout(Duration::from_secs(5), accept_request(&listener)).await??;
    complete(&mut second).await?;
    assert!(matches!(
        read_reply(&mut read).await?.body,
        Body::Response { id: 3, .. }
    ));
    write.shutdown().await?;
    tokio::time::timeout(Duration::from_secs(1), server).await???;
    Ok(())
}

#[tokio::test]
async fn full_request_queue_closes_instead_of_hiding_eof() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/", listener.local_addr()?);
    let (client, server) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(server);
    let server = tokio::spawn(serve(BufReader::new(read), write, Service::new()?));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_frame(
        &mut write,
        &request(
            1,
            "initialize",
            json!({"protocol_version": PROVIDER_VERSION}),
        ),
    )
    .await?;
    read_reply(&mut read).await?;
    write_frame(&mut write, &step(2, endpoint)).await?;
    let _http = tokio::time::timeout(Duration::from_secs(5), accept_request(&listener)).await??;
    for id in 3..=(3 + REQUEST_QUEUE as u64) {
        write_frame(&mut write, &request(id, "private.method", Value::Null)).await?;
    }
    assert!(
        matches!(read_reply(&mut read).await?.body, Body::Error { id: None, error } if error.code == "slow_consumer")
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), server).await??,
        Err(Error::Overloaded)
    ));
    Ok(())
}
