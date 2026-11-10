//! App-wide registry of long-running file jobs (exports, dumps, transfers)
//! for the status-bar task center. Pure: the app entity owns one instance
//! and polls it on `cx.notify()`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

const TASK_HISTORY_CAP: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Export,
    Dump,
    Transfer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Finished,
    Failed(String),
    Cancelled,
}

/// Shared with the worker: cancel flag + counters it bumps as it goes.
#[derive(Debug, Default, Clone)]
pub struct TaskProgress {
    cancel: Arc<AtomicBool>,
    rows: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    /// `0` when unknown.
    total_rows: Arc<AtomicU64>,
}

impl TaskProgress {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn add_rows(&self, n: u64) {
        self.rows.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_total_rows(&self, n: u64) {
        self.total_rows.store(n, Ordering::Relaxed);
    }

    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn total_rows(&self) -> u64 {
        self.total_rows.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub id: TaskId,
    pub kind: TaskKind,
    /// Human label, e.g. "Export CSV → results.csv".
    pub label: String,
    pub progress: TaskProgress,
    pub state: TaskState,
    #[allow(dead_code)]
    pub started: Instant,
    /// File produced on success (for Reveal).
    pub output_path: Option<PathBuf>,
}

#[derive(Default)]
pub struct BackgroundTasks {
    next_id: u64,
    tasks: Vec<BackgroundTask>,
}

impl BackgroundTasks {
    pub fn start(
        &mut self,
        kind: TaskKind,
        label: impl Into<String>,
        output_path: Option<PathBuf>,
    ) -> (TaskId, TaskProgress) {
        let id = TaskId(self.next_id);
        self.next_id += 1;
        let progress = TaskProgress::default();
        self.tasks.insert(
            0,
            BackgroundTask {
                id,
                kind,
                label: label.into(),
                progress: progress.clone(),
                state: TaskState::Running,
                started: Instant::now(),
                output_path,
            },
        );
        (id, progress)
    }

    pub fn finish(&mut self, id: TaskId, state: TaskState) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.state = state;
        }
        self.trim_finished_history();
    }

    pub fn cancel(&mut self, id: TaskId) {
        if let Some(task) = self.tasks.iter().find(|t| t.id == id) {
            task.progress.cancel();
        }
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count()
    }

    pub fn tasks(&self) -> &[BackgroundTask] {
        &self.tasks
    }

    pub fn clear_finished(&mut self) {
        self.tasks.retain(|t| t.state == TaskState::Running);
    }

    pub fn get(&self, id: TaskId) -> Option<&BackgroundTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn trim_finished_history(&mut self) {
        let finished: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state != TaskState::Running)
            .map(|(i, _)| i)
            .collect();
        if finished.len() <= TASK_HISTORY_CAP {
            return;
        }
        let to_remove = finished.len() - TASK_HISTORY_CAP;
        for idx in finished.into_iter().rev().take(to_remove) {
            self.tasks.remove(idx);
        }
    }
}

/// Human byte size with one decimal ("5.6 MB").
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n == 0 {
        return "0 B".to_string();
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{} B", n);
    }
    format!("{:.1} {}", size, UNITS[unit])
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// "1,234 rows · 5.6 MB" / "1,234 / 10,000 rows · 5.6 MB" for the popover.
pub fn progress_label(p: &TaskProgress) -> String {
    let rows = p.rows();
    let total = p.total_rows();
    let bytes = human_bytes(p.bytes());
    if total > 0 {
        crate::localization::format(
            "background_tasks.progress.with_total",
            &[
                ("rows", &format_with_commas(rows)),
                ("total_rows", &format_with_commas(total)),
                ("bytes", &bytes),
            ],
        )
    } else {
        crate::localization::format(
            "background_tasks.progress.without_total",
            &[("rows", &format_with_commas(rows)), ("bytes", &bytes)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_returns_increasing_ids_and_running_count() {
        let mut tasks = BackgroundTasks::default();
        let (a, _) = tasks.start(TaskKind::Export, "a", None);
        let (b, _) = tasks.start(TaskKind::Dump, "b", None);
        assert_eq!(a, TaskId(0));
        assert_eq!(b, TaskId(1));
        assert_eq!(tasks.running_count(), 2);
    }

    #[test]
    fn cancel_sets_flag_visible_to_worker() {
        let mut tasks = BackgroundTasks::default();
        let (id, progress) = tasks.start(TaskKind::Export, "x", None);
        tasks.cancel(id);
        assert!(progress.is_cancelled());
    }

    #[test]
    fn finish_moves_task_out_of_running() {
        let mut tasks = BackgroundTasks::default();
        let (id, _) = tasks.start(TaskKind::Export, "x", None);
        assert_eq!(tasks.running_count(), 1);
        tasks.finish(id, TaskState::Finished);
        assert_eq!(tasks.running_count(), 0);
        assert_eq!(tasks.get(id).unwrap().state, TaskState::Finished);
    }

    #[test]
    fn progress_label_formats_rows_and_bytes() {
        let progress = TaskProgress::default();
        progress.add_rows(1234);
        progress.add_bytes(5_600_000);
        assert_eq!(progress_label(&progress), "1,234 rows · 5.6 MB");
        let zero = TaskProgress::default();
        assert_eq!(progress_label(&zero), "0 rows · 0 B");
    }

    #[test]
    fn history_is_capped() {
        let mut tasks = BackgroundTasks::default();
        let ids: Vec<TaskId> = (0..25)
            .map(|i| {
                let (id, _) = tasks.start(TaskKind::Export, format!("t{i}"), None);
                tasks.finish(id, TaskState::Finished);
                id
            })
            .collect();
        assert_eq!(
            tasks
                .tasks()
                .iter()
                .filter(|t| t.state != TaskState::Running)
                .count(),
            TASK_HISTORY_CAP
        );
        assert!(!tasks.tasks().iter().any(|t| t.id == ids[0]));
    }
}
