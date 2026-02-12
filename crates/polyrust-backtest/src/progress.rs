use indicatif::ProgressBar;
use std::sync::{LazyLock, Mutex};

/// Global slot for the currently active progress bar.
/// When set, tracing output is routed through `pb.println()` to avoid corruption.
static ACTIVE_PROGRESS_BAR: LazyLock<Mutex<Option<ProgressBar>>> =
    LazyLock::new(|| Mutex::new(None));

/// Read the active progress bar (if any). Returns `None` if no bar is registered
/// or the mutex is poisoned.
pub fn active_progress_bar() -> Option<ProgressBar> {
    ACTIVE_PROGRESS_BAR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// RAII guard that registers a progress bar on creation and clears it on drop.
///
/// Panic-safe: uses `unwrap_or_else(|e| e.into_inner())` to recover from poisoned mutex.
pub struct ProgressBarGuard;

impl ProgressBarGuard {
    /// Register a progress bar as the active bar for tracing output routing.
    pub fn register(pb: &ProgressBar) -> Self {
        let mut slot = ACTIVE_PROGRESS_BAR
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(pb.clone());
        Self
    }
}

impl Drop for ProgressBarGuard {
    fn drop(&mut self) {
        let mut slot = ACTIVE_PROGRESS_BAR
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *slot = None;
    }
}
