//! Sweeping one target dir: plan, check for live processes, then (only if asked) delete.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::live::{self, LiveUse, Processes};
use crate::plan::{self, Unit};

pub struct Options {
    /// Build output last used before this is stale.
    pub cutoff: SystemTime,
    /// Delete; otherwise only report.
    pub apply: bool,
    /// Where to look for running processes, normally [`Processes::native`].
    pub processes: Processes,
    /// This process, which may itself run from a target dir being swept.
    pub own_pid: u32,
}

#[derive(Debug)]
pub enum Outcome {
    /// A process uses the target dir, so none of it was touched.
    Live(LiveUse),
    /// The stale units found (and removed, when applying), and the disk space they account for.
    /// No units means nothing was old enough.
    Stale {
        units: Vec<Unit>,
        bytes: u64,
        failures: Vec<(PathBuf, io::Error)>,
    },
}

#[derive(Debug)]
pub struct Report {
    pub target: PathBuf,
    pub outcome: Outcome,
    pub warnings: Vec<String>,
}

/// Sweeps one target dir.
///
/// The live-process check runs after planning, immediately before any delete, so the window for a
/// process to start in between is as short as it can be. An error means the target dir or the
/// process list could not be read; nothing was deleted.
pub fn sweep_target(target: &Path, options: &Options) -> io::Result<Report> {
    let plan = plan::plan_target(target, options.cutoff)?;
    let bytes = plan::reclaimable_bytes(&plan.units);

    // Processes report resolved paths; command lines may use the path as discovered.
    let mut names = vec![target.to_path_buf()];
    if let Ok(resolved) = fs::canonicalize(target)
        && resolved != target
    {
        names.push(resolved);
    }
    if let Some(live) = live::find_live_use(&options.processes, &names, options.own_pid)? {
        return Ok(Report {
            target: target.to_path_buf(),
            outcome: Outcome::Live(live),
            warnings: plan.warnings,
        });
    }

    let mut failures = Vec::new();
    if options.apply {
        for unit in &plan.units {
            let removed = if plan::is_deletable(target, &unit.path) {
                remove(&unit.path)
            } else {
                Err(io::Error::other(
                    "refused: not inside a cache dir, or protected",
                ))
            };
            if let Err(err) = removed {
                failures.push((unit.path.clone(), err));
            }
        }
    }
    Ok(Report {
        target: target.to_path_buf(),
        outcome: Outcome::Stale {
            units: plan.units,
            bytes,
            failures,
        },
        warnings: plan.warnings,
    })
}

/// Removes a file, symlink, or directory tree. `remove_dir_all` does not follow symlinks.
fn remove(path: &Path) -> io::Result<()> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_dir() {
        fs::remove_dir_all(path)
    } else if is_dir_link(&file_type) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Windows makes a symlink (or junction) to a directory a directory entry of its own, removed with
/// `remove_dir`. Elsewhere every symlink is removed like a file.
#[cfg(windows)]
fn is_dir_link(file_type: &fs::FileType) -> bool {
    use std::os::windows::fs::FileTypeExt;
    file_type.is_symlink_dir()
}

#[cfg(not(windows))]
fn is_dir_link(_: &fs::FileType) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Process;
    use crate::testutil::{age_dirs, days_ago, file};

    /// A target dir with one stale and one fresh cache entry, and an ancient release binary.
    fn target() -> tempfile::TempDir {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("debug/incremental/old-1/s-a/dep-graph.bin"), 40, 40);
        file(&t.join("debug/deps/libfresh-2.rlib"), 0, 0);
        file(&t.join("release/server"), 400, 400);
        age_dirs(t, 40);
        target
    }

    fn options(apply: bool, processes: Processes) -> Options {
        Options {
            cutoff: days_ago(14),
            apply,
            processes,
            own_pid: 0,
        }
    }

    fn nothing_running() -> Processes {
        Processes::Fixed(Vec::new())
    }

    fn running(pid: u32, exe: PathBuf) -> Processes {
        Processes::Fixed(vec![Process {
            pid,
            exe: Some(exe),
            args: Vec::new(),
        }])
    }

    #[test]
    fn a_dry_run_deletes_nothing() {
        let target = target();
        let report = sweep_target(target.path(), &options(false, nothing_running())).unwrap();

        let Outcome::Stale {
            units,
            bytes,
            failures,
        } = report.outcome
        else {
            panic!("not live")
        };
        assert_eq!(units.len(), 1);
        assert!(bytes > 0);
        assert!(failures.is_empty());
        assert!(target.path().join("debug/incremental/old-1").exists());
    }

    #[test]
    fn applying_deletes_only_the_stale_units() {
        let target = target();
        let t = target.path();
        let report = sweep_target(t, &options(true, nothing_running())).unwrap();

        let Outcome::Stale {
            units, failures, ..
        } = report.outcome
        else {
            panic!("not live")
        };
        // Everything in incremental/ was stale, so it went as one unit.
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, t.join("debug/incremental"));
        assert!(failures.is_empty(), "{failures:?}");
        assert!(!t.join("debug/incremental").exists());
        assert!(t.join("debug/deps").is_dir());
        assert!(t.join("debug/deps/libfresh-2.rlib").exists());
        assert!(t.join("release/server").exists());
    }

    #[test]
    fn a_live_process_skips_the_whole_target() {
        let target = target();
        let t = target.path();
        let report =
            sweep_target(t, &options(true, running(321, t.join("release/server")))).unwrap();

        let Outcome::Live(live) = report.outcome else {
            panic!("expected a live process")
        };
        assert_eq!(live.pid, 321);
        assert!(t.join("debug/incremental/old-1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_live_process_found_through_a_symlinked_path() {
        let target = target();
        let links = tempfile::tempdir().unwrap();
        let via_link = links.path().join("target");
        std::os::unix::fs::symlink(target.path(), &via_link).unwrap();
        // The process reports the resolved path; the sweep was given the symlinked one.
        let resolved = fs::canonicalize(target.path()).unwrap();

        let report = sweep_target(
            &via_link,
            &options(true, running(8, resolved.join("release/server"))),
        )
        .unwrap();
        assert!(matches!(report.outcome, Outcome::Live(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_proc_means_no_sweep() {
        let target = target();
        let result = sweep_target(
            target.path(),
            &options(true, Processes::Proc(PathBuf::from("/nonexistent/proc"))),
        );
        assert!(result.is_err());
        assert!(target.path().join("debug/incremental/old-1").exists());
    }
}
