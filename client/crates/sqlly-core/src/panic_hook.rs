//! Global panic hook that logs panics to a crash file and stderr.
//!
//! Call [`install_panic_hook`] once at process startup (before any work that
//! could panic). Every panic in any thread or async task triggers the hook,
//! which writes a single-line record to the crash log and then chains to the
//! previously installed hook so default stderr output is preserved.
//!
//! The crash log path defaults to `$TMPDIR/sqlly-crash.log` and can be
//! overridden with the `SQLLY_CRASH_LOG` environment variable.
//!
//! Log line format (one line per panic, grep-able):
//! ```text
//! <unix_millis> PANIC thread=<name> msg=<payload> location=<file:line:col> backtrace=<...>
//! ```

use std::backtrace::Backtrace;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

/// Environment variable that overrides the crash log file path.
const CRASH_LOG_ENV: &str = "SQLLY_CRASH_LOG";

/// Install the global panic hook. Safe to call multiple times — only the first
/// call installs the hook; subsequent calls are no-ops.
///
/// The hook:
/// 1. Writes a one-line record (timestamp, thread name, payload, location,
///    backtrace) to the crash log file.
/// 2. Chains to the previously installed hook so stderr backtraces still fire.
///
/// The crash log path is resolved from `SQLLY_CRASH_LOG` at panic time, or
/// defaults to `$TMPDIR/sqlly-crash.log`.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash_record(info);
        previous(info);
    }));
}

/// Returns the crash log path that would be used right now (resolved from
/// `SQLLY_CRASH_LOG` or the default). Useful for diagnostics.
pub fn crash_log_path() -> PathBuf {
    resolve_crash_log_path()
}

fn resolve_crash_log_path() -> PathBuf {
    if let Ok(path) = std::env::var(CRASH_LOG_ENV)
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join("sqlly-crash.log")
}

/// Extract a human-readable message from a panic payload (`Box<dyn Any>`).
fn extract_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    // `downcast_ref` on `dyn Any + Send` checks the TypeId of the concrete
    // type stored inside the trait object. Panics in Rust are typically
    // `panic!("msg")` (payload is `&'static str`) or
    // `panic!("{}", format!("msg"))` (payload is `String`).
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Escape newlines in a string so each log record stays on one line.
fn single_line(s: &str) -> String {
    s.replace('\n', "\\n")
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_crash_record(info: &std::panic::PanicHookInfo<'_>) {
    let path = resolve_crash_log_path();

    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let payload = info.payload();
    let msg = extract_panic_message(payload);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".to_string());
    let backtrace = Backtrace::force_capture();
    let bt = single_line(&backtrace.to_string());

    let line = format!(
        "{ts} PANIC thread={thread} msg={msg} location={loc} backtrace={bt}",
        ts = now_millis(),
        thread = single_line(thread_name),
        msg = single_line(&msg),
        loc = single_line(&location),
        bt = bt,
    );

    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            writeln!(f, "{line}")?;
            f.flush()
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static HOOK_TEST_RAN: AtomicBool = AtomicBool::new(false);

    #[test]
    fn extract_panic_message_str() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        let inner: &(dyn std::any::Any + Send) = &*payload;
        assert_eq!(extract_panic_message(inner), "boom");
    }

    #[test]
    fn extract_panic_message_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        let inner: &(dyn std::any::Any + Send) = &*payload;
        assert_eq!(extract_panic_message(inner), "boom");
    }

    #[test]
    fn extract_panic_message_other() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        let inner: &(dyn std::any::Any + Send) = &*payload;
        assert_eq!(extract_panic_message(inner), "<non-string panic payload>");
    }

    #[test]
    fn single_line_replaces_newlines() {
        assert_eq!(single_line("a\nb\nc"), "a\\nb\\nc");
        assert_eq!(single_line("no newlines"), "no newlines");
    }

    #[test]
    fn resolve_path_from_env() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set(CRASH_LOG_ENV, "/tmp/custom-crash.log");
        assert_eq!(
            resolve_crash_log_path(),
            PathBuf::from("/tmp/custom-crash.log")
        );
    }

    #[test]
    fn resolve_path_default() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.remove(CRASH_LOG_ENV);
        let path = resolve_crash_log_path();
        assert!(
            path.starts_with(std::env::temp_dir())
                || path == std::env::temp_dir().join("sqlly-crash.log")
        );
    }

    /// Verify the hook writes a PANIC record to the crash log when a panic
    /// occurs. Uses `catch_unwind` which triggers the panic hook.
    #[test]
    fn hook_writes_crash_record_on_panic() {
        if HOOK_TEST_RAN.swap(true, Ordering::SeqCst) {
            return; // Only run once per process — panic hooks are global.
        }
        let mut env = sqlly_testkit::EnvGuard::acquire();

        let log_path = std::env::temp_dir().join(format!(
            "sqlly-crash-hook-test-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&log_path);
        env.set(CRASH_LOG_ENV, log_path.to_str().expect("utf8 temp path"));

        install_panic_hook();

        let result = std::panic::catch_unwind(|| {
            panic!("hook-test-panic-message");
        });
        assert!(result.is_err());

        let log_contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log_contents.contains("PANIC"),
            "expected PANIC in crash log, got:\n{}",
            log_contents
        );
        assert!(
            log_contents.contains("hook-test-panic-message"),
            "expected panic message in crash log, got:\n{}",
            log_contents
        );

        let _ = std::fs::remove_file(&log_path);
    }
}
