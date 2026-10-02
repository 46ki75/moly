//! Generic host-owned secrets; only the selected reference crosses the Provider channel.
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// Shared memory-only values, never discovered from files, environment, or Debug.
#[derive(Clone, Default)]
pub struct SecretStore {
    values: Arc<RwLock<HashMap<String, String>>>,
}
impl SecretStore {
    /// Insert or replace an opaque Client-supplied value.
    pub fn insert(&self, key: String, value: String) {
        // Poisoning does not change the map's invariants; never include secrets
        // in panic diagnostics when recovering another task's poisoned lock.
        self.values
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
    }
    /// Copy exactly the referenced value; this is not a zeroizing credential vault.
    pub fn get(&self, key: &str) -> Option<String> {
        self.values
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_clones_share_insert_and_replace() {
        let secrets = SecretStore::default();
        let shared = secrets.clone();
        assert_eq!(shared.get("key"), None);
        secrets.insert("key".into(), "first".into());
        assert_eq!(shared.get("key").as_deref(), Some("first"));
        shared.insert("key".into(), "second".into());
        assert_eq!(secrets.get("key").as_deref(), Some("second"));
    }
}
