//! Credentials for authenticated remotes.
//!
//! The issued values are NOT cryptographic identities: the secret is
//! derived from a counter, a clock read, and blake3, with no OS entropy
//! source. That keeps this crate dependency-free. The `CredStore` trait is
//! the seam where a real keypair lands; remotes trust any locally issued
//! credential (in-process trust model, documented in RUBRIC_O5 §4/§7).

use std::sync::{Mutex, OnceLock};

use crate::model::{VersionError, VersionResult};

#[derive(Clone)]
pub struct Credential {
    pub kid: String,
    pub secret: [u8; 32],
}

pub trait CredStore {
    fn issue(&mut self) -> Credential;
    fn list(&self) -> Vec<String>;
    fn active(&self) -> Option<String>;
    fn set_active(&mut self, kid: &str) -> VersionResult<()>;
    fn remove(&mut self, kid: &str) -> VersionResult<()>;
}

/// Process-wide credential store, shared with the mem transport's auth
/// check (same lifetime model as the mem remote hub).
pub struct MemCredStore {
    creds: Vec<Credential>,
    active: Option<String>,
    counter: u64,
    now_nanos: u64,
}

impl MemCredStore {
    pub fn new() -> Self {
        MemCredStore {
            creds: Vec::new(),
            active: None,
            counter: 0,
            now_nanos: 0,
        }
    }

    /// Clock override so tests get deterministic kids.
    pub fn set_now(&mut self, now_nanos: u64) {
        self.now_nanos = now_nanos;
    }

    fn issue_at(&mut self, now_nanos: u64) -> Credential {
        self.counter += 1;
        let mut seed = Vec::with_capacity(48);
        seed.extend_from_slice(&self.counter.to_le_bytes());
        seed.extend_from_slice(&now_nanos.to_le_bytes());
        seed.extend_from_slice(&self.counter.to_be_bytes());
        let secret = *blake3::hash(&seed).as_bytes();
        let kid = hex::encode(blake3::hash(&secret).as_bytes());
        Credential { kid, secret }
    }
}

impl Default for MemCredStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredStore for MemCredStore {
    fn issue(&mut self) -> Credential {
        let now = self.now_nanos;
        let credential = self.issue_at(now);
        self.active = Some(credential.kid.clone());
        self.creds.push(credential.clone());
        credential
    }

    fn list(&self) -> Vec<String> {
        self.active
            .iter()
            .cloned()
            .chain(
                self.creds
                    .iter()
                    .filter(|credential| Some(credential.kid.as_str()) != self.active.as_deref())
                    .map(|credential| credential.kid.clone()),
            )
            .collect()
    }

    fn active(&self) -> Option<String> {
        self.active.clone()
    }

    fn set_active(&mut self, kid: &str) -> VersionResult<()> {
        if self.creds.iter().any(|c| c.kid == kid) {
            self.active = Some(kid.to_string());
            Ok(())
        } else {
            Err(VersionError::NoSuchCredential)
        }
    }

    fn remove(&mut self, kid: &str) -> VersionResult<()> {
        let before = self.creds.len();
        self.creds.retain(|c| c.kid != kid);
        if self.creds.len() == before {
            return Err(VersionError::NoSuchCredential);
        }
        if self.active.as_deref() == Some(kid) {
            self.active = None;
        }
        Ok(())
    }
}

fn global_store() -> &'static Mutex<MemCredStore> {
    static STORE: OnceLock<Mutex<MemCredStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(MemCredStore::new()))
}

/// Issue a credential into the process-wide store and make it active.
pub fn issue_global() -> Credential {
    with_global(|store| {
        store.set_now(clock_nanos());
        store.issue()
    })
}

/// The process-wide active credential id, if any (used by mem auth).
pub fn active_kid() -> Option<String> {
    global_store().lock().unwrap().active()
}

/// Run a closure against the process-wide store (specs and tests).
pub fn with_global<R>(f: impl FnOnce(&mut MemCredStore) -> R) -> R {
    let mut store = global_store().lock().unwrap();
    f(&mut store)
}

fn clock_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creds_issue_lists_and_activates() {
        let mut store = MemCredStore::new();
        store.set_now(1_000);
        let a = store.issue();
        store.set_now(2_000);
        let b = store.issue();
        assert_ne!(a.kid, b.kid);
        // Issuing activates the newest credential.
        assert_eq!(store.active().as_deref(), Some(b.kid.as_str()));
        assert_eq!(store.list(), vec![b.kid, a.kid]);
    }

    #[test]
    fn creds_deterministic_under_same_clock() {
        let mut x = MemCredStore::new();
        let mut y = MemCredStore::new();
        x.set_now(777);
        y.set_now(777);
        assert_eq!(x.issue().kid, y.issue().kid);
    }

    #[test]
    fn creds_set_active_requires_known_kid() {
        let mut store = MemCredStore::new();
        let a = store.issue();
        store.issue();
        store.set_active(&a.kid).unwrap();
        assert_eq!(store.active().as_deref(), Some(a.kid.as_str()));
        assert_eq!(
            store.set_active("nope").unwrap_err(),
            VersionError::NoSuchCredential
        );
    }

    #[test]
    fn creds_remove_drops_and_deactivates() {
        let mut store = MemCredStore::new();
        let a = store.issue();
        store.remove(&a.kid).unwrap();
        assert!(store.list().is_empty());
        assert_eq!(store.active(), None);
        assert_eq!(
            store.remove(&a.kid).unwrap_err(),
            VersionError::NoSuchCredential
        );
    }
}
