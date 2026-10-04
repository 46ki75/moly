//! Bundled, read-only workspace file capability. No commands are executed.
//!
//! Canonical path checks reject ordinary traversal and symlink escapes, but are
//! **not an OS sandbox**: canonicalization, metadata checks, and opening are
//! separate operations. A concurrently modified filesystem can exploit this
//! TOCTOU window, including replacing a checked file with a symlink or special
//! file. Hard links and mounts also do not establish data provenance. Use only
//! with a trusted workspace, or put the executor in an external OS sandbox.
//! Model text is never interpreted as a command or granted process execution.

use std::{io, path::Path};

use moly_protocol::{ExecutorId, ProtocolError, ToolDefinition, ToolExecute, ToolResult};
use serde_json::json;
use tokio::io::AsyncReadExt;

const MAX_FILE_BYTES: u64 = 64 * 1024;
const GENERATION: u64 = 1;

// A known execution outcome is model context, not an RPC/authority failure.
// Static descriptions deliberately discard paths and native I/O diagnostics.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum ReadFailure {
    #[error("read_file requires exactly one nonempty string path")]
    InvalidArguments,
    #[error("File path is outside the workspace")]
    OutsideWorkspace,
    #[error("Path does not identify a regular file")]
    NotRegularFile,
    #[error("File is not valid UTF-8 text")]
    InvalidUtf8,
    #[error("File exceeds 64 KiB")]
    TooLarge,
    #[error("Requested file was not found")]
    NotFound,
    #[error("Requested file is not accessible")]
    PermissionDenied,
    #[error("Workspace file could not be read")]
    Io,
}

impl ReadFailure {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_params",
            Self::OutsideWorkspace => "tool_path_outside_workspace",
            Self::NotRegularFile => "tool_invalid_path",
            Self::InvalidUtf8 => "tool_invalid_utf8",
            Self::TooLarge => "tool_result_too_large",
            Self::NotFound => "tool_file_not_found",
            Self::PermissionDenied => "tool_permission_denied",
            Self::Io => "tool_error",
        }
    }

    fn output(self) -> serde_json::Value {
        json!({"error": {"code": self.code(), "message": self.to_string()}})
    }
}

impl From<io::Error> for ReadFailure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Io,
        }
    }
}

/// An independent logical executor for the `read_file` capability.
///
/// Lease checks fence other executors and generations, but are not cryptographic
/// authorization or replay protection. The server remains the lease authority.
/// Filesystem confinement is best-effort; see this module's sandbox limitations.
pub struct LocalExecutor {
    id: ExecutorId,
}

impl LocalExecutor {
    /// Allocate an executor identity independent of any OS process or workspace.
    pub fn new() -> Self {
        Self {
            id: ExecutorId::new(),
        }
    }

    /// Return this instance's stable logical identity.
    pub fn id(&self) -> ExecutorId {
        self.id
    }

    /// Describe the only bundled capability: read a UTF-8 workspace file.
    ///
    /// The argument object contains exactly `path: string`; files are limited to
    /// 64 KiB. Relative paths are resolved under the supplied workspace root.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description:
                "Read a UTF-8 file within the workspace (maximum 64 KiB). No command execution."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false,
            }),
        }
    }

    /// Execute a generation-1 lease addressed to this executor.
    ///
    /// Returns `{"content": string}` or `{"error": {"code": string, "message": string}}`
    /// with the exact request lease. Only authority/configuration failures are RPC
    /// errors. Failure descriptions never expose paths, contents, arguments, or
    /// underlying filesystem diagnostics.
    /// Both metadata and the bytes actually read are checked against 64 KiB.
    /// Canonical path containment does not eliminate the documented TOCTOU race.
    #[tracing::instrument(
        name = "local_tool",
        skip_all,
        fields(
            session_id = %request.session_id,
            executor_id = %self.id,
            tool_run_id = %request.lease.tool_run_id,
            generation = request.lease.generation,
        )
    )]
    pub async fn execute(
        &self,
        workspace: &str,
        request: ToolExecute,
    ) -> Result<ToolResult, ProtocolError> {
        if request.lease.executor_id != self.id || request.lease.generation != GENERATION {
            return Err(ProtocolError::new(
                "stale_lease",
                "Tool lease does not match this executor and generation",
            ));
        }
        if request.name != "read_file" {
            return Err(ProtocolError::new(
                "tool_not_found",
                "This executor supports only read_file",
            ));
        }
        let workspace = Path::new(workspace);
        if !workspace.is_absolute() {
            return Err(invalid_workspace());
        }
        let workspace = tokio::fs::canonicalize(workspace)
            .await
            .map_err(|_| invalid_workspace())?;
        let root_metadata = tokio::fs::metadata(&workspace)
            .await
            .map_err(|_| invalid_workspace())?;
        if !root_metadata.is_dir() {
            return Err(invalid_workspace());
        }
        let output = match read_file(&workspace, &request.arguments).await {
            Ok(content) => json!({"content": content}),
            Err(failure) => failure.output(),
        };
        Ok(ToolResult {
            lease: request.lease,
            output,
        })
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

async fn read_file(workspace: &Path, arguments: &serde_json::Value) -> Result<String, ReadFailure> {
    let arguments = arguments.as_object().ok_or(ReadFailure::InvalidArguments)?;
    let path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty() && !path.contains('\0'))
        .filter(|_| arguments.len() == 1)
        .ok_or(ReadFailure::InvalidArguments)?;
    // Path::join preserves absolute paths, and starts_with compares whole
    // canonical components, not a vulnerable textual root-prefix match.
    let path = tokio::fs::canonicalize(workspace.join(path)).await?;
    if !path.starts_with(workspace) {
        return Err(ReadFailure::OutsideWorkspace);
    }
    // Check before open to reject ordinary FIFOs/devices without blocking.
    // Recheck the opened handle as well, without claiming race-free opening.
    check_file(&tokio::fs::metadata(&path).await?)?;
    let file = tokio::fs::File::open(&path).await?;
    check_file(&file.metadata().await?)?;
    let content = read_bounded(file).await?;
    String::from_utf8(content).map_err(|_| ReadFailure::InvalidUtf8)
}

fn check_file(metadata: &std::fs::Metadata) -> Result<(), ReadFailure> {
    if !metadata.is_file() {
        return Err(ReadFailure::NotRegularFile);
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ReadFailure::TooLarge);
    }
    Ok(())
}

async fn read_bounded(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, ReadFailure> {
    let mut content = Vec::new();
    // The extra byte distinguishes an exact-limit file from a file that grew
    // after metadata validation, without an unbounded read or allocation.
    reader
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .await?;
    if content.len() as u64 > MAX_FILE_BYTES {
        return Err(ReadFailure::TooLarge);
    }
    Ok(content)
}

fn invalid_workspace() -> ProtocolError {
    ProtocolError::new(
        "invalid_config",
        "Expected an accessible absolute workspace directory",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use moly_protocol::{SessionId, ToolLease, ToolRunId};
    use serde_json::Value;
    use std::error::Error;

    type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

    fn request(executor: &LocalExecutor, arguments: Value) -> ToolExecute {
        ToolExecute {
            session_id: SessionId::new(),
            lease: ToolLease {
                tool_run_id: ToolRunId::new(),
                executor_id: executor.id(),
                generation: GENERATION,
            },
            name: "read_file".into(),
            arguments,
        }
    }

    fn workspace(directory: &tempfile::TempDir) -> Result<&str, Box<dyn Error + Send + Sync>> {
        directory
            .path()
            .to_str()
            .ok_or_else(|| "non-UTF-8 test workspace".into())
    }

    async fn read_failure(
        executor: &LocalExecutor,
        workspace: &str,
        arguments: Value,
    ) -> Result<ProtocolError, Box<dyn Error + Send + Sync>> {
        let request = request(executor, arguments);
        let lease = request.lease.clone();
        let result = executor.execute(workspace, request).await?;
        assert_eq!(result.lease, lease);
        assert_eq!(
            result.output.as_object().map(|output| output.len()),
            Some(1)
        );
        Ok(serde_json::from_value(result.output["error"].clone())?)
    }

    #[test]
    fn io_failure_categories_discard_native_diagnostics() {
        for (kind, code, message) in [
            (
                io::ErrorKind::NotFound,
                "tool_file_not_found",
                "Requested file was not found",
            ),
            (
                io::ErrorKind::PermissionDenied,
                "tool_permission_denied",
                "Requested file is not accessible",
            ),
            (
                io::ErrorKind::Other,
                "tool_error",
                "Workspace file could not be read",
            ),
        ] {
            let failure =
                ReadFailure::from(io::Error::new(kind, "private-path-and-native-diagnostic"));
            assert_eq!(
                failure.output(),
                json!({"error":{"code":code, "message":message}})
            );
        }
    }

    #[test]
    fn identities_are_independent_and_definition_is_read_only() {
        let first = LocalExecutor::new();
        let second = LocalExecutor::default();
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id(), first.id());
        let definition = LocalExecutor::definition();
        assert_eq!(definition.name, "read_file");
        assert_eq!(
            definition.input_schema["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(definition.input_schema["required"], json!(["path"]));
        assert_eq!(definition.input_schema["additionalProperties"], false);
    }

    #[tokio::test]
    async fn relative_and_absolute_paths_return_content_and_exact_lease() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.txt");
        let content = "hello\nUnicode: 🦀\n$(touch should-not-exist)";
        tokio::fs::write(&path, content).await?;
        let executor = LocalExecutor::new();
        for path in ["notes.txt".to_owned(), path.to_string_lossy().into_owned()] {
            let request = request(&executor, json!({"path": path}));
            let lease = request.lease.clone();
            let result = executor.execute(workspace(&directory)?, request).await?;
            assert_eq!(result.lease, lease);
            assert_eq!(result.output, json!({"content": content}));
        }
        assert!(!directory.path().join("should-not-exist").exists());
        Ok(())
    }

    #[tokio::test]
    async fn mismatched_executor_and_generation_are_rejected_before_io() -> TestResult {
        let executor = LocalExecutor::new();
        let other = LocalExecutor::new();
        let wrong_executor = request(&other, json!({"path": "private"}));
        assert_eq!(
            executor
                .execute("not-a-workspace", wrong_executor)
                .await
                .err()
                .ok_or("expected lease rejection")?
                .code,
            "stale_lease"
        );
        for generation in [0, 2, u64::MAX] {
            let mut request = request(&executor, json!({"path": "private"}));
            request.lease.generation = generation;
            assert_eq!(
                executor
                    .execute("not-a-workspace", request)
                    .await
                    .err()
                    .ok_or("expected generation rejection")?
                    .code,
                "stale_lease"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_arguments_and_shell_capabilities_are_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executor = LocalExecutor::new();
        for arguments in [
            json!(null),
            json!("notes.txt"),
            json!({}),
            json!({"path": 1}),
            json!({"path": ""}),
            json!({"path": "a\u{0000}b"}),
            json!({"path": "notes.txt", "command": "touch should-not-exist"}),
        ] {
            let error = read_failure(&executor, workspace(&directory)?, arguments).await?;
            assert_eq!(error.code, "invalid_params");
        }
        let mut shell = request(&executor, json!({"command": "touch should-not-exist"}));
        shell.name = "shell".into();
        let error = executor
            .execute(workspace(&directory)?, shell)
            .await
            .err()
            .ok_or("expected unsupported tool")?;
        assert_eq!(error.code, "tool_not_found");
        assert!(!directory.path().join("should-not-exist").exists());
        Ok(())
    }

    #[tokio::test]
    async fn traversal_absolute_escape_and_root_prefix_neighbor_are_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("workspace");
        let neighbor = directory.path().join("workspace-neighbor");
        tokio::fs::create_dir(&root).await?;
        tokio::fs::create_dir(&neighbor).await?;
        tokio::fs::write(directory.path().join("private.txt"), "private").await?;
        tokio::fs::write(neighbor.join("private.txt"), "private").await?;
        let executor = LocalExecutor::new();
        for path in [
            "../private.txt".to_owned(),
            directory
                .path()
                .join("private.txt")
                .to_string_lossy()
                .into_owned(),
            neighbor.join("private.txt").to_string_lossy().into_owned(),
        ] {
            let error = read_failure(
                &executor,
                root.to_str().ok_or("non-UTF-8 root")?,
                json!({"path": path}),
            )
            .await?;
            assert_eq!(error.code, "tool_path_outside_workspace");
            assert!(!error.message.contains("private.txt"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn file_failures_return_error_results_but_bad_workspaces_remain_fatal() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executor = LocalExecutor::new();
        tokio::fs::write(directory.path().join("binary"), [0xff, 0xfe]).await?;
        for (path, code) in [
            (".", "tool_invalid_path"),
            ("missing-private-file", "tool_file_not_found"),
            ("binary", "tool_invalid_utf8"),
        ] {
            let error =
                read_failure(&executor, workspace(&directory)?, json!({"path": path})).await?;
            assert_eq!(error.code, code);
            assert!(!error.message.contains("missing-private-file"));
        }
        for root in [
            "relative-root".to_owned(),
            directory
                .path()
                .join("binary")
                .to_string_lossy()
                .into_owned(),
            directory
                .path()
                .join("missing-root")
                .to_string_lossy()
                .into_owned(),
        ] {
            let error = executor
                .execute(&root, request(&executor, json!({"path": "."})))
                .await
                .err()
                .ok_or("expected workspace rejection")?;
            assert_eq!(error.code, "invalid_config");
        }
        Ok(())
    }

    #[tokio::test]
    async fn file_limit_is_inclusive_and_reads_remain_bounded_without_metadata() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executor = LocalExecutor::new();
        for length in [0, MAX_FILE_BYTES, MAX_FILE_BYTES + 1] {
            let bytes = vec![b'x'; length as usize];
            tokio::fs::write(directory.path().join("sized"), &bytes).await?;
            let result = executor
                .execute(
                    workspace(&directory)?,
                    request(&executor, json!({"path": "sized"})),
                )
                .await;
            if length <= MAX_FILE_BYTES {
                assert_eq!(
                    result?.output["content"]
                        .as_str()
                        .ok_or("missing content")?
                        .len(),
                    length as usize
                );
            } else {
                let result = result?;
                assert_eq!(
                    result.output,
                    json!({"error":{
                        "code":"tool_result_too_large", "message":"File exceeds 64 KiB",
                    }})
                );
            }
        }
        // An over-limit reader models growth after the initial metadata check.
        let bytes = vec![b'x'; (MAX_FILE_BYTES + 1024) as usize];
        let mut reader = bytes.as_slice();
        let error = read_bounded(&mut reader)
            .await
            .err()
            .ok_or("expected bounded reader rejection")?;
        assert_eq!(error.code(), "tool_result_too_large");
        assert_eq!(bytes.len() - reader.len(), (MAX_FILE_BYTES + 1) as usize);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escapes_fail_but_in_root_links_and_workspace_aliases_work() -> TestResult {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = directory.path().join("workspace");
        tokio::fs::create_dir(&root).await?;
        tokio::fs::write(root.join("inside"), "inside").await?;
        tokio::fs::write(directory.path().join("outside"), "outside").await?;
        symlink(directory.path().join("outside"), root.join("escape"))?;
        symlink(root.join("inside"), root.join("link"))?;
        symlink(&root, directory.path().join("workspace-alias"))?;
        let executor = LocalExecutor::new();
        let error = read_failure(
            &executor,
            root.to_str().ok_or("non-UTF-8 root")?,
            json!({"path": "escape"}),
        )
        .await?;
        assert_eq!(error.code, "tool_path_outside_workspace");
        let alias = directory.path().join("workspace-alias");
        let result = executor
            .execute(
                alias.to_str().ok_or("non-UTF-8 alias")?,
                request(&executor, json!({"path": "link"})),
            )
            .await?;
        assert_eq!(result.output, json!({"content": "inside"}));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn special_files_are_rejected_without_opening() -> TestResult {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("socket");
        let _listener = tokio::net::UnixListener::bind(&socket)?;
        let executor = LocalExecutor::new();
        let error =
            read_failure(&executor, workspace(&directory)?, json!({"path": "socket"})).await?;
        assert_eq!(error.code, "tool_invalid_path");
        Ok(())
    }
}
