//! Fixed-identity protocol traces survive implementation and transport changes.

use crate::transport;
use moly_protocol::{
    Body, EventKind, Initialized, Message, RunStart, SessionEvent, SessionRef, Subscribe,
    ToolExecute, ToolResult, ToolsRegister, VERSION,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::io::BufReader;

type TestError = Box<dyn Error + Send + Sync>;
const TRACE: &str = include_str!("../traces/hosted-tool-duplex-replay.jsonl");

// Keep the original v1 asset and its exact schema, even though live Servers now
// negotiate v3 authentication. Envelope, tool leases, and event ordering survive.
#[derive(Deserialize, Serialize)]
struct LegacyConfig {
    model_endpoint: String,
    model: String,
    workspace: String,
    secret_ref: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct ConfigApply {
    base_revision: u64,
    config: LegacyConfig,
}
#[derive(Deserialize, Serialize)]
struct ConfigSnapshot {
    revision: u64,
    config: Option<LegacyConfig>,
}

fn roundtrip<T: DeserializeOwned + Serialize>(value: &Value) -> Result<T, TestError> {
    let typed: T = serde_json::from_value(value.clone())?;
    assert_eq!(serde_json::to_value(&typed)?, *value);
    Ok(typed)
}

#[tokio::test]
async fn canonical_hosted_tool_trace_roundtrips_and_preserves_duplex_replay_order()
-> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        assert!(TRACE.ends_with('\n'));
        let mut reader = BufReader::new(TRACE.as_bytes());
        let mut messages = Vec::new();
        for line in TRACE.lines() {
            let value: Value = serde_json::from_str(line)?;
            let message: Message = roundtrip(&value)?;
            assert_eq!(message.version, VERSION);
            let framed = transport::read_frame(&mut reader)
                .await?
                .ok_or("missing trace frame")?;
            assert_eq!(serde_json::to_value(framed)?, value);
            let bytes = transport::encode_frame(&message)?;
            assert_eq!(bytes.last(), Some(&b'\n'));
            assert_eq!(serde_json::from_slice::<Value>(&bytes)?, value);
            messages.push(message);
        }
        assert!(transport::read_frame(&mut reader).await?.is_none());

        let mut events = Vec::new();
        let mut methods = Vec::new();
        let mut execute = None;
        let mut tool_result = None;
        let mut nested_request = None;
        let mut nested_response = None;
        let mut replay_request = None;
        for (index, message) in messages.iter().enumerate() {
            match &message.body {
                Body::Request { id, method, params } => {
                    methods.push(method.as_str());
                    match method.as_str() {
                        "initialize" => assert_eq!(params, &json!({"protocol_version":VERSION})),
                        "config.apply" => {
                            let apply: ConfigApply = roundtrip(params)?;
                            assert_eq!(apply.base_revision, 0);
                            assert!(apply.config.secret_ref.is_none());
                        }
                        "session.create" => assert!(params.is_null()),
                        "tools.register" => {
                            let registration: ToolsRegister = roundtrip(params)?;
                            assert_eq!(registration.tools.len(), 1);
                            assert_eq!(registration.tools[0].name, "client_echo");
                        }
                        "subscribe" => {
                            let subscribe: Subscribe = roundtrip(params)?;
                            if *id == 8 {
                                assert_eq!(subscribe.after_seq, 4);
                                replay_request = Some(index);
                            } else {
                                assert_eq!(subscribe.after_seq, 0);
                            }
                        }
                        "run.start" => {
                            let run: RunStart = roundtrip(params)?;
                            assert_eq!(run.message, "Echo hello");
                        }
                        "tool.execute" => {
                            assert_eq!(*id, 1);
                            execute = Some((index, roundtrip::<ToolExecute>(params)?));
                        }
                        "config.get" => {
                            assert_eq!(*id, 7);
                            assert!(params.is_null());
                            nested_request = Some(index);
                        }
                        _ => return Err("unexpected canonical method".into()),
                    }
                }
                Body::Response { id, result } => match *id {
                    1 if result.get("server_id").is_some() => {
                        let initialized: Initialized = roundtrip(result)?;
                        assert_eq!(
                            initialized.server_id.to_string(),
                            "00000000-0000-4000-8000-000000000001"
                        );
                        assert_eq!(initialized.role, "server");
                        assert_eq!(initialized.protocol_version, VERSION);
                    }
                    1 => tool_result = Some((index, roundtrip::<ToolResult>(result)?)),
                    2 | 7 => {
                        let config: ConfigSnapshot = roundtrip(result)?;
                        assert_eq!(config.revision, 1);
                        if *id == 7 {
                            nested_response = Some(index);
                        }
                    }
                    3 => {
                        let session: SessionRef = roundtrip(result)?;
                        assert_eq!(
                            session.session_id.to_string(),
                            "00000000-0000-4000-8000-000000000002"
                        );
                    }
                    4 => assert_eq!(
                        result["executor_id"],
                        "00000000-0000-4000-8000-000000000003"
                    ),
                    5 => assert_eq!(result, &json!({"live_head":1})),
                    6 => assert_eq!(result["run_id"], "00000000-0000-4000-8000-000000000004"),
                    8 => assert_eq!(result, &json!({"live_head":9})),
                    _ => return Err("unexpected canonical response".into()),
                },
                Body::Event { event, params } => {
                    assert_eq!(event, "session.event");
                    events.push((index, roundtrip::<SessionEvent>(params)?));
                }
                Body::Error { .. } => {
                    return Err("unexpected error in successful canonical trace".into());
                }
            }
        }
        assert_eq!(
            methods,
            [
                "initialize",
                "config.apply",
                "session.create",
                "tools.register",
                "subscribe",
                "run.start",
                "tool.execute",
                "config.get",
                "subscribe"
            ]
        );
        assert_eq!(events.len(), 14);
        let authoritative = &events[..9];
        for (offset, (_, event)) in authoritative.iter().enumerate() {
            assert_eq!(event.seq, offset as u64 + 1);
            assert_eq!(
                event.session_id.to_string(),
                "00000000-0000-4000-8000-000000000002"
            );
            let value = serde_json::to_value(event)?;
            if let Some(run) = value.get("run_id") {
                assert_eq!(run, "00000000-0000-4000-8000-000000000004");
            }
        }
        let kinds = authoritative
            .iter()
            .map(|(_, event)| Ok::<_, TestError>(serde_json::to_value(event)?["kind"].clone()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            kinds,
            json!([
                "session_created",
                "message_accepted",
                "run_started",
                "model_call_started",
                "tool_started",
                "tool_completed",
                "model_call_started",
                "assistant_message",
                "run_completed"
            ])
            .as_array()
            .ok_or("expected kind array")?
            .to_vec()
        );

        let (execute_index, execute) = execute.ok_or("missing reverse tool request")?;
        let (result_index, result) = tool_result.ok_or("missing reverse tool response")?;
        assert_eq!(execute.session_id, authoritative[0].1.session_id);
        assert_eq!(execute.name, "client_echo");
        assert_eq!(execute.arguments, json!({"text":"hello"}));
        assert_eq!(execute.lease, result.lease);
        assert_eq!(execute.lease.generation, 1);
        assert_eq!(
            execute.lease.executor_id.to_string(),
            "00000000-0000-4000-8000-000000000003"
        );
        assert_eq!(
            result.output,
            json!({"revision":1, "echo":{"text":"hello"}})
        );
        match (&authoritative[4].1.kind, &authoritative[5].1.kind) {
            (
                EventKind::ToolStarted { lease, .. },
                EventKind::ToolCompleted {
                    lease: completed, ..
                },
            ) => {
                assert_eq!(lease, &execute.lease);
                assert_eq!(lease, completed);
            }
            _ => return Err("invalid canonical tool ordering".into()),
        }
        assert!(authoritative[4].0 < execute_index);
        assert!(execute_index < nested_request.ok_or("missing nested request")?);
        assert!(
            nested_request.ok_or("missing nested request")?
                < nested_response.ok_or("missing nested response")?
        );
        assert!(nested_response.ok_or("missing nested response")? < result_index);
        assert!(result_index < authoritative[5].0);
        assert!(authoritative[8].0 < replay_request.ok_or("missing replay request")?);
        match (&authoritative[3].1.kind, &authoritative[6].1.kind) {
            (
                EventKind::ModelCallStarted {
                    model_call_id: first,
                    ..
                },
                EventKind::ModelCallStarted {
                    model_call_id: second,
                    ..
                },
            ) => assert_ne!(first, second),
            _ => return Err("missing canonical model calls".into()),
        }
        for ((index, replayed), (_, committed)) in events[9..].iter().zip(&authoritative[4..]) {
            assert!(replay_request.ok_or("missing replay request")? < *index);
            assert_eq!(
                serde_json::to_value(replayed)?,
                serde_json::to_value(committed)?
            );
        }
        Ok(())
    })
    .await?
}
