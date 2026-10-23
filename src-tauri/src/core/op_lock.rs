use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::AppError;

/// Serialises operations that change chat.db, attachment files or the
/// retained-attachment list (backups, deletes, orphan scans and cleans).
/// Running two at once can corrupt each other's assumptions — e.g. an
/// orphan scan during a messages-only delete — so a second one is refused
/// rather than queued.
#[derive(Debug, Default, Clone)]
pub struct OperationLock(Arc<Mutex<()>>);

impl OperationLock {
    pub fn try_acquire(&self, what: &str) -> Result<OwnedMutexGuard<()>, AppError> {
        self.0.clone().try_lock_owned().map_err(|_| {
            AppError::Other(format!(
                "Another backup, delete or cleanup is still running. Wait for it to finish \
                 before starting {what}."
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_operation_is_refused_until_first_finishes() {
        let lock = OperationLock::default();
        let guard = lock.try_acquire("a delete").unwrap();
        let err = lock.try_acquire("a cleanup").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("still running")));
        drop(guard);
        assert!(lock.try_acquire("a cleanup").is_ok());
    }
}
