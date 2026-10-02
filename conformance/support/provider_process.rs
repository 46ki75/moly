//! Launch configuration only: the fixture never spawns Providers; the Server does.
use crate::protocol::model::{ComponentCommand, ProviderConfig};
use serde_json::Value;
use std::{env, io, path::PathBuf};

pub(crate) fn provider(options: Value) -> io::Result<ProviderConfig> {
    let path = match env::var_os("MOLY_TEST_PROVIDER_BIN") {
        Some(path) => PathBuf::from(path),
        None => env::current_exe()?
            .parent()
            .and_then(|deps| deps.parent())
            .ok_or_else(|| io::Error::other("test executable has no profile directory"))?
            .join(format!("moly-provider-openai{}", env::consts::EXE_SUFFIX)),
    };
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "moly-provider-openai binary not found at {}; run `cargo build --locked --workspace --bins`, or set MOLY_TEST_PROVIDER_BIN",
                path.display()
            ),
        ));
    }
    let mut environment = std::collections::BTreeMap::new();
    environment.insert("RUST_LOG".into(), "off".into());
    #[cfg(windows)]
    if let Ok(root) = env::var("SystemRoot") {
        environment.insert("SystemRoot".into(), root);
    }
    Ok(ProviderConfig {
        command: ComponentCommand {
            executable: path.to_string_lossy().into_owned(),
            args: vec![],
            env: environment,
        },
        options,
    })
}
