//! Detecting processes that run out of a target dir.
//!
//! A binary running from `target/` keeps working after its file is deleted (the process holds the
//! inode), but anything that starts it again by path does not: an MCP server launched from
//! `target/release/…` by an editor config, a test binary, a build script. So a target dir any
//! process touches this way is skipped whole.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How a process was found to be using a target dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The process's executable lives under it.
    Exe,
    /// An argument names a path under it.
    Cmdline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUse {
    pub pid: u32,
    pub via: Via,
    pub path: PathBuf,
}

/// Returns the first process under `proc_root` (normally `/proc`) whose executable or command line
/// points inside any of `dirs`, ignoring `own_pid`.
///
/// Fails only when `proc_root` itself cannot be listed, so a caller can refuse to delete rather than
/// assume nothing is running. Processes that exit mid-scan, or whose executable belongs to another
/// user, are passed over.
pub fn find_live_use(
    proc_root: &Path,
    dirs: &[PathBuf],
    own_pid: u32,
) -> io::Result<Option<LiveUse>> {
    let mut pids: Vec<u32> = fs::read_dir(proc_root)?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&pid| pid != own_pid)
        .collect();
    pids.sort_unstable();

    for pid in pids {
        let process = proc_root.join(pid.to_string());
        if let Ok(exe) = fs::read_link(process.join("exe"))
            && let Some(path) = exe_candidates(&exe).find(|p| inside(p, dirs))
        {
            return Ok(Some(LiveUse {
                pid,
                via: Via::Exe,
                path: path.to_path_buf(),
            }));
        }
        if let Ok(cmdline) = fs::read(process.join("cmdline")) {
            let args = cmdline.split(|&b| b == 0).filter(|arg| !arg.is_empty());
            if let Some(path) = args.flat_map(arg_candidates).find(|p| inside(p, dirs)) {
                return Ok(Some(LiveUse {
                    pid,
                    via: Via::Cmdline,
                    path: path.to_path_buf(),
                }));
            }
        }
    }
    Ok(None)
}

/// The executable link without the ` (deleted)` suffix the kernel appends once the file has been
/// replaced or removed (the process is still running that build), then as read, in case a path
/// really does end that way.
fn exe_candidates(exe: &Path) -> impl Iterator<Item = &Path> {
    let bytes = exe.as_os_str().as_bytes();
    let stripped = bytes
        .strip_suffix(b" (deleted)")
        .map(|b| Path::new(OsStr::from_bytes(b)));
    stripped.into_iter().chain(std::iter::once(exe))
}

/// An argument as a path, and the value half of a `key=path` argument such as
/// `-Cincremental=/…/target/debug/incremental`.
fn arg_candidates(arg: &[u8]) -> impl Iterator<Item = &Path> {
    let value = arg
        .iter()
        .position(|&b| b == b'=')
        .map(|at| Path::new(OsStr::from_bytes(&arg[at + 1..])));
    std::iter::once(Path::new(OsStr::from_bytes(arg))).chain(value)
}

/// Component-wise, so `/p/target2` is not inside `/p/target`.
fn inside(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| path.starts_with(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct FakeProc(tempfile::TempDir);

    impl FakeProc {
        fn new() -> Self {
            let proc = FakeProc(tempfile::tempdir().unwrap());
            // Things real /proc has that are not processes.
            fs::create_dir_all(proc.0.path().join("sys")).unwrap();
            fs::write(proc.0.path().join("uptime"), "1 1").unwrap();
            proc
        }

        fn process(&self, pid: u32, exe: Option<&str>, args: &[&str]) {
            let dir = self.0.path().join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            if let Some(exe) = exe {
                symlink(exe, dir.join("exe")).unwrap();
            }
            let mut cmdline = args.join("\0").into_bytes();
            cmdline.push(0);
            fs::write(dir.join("cmdline"), cmdline).unwrap();
        }

        fn find(&self, dir: &str, own_pid: u32) -> Option<LiveUse> {
            find_live_use(self.0.path(), &[PathBuf::from(dir)], own_pid).unwrap()
        }
    }

    const TARGET: &str = "/home/jim/Dev/bandy/bandy-rust/target";

    #[test]
    fn nothing_running_from_the_target() {
        let proc = FakeProc::new();
        proc.process(1, Some("/usr/lib/systemd/systemd"), &["/sbin/init"]);
        proc.process(2, Some("/home/jim/.cargo/bin/cargo"), &["cargo", "build"]);
        // A kernel thread: no exe link, empty cmdline.
        proc.process(3, None, &[]);
        assert_eq!(proc.find(TARGET, 0), None);
    }

    #[test]
    fn an_executable_under_the_target() {
        let proc = FakeProc::new();
        proc.process(40, Some("/usr/bin/bash"), &["bash"]);
        proc.process(
            41,
            Some(&format!("{TARGET}/release/bandy-mcp")),
            &["bandy-mcp"],
        );
        assert_eq!(
            proc.find(TARGET, 0),
            Some(LiveUse {
                pid: 41,
                via: Via::Exe,
                path: format!("{TARGET}/release/bandy-mcp").into()
            })
        );
    }

    #[test]
    fn an_executable_whose_file_was_replaced() {
        let proc = FakeProc::new();
        proc.process(
            7,
            Some(&format!("{TARGET}/release/bandy-mcp (deleted)")),
            &["bandy-mcp"],
        );
        let found = proc.find(TARGET, 0).unwrap();
        assert_eq!((found.pid, found.via), (7, Via::Exe));
        assert_eq!(
            found.path,
            PathBuf::from(format!("{TARGET}/release/bandy-mcp"))
        );
    }

    #[test]
    fn a_path_on_the_command_line() {
        let proc = FakeProc::new();
        proc.process(
            9,
            Some("/usr/bin/valgrind"),
            &["valgrind", &format!("{TARGET}/debug/deps/test-1")],
        );
        assert_eq!(
            proc.find(TARGET, 0).map(|u| (u.pid, u.via)),
            Some((9, Via::Cmdline))
        );
    }

    #[test]
    fn a_path_as_the_value_of_a_flag() {
        let proc = FakeProc::new();
        let incremental = format!("-Cincremental={TARGET}/debug/incremental");
        proc.process(
            11,
            Some("/home/jim/.rustup/toolchains/stable/bin/rustc"),
            &["rustc", &incremental],
        );
        let found = proc.find(TARGET, 0).unwrap();
        assert_eq!(
            found.path,
            PathBuf::from(format!("{TARGET}/debug/incremental"))
        );
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_inside() {
        let proc = FakeProc::new();
        proc.process(
            5,
            Some(&format!("{TARGET}2/release/x")),
            &[&format!("{TARGET}-old/x")],
        );
        assert_eq!(proc.find(TARGET, 0), None);
    }

    #[test]
    fn our_own_process_is_ignored() {
        let proc = FakeProc::new();
        proc.process(
            99,
            Some(&format!("{TARGET}/release/cargo-sweep")),
            &["cargo-sweep"],
        );
        assert_eq!(proc.find(TARGET, 99), None);
        assert!(proc.find(TARGET, 100).is_some());
    }

    #[test]
    fn any_of_several_names_for_the_target() {
        let proc = FakeProc::new();
        proc.process(12, Some("/real/path/target/debug/app"), &["app"]);
        let dirs = [PathBuf::from(TARGET), PathBuf::from("/real/path/target")];
        assert!(find_live_use(proc.0.path(), &dirs, 0).unwrap().is_some());
    }

    #[test]
    fn an_unreadable_proc_is_an_error() {
        assert!(
            find_live_use(Path::new("/nonexistent/proc"), &[PathBuf::from(TARGET)], 0).is_err()
        );
    }

    #[test]
    fn the_real_proc_sees_this_test_binary() {
        let exe = std::env::current_exe().unwrap();
        let found = find_live_use(Path::new("/proc"), std::slice::from_ref(&exe), 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.pid, found.via, found.path),
            (std::process::id(), Via::Exe, exe)
        );
    }
}
