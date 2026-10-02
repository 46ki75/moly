//! Deterministic framing checks over fragmented and coalesced byte streams.

use crate::transport::{Error, encode_frame, read_frame};
use moly_protocol::{Body, MAX_FRAME_BYTES, Message, VERSION};
use serde_json::{Value, json};
use std::{error::Error as StdError, io, time::Duration};
use tokio::io::{AsyncWriteExt, BufReader};

type TestResult = Result<(), Box<dyn StdError + Send + Sync>>;

fn event(text: &str) -> Message {
    Message::new(Body::Event {
        event: "session.event".into(),
        params: json!({"text": text}),
    })
}

fn assert_same(actual: Option<Message>, expected: &Message) -> TestResult {
    let actual = actual.ok_or_else(|| io::Error::other("expected a frame, not EOF"))?;
    assert_eq!(
        serde_json::to_value(actual)?,
        serde_json::to_value(expected)?
    );
    Ok(())
}

#[tokio::test]
async fn split_and_coalesced_lf_frames_preserve_message_boundaries() -> TestResult {
    let messages = [
        event("embedded\nnewline and Unicode: 日本語"),
        Message::new(Body::Request {
            id: 7,
            method: "future.method".into(),
            params: json!({"value": [1, 2]}),
        }),
        Message::new(Body::Response {
            id: 7,
            result: Value::Null,
        }),
    ];
    let mut bytes = Vec::new();
    for message in &messages {
        bytes.extend(encode_frame(message)?);
    }
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 3);

    // Capacity one also splits multibyte UTF-8 and delivers LF separately.
    // The final capacity puts all three frames in a single fill_buf result.
    for capacity in [1, 2, 7, bytes.len()] {
        let mut reader = BufReader::with_capacity(capacity, bytes.as_slice());
        for message in &messages {
            assert_same(read_frame(&mut reader).await?, message)?;
        }
        assert!(read_frame(&mut reader).await?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn unknown_fields_and_methods_are_not_framing_errors() -> TestResult {
    let bytes = br#"{"version":1,"type":"request","id":41,"method":"future.operation","params":{"future":true},"future_envelope":{"value":1}}
"#;
    let mut reader = BufReader::with_capacity(3, bytes.as_slice());
    let message = read_frame(&mut reader)
        .await?
        .ok_or_else(|| io::Error::other("expected request"))?;
    assert_eq!(message.version, VERSION);
    match message.body {
        Body::Request { id, method, params } => {
            assert_eq!(id, 41);
            assert_eq!(method, "future.operation");
            assert_eq!(params, json!({"future": true}));
        }
        other => return Err(io::Error::other(format!("expected request, got {other:?}")).into()),
    }
    Ok(())
}

#[tokio::test]
async fn request_without_params_defaults_to_null() -> TestResult {
    let bytes = b"{\"version\":1,\"type\":\"request\",\"id\":1,\"method\":\"future\"}\n";
    let mut reader = BufReader::new(bytes.as_slice());
    assert!(matches!(
        read_frame(&mut reader).await?,
        Some(Message {
            body: Body::Request {
                params: Value::Null,
                ..
            },
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn invalid_utf8_and_json_are_rejected() -> TestResult {
    let invalid: &[&[u8]] = &[
        b"{\"version\":1,\"type\":\"event\",\"event\":\"x\",\"params\":\"\xff\"}\n",
        b"{\"version\":1,\"type\":\"event\",\"event\":\"x\",\"params\":\"\xc3\x28\"}\n",
        b"\xff\n",
        b"{\n",
        b"{} {}\n",
    ];
    for bytes in invalid {
        let mut reader = BufReader::with_capacity(1, *bytes);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::Frame("invalid UTF-8 or JSON"))
        ));
    }
    Ok(())
}

#[tokio::test]
async fn nonobjects_and_invalid_envelopes_are_rejected() -> TestResult {
    let nonobjects: &[&[u8]] = &[b"[]\n", b"null\n", b"1\n", b"\"text\"\n"];
    for bytes in nonobjects {
        let mut reader = BufReader::new(*bytes);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::Frame("expected JSON object"))
        ));
    }
    let envelopes: &[&[u8]] = &[
        b"{}\n",
        b"{\"version\":1,\"type\":\"future_type\"}\n",
        b"{\"version\":1,\"type\":\"request\",\"method\":\"x\"}\n",
        b"{\"version\":1,\"type\":\"request\",\"id\":1,\"method\":false}\n",
        b"{\"version\":1,\"type\":\"response\",\"id\":-1,\"result\":null}\n",
    ];
    for bytes in envelopes {
        let mut reader = BufReader::new(*bytes);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::Frame("invalid envelope"))
        ));
    }
    let mut unsupported = event("version");
    unsupported.version = VERSION + 1;
    let bytes = encode_frame(&unsupported)?;
    let mut reader = BufReader::new(bytes.as_slice());
    assert!(matches!(
        read_frame(&mut reader).await,
        Err(Error::Frame("unsupported version"))
    ));
    Ok(())
}

#[tokio::test]
async fn eof_requires_the_final_lf_even_after_a_complete_json_object() -> TestResult {
    let complete = encode_frame(&event("complete"))?;
    let partials = [b"{".as_slice(), &complete[..complete.len() - 1]];
    for partial in partials {
        let mut reader = BufReader::with_capacity(2, partial);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::Frame("partial frame at EOF"))
        ));
    }

    let mut bytes = complete.clone();
    bytes.extend(b"{\"version\":");
    let mut reader = BufReader::with_capacity(2, bytes.as_slice());
    assert_same(read_frame(&mut reader).await?, &event("complete"))?;
    assert!(matches!(
        read_frame(&mut reader).await,
        Err(Error::Frame("partial frame at EOF"))
    ));
    let mut empty = BufReader::new(b"".as_slice());
    assert!(read_frame(&mut empty).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn frame_limit_excludes_lf_and_accepts_the_exact_boundary() -> TestResult {
    let overhead = encode_frame(&event(""))?.len() - 1;
    let message = event(&"x".repeat(MAX_FRAME_BYTES - overhead));
    let bytes = encode_frame(&message)?;
    assert_eq!(bytes.len(), MAX_FRAME_BYTES + 1);
    assert_eq!(bytes.last(), Some(&b'\n'));
    let mut reader = BufReader::with_capacity(127, bytes.as_slice());
    assert_same(read_frame(&mut reader).await?, &message)?;
    assert!(read_frame(&mut reader).await?.is_none());

    let oversized = event(&"x".repeat(MAX_FRAME_BYTES - overhead + 1));
    assert!(matches!(
        encode_frame(&oversized),
        Err(Error::Frame("frame too large"))
    ));
    let mut oversized_bytes = serde_json::to_vec(&oversized)?;
    oversized_bytes.push(b'\n');
    let mut reader = BufReader::with_capacity(127, oversized_bytes.as_slice());
    assert!(matches!(
        read_frame(&mut reader).await,
        Err(Error::Frame("frame too large"))
    ));
    Ok(())
}

#[tokio::test]
async fn oversized_unterminated_frame_fails_before_eof_or_newline() -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut writer, stream) = tokio::io::duplex(MAX_FRAME_BYTES + 1);
        writer.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]).await?;
        let mut reader = BufReader::with_capacity(4096, stream);
        // Keep writer alive: a decoder that waits for LF or EOF would hang.
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::Frame("frame too large"))
        ));
        drop(writer);
        Ok::<(), Box<dyn StdError + Send + Sync>>(())
    })
    .await?
}

#[tokio::test]
async fn oversize_detection_does_not_consume_an_unbounded_input() -> TestResult {
    let bytes = vec![b'x'; MAX_FRAME_BYTES * 3];
    let mut reader = BufReader::with_capacity(4096, bytes.as_slice());
    assert!(matches!(
        read_frame(&mut reader).await,
        Err(Error::Frame("frame too large"))
    ));
    let fetched = bytes.len() - reader.get_ref().len();
    assert!(fetched <= MAX_FRAME_BYTES + 4096);
    Ok(())
}
