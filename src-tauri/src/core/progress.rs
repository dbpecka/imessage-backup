use imessage_database::exporters::progress::ProgressReporter;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

#[allow(dead_code)]
pub const BACKUP_EVENT: &str = "backup-progress";

/// Payload for the `backup-progress` event emitted to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub total: u64,
    pub position: u64,
    pub message: String,
    pub done: bool,
}

/// [`ProgressReporter`] implementation that forwards updates to the frontend
/// as Tauri events. One instance per export run; safe to share across threads
/// because the inner state is behind a mutex and `AppHandle` is `Clone +
/// Send + Sync`.
pub struct TauriProgress {
    app: AppHandle,
    event: String,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default, Clone)]
struct Inner {
    total: u64,
    position: u64,
    message: String,
}

impl TauriProgress {
    pub fn new(app: AppHandle, event: impl Into<String>) -> Self {
        Self {
            app,
            event: event.into(),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn emit(&self, done: bool) {
        let snap = match self.inner.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let _ = self.app.emit(
            &self.event,
            ProgressPayload {
                total: snap.total,
                position: snap.position,
                message: snap.message,
                done,
            },
        );
    }
}

impl ProgressReporter for TauriProgress {
    fn start(&self, total: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.total = total;
            g.position = 0;
            g.message = String::from("Starting export");
        }
        self.emit(false);
    }

    fn set_position(&self, position: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.position = position;
        }
        self.emit(false);
    }

    fn set_message(&self, message: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.message = message.to_string();
        }
        self.emit(false);
    }

    fn finish(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.position = g.total;
            g.message = String::from("Done");
        }
        self.emit(true);
    }
}
