//! Bundled, read-only workspace file capability. No commands are executed.
//!
//! Canonical path checks reject ordinary traversal and symlink escapes, but are
//! **not an OS sandbox**: canonicalization, metadata checks, and opening are
//! separate operations. A concurrently modified filesystem can exploit this
//! TOCTOU window, including replacing a checked file with a symlink or special
//! file. Hard links and mounts also do not establish data provenance. Use only
//! with a trusted workspace, or put the executor in an external OS sandbox.
//! Model text is never interpreted as a command or granted process execution.

use std::path::Path;

use moly_protocol::{ExecutorId, ProtocolError, ToolDefinition, ToolExecute, ToolResult};
use serde_json::json;
use tokio::io::AsyncReadExt;

const MAX_FILE_BYTES: u64 = 64 * 1024;
const GENERATION: u64 = 1;

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
    /// Returns `{"content": string}` and the exact request lease. Errors never
    /// expose paths, contents, arguments, or underlying filesystem diagnostics.
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
        let arguments = request
            .arguments
            .as_object()
            .ok_or_else(invalid_arguments)?;
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty() && !path.contains('\0'))
            .filter(|_| arguments.len() == 1)
            .ok_or_else(invalid_arguments)?;
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
        // Path::join preserves absolute paths, and starts_with compares whole
        // canonical components, not a vulnerable textual root-prefix match.
        let path = tokio::fs::canonicalize(workspace.join(path))
            .await
            .map_err(|_| file_error())?;
        if !path.starts_with(&workspace) {
            return Err(ProtocolError::new(
                "tool_path_outside_workspace",
                "File path is outside the workspace",
            ));
        }
        // Check before open to reject ordinary FIFOs/devices without blocking.
        // Recheck the opened handle as well, without claiming race-free opening.
        check_file(&tokio::fs::metadata(&path).await.map_err(|_| file_error())?)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|_| file_error())?;
        check_file(&file.metadata().await.map_err(|_| file_error())?)?;
        let content = read_bounded(file).await?;
        let content = String::from_utf8(content)
            .map_err(|_| ProtocolError::new("tool_invalid_utf8", "File is not valid UTF-8 text"))?;
        Ok(ToolResult {
            lease: request.lease,
            output: json!({"content": content}),
        })
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

fn check_file(metadata: &std::fs::Metadata) -> Result<(), ProtocolError> {
    if !metadata.is_file() {
        return Err(ProtocolError::new(
            "tool_invalid_path",
            "Path does not identify a regular file",
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(file_too_large());
    }
    Ok(())
}

async fn read_bounded(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, ProtocolError> {
    let mut content = Vec::new();
    // The extra byte distinguishes an exact-limit file from a file that grew
    // after metadata validation, without an unbounded read or allocation.
    reader
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .await
        .map_err(|_| file_error())?;
    if content.len() as u64 > MAX_FILE_BYTES {
        return Err(file_too_large());
    }
    Ok(content)
}

fn invalid_arguments() -> ProtocolError {
    ProtocolError::new(
        "invalid_params",
        "read_file requires exactly one nonempty string path",
    )
}

fn invalid_workspace() -> ProtocolError {
    ProtocolError::new(
        "invalid_config",
        "Expected an accessible absolute workspace directory",
    )
}

fn file_error() -> ProtocolError {
    ProtocolError::new("tool_error", "Workspace file could not be read")
}

fn file_too_large() -> ProtocolError {
    ProtocolError::new("tool_result_too_large", "File exceeds 64 KiB")
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
            let error = executor
                .execute(workspace(&directory)?, request(&executor, arguments))
                .await
                .err()
                .ok_or("expected argument rejection")?;
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
            let error = executor
                .execute(
                    root.to_str().ok_or("non-UTF-8 root")?,
                    request(&executor, json!({"path": path})),
                )
                .await
                .err()
                .ok_or("expected escape rejection")?;
            assert_eq!(error.code, "tool_path_outside_workspace");
            assert!(!error.message.contains("private.txt"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn nonfiles_bad_workspaces_missing_files_and_binary_are_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executor = LocalExecutor::new();
        tokio::fs::write(directory.path().join("binary"), [0xff, 0xfe]).await?;
        for (path, code) in [
            (".", "tool_invalid_path"),
            ("missing-private-file", "tool_error"),
            ("binary", "tool_invalid_utf8"),
        ] {
            let error = executor
                .execute(
                    workspace(&directory)?,
                    request(&executor, json!({"path": path})),
                )
                .await
                .err()
                .ok_or("expected file rejection")?;
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
                assert_eq!(
                    result.err().ok_or("expected oversized rejection")?.code,
                    "tool_result_too_large"
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
        assert_eq!(error.code, "tool_result_too_large");
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
        let error = executor
            .execute(
                root.to_str().ok_or("non-UTF-8 root")?,
                request(&executor, json!({"path": "escape"})),
            )
            .await
            .err()
            .ok_or("expected symlink rejection")?;
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
        let error = executor
            .execute(
                workspace(&directory)?,
                request(&executor, json!({"path": "socket"})),
            )
            .await
            .err()
            .ok_or("expected nonfile rejection")?;
        assert_eq!(error.code, "tool_invalid_path");
        Ok(())
    }
}
