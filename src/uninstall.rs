//! Platform-neutral uninstall: enumerate and delete every on-disk artifact
//! the app creates. The app never removes its own binary and never executes
//! a shell removal command — it only hands the person a copyable one
//! (`removal_command`); the person runs the final deletion.

use std::path::{Path, PathBuf};

/// Every artifact this installation creates: the `battery-viz` data
/// directory (the 1-minute `history.json` and the FR-7 daily record
/// `daily-record.json` both live inside it) and the CSV export file next to
/// it. Paths are returned whether or not they exist; the UI lists only the
/// ones that do.
pub fn paths() -> Vec<PathBuf> {
    match dirs::data_dir() {
        Some(data) => artifacts_for(&data.join("battery-viz").join("history.json")),
        None => Vec::new(),
    }
}

/// The two artifacts for an installation, derived from the history path the
/// app persists to (`<data dir>/battery-viz/history.json`): its containing
/// `battery-viz` directory — which covers every store inside it, including
/// the FR-7 daily record — and the sibling CSV export. Deriving from the
/// history path keeps the app's own confirm flow pointed at the same data
/// dir it was booted with (the seam tests use).
pub fn artifacts_for(history_path: &Path) -> Vec<PathBuf> {
    let Some(app_dir) = history_path.parent() else {
        return Vec::new();
    };
    let Some(data_dir) = app_dir.parent() else {
        return vec![app_dir.to_path_buf()];
    };
    vec![
        app_dir.to_path_buf(),
        data_dir.join("battery-viz-export.csv"),
    ]
}

/// Delete every artifact, reporting each path's outcome — nothing silent.
/// A path that does not exist reports success: the machine is already clean
/// for that artifact ("removal with nothing stored succeeds").
///
/// The app deletes via `remove_paths` with targets derived from its own
/// known history path (the seam tests point at a temp dir); this global
/// form re-derives the same set for direct callers.
#[allow(dead_code)]
pub fn remove_all() -> Vec<Result<PathBuf, String>> {
    remove_paths(paths())
}

/// Delete an explicit list of artifacts; shared worker for `remove_all` and
/// the app's own confirm flow (whose targets come from its known history
/// path, so tests can point that at a temp dir).
pub(crate) fn remove_paths(paths: Vec<PathBuf>) -> Vec<Result<PathBuf, String>> {
    paths.into_iter().map(remove_path).collect()
}

fn remove_path(path: PathBuf) -> Result<PathBuf, String> {
    if !path.exists() {
        return Ok(path);
    }
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match removed {
        Ok(()) => Ok(path),
        Err(e) => Err(format!("delete {}: {e}", path.display())),
    }
}

/// Quote a string for a PowerShell command line: a single-quoted literal
/// with the quote doubled. PowerShell has no backslash escaping inside
/// single quotes — the POSIX `'\''` form the Windows branch used to emit
/// breaks any path containing an apostrophe (re-audit N11a).
#[cfg(any(target_os = "windows", test))]
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The full copyable removal command for a Windows install: delete the
/// binary, then delete the toast AUMID registry key the app created at
/// `HKCU\SOFTWARE\Classes\AppUserModelId\Battery Viz` (re-audit N11b —
/// an uninstall that leaves the key behind leaves a ghost toast sender).
/// Pure so fixture tests can run it on every host.
#[cfg(any(target_os = "windows", test))]
fn windows_removal_command(exe: &str, aumid_key_ps: &str) -> String {
    format!(
        "Remove-Item {}; Remove-Item -Path {}",
        ps_quote(exe),
        ps_quote(aumid_key_ps)
    )
}

/// The copyable command that removes the app binary — shown to the person,
/// never executed by the app. `rm -f` on macOS/POSIX; on Windows a
/// PowerShell line that also removes the AUMID registry key the app
/// registered for toasts.
pub fn removal_command(exe: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        windows_removal_command(exe, &crate::notify::windows::aumid_key_ps())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let quoted = format!("'{}'", exe.replace('\'', "'\\''"));
        format!("rm -f {quoted}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh temp dir standing in for the platform data dir.
    fn temp_data_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "battery-viz-uninstall-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp data dir");
        base
    }

    #[test]
    fn paths_returns_both_artifacts() {
        let base = temp_data_dir("paths");
        let artifacts = artifacts_for(&base.join("battery-viz").join("history.json"));
        assert_eq!(artifacts.len(), 2);
        assert_eq!(
            artifacts[0].file_name(),
            Some(std::ffi::OsStr::new("battery-viz"))
        );
        assert_eq!(
            artifacts[1].file_name(),
            Some(std::ffi::OsStr::new("battery-viz-export.csv"))
        );
        assert_eq!(artifacts[1].parent(), Some(base.as_path()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn global_paths_lists_both_artifacts_when_data_dir_exists() {
        if dirs::data_dir().is_none() {
            return; // no platform data dir — nothing to enumerate
        }
        assert_eq!(paths().len(), 2);
    }

    #[test]
    fn deletion_covers_the_fr7_daily_record_store() {
        // The FR-7 record (`daily-record.json`) is a sibling of
        // `history.json` inside the app dir, so the directory entry in the
        // deletion list removes it — no separate listing, no double delete.
        let base = temp_data_dir("fr7-record");
        let record = base.join("battery-viz").join("daily-record.json");
        let artifacts = artifacts_for(&base.join("battery-viz").join("history.json"));
        assert!(
            artifacts.iter().any(|p| record.starts_with(p)),
            "daily record {record:?} must fall inside a listed deletion target: {artifacts:?}"
        );
        // And end to end: deleting the listed artifacts removes the record.
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(&record, "{}").unwrap();
        let results = remove_paths(artifacts);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert!(!record.exists(), "daily record deleted with the app dir");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_all_deletes_existing_dir_and_file_per_path() {
        let base = temp_data_dir("delete");
        let app_dir = base.join("battery-viz");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("history.json"), "{}").unwrap();
        let csv = base.join("battery-viz-export.csv");
        std::fs::write(&csv, "col\n").unwrap();

        let results = remove_paths(artifacts_for(&app_dir.join("history.json")));
        assert_eq!(results.len(), 2, "one result per path — nothing silent");
        assert!(
            results.iter().all(Result::is_ok),
            "all deleted: {results:?}"
        );
        assert!(!app_dir.exists() && !csv.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_with_nothing_stored_succeeds() {
        let base = temp_data_dir("empty");
        let results = remove_paths(artifacts_for(
            &base.join("battery-viz").join("history.json"),
        ));
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_only_failure_surfaces_as_error_not_panic() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let base = temp_data_dir("readonly");
            let app_dir = base.join("battery-viz");
            std::fs::create_dir_all(&app_dir).unwrap();
            std::fs::write(app_dir.join("history.json"), "{}").unwrap();
            std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            // If the write bit is not honored here (e.g. running as root), a
            // forced failure cannot be constructed — skip rather than lie.
            let probe = app_dir.join(".probe");
            if std::fs::write(&probe, b"x").is_ok() {
                let _ = std::fs::remove_dir_all(&base);
                return;
            }
            let _ = std::fs::remove_file(&probe);

            let results = remove_paths(artifacts_for(&app_dir.join("history.json")));
            assert!(results[0].is_err(), "read-only dir must error: {results:?}");
            assert!(results[1].is_ok(), "absent csv still succeeds: {results:?}");

            // Restore so cleanup can run.
            std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn removal_command_quotes_the_binary_path() {
        assert_eq!(
            removal_command("/opt/App/battery-viz"),
            "rm -f '/opt/App/battery-viz'"
        );
        assert_eq!(removal_command("/opt/my'app"), "rm -f '/opt/my'\\''app'");
    }

    // N11a: the Windows removal command runs under PowerShell, so its
    // quoting must be PowerShell's (doubled single quotes), not POSIX
    // backslash-escaping. Pure helpers, so these run on every host.
    #[test]
    fn windows_removal_command_uses_powershell_quoting_and_removes_the_aumid_key() {
        let cmd = windows_removal_command(
            r"C:\Apps\battery-viz.exe",
            r"HKCU:\SOFTWARE\Classes\AppUserModelId\Battery Viz",
        );
        assert_eq!(
            cmd,
            "Remove-Item 'C:\\Apps\\battery-viz.exe'; Remove-Item -Path \
             'HKCU:\\SOFTWARE\\Classes\\AppUserModelId\\Battery Viz'"
        );
    }

    #[test]
    fn powershell_quoting_doubles_embedded_quotes() {
        assert_eq!(
            ps_quote("C:\\my'app\\battery-viz.exe"),
            "'C:\\my''app\\battery-viz.exe'"
        );
        assert_eq!(ps_quote("plain path.exe"), "'plain path.exe'");
    }
}
