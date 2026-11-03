//! Kill switch and startup self-tests (DESIGN §9, §12). Unknown means stop.

use crate::risk::{self, Limits};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillSwitch {
    Clear,
    Engaged(String),
}

/// `$BMV_HOME/data/STOP`, from `config::home()`.
pub fn stop_path(home: &Path) -> PathBuf {
    home.join("data").join("STOP")
}

/// Present means engaged. Only a clean "not found" is clear: any other error
/// (permissions, a file where a directory should be) is engaged.
pub fn kill_switch(path: &Path) -> KillSwitch {
    if !path.is_absolute() {
        // A relative path depends on the cwd, so we can't know which file we checked.
        return KillSwitch::Engaged(format!("kill switch path not absolute: {path:?}"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => KillSwitch::Engaged(format!("STOP file present: {path:?}")),
        Err(e) if e.kind() == ErrorKind::NotFound => KillSwitch::Clear,
        Err(e) => KillSwitch::Engaged(format!("kill switch unreadable {path:?}: {e}")),
    }
}

/// Fresh, unique dir under the system temp dir; caller removes it.
fn scratch_dir(tag: &str) -> Result<PathBuf, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "bmv-safety-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).map_err(|e| format!("self-test temp dir {d:?}: {e}"))?;
    Ok(d)
}

/// Proves on this machine that absent reads Clear, present reads Engaged, and
/// an unreadable path reads Engaged.
pub fn kill_switch_self_test() -> Result<(), String> {
    let dir = scratch_dir("selftest")?;
    let r = kill_switch_cases(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn kill_switch_cases(dir: &Path) -> Result<(), String> {
    let stop = dir.join("STOP");
    let io = |e: std::io::Error| format!("kill-switch self-test: {e}");
    if kill_switch(&stop) != KillSwitch::Clear {
        return Err("kill-switch self-test: absent STOP not Clear".into());
    }
    std::fs::write(&stop, b"").map_err(io)?;
    if kill_switch(&stop) == KillSwitch::Clear {
        return Err("kill-switch self-test: present STOP not Engaged".into());
    }
    // STOP is a file, so STOP/x fails with "not a directory": a non-NotFound
    // error that, unlike chmod, still fails when running as root.
    if kill_switch(&stop.join("x")) == KillSwitch::Clear {
        return Err("kill-switch self-test: read error not Engaged".into());
    }
    if kill_switch(Path::new("data/STOP")) == KillSwitch::Clear {
        return Err("kill-switch self-test: relative path not Engaged".into());
    }
    Ok(())
}

/// Startup gate: any failure means no trading today.
pub fn startup_checks(limits: &Limits, home: &Path) -> Result<(), String> {
    risk::self_test(limits)?;
    kill_switch_self_test()?;
    match kill_switch(&stop_path(home)) {
        KillSwitch::Clear => Ok(()),
        KillSwitch::Engaged(why) => Err(format!("kill switch engaged at startup: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;

    #[test]
    fn absent_is_clear_present_is_engaged() {
        let d = scratch_dir("t1").unwrap();
        let stop = d.join("STOP");
        assert_eq!(kill_switch(&stop), KillSwitch::Clear);
        std::fs::write(&stop, b"").unwrap();
        assert!(matches!(kill_switch(&stop), KillSwitch::Engaged(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stop_as_directory_is_engaged() {
        let d = scratch_dir("t2").unwrap();
        let stop = d.join("STOP");
        std::fs::create_dir(&stop).unwrap();
        assert!(matches!(kill_switch(&stop), KillSwitch::Engaged(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn non_not_found_error_is_engaged() {
        let d = scratch_dir("t3").unwrap();
        let file = d.join("data");
        std::fs::write(&file, b"").unwrap();
        // data is a file, so data/STOP is ENOTDIR, not NotFound.
        assert!(matches!(
            kill_switch(&file.join("STOP")),
            KillSwitch::Engaged(_)
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_parent_is_engaged() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch_dir("t4").unwrap();
        let locked = d.join("data");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores permissions, so the case is untestable there.
        let enforced = std::fs::read_dir(&locked).is_err();
        let got = kill_switch(&locked.join("STOP"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
        if enforced {
            assert!(matches!(got, KillSwitch::Engaged(_)), "{got:?}");
        }
    }

    #[test]
    fn relative_path_is_engaged() {
        assert!(matches!(
            kill_switch(Path::new("data/STOP")),
            KillSwitch::Engaged(_)
        ));
    }

    #[test]
    fn stop_path_is_under_data() {
        assert_eq!(
            stop_path(Path::new("/h")),
            PathBuf::from("/h").join("data").join("STOP")
        );
    }

    #[test]
    fn self_test_passes_here() {
        kill_switch_self_test().unwrap();
    }

    #[test]
    fn startup_ok_when_clear_fails_when_engaged() {
        let home = scratch_dir("t5").unwrap();
        let limits = Limits::for_mode(Mode::Paper);
        assert_eq!(startup_checks(&limits, &home), Ok(()));
        std::fs::create_dir(home.join("data")).unwrap();
        std::fs::write(stop_path(&home), b"").unwrap();
        let err = startup_checks(&limits, &home).unwrap_err();
        assert!(err.contains("engaged"), "{err}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn startup_fails_on_bad_home() {
        let limits = Limits::for_mode(Mode::Live);
        assert!(startup_checks(&limits, Path::new("relative")).is_err());
    }
}
