//! Test-owned launch policy and audit helpers; no executable implementation imports.
use moly_provider_client::protocol::{
    AuthAttemptId, ModelCallId, ProtocolError, RunId, SessionId, ToolDefinition,
    auth::{AuthOperation, ProviderAuthRequest},
    model::{
        CallKind, ComponentCommand, InferenceContext, ModelMessage, ModelRequest, ProviderConfig,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
};

pub(crate) type TestError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type TestResult = Result<(), TestError>;
pub(crate) const OLD: &str = r#"{"token":"auth-secret-original-sentinel","generation":0}"#;
pub(crate) const LOGIN: &str = r#"{"token":"auth-secret-login-sentinel","generation":1}"#;
pub(crate) const OTHER: &str = "unselected-credential-secret-sentinel";
pub(crate) const STALE: &str = "stale-request-credential-secret-sentinel";
pub(crate) const USER: &str = "provider-user-content-sentinel: α\nsecond line";
pub(crate) const URL: &str = "https://auth.example.invalid/sign-in?state=auth-url-private-sentinel";

pub(crate) async fn bounded(future: impl Future<Output = TestResult>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(25), future).await?
}

pub(crate) struct Inputs {
    _directory: TempDir,
    python: String,
    environment: BTreeMap<String, String>,
    audit: PathBuf,
}

impl Inputs {
    pub(crate) async fn new() -> Result<Self, TestError> {
        let directory = tempfile::Builder::new().prefix("moly-mpp-").tempdir()?;
        let interpreter = if cfg!(windows) { "python" } else { "python3" };
        // Interpreter discovery belongs to this test host. Only the resolved
        // absolute path below is passed to the SDK's Provider launch operation.
        let output = Command::new(interpreter)
            .args([
                "-I", "-c",
                "import json, os, sys; print(json.dumps({'executable':sys.executable,'cf_encoding':os.environ.get('__CF_USER_TEXT_ENCODING')}))",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| std::io::Error::other(format!(
                "Python 3 is required for MPP conformance; install {interpreter} on PATH: {error}"
            )))?;
        assert!(
            output.status.success(),
            "Python executable resolution failed"
        );
        let resolution: Value = serde_json::from_slice(&output.stdout)?;
        let python = resolution["executable"]
            .as_str()
            .ok_or("missing sys.executable")?
            .to_owned();
        assert!(Path::new(&python).is_absolute() && Path::new(&python).is_file());
        let mut environment = BTreeMap::from([
            ("EXPLICIT_COMPONENT_MARKER".into(), "explicit-only".into()),
            ("PYTHONUTF8".into(), "1".into()),
            ("PYTHONCOERCECLOCALE".into(), "0".into()),
        ]);
        environment.insert("LC_CTYPE".into(), "C".into());
        #[cfg(windows)]
        if let Ok(root) = std::env::var("SystemRoot") {
            environment.insert("SYSTEMROOT".into(), root);
        }
        // macOS Python may inject this value during startup even with env_clear.
        // Supply the measured value, rather than exempt unknown audit entries.
        #[cfg(target_os = "macos")]
        if let Some(encoding) = resolution["cf_encoding"].as_str() {
            environment.insert("__CF_USER_TEXT_ENCODING".into(), encoding.to_owned());
        }
        let audit = directory.path().join("synthetic-provider-audit.jsonl");
        Ok(Self {
            _directory: directory,
            python,
            environment,
            audit,
        })
    }

    fn config(
        &self,
        fixture: &str,
        args: Vec<String>,
        options: Value,
    ) -> Result<ProviderConfig, TestError> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance")
            .join(fixture)
            .canonicalize()?;
        let mut command_args = vec!["-S".into(), fixture.to_string_lossy().into_owned()];
        command_args.extend(args);
        Ok(ProviderConfig {
            command: ComponentCommand {
                executable: self.python.clone(),
                args: command_args,
                env: self.environment.clone(),
            },
            options,
        })
    }

    pub(crate) fn model(&self, scenario: &str) -> Result<ProviderConfig, TestError> {
        self.config(
            "model-provider/provider.py",
            vec![scenario.into(), self.audit.to_string_lossy().into_owned()],
            json!([null, {"opaque":[true, 7]}, "implementation-defined"]),
        )
    }

    pub(crate) fn auth(
        &self,
        scenario: &str,
        gate: Option<u16>,
    ) -> Result<ProviderConfig, TestError> {
        let mut options = json!({"scenario":scenario});
        if let Some(port) = gate {
            options["gate_port"] = json!(port);
        }
        self.config(
            "auth/provider.py",
            vec![self.audit.to_string_lossy().into_owned()],
            options,
        )
    }

    pub(crate) fn mpp(
        &self,
        scenario: &str,
        gate: Option<u16>,
    ) -> Result<ProviderConfig, TestError> {
        let mut args = vec![scenario.into(), self.audit.to_string_lossy().into_owned()];
        if let Some(port) = gate {
            args.push(port.to_string());
        }
        self.config("mpp/provider.py", args, json!({"stage":0}))
    }

    pub(crate) fn records(&self) -> Result<Vec<Value>, TestError> {
        if !self.audit.exists() {
            return Ok(vec![]);
        }
        fs::read_to_string(&self.audit)?
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }
}

pub(crate) fn request() -> ModelRequest {
    ModelRequest {
        options: json!({"stale":true}),
        credential: Some(STALE.into()),
        context: InferenceContext {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            model_call_id: ModelCallId::new(),
            call_kind: CallKind::Primary,
        },
        messages: vec![ModelMessage::User { text: USER.into() }],
        tools: vec![ToolDefinition {
            name: "component_echo".into(),
            description: "Test host capability".into(),
            input_schema: json!({"type":"object"}),
        }],
    }
}

pub(crate) fn auth_request(operation: AuthOperation) -> ProviderAuthRequest {
    ProviderAuthRequest {
        attempt_id: AuthAttemptId::new(),
        operation,
        options: json!({"scenario":"stale-options-must-not-be-used"}),
        credential: Some(STALE.into()),
    }
}

pub(crate) fn model_requests<'a>(records: &'a [Value], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["request"]["method"] == method)
        .collect()
}

pub(crate) fn auth_messages<'a>(records: &'a [Value], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["message"]["method"] == method)
        .collect()
}

pub(crate) fn redacted(text: &str) {
    for sentinel in [
        "provider-message-secret-sentinel",
        "provider-code-secret-sentinel",
        "provider-stderr-secret-sentinel",
        "auth-secret-",
        "auth-url-private-sentinel",
        "mpp-wire-secret-sentinel",
        OTHER,
        STALE,
        USER,
    ] {
        assert!(
            !text.contains(sentinel),
            "sensitive Provider data escaped: {sentinel}"
        );
    }
}

pub(crate) fn error_code(error: ProtocolError, expected: &str) {
    redacted(&error.to_string());
    redacted(&format!("{error:?}"));
    redacted(&serde_json::to_string(&error).expect("ProtocolError is serializable"));
    assert_eq!(error.code, expected);
}

pub(crate) async fn gate(
    listener: &TcpListener,
) -> Result<(BufReader<TcpStream>, Value), TestError> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    assert_ne!(
        stream.read_line(&mut line).await?,
        0,
        "missing child gate record"
    );
    Ok((stream, serde_json::from_str(&line)?))
}

pub(crate) async fn child_alive(stream: &mut BufReader<TcpStream>) -> TestResult {
    stream.get_mut().write_all(b"?").await?;
    let mut reply = [0];
    stream.read_exact(&mut reply).await?;
    assert_eq!(reply, *b"!", "child must still be gated");
    Ok(())
}

pub(crate) async fn child_stopped(stream: &mut BufReader<TcpStream>) -> TestResult {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut [0])).await??,
        0,
        "Provider-owned socket must close when the child is cleaned up"
    );
    Ok(())
}
