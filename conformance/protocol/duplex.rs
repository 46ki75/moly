//! Full-duplex correlation, cancellation, and connection failure checks.

use crate::transport::{Error, Inbox, Incoming, Peer, read_frame};
use moly_protocol::{Body, MAX_FRAME_BYTES, ProtocolError};
use serde_json::{Value, json};
use std::{error::Error as StdError, future::Future, io, time::Duration};
use tokio::io::{AsyncWriteExt, BufReader};

type BoxError = Box<dyn StdError + Send + Sync>;
type TestResult = Result<(), BoxError>;

async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), future).await?
}

async fn next_request(inbox: &mut Inbox) -> Result<(u64, String, Value), BoxError> {
    match inbox.recv().await {
        Some(Incoming::Request { id, method, params }) => Ok((id, method, params)),
        other => Err(io::Error::other(format!("expected request, got {other:?}")).into()),
    }
}

async fn next_event(inbox: &mut Inbox, name: &str, value: Value) -> TestResult {
    match inbox.recv().await {
        Some(Incoming::Event { event, params }) => {
            assert_eq!(event, name);
            assert_eq!(params, value);
            Ok(())
        }
        other => Err(io::Error::other(format!("expected event, got {other:?}")).into()),
    }
}

fn request_task(
    peer: &Peer,
    method: &'static str,
    params: Value,
) -> tokio::task::JoinHandle<Result<Value, Error>> {
    let peer = peer.clone();
    tokio::spawn(async move { peer.request(method, params).await })
}

#[tokio::test]
async fn overlapping_reverse_requests_use_direction_local_ids_and_out_of_order_replies()
-> TestResult {
    bounded(async {
        // Tiny buffers force the single writer to handle partial stream writes.
        let (a, b) = tokio::io::duplex(17);
        let (left, mut left_inbox) = Peer::spawn(a);
        let (right, mut right_inbox) = Peer::spawn(b);
        let forward_one = request_task(&left, "forward.one", json!(1));
        let forward_two = request_task(&left, "forward.two", json!(2));
        let reverse_one = request_task(&right, "tool.execute.one", json!(3));
        let reverse_two = request_task(&right, "tool.execute.two", json!(4));

        let collect = async |inbox: &mut Inbox| -> Result<Vec<(u64, String, Value)>, BoxError> {
            Ok(vec![next_request(inbox).await?, next_request(inbox).await?])
        };
        let (forward, reverse) =
            tokio::try_join!(collect(&mut right_inbox), collect(&mut left_inbox))?;
        let mut forward_ids: Vec<_> = forward.iter().map(|request| request.0).collect();
        let mut reverse_ids: Vec<_> = reverse.iter().map(|request| request.0).collect();
        forward_ids.sort_unstable();
        reverse_ids.sort_unstable();
        assert_eq!(forward_ids, [1, 2]);
        assert_eq!(forward_ids, reverse_ids);

        tokio::try_join!(
            left.event("left.event", json!({"side": "left"})),
            right.event("right.event", json!({"side": "right"})),
        )?;
        for (id, method, params) in forward.iter().rev() {
            assert!(method.starts_with("forward."));
            right.respond(*id, Ok(json!({"forward": params}))).await?;
        }
        for (id, method, params) in reverse.iter().rev() {
            assert!(method.starts_with("tool.execute."));
            left.respond(*id, Ok(json!({"reverse": params}))).await?;
        }
        assert_eq!(forward_one.await??, json!({"forward": 1}));
        assert_eq!(forward_two.await??, json!({"forward": 2}));
        assert_eq!(reverse_one.await??, json!({"reverse": 3}));
        assert_eq!(reverse_two.await??, json!({"reverse": 4}));
        next_event(&mut right_inbox, "left.event", json!({"side": "left"})).await?;
        next_event(&mut left_inbox, "right.event", json!({"side": "right"})).await?;
        left.close();
        right.closed().await;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn unknown_method_reaches_dispatcher_and_correlated_error_keeps_connection_open() -> TestResult
{
    bounded(async {
        let (a, b) = tokio::io::duplex(128);
        let (left, mut left_inbox) = Peer::spawn(a);
        let (right, mut right_inbox) = Peer::spawn(b);
        let unknown = request_task(&left, "future.operation", json!({"future": true}));
        let (id, method, params) = next_request(&mut right_inbox).await?;
        assert_eq!(method, "future.operation");
        assert_eq!(params, json!({"future": true}));
        let error = ProtocolError::new("unknown_method", "unsupported operation");
        right.respond(id, Err(error.clone())).await?;
        match unknown.await? {
            Err(Error::Remote(actual)) => assert_eq!(actual, error),
            other => {
                return Err(
                    io::Error::other(format!("expected remote error, got {other:?}")).into(),
                );
            }
        }
        let known = request_task(&left, "known", Value::Null);
        let (id, method, _) = next_request(&mut right_inbox).await?;
        assert_eq!(method, "known");
        right.respond(id, Ok(json!("still connected"))).await?;
        assert_eq!(known.await??, json!("still connected"));
        right.event("after.error", Value::Null).await?;
        next_event(&mut left_inbox, "after.error", Value::Null).await?;
        left.close();
        Ok(())
    })
    .await
}

#[tokio::test]
async fn dropped_request_futures_release_slots_and_late_replies_are_ignored() -> TestResult {
    bounded(async {
        let (a, b) = tokio::io::duplex(128);
        let (left, mut left_inbox) = Peer::spawn(a);
        let (right, mut right_inbox) = Peer::spawn(b);
        let mut cancelled_ids = Vec::new();
        // Twice the outstanding-request limit, deliberately with no replies:
        // retaining any cancelled entries would eventually return Busy.
        for index in 0..256 {
            let mut future = Box::pin(left.request("cancelled", json!(index)));
            let (id, method, params) = tokio::select! {
                result = &mut future => return Err(io::Error::other(format!("request completed before cancellation: {result:?}")).into()),
                request = next_request(&mut right_inbox) => request?,
            };
            assert_eq!(method, "cancelled");
            assert_eq!(params, json!(index));
            drop(future);
            cancelled_ids.push(id);
        }
        let live = request_task(&left, "live", Value::Null);
        let (live_id, method, _) = next_request(&mut right_inbox).await?;
        assert_eq!(method, "live");
        assert!(!cancelled_ids.contains(&live_id));
        for (index, id) in cancelled_ids.into_iter().enumerate() {
            let reply = if index % 2 == 0 {
                Ok(json!("late success"))
            } else {
                Err(ProtocolError::new("late_error", "cancelled request"))
            };
            right.respond(id, reply).await?;
        }
        // Reader order makes this a deterministic barrier after all late replies.
        right.event("late.replies.drained", Value::Null).await?;
        next_event(&mut left_inbox, "late.replies.drained", Value::Null).await?;
        assert!(!live.is_finished());
        right.respond(live_id, Ok(json!("live result"))).await?;
        assert_eq!(live.await??, json!("live result"));
        left.close();
        Ok(())
    }).await
}

#[tokio::test]
async fn pending_requests_fail_on_local_close_remote_close_and_inbox_drop() -> TestResult {
    bounded(async {
        for close_kind in 0..3 {
            let (a, b) = tokio::io::duplex(128);
            let (left, mut left_inbox) = Peer::spawn(a);
            let (right, right_inbox) = Peer::spawn(b);
            let mut right_inbox = Some(right_inbox);
            let mut pending = Vec::new();
            for index in 0..4 {
                pending.push(request_task(&left, "pending", json!(index)));
            }
            let inbox = right_inbox
                .as_mut()
                .ok_or_else(|| io::Error::other("missing inbox"))?;
            for _ in 0..pending.len() {
                next_request(inbox).await?;
            }
            match close_kind {
                0 => left.close(),
                1 => right.close(),
                _ => drop(right_inbox.take()),
            }
            for request in pending {
                assert!(matches!(request.await?, Err(Error::Closed)));
            }
            left.closed().await;
            right.closed().await;
            assert!(left_inbox.recv().await.is_none());
            assert!(matches!(
                left.request("after.close", Value::Null).await,
                Err(Error::Closed)
            ));
            assert!(matches!(
                left.event("after.close", Value::Null).await,
                Err(Error::Closed)
            ));
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn transport_eof_fails_all_pending_requests() -> TestResult {
    bounded(async {
        let (a, b) = tokio::io::duplex(128);
        let (peer, mut inbox) = Peer::spawn(a);
        let first = request_task(&peer, "first", Value::Null);
        let second = request_task(&peer, "second", Value::Null);
        let mut reader = BufReader::new(b);
        for _ in 0..2 {
            assert!(matches!(
                read_frame(&mut reader).await?,
                Some(moly_protocol::Message {
                    body: Body::Request { .. },
                    ..
                })
            ));
        }
        drop(reader);
        assert!(matches!(first.await?, Err(Error::Closed)));
        assert!(matches!(second.await?, Err(Error::Closed)));
        peer.closed().await;
        assert!(inbox.recv().await.is_none());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn malformed_and_oversized_frames_emit_uncorrelated_fatal_errors() -> TestResult {
    bounded(async {
        let invalid_frames = [
            b"{\"version\":1,\"type\":\"future_type\"}\n".to_vec(),
            b"\xff\n".to_vec(),
            vec![b'x'; MAX_FRAME_BYTES + 1],
        ];
        for invalid in invalid_frames {
            let (a, b) = tokio::io::duplex(4096);
            let (peer, mut inbox) = Peer::spawn(a);
            let pending = request_task(&peer, "pending", Value::Null);
            let (reader, mut writer) = tokio::io::split(b);
            let mut reader = BufReader::new(reader);
            assert!(read_frame(&mut reader).await?.is_some());
            writer.write_all(&invalid).await?;
            match read_frame(&mut reader).await? {
                Some(moly_protocol::Message {
                    body: Body::Error { id, error },
                    ..
                }) => {
                    assert_eq!(id, None);
                    assert_eq!(error.code, "invalid_frame");
                }
                other => {
                    return Err(
                        io::Error::other(format!("expected fatal error, got {other:?}")).into(),
                    );
                }
            }
            assert!(matches!(pending.await?, Err(Error::Closed)));
            peer.closed().await;
            assert!(inbox.recv().await.is_none());
        }
        Ok(())
    })
    .await
}
