//! `InMemoryAstIndex` — `Mutex<HashMap>`-backed [`AstIndexPort`] test double.
//!
//! No I/O, no dependencies outside `std`. Suitable for unit tests and for
//! running the port contract suite without a real filesystem.
//!
//! # Interior mutability
//!
//! [`AstIndexPort`] requires `&self` methods so implementations can live behind
//! an `Arc`. This fake achieves that through `Mutex<HashMap<String, Vec<u8>>>`.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ports::ast_index::validate_key;
use crate::ports::{AstIndexPort, PortError};

/// A purely in-memory implementation of [`AstIndexPort`].
///
/// Backed by a `Mutex<HashMap<String, Vec<u8>>>`. All operations are O(1)
/// average under the lock.
///
/// # Example
///
/// ```rust
/// use core_engine::adapters::InMemoryAstIndex;
/// use core_engine::ports::AstIndexPort;
///
/// let store = InMemoryAstIndex::new();
/// store.put("symbols", b"binary data").unwrap();
/// assert_eq!(store.get("symbols").unwrap(), Some(b"binary data".to_vec()));
/// store.delete("symbols").unwrap();
/// assert_eq!(store.get("symbols").unwrap(), None);
/// ```
#[derive(Debug, Default)]
pub struct InMemoryAstIndex {
    entries: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryAstIndex {
    /// Create an empty in-memory AST index store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl AstIndexPort for InMemoryAstIndex {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), PortError> {
        validate_key(key)?;
        self.entries
            .lock()
            .expect("InMemoryAstIndex mutex poisoned")
            .insert(key.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, PortError> {
        validate_key(key)?;
        Ok(self
            .entries
            .lock()
            .expect("InMemoryAstIndex mutex poisoned")
            .get(key)
            .cloned())
    }

    fn delete(&self, key: &str) -> Result<(), PortError> {
        validate_key(key)?;
        self.entries
            .lock()
            .expect("InMemoryAstIndex mutex poisoned")
            .remove(key);
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>, PortError> {
        Ok(self
            .entries
            .lock()
            .expect("InMemoryAstIndex mutex poisoned")
            .keys()
            .cloned()
            .collect())
    }
}
