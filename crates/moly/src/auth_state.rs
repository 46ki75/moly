//! Explicit CLI-owned nonsecret OAuth identity, separate from Server credentials.
use crate::Error;
use moly_client::protocol::ConnectionId;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 64 * 1024;
pub(crate) struct AuthState {
    path: PathBuf,
    pub(crate) host_id: String,
    pub(crate) registration: Option<Value>,
}
impl AuthState {
    pub(crate) fn resolve(create: bool) -> Result<Self, Error> {
        let path = std::env::var_os("MOLY_AUTH_STATE_FILE")
            .filter(|path| !path.is_empty())
            .ok_or(Error::AuthState(
                "MOLY_AUTH_STATE_FILE is required for openai-codex",
            ))?;
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        Self::load(path, create)
    }
    pub(crate) fn load(path: PathBuf, create: bool) -> Result<Self, Error> {
        if cfg!(not(unix)) {
            return Err(Error::AuthState(
                "owner-only auth state requires Unix in this pilot",
            ));
        }
        if !create {
            private_regular(&fs::symlink_metadata(&path)?)?;
        }
        let _lock = lock(&path)?;
        match read(&path) {
            Ok((host_id, registration)) => Ok(Self {
                path,
                host_id,
                registration,
            }),
            Err(Error::Io(error)) if create && error.kind() == std::io::ErrorKind::NotFound => {
                let state = Self {
                    path,
                    host_id: format!("urn:uuid:{}", ConnectionId::new()),
                    registration: None,
                };
                atomic_write(&state.path, &state.value())?;
                Ok(state)
            }
            Err(error) => Err(error),
        }
    }
    fn value(&self) -> Value {
        let mut value = json!({"host_id":self.host_id});
        if let Some(registration) = &self.registration {
            value["registration"] = registration.clone();
        }
        value
    }
    pub(crate) fn persist(&mut self, registration: Value) -> Result<(), Error> {
        validate_registration(&registration)?;
        // Status can recover a missing registration, but cannot silently switch
        // an existing account/profile even if the Server has lost its config copy.
        if self
            .registration
            .as_ref()
            .is_some_and(|saved| saved != &registration)
        {
            return Err(Error::AuthState(
                "conflicting registration; reconcile explicitly",
            ));
        }
        if registration
            .get("host_id")
            .is_some_and(|host| host.as_str() != Some(&self.host_id))
        {
            return Err(invalid());
        }
        let _lock = lock(&self.path)?;
        let (host_id, previous) = read(&self.path)?;
        if host_id != self.host_id
            || (previous != self.registration && previous.as_ref() != Some(&registration))
        {
            return Err(Error::AuthState(
                "local auth state changed; reconcile explicitly",
            ));
        }
        let value = json!({"host_id":host_id, "registration":registration});
        atomic_write(&self.path, &value)?;
        self.registration = Some(registration);
        Ok(())
    }
}

fn invalid() -> Error {
    Error::AuthState("invalid nonsecret auth state")
}
fn read(path: &Path) -> Result<(String, Option<Value>), Error> {
    let metadata = fs::symlink_metadata(path)?;
    private_regular(&metadata)?;
    let file = File::open(path)?;
    same_file(&metadata, &file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let object = value.as_object().ok_or_else(invalid)?;
    if object
        .keys()
        .any(|key| key != "host_id" && key != "registration")
    {
        return Err(invalid());
    }
    let host_id = object
        .get("host_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if !valid_host_id(host_id) {
        return Err(invalid());
    }
    let registration = object.get("registration").cloned();
    if let Some(registration) = &registration {
        validate_registration(registration)?;
        if registration
            .get("host_id")
            .is_some_and(|host| host.as_str() != Some(host_id))
        {
            return Err(invalid());
        }
    }
    Ok((host_id.into(), registration))
}
fn valid_host_id(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("urn:uuid:") else {
        return false;
    };
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, byte)| match i {
            8 | 13 | 18 | 23 => byte == b'-',
            14 => byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'),
        })
}

// Registration remains Provider-defined, but durable Client state must never be
// a credential sink. This is a defensive format constraint, not a secret detector.
pub(crate) fn validate_registration(value: &Value) -> Result<(), Error> {
    fn nonsecret(value: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        match value {
            Value::Object(object) => object.iter().all(|(key, value)| {
                let normalized: String = key
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .flat_map(char::to_lowercase)
                    .collect();
                key.is_ascii()
                    && key.len() <= 128
                    && !["token", "secret", "password", "credential", "privatekey"]
                        .iter()
                        .any(|part| normalized.contains(part))
                    && !matches!(
                        normalized.as_str(),
                        "code" | "authorization" | "apikey" | "cookie"
                    )
                    && nonsecret(value, depth + 1)
            }),
            Value::Array(values) => {
                values.len() <= 64 && values.iter().all(|value| nonsecret(value, depth + 1))
            }
            Value::String(text) => {
                text.len() <= 8192
                    && !text.chars().any(char::is_control)
                    && !text.starts_with("Bearer ")
                    && !text.starts_with("eyJ")
            }
            _ => true,
        }
    }
    if !value.is_object() || !nonsecret(value, 0) || value.to_string().len() > 16 * 1024 {
        return Err(invalid());
    }
    Ok(())
}

fn private_regular(metadata: &fs::Metadata) -> Result<(), Error> {
    if !metadata.is_file() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::AuthState("auth state files must be owner-only"));
        }
    }
    #[cfg(not(unix))]
    return Err(Error::AuthState(
        "owner-only auth state requires Unix in this pilot",
    ));
    #[cfg(unix)]
    Ok(())
}
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> Result<(), Error> {
    private_regular(before)?;
    private_regular(after)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(invalid());
        }
    }
    Ok(())
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn lock(path: &Path) -> Result<File, Error> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let path = PathBuf::from(name);
    let file = match private_options().create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            private_regular(&metadata)?;
            let file = private_options().open(&path)?;
            same_file(&metadata, &file.metadata()?)?;
            file
        }
        Err(error) => return Err(error.into()),
    };
    private_regular(&file.metadata()?)?;
    file.try_lock()
        .map_err(|_| Error::AuthState("could not lock auth state; retry explicitly"))?;
    Ok(file)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn atomic_write(path: &Path, value: &Value) -> Result<(), Error> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        private_regular(&metadata)?;
    }
    let parent = path.parent().ok_or_else(invalid)?;
    let temporary_path = parent.join(format!(".moly-auth-{}", ConnectionId::new()));
    let mut file = private_options().create_new(true).open(&temporary_path)?;
    let temporary = Temporary(temporary_path);
    private_regular(&file.metadata()?)?;
    file.write_all(value.to_string().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary.0, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn state_is_stable_private_atomic_and_only_nonsecret() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("auth.json");
        let mut state = AuthState::load(path.clone(), true)?;
        assert!(valid_host_id(&state.host_id));
        let host = state.host_id.clone();
        state.persist(json!({"client_id":"issued-client", "issuer":"https://example.test", "subject":"account"}))?;
        let restored = AuthState::load(path.clone(), false)?;
        assert_eq!(restored.host_id, host);
        assert_eq!(restored.registration, state.registration);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::read_dir(directory.path())?.count(),
            2,
            "only state and explicit sidecar lock remain"
        );
        for registration in [
            json!({"client_id":"conflicting-account"}),
            json!({"access_token":"must-not-persist"}),
            json!({"nested":{"client_secret":"secret"}}),
            json!({"id_token_hint":"hint"}),
            json!({"opaque":"eyJtoken"}),
            json!({"host_id":"urn:uuid:00000000-0000-4000-8000-000000000001"}),
            json!("not-object"),
        ] {
            assert!(state.persist(registration).is_err());
        }
        assert_eq!(
            AuthState::load(path, false)?.registration,
            restored.registration
        );
        Ok(())
    }
    #[test]
    fn unsafe_or_changed_state_is_not_overwritten() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("auth.json");
        let mut state = AuthState::load(path.clone(), true)?;
        let held = lock(&path)?;
        assert!(
            AuthState::load(path.clone(), false).is_err(),
            "local state contention must not block the UI"
        );
        drop(held);
        let mut concurrent = AuthState::load(path.clone(), false)?;
        concurrent.persist(json!({"client_id":"other"}))?;
        assert!(state.persist(json!({"client_id":"mine"})).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(AuthState::load(path.clone(), false).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let link = directory.path().join("link");
        symlink(&path, &link)?;
        assert!(AuthState::load(link, false).is_err());
        fs::write(
            &path,
            json!({"host_id":state.host_id,"access_token":"secret"}).to_string(),
        )?;
        assert!(AuthState::load(path, false).is_err());
        assert!(!invalid().to_string().contains("secret\""));
        Ok(())
    }
}
