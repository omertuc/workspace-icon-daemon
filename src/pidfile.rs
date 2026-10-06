//! The PID file that lets a new daemon replace a running one.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Linux's process start time, which guards against PID reuse.
fn process_start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces; the fields after it start at field 3 (state).
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 2..)?
        .split_whitespace()
        .collect();
    fields.get(19).map(ToString::to_string)
}

/// Publish this daemon's PID and start time.
pub fn write(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let pid = std::process::id();
    std::fs::write(
        path,
        format!("{pid} {}\n", process_start_time(pid).unwrap_or_default()),
    )
    .with_context(|| format!("writing {}", path.display()))
}

/// Remove the PID file only if it still identifies this process.
pub fn remove_own(path: &Path) {
    let recorded = std::fs::read_to_string(path).ok();
    let pid = recorded
        .as_deref()
        .and_then(|t| t.split_whitespace().next()?.parse::<u32>().ok());
    if pid == Some(std::process::id()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Ask the daemon recorded in `path` to exit, waiting briefly for it.
pub fn stop_running_daemon(path: &Path) -> bool {
    let recorded = std::fs::read_to_string(path).unwrap_or_default();
    let mut parts = recorded.split_whitespace();
    let (Some(Ok(pid)), Some(start)) = (parts.next().map(str::parse::<u32>), parts.next()) else {
        let _ = std::fs::remove_file(path);
        return false;
    };
    if pid == std::process::id() || process_start_time(pid).as_deref() != Some(start) {
        let _ = std::fs::remove_file(path);
        return false;
    }
    let Ok(signed_pid) = i32::try_from(pid) else {
        let _ = std::fs::remove_file(path);
        return false;
    };
    // SAFETY: kill has no memory-safety preconditions.
    if unsafe { libc_kill(signed_pid, 15) } != 0 {
        let _ = std::fs::remove_file(path);
        return false;
    }
    log::info!("Asked running daemon (PID {pid}) to exit");
    for _ in 0..50 {
        if !path.exists() || process_start_time(pid).is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pid_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        write(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&std::process::id().to_string()));
        assert_eq!(text.split_whitespace().count(), 2);
        // Never signals itself, and drops a file naming this process.
        assert!(!stop_running_daemon(&path));
        assert!(!path.exists());
        write(&path).unwrap();
        remove_own(&path);
        assert!(!path.exists());
    }

    #[test]
    fn stale_pid_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, "1 12345\n").unwrap();
        assert!(!stop_running_daemon(&path));
        assert!(!path.exists());
    }
}
