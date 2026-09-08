//! Tracks running ffmpeg children so a long job can be cancelled.
//!
//! Shape is dictated by `CommandChild::kill(self)`, which consumes the child — the handle
//! therefore cannot be killed through a shared reference and must be *taken* out of the map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri_plugin_shell::process::CommandChild;

#[derive(Default)]
pub struct JobRegistry {
    // std::sync::Mutex, never held across an await point.
    inner: Mutex<HashMap<String, JobHandle>>,
}

struct JobHandle {
    child: Option<CommandChild>,
    cancelled: Arc<AtomicBool>,
}

impl JobRegistry {
    /// Registers a job before its first pass starts, returning the flag the run loop polls.
    pub fn register(&self, job_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut map = self.inner.lock().expect("job registry poisoned");
        map.insert(
            job_id.to_string(),
            JobHandle {
                child: None,
                cancelled: flag.clone(),
            },
        );
        flag
    }

    /// Attaches the child for the pass that is now running. A job spans several ffmpeg
    /// invocations, so this is called once per pass.
    pub fn attach_child(&self, job_id: &str, child: CommandChild) {
        let mut map = self.inner.lock().expect("job registry poisoned");
        if let Some(handle) = map.get_mut(job_id) {
            handle.child = Some(child);
        }
    }

    /// Detaches the current child once a pass has finished on its own.
    pub fn detach_child(&self, job_id: &str) {
        let mut map = self.inner.lock().expect("job registry poisoned");
        if let Some(handle) = map.get_mut(job_id) {
            handle.child = None;
        }
    }

    pub fn is_cancelled(&self, job_id: &str) -> bool {
        let map = self.inner.lock().expect("job registry poisoned");
        map.get(job_id)
            .map(|h| h.cancelled.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Flags the job as cancelled and hands back the running child to be killed.
    ///
    /// The flag is set *before* the kill so the run loop can tell a deliberate cancel from a
    /// genuine failure: a killed ffmpeg surfaces as a signal on Unix and a nonzero exit code
    /// on Windows, neither of which is distinguishable from a crash on its own.
    pub fn cancel(&self, job_id: &str) -> Result<Option<CommandChild>, String> {
        let mut map = self.inner.lock().expect("job registry poisoned");
        let handle = map
            .get_mut(job_id)
            .ok_or_else(|| format!("No job in progress for {job_id}"))?;
        handle.cancelled.store(true, Ordering::SeqCst);
        Ok(handle.child.take())
    }

    pub fn finish(&self, job_id: &str) {
        let mut map = self.inner.lock().expect("job registry poisoned");
        map.remove(job_id);
    }
}
