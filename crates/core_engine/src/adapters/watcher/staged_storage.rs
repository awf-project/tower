//! Stages watcher record changes into one atomic storage delta. A failed
//! durable commit never exposes the staged workspace or emits file events.
use crate::domain::{ContentHash, FileId, VirtualFile};
use crate::ports::storage::ReconciliationDelta;
use crate::ports::{PortError, StoragePort};
use std::sync::{Arc, Mutex};

pub(super) struct StagedStorage {
    inner: Box<dyn StoragePort + Send>,
    pub(super) delta: Arc<Mutex<Option<ReconciliationDelta>>>,
}
impl StagedStorage {
    pub(super) fn new(
        inner: Box<dyn StoragePort + Send>,
        delta: Arc<Mutex<Option<ReconciliationDelta>>>,
    ) -> Self {
        Self { inner, delta }
    }
}
impl StoragePort for StagedStorage {
    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.inner.get(id)
    }
    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        if let Some(delta) = self
            .delta
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            delta.additions.push(file);
            return Ok(());
        }
        self.inner.put(file)
    }
    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        if let Some(delta) = self
            .delta
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            delta.removals.push(id);
            return Ok(());
        }
        self.inner.delete(id)
    }
    fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
        for file in files {
            self.put(file.clone())?;
        }
        Ok(())
    }
    fn apply_reconciliation_delta(&mut self, delta: &ReconciliationDelta) -> Result<(), PortError> {
        if let Some(staged) = self
            .delta
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            staged.additions.extend(delta.additions.iter().cloned());
            staged.removals.extend(delta.removals.iter().copied());
            return Ok(());
        }
        self.inner.apply_reconciliation_delta(delta)
    }
    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.put_blob(hash, bytes)
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
        self.inner.get_blob(hash)
    }
    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.inner.mark_scan_complete()
    }
    fn is_scan_complete(&self) -> Result<bool, PortError> {
        self.inner.is_scan_complete()
    }
}
