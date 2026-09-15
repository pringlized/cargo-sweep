//! Finding Cargo target dirs under a root.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// Directories never worth descending into while looking for projects.
const SKIP_DIRS: &[&str] = &[".git", "node_modules"];

/// What discovery found: the target dirs, plus paths it could not read.
#[derive(Debug, Default)]
pub struct Found {
    pub targets: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Walks `root` for `Cargo.toml` files and returns each project's `target/` dir, sorted by path.
///
/// Symlinks are not followed. A project's own `target/` is not descended into, but the rest of the
/// project is, so nested independent projects are still found. A `target/` only counts when Cargo
/// left its marker files in it, so an unrelated `target/` beside a `Cargo.toml` is left alone.
pub fn find_targets(root: &Path) -> Found {
    let mut found = Found::default();
    let walk = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !skip(e));
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                found.warnings.push(format!("cannot read {err}"));
                continue;
            }
        };
        if entry.file_type().is_file() && entry.file_name() == "Cargo.toml" {
            let target = entry.path().with_file_name("target");
            if is_cargo_target(&target) {
                found.targets.push(target);
            }
        }
    }
    found
}

fn skip(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name();
    SKIP_DIRS.iter().any(|skip| name == OsStr::new(skip))
        || (name == "target" && entry.path().with_file_name("Cargo.toml").is_file())
}

/// A real directory (not a symlink) carrying the files Cargo writes into every target dir.
fn is_cargo_target(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && (path.join("CACHEDIR.TAG").is_file() || path.join(".rustc_info.json").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(dir: &Path) {
        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.join("target/CACHEDIR.TAG"), "").unwrap();
    }

    #[test]
    fn finds_projects_including_nested_ones() {
        let root = tempfile::tempdir().unwrap();
        project(&root.path().join("a"));
        project(&root.path().join("b/prototypes/inner"));
        // A workspace member without its own target dir.
        fs::create_dir_all(root.path().join("a/crates/member")).unwrap();
        fs::write(root.path().join("a/crates/member/Cargo.toml"), "").unwrap();

        let found = find_targets(root.path());
        assert_eq!(
            found.targets,
            vec![
                root.path().join("a/target"),
                root.path().join("b/prototypes/inner/target")
            ]
        );
    }

    #[test]
    fn does_not_descend_into_target_git_or_node_modules() {
        let root = tempfile::tempdir().unwrap();
        project(&root.path().join("a"));
        // Cargo vendors packaged crates under target/package; those are not projects of their own.
        project(&root.path().join("a/target/package/dep-1.0.0"));
        project(&root.path().join("web/node_modules/some-crate"));
        project(&root.path().join("repo/.git/modules/sub"));

        let found = find_targets(root.path());
        assert_eq!(found.targets, vec![root.path().join("a/target")]);
    }

    #[test]
    fn ignores_target_dirs_cargo_did_not_make() {
        let root = tempfile::tempdir().unwrap();
        // No Cargo marker files.
        fs::create_dir_all(root.path().join("java/target/debug")).unwrap();
        fs::write(root.path().join("java/Cargo.toml"), "").unwrap();
        // Marker files but no Cargo.toml beside it.
        fs::create_dir_all(root.path().join("orphan/target")).unwrap();
        fs::write(root.path().join("orphan/target/.rustc_info.json"), "{}").unwrap();

        assert!(find_targets(root.path()).targets.is_empty());
    }

    #[test]
    fn ignores_a_symlinked_target() {
        let root = tempfile::tempdir().unwrap();
        project(&root.path().join("real"));
        fs::create_dir_all(root.path().join("linked")).unwrap();
        fs::write(root.path().join("linked/Cargo.toml"), "").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("real/target"),
            root.path().join("linked/target"),
        )
        .unwrap();

        assert_eq!(
            find_targets(root.path()).targets,
            vec![root.path().join("real/target")]
        );
    }
}
