//! Single-instance guard (re-audit Low 6): two app instances would both
//! checkpoint to the same `history.json` / daily record and could clobber
//! each other's writes. The second instance takes an advisory lock file
//! next to the data and exits instead of starting a second dashboard.
//!
//! Mechanism: an OS advisory file lock (`File::try_lock`), held by an
//! `InstanceGuard` for the process's lifetime. The OS releases it on exit
//! or crash — there is no stale-lock file to clean up and no pid-liveness
//! guessing to get wrong.

use std::fs::File;
use std::path::{Path, PathBuf};

/// Where the instance lock lives: beside the data the app writes (the
/// `battery-viz` data dir), or a pid-specific temp file when the platform
/// data dir is unavailable (no data dir means no shared files to protect
/// either — the lock is belt-and-braces there).
pub fn lock_path() -> PathBuf {
    match dirs::data_dir() {
        Some(data) => data.join("battery-viz").join("battery-viz.lock"),
        None => std::env::temp_dir().join(format!("battery-viz-{}.lock", std::process::id())),
    }
}

/// Try to take the instance lock at `lock_path`. `Ok(guard)` holds the
/// advisory lock for the process's lifetime; `Err(reason)` means another
/// instance holds it (or the lock could not be taken) and this instance
/// must exit instead of racing a second dashboard on the same files.
pub fn acquire(lock_path: &Path) -> Result<InstanceGuard, String> {
    use std::io::Write;
    // First launch (or first launch after uninstall) has no data dir yet:
    // create the parent before opening the lock file, or File::create hits
    // ENOENT and the app silently never starts.
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut file =
        File::create(lock_path).map_err(|e| format!("create {}: {e}", lock_path.display()))?;
    file.try_lock().map_err(|e| {
        format!(
            "another Battery Viz instance appears to be running (lock {}: {e})",
            lock_path.display()
        )
    })?;
    // The pid is for the person inspecting the lock file, not for liveness
    // checks — the OS lock is the authority.
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(InstanceGuard { _file: file })
}

/// Holds the advisory lock; dropping it (or process exit) releases it.
pub struct InstanceGuard {
    _file: File,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "battery-viz-instance-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("battery-viz.lock")
    }

    #[test]
    fn released_lock_is_reacquirable() {
        let path = temp_lock("reacquire");
        drop(acquire(&path).expect("first acquire"));
        acquire(&path).expect("re-acquire after the guard is dropped");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // B1 regression: on a true first launch the data dir does not exist yet.
    // The test must NOT pre-create the parent — that is exactly the gap the
    // original code fell into (ENOENT, app never starts).
    #[test]
    fn first_launch_creates_the_lock_directory() {
        let path = temp_lock("first-launch-missing-parent");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        assert!(
            !path.parent().unwrap().exists(),
            "the parent dir must be absent to reproduce the first-launch condition"
        );
        let _guard = acquire(&path).expect("acquire must create the missing parent dir");
        assert!(path.is_file(), "the lock file exists after first acquire");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // A second lock from a second PROCESS is the real conflict; on Linux
    // the std lock is per open file description, so the conflict is also
    // observable in-process. On other platforms fcntl locks are
    // per-process, which would let a same-process second acquire succeed
    // even though cross-process instances still conflict — so the
    // in-process assertion runs on Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn second_instance_cannot_take_the_lock() {
        let path = temp_lock("conflict");
        let _guard = acquire(&path).expect("first acquire");
        let second = acquire(&path);
        let err = match second {
            // Match, not unwrap_err: that would need Debug on the guard.
            Err(reason) => reason,
            Ok(_) => panic!("a second instance must not race the first on the same files"),
        };
        assert!(
            err.contains("another Battery Viz instance"),
            "the reason names the conflict: {err}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
