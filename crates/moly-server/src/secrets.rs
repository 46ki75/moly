//! Generic host-owned, memory-only credential slots. No credential interpretation.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Slot = Arc<AsyncMutex<Option<String>>>;

/// Shared opaque values, never discovered from files, environment, or Debug.
#[derive(Clone, Default)]
pub struct SecretStore {
    values: Arc<Mutex<HashMap<String, Slot>>>,
}
impl SecretStore {
    /// Lease exactly one reference for an entire Provider operation.
    ///
    /// Serializing even inference is deliberate in this pilot: a newly spawned
    /// process must never receive a stale rotating token. Dropping the operation
    /// releases the lease, but preserves credential replacements already committed.
    pub async fn acquire(&self, key: &str) -> OwnedMutexGuard<Option<String>> {
        let slot = self
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.to_owned())
            .or_default()
            .clone();
        slot.lock_owned().await
    }
    /// Replace a Client-supplied value without racing a Provider credential update.
    pub async fn insert(&self, key: String, value: String) {
        *self.acquire(&key).await = Some(value);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn clones_serialize_updates_and_retain_commits_after_cancellation() {
        let secrets = SecretStore::default();
        let shared = secrets.clone();
        assert!(shared.acquire("key").await.is_none());
        secrets.insert("key".into(), "first".into()).await;
        let mut lease = secrets.acquire("key").await;
        *lease = Some("rotated".into());
        let replace = shared.insert("key".into(), "second".into());
        tokio::pin!(replace);
        // Check Pending directly: no scheduling/timing assumption is needed.
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(replace.as_mut(), &mut context).is_pending());
        drop(lease);
        replace.await;
        assert_eq!(shared.acquire("key").await.as_deref(), Some("second"));
        assert!(shared.acquire("unselected").await.is_none());
    }
}
