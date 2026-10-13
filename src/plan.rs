//! Deciding what inside a target dir is stale, and how much deleting it frees.
//!
//! "Last used" is the later of a file's access and modification times. The filesystem must update
//! access times for them to mean anything (on Linux, mounted `relatime`, the default, or
//! `strictatime`): then an rlib a build still links against, or a fingerprint Cargo still reads,
//! stays fresh even if it was compiled months ago. Directories only count their modification time, because merely listing
//! a directory (as this tool does) updates its access time.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

/// Subdirectories of a target dir that hold throwaway build output, along with `flycheck<N>`
/// (rust-analyzer's check dirs). `release/` is absent on purpose.
const CACHE_DIRS: &[&str] = &["debug", "llvm-cov-target", "tmp", "sqlx-tmp"];

/// Names never removed, and never removed around: a directory containing one is not removed whole.
/// `release` holds binaries that may be installed or running; `.cargo-lock` is the lock a running
/// Cargo holds, and removing it would let a second build in beside the first.
const PROTECTED: &[&str] = &["release", ".cargo-lock"];

/// Each child of these directories is one compilation unit (a crate's build script output,
/// fingerprint, or incremental session history). A unit is removed whole or not at all.
const ATOMIC_PARENTS: &[&str] = &["build", ".fingerprint", "incremental"];

/// A file or directory to remove, with the last time anything in it was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub path: PathBuf,
    pub last_used: SystemTime,
}

/// The stale units of one target dir, plus anything that could not be read while looking.
#[derive(Debug, Default)]
pub struct Plan {
    pub units: Vec<Unit>,
    pub warnings: Vec<String>,
}

pub fn is_cache_dir(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    CACHE_DIRS.contains(&name)
        || name
            .strip_prefix("flycheck")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_protected(name: &OsStr) -> bool {
    PROTECTED.iter().any(|p| name == OsStr::new(p))
}

/// Lists everything in `target`'s cache dirs not used since `cutoff`.
///
/// A directory whose whole contents are stale is listed once, as itself, rather than file by file.
/// A cache dir itself is never listed; its stale contents are.
pub fn plan_target(target: &Path, cutoff: SystemTime) -> io::Result<Plan> {
    let mut plan = Plan::default();
    let mut cache_dirs: Vec<PathBuf> = fs::read_dir(target)?
        .filter_map(Result::ok)
        .filter(|e| is_cache_dir(&e.file_name()) && e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    cache_dirs.sort();
    for dir in cache_dirs {
        let scan = scan_dir(&dir, cutoff, &mut plan.warnings);
        plan.units.extend(scan.units);
    }
    Ok(plan)
}

/// What scanning one directory found.
struct Scan {
    /// The latest use of the directory or anything under it.
    newest: SystemTime,
    /// Whether the directory could be removed whole: nothing protected or unreadable inside.
    removable: bool,
    /// Stale descendants, for when the directory as a whole is not stale.
    units: Vec<Unit>,
}

fn scan_dir(dir: &Path, cutoff: SystemTime, warnings: &mut Vec<String>) -> Scan {
    let unreadable = |warnings: &mut Vec<String>, err: io::Error| {
        warnings.push(format!("cannot read {}: {err}", dir.display()));
        Scan {
            newest: SystemTime::now(),
            removable: false,
            units: Vec::new(),
        }
    };
    let (mut newest, entries) = match fs::symlink_metadata(dir)
        .and_then(|m| m.modified())
        .and_then(|mtime| Ok((mtime, fs::read_dir(dir)?)))
    {
        Ok(read) => read,
        Err(err) => return unreadable(warnings, err),
    };
    let atomic = dir
        .file_name()
        .is_some_and(|n| ATOMIC_PARENTS.iter().any(|a| n == OsStr::new(a)));
    let mut removable = true;
    let mut units = Vec::new();

    for entry in entries {
        let (entry, meta) = match entry.and_then(|e| Ok((e.metadata()?, e))) {
            Ok((meta, entry)) => (entry, meta),
            Err(err) => {
                warnings.push(format!("cannot read an entry of {}: {err}", dir.display()));
                removable = false;
                continue;
            }
        };
        if is_protected(&entry.file_name()) {
            removable = false;
            continue;
        }
        let path = entry.path();
        let child = if meta.is_dir() {
            scan_dir(&path, cutoff, warnings)
        } else {
            match last_used(&meta) {
                Ok(used) => Scan {
                    newest: used,
                    removable: true,
                    units: Vec::new(),
                },
                Err(err) => {
                    warnings.push(format!("cannot read times of {}: {err}", path.display()));
                    Scan {
                        newest: SystemTime::now(),
                        removable: false,
                        units: Vec::new(),
                    }
                }
            }
        };

        newest = newest.max(child.newest);
        removable &= child.removable;
        if child.removable && child.newest < cutoff {
            units.push(Unit {
                path,
                last_used: child.newest,
            });
        } else if !atomic {
            units.extend(child.units);
        }
    }
    Scan {
        newest,
        removable,
        units,
    }
}

fn last_used(meta: &Metadata) -> io::Result<SystemTime> {
    Ok(meta.accessed()?.max(meta.modified()?))
}

/// The last line of defence before a delete: `unit` must sit strictly inside one of `target`'s
/// cache dirs, through plain path components, none of them protected.
pub fn is_deletable(target: &Path, unit: &Path) -> bool {
    let Ok(relative) = unit.strip_prefix(target) else {
        return false;
    };
    let mut components = relative.components();
    let Some(Component::Normal(cache_dir)) = components.next() else {
        return false;
    };
    let mut depth = 0;
    for component in components {
        match component {
            Component::Normal(name) if !is_protected(name) => depth += 1,
            _ => return false,
        }
    }
    is_cache_dir(cache_dir) && depth > 0
}

/// Disk space removing `units` would give back.
///
/// Counts allocated blocks, not apparent sizes. Cargo hard-links binaries out of `deps/` and
/// incremental sessions hard-link their predecessors' files, so a file is only counted when every
/// one of its links is among the units; otherwise removing it frees nothing.
///
/// On Windows, stable Rust exposes neither allocated size nor hard-link identity, so each file
/// counts at its apparent size, once per link. The figure can come out high there.
pub fn reclaimable_bytes(units: &[Unit]) -> u64 {
    // (device, inode) -> (links seen among the units, total links, bytes)
    let mut linked: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut other_bytes = 0;
    for unit in units {
        for entry in WalkDir::new(&unit.path)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            let Ok(meta) = entry.metadata() else { continue };
            let bytes = allocated_bytes(&meta);
            match link_identity(&meta) {
                Some((id, links)) if !meta.is_dir() => {
                    linked.entry(id).or_insert((0, links, bytes)).0 += 1;
                }
                _ => other_bytes += bytes,
            }
        }
    }
    let linked_bytes: u64 = linked
        .values()
        .filter(|(seen, links, _)| seen >= links)
        .map(|(_, _, bytes)| bytes)
        .sum();
    other_bytes + linked_bytes
}

#[cfg(unix)]
fn allocated_bytes(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_bytes(meta: &Metadata) -> u64 {
    meta.len()
}

/// The file's (device, inode) and its number of links, where the platform tells.
#[cfg(unix)]
fn link_identity(meta: &Metadata) -> Option<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    Some(((meta.dev(), meta.ino()), meta.nlink()))
}

#[cfg(not(unix))]
fn link_identity(_: &Metadata) -> Option<((u64, u64), u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{age_dirs, days_ago, file, set_times};

    fn paths(plan: &Plan, target: &Path) -> Vec<String> {
        let mut paths: Vec<String> = plan
            .units
            .iter()
            .map(|u| u.path.strip_prefix(target).unwrap().display().to_string())
            .collect();
        paths.sort();
        paths
    }

    fn cutoff() -> SystemTime {
        days_ago(14)
    }

    #[test]
    fn removes_stale_files_and_keeps_recent_ones() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("debug/deps/libold-1.rlib"), 30, 30);
        file(&t.join("debug/deps/libnew-2.rlib"), 1, 1);
        age_dirs(&t.join("debug"), 30);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["debug/deps/libold-1.rlib"]);
    }

    #[test]
    fn a_recent_read_keeps_an_old_build_fresh() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        // Compiled 60 days ago, linked against yesterday.
        file(&t.join("debug/deps/libserde-1.rlib"), 1, 60);
        age_dirs(&t.join("debug"), 60);

        assert!(plan_target(t, cutoff()).unwrap().units.is_empty());
    }

    #[test]
    fn listing_a_directory_does_not_make_it_fresh() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("debug/deps/libold-1.rlib"), 30, 30);
        age_dirs(&t.join("debug"), 30);
        // Directory access time says today, as it will after any scan.
        set_times(&t.join("debug/deps"), 0, 30);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["debug/deps"]);
    }

    #[test]
    fn a_wholly_stale_directory_is_one_unit() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("debug/build/ring-abc/out/a.o"), 30, 30);
        file(&t.join("debug/build/ring-abc/output"), 30, 30);
        file(&t.join("debug/deps/libkeep-1.rlib"), 1, 1);
        age_dirs(&t.join("debug"), 30);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["debug/build"]);
        assert!(plan.units[0].last_used < days_ago(29));
    }

    #[test]
    fn a_compilation_unit_with_anything_recent_is_kept_whole() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(
            &t.join("debug/incremental/agito-1/s-old/dep-graph.bin"),
            30,
            30,
        );
        file(
            &t.join("debug/incremental/agito-1/s-new/dep-graph.bin"),
            0,
            0,
        );
        file(
            &t.join("debug/incremental/stale-2/s-old/dep-graph.bin"),
            30,
            30,
        );
        file(&t.join("debug/.fingerprint/serde-3/lib-serde"), 30, 30);
        file(&t.join("debug/.fingerprint/serde-3/lib-serde.json"), 1, 1);
        age_dirs(&t.join("debug"), 30);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["debug/incremental/stale-2"]);
    }

    #[test]
    fn release_is_never_listed_nor_removed_around() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("release/agito-server"), 90, 90);
        file(&t.join("llvm-cov-target/release/deps/libx-1.rlib"), 90, 90);
        file(&t.join("llvm-cov-target/debug/deps/libx-1.rlib"), 90, 90);
        age_dirs(t, 90);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["llvm-cov-target/debug"]);
    }

    #[test]
    fn the_cargo_lock_is_left_alone() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&t.join("debug/.cargo-lock"), 90, 90);
        file(&t.join("debug/agito-server"), 90, 90);
        age_dirs(t, 90);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(paths(&plan, t), ["debug/agito-server"]);
    }

    #[test]
    fn only_cache_dirs_are_considered() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        for dir in [
            "debug",
            "tmp",
            "sqlx-tmp",
            "flycheck0",
            "flycheck12",
            "doc",
            "package",
            "flycheck",
            "x86_64-unknown-linux-gnu",
        ] {
            file(&t.join(dir).join("stale"), 90, 90);
        }
        file(&t.join(".rustc_info.json"), 90, 90);
        age_dirs(t, 90);

        let plan = plan_target(t, cutoff()).unwrap();
        assert_eq!(
            paths(&plan, t),
            [
                "debug/stale",
                "flycheck0/stale",
                "flycheck12/stale",
                "sqlx-tmp/stale",
                "tmp/stale"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_units_not_followed() {
        let target = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let t = target.path();
        file(&elsewhere.path().join("precious/data"), 0, 0);
        fs::create_dir_all(t.join("tmp")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("precious"), t.join("tmp/link")).unwrap();

        // The link's own times are fresh, so nothing is stale yet; the point is that the scan did
        // not descend into the target of the link.
        let plan = plan_target(t, SystemTime::now() + crate::testutil::DAY).unwrap();
        assert_eq!(paths(&plan, t), ["tmp/link"]);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn deletable_paths_are_inside_a_cache_dir_and_unprotected() {
        let t = Path::new("/home/jim/Dev/p/target");
        assert!(is_deletable(t, &t.join("debug/deps/libx.rlib")));
        assert!(is_deletable(t, &t.join("flycheck3/debug")));
        assert!(!is_deletable(t, &t.join("debug")), "a cache dir itself");
        assert!(!is_deletable(t, t), "the target dir itself");
        assert!(!is_deletable(t, &t.join("release/agito-server")));
        assert!(!is_deletable(t, &t.join("llvm-cov-target/release/x")));
        assert!(!is_deletable(t, &t.join("debug/.cargo-lock")));
        assert!(!is_deletable(t, &t.join("doc/index.html")));
        assert!(!is_deletable(t, &t.join("debug/../release/x")));
        assert!(!is_deletable(t, Path::new("/home/jim/Dev/p/src/main.rs")));
        assert!(!is_deletable(
            t,
            Path::new("/home/jim/Dev/p/target2/debug/x")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_only_count_when_every_link_goes() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("deps")).unwrap();
        fs::write(d.join("deps/bin-1"), vec![7u8; 64 * 1024]).unwrap();
        fs::hard_link(d.join("deps/bin-1"), d.join("bin")).unwrap();
        let unit = |p: &str| Unit {
            path: d.join(p),
            last_used: SystemTime::UNIX_EPOCH,
        };
        let one_file = fs::metadata(d.join("bin")).unwrap().blocks() * 512;
        assert!(one_file >= 64 * 1024);

        assert_eq!(
            reclaimable_bytes(&[unit("deps/bin-1")]),
            0,
            "the other link keeps it alive"
        );
        assert_eq!(
            reclaimable_bytes(&[unit("deps/bin-1"), unit("bin")]),
            one_file,
            "counted once"
        );
    }
}
