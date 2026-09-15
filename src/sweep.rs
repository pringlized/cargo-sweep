//! Sweeping one target dir: plan, check for live processes, then (only if asked) delete.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::live::{self, LiveUse};
use crate::plan::{self, Unit};

pub struct Options {
    /// Build output last used before this is stale.
    pub cutoff: SystemTime,
    /// Delete; otherwise only report.
    pub apply: bool,
    /// Where to look for running processes, normally `/proc`.
    pub proc_root: PathBuf,
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
/// process to start in between is as short as it can be. An error means the target dir or `/proc`
/// could not be read; nothing was deleted.
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
    if let Some(live) = live::find_live_use(&options.proc_root, &names, options.own_pid)? {
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
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{age_dirs, days_ago, file};
    use std::os::unix::fs::symlink;

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

    fn options(apply: bool, proc_root: &Path) -> Options {
        Options {
            cutoff: days_ago(14),
            apply,
            proc_root: proc_root.to_path_buf(),
            own_pid: 0,
        }
    }

    fn empty_proc() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_dry_run_deletes_nothing() {
        let (target, proc) = (target(), empty_proc());
        let report = sweep_target(target.path(), &options(false, proc.path())).unwrap();

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
        let (target, proc) = (target(), empty_proc());
        let t = target.path();
        let report = sweep_target(t, &options(true, proc.path())).unwrap();

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
        let (target, proc) = (target(), empty_proc());
        let t = target.path();
        fs::create_dir_all(proc.path().join("321")).unwrap();
        symlink(t.join("release/server"), proc.path().join("321/exe")).unwrap();

        let report = sweep_target(t, &options(true, proc.path())).unwrap();
        let Outcome::Live(live) = report.outcome else {
            panic!("expected a live process")
        };
        assert_eq!(live.pid, 321);
        assert!(t.join("debug/incremental/old-1").exists());
    }

    #[test]
    fn a_live_process_found_through_a_symlinked_path() {
        let (target, proc) = (target(), empty_proc());
        let links = tempfile::tempdir().unwrap();
        let via_link = links.path().join("target");
        symlink(target.path(), &via_link).unwrap();
        // The process reports the resolved path; the sweep was given the symlinked one.
        let resolved = fs::canonicalize(target.path()).unwrap();
        fs::create_dir_all(proc.path().join("8")).unwrap();
        symlink(resolved.join("release/server"), proc.path().join("8/exe")).unwrap();

        let report = sweep_target(&via_link, &options(true, proc.path())).unwrap();
        assert!(matches!(report.outcome, Outcome::Live(_)));
    }

    #[test]
    fn no_proc_means_no_sweep() {
        let target = target();
        let result = sweep_target(
            target.path(),
            &options(true, Path::new("/nonexistent/proc")),
        );
        assert!(result.is_err());
        assert!(target.path().join("debug/incremental/old-1").exists());
    }
}
