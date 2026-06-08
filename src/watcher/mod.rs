use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use self::{events::ChangeSet, overlay::Overlay, reconcile::ReconcileResult};

pub mod events;
pub mod overlay;
pub mod reconcile;

/// Watcher tracks file system changes in a workspace and provides
/// reconcile capabilities to compare current state against the snapshot.
pub struct Watcher {
    pub workspace_root: PathBuf,
    pub running: bool,
    pub last_event_at: Option<u64>,
    pub last_reconcile_at: Option<u64>,
    pub queue_len: usize,
    overlay: Overlay,
}

impl Watcher {
    /// Create a new Watcher for the given workspace root.
    /// Does NOT start background watching — that requires a long-running
    /// process; this CLI only supports run_once / reconcile loops.
    pub fn start(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        Ok(Self {
            workspace_root: root.clone(),
            running: false,
            last_event_at: None,
            last_reconcile_at: None,
            queue_len: 0,
            overlay: Overlay::new(root),
        })
    }

    /// Run a single reconcile pass: scan workspace, compare with snapshot.
    /// Returns the reconcile result. Does NOT start long-running watch.
    pub fn run_once(&mut self) -> Result<ReconcileResult> {
        let result = reconcile::reconcile(&self.workspace_root)?;
        self.last_reconcile_at = Some(now_ms());
        self.overlay.update_from_reconcile(&result);
        Ok(result)
    }

    /// Run reconcile (same as run_once; alias for clarity).
    pub fn reconcile(&mut self) -> Result<ReconcileResult> {
        self.run_once()
    }

    /// Collect a batch of file system events into a normalized change set.
    /// Uses notify crate to watch for events with a debounce window.
    pub fn collect_events(&mut self, debounce_ms: u64) -> Result<ChangeSet> {
        self.running = true;
        let result = self.collect_events_running(debounce_ms);
        self.running = false;
        result
    }

    fn collect_events_running(&mut self, debounce_ms: u64) -> Result<ChangeSet> {
        use notify::{Event, RecursiveMode, Watcher as _};

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .context("failed to create file watcher")?;

        watcher
            .watch(&self.workspace_root, RecursiveMode::Recursive)
            .context("failed to start watching workspace")?;

        let start = Instant::now();
        let timeout = Duration::from_millis(debounce_ms);
        let mut raw_events: Vec<Event> = Vec::new();

        // Collect events until debounce window expires with no new events
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    // Filter out events for skipped paths
                    if !events::should_skip_event(&event) {
                        raw_events.push(event);
                    }
                    self.last_event_at = Some(now_ms());
                    // Break when debounce window has elapsed after events arrived
                    let elapsed = start.elapsed();
                    if elapsed >= timeout && !raw_events.is_empty() {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("watch error: {:?}", e);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No events for 100ms; if we've waited past debounce window, break
                    if start.elapsed() >= timeout {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        // Explicitly drop the watcher to stop watching
        drop(watcher);

        self.queue_len = raw_events.len();
        let change_set = events::normalize_events(&raw_events, &self.workspace_root);
        Ok(change_set)
    }

    /// Return the current watcher state as a JSON value.
    pub fn status(&self) -> Value {
        let overlay_status = self.overlay.status();
        let stale = overlay_status["stale"].as_bool().unwrap_or(false);

        json!({
            "running": self.running,
            "state": if self.running { "collecting" } else { "idle" },
            "root": self.workspace_root,
            "queueLength": self.queue_len,
            "stale": stale,
            "lastEventAt": self.last_event_at,
            "lastReconcileAt": self.last_reconcile_at,
            "mode": "reconcile_on_demand",
            "overlay": overlay_status,
        })
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_marks_stale_when_overlay_only_has_added_and_deleted_files() {
        // Given: a reconcile result where only added/deleted files make the overlay stale.
        let mut watcher = Watcher::start(Path::new("/tmp/codetrail-watch-status")).unwrap();
        let result = ReconcileResult {
            dirty_files: Vec::new(),
            added_files: vec!["src/new.rs".to_string()],
            deleted_files: vec!["src/old.rs".to_string()],
            stale: true,
            total_files_scanned: 1,
            reconciled_at: 0,
        };
        watcher.overlay.update_from_reconcile(&result);

        // When: top-level watcher status is rendered.
        let status = watcher.status();

        // Then: top-level stale follows the overlay stale state, not only dirty files.
        assert_eq!(status["stale"], true);
        assert_eq!(status["overlay"]["stale"], true);
        assert_eq!(status["overlay"]["addedCount"], 1);
        assert_eq!(status["overlay"]["deletedCount"], 1);
    }

    #[test]
    fn status_reports_idle_state_for_on_demand_watcher() {
        let watcher = Watcher::start(Path::new("/tmp/codetrail-watch-idle")).unwrap();
        let status = watcher.status();

        assert_eq!(status["running"], false);
        assert_eq!(status["state"], "idle");
        assert_eq!(status["mode"], "reconcile_on_demand");
    }
}
