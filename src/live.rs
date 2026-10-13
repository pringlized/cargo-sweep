//! Detecting processes that run out of a target dir.
//!
//! A binary running from `target/` can outlive its file (on Linux and macOS the process holds the
//! inode; on Windows the file is locked and the delete fails), but anything that starts it again by
//! path does not: an MCP server launched from `target/release/…` by an editor config, a test
//! binary, a build script. So a target dir any process touches this way is skipped whole.
//!
//! On Linux the process table is read from `/proc`. Everywhere else it comes from the OS through the
//! `sysinfo` crate.

use std::ffi::{OsStr, OsString};
use std::io;
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

/// One running process, as far as this tool cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// `None` when the OS would not say (a kernel thread, or another user's process).
    pub exe: Option<PathBuf>,
    pub args: Vec<OsString>,
}

/// Where the list of running processes comes from.
#[derive(Debug, Clone)]
pub enum Processes {
    /// A `/proc` directory: the real one, or a fake one in tests.
    #[cfg(target_os = "linux")]
    Proc(PathBuf),
    /// The OS process table, through `sysinfo`.
    System,
    /// A fixed list, for tests.
    Fixed(Vec<Process>),
}

impl Processes {
    /// The best source on this platform.
    pub fn native() -> Self {
        #[cfg(target_os = "linux")]
        return Processes::Proc(PathBuf::from("/proc"));
        #[cfg(not(target_os = "linux"))]
        return Processes::System;
    }
}

/// Returns the first process whose executable or command line points inside any of `dirs`, ignoring
/// `own_pid`.
///
/// Fails only when the process list itself cannot be read, so a caller can refuse to delete rather
/// than assume nothing is running. Processes that exit mid-scan, or whose details belong to another
/// user, are passed over.
pub fn find_live_use(
    source: &Processes,
    dirs: &[PathBuf],
    own_pid: u32,
) -> io::Result<Option<LiveUse>> {
    let read;
    let processes = match source {
        #[cfg(target_os = "linux")]
        Processes::Proc(root) => {
            read = read_proc(root)?;
            &read
        }
        Processes::System => {
            read = read_system()?;
            &read
        }
        Processes::Fixed(list) => list,
    };
    let dirs: Vec<PathBuf> = dirs.iter().map(|d| comparable(d)).collect();

    for process in processes.iter().filter(|p| p.pid != own_pid) {
        if let Some(exe) = &process.exe
            && let Some(path) = exe_candidates(exe).find(|p| inside(p, &dirs))
        {
            return Ok(Some(LiveUse {
                pid: process.pid,
                via: Via::Exe,
                path: path.to_path_buf(),
            }));
        }
        let args = process.args.iter().filter(|arg| !arg.is_empty());
        if let Some(path) = args
            .flat_map(|a| arg_candidates(a))
            .find(|p| inside(p, &dirs))
        {
            return Ok(Some(LiveUse {
                pid: process.pid,
                via: Via::Cmdline,
                path: path.to_path_buf(),
            }));
        }
    }
    Ok(None)
}

/// Every numbered directory under `proc_root`, sorted by pid.
#[cfg(target_os = "linux")]
fn read_proc(proc_root: &Path) -> io::Result<Vec<Process>> {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    let mut pids: Vec<u32> = fs::read_dir(proc_root)?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();

    Ok(pids
        .into_iter()
        .map(|pid| {
            let dir = proc_root.join(pid.to_string());
            let args = fs::read(dir.join("cmdline"))
                .map(|cmdline| {
                    cmdline
                        .split(|&b| b == 0)
                        .map(|arg| OsStr::from_bytes(arg).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            Process {
                pid,
                exe: fs::read_link(dir.join("exe")).ok(),
                args,
            }
        })
        .collect())
}

/// The OS process table, sorted by pid.
fn read_system() -> io::Result<Vec<Process>> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let mut processes: Vec<Process> = system
        .processes()
        .iter()
        .map(|(pid, process)| Process {
            pid: pid.as_u32(),
            exe: process.exe().map(Path::to_path_buf),
            args: process.cmd().to_vec(),
        })
        .collect();
    // sysinfo reports failure as an empty table. There is always at least this process, so an
    // empty table means the OS would not answer, and nothing should be deleted.
    if processes.is_empty() {
        return Err(io::Error::other("could not list running processes"));
    }
    processes.sort_unstable_by_key(|p| p.pid);
    Ok(processes)
}

/// The executable path without the ` (deleted)` suffix Linux appends once the file has been
/// replaced or removed (the process is still running that build), then as read, in case a path
/// really does end that way.
fn exe_candidates(exe: &Path) -> impl Iterator<Item = &Path> {
    let stripped = exe
        .as_os_str()
        .as_encoded_bytes()
        .strip_suffix(b" (deleted)")
        // SAFETY: the bytes come from an OsStr and end where the ASCII suffix " (deleted)" began,
        // which is a valid boundary to split encoded bytes at.
        .map(|b| Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(b) }));
    stripped.into_iter().chain(std::iter::once(exe))
}

/// An argument as a path, and the value half of a `key=path` argument such as
/// `-Cincremental=/…/target/debug/incremental`.
fn arg_candidates(arg: &OsStr) -> impl Iterator<Item = &Path> {
    let bytes = arg.as_encoded_bytes();
    let value = bytes
        .iter()
        .position(|&b| b == b'=')
        // SAFETY: the bytes come from an OsStr and start just after an ASCII '=', which is a valid
        // boundary to split encoded bytes at.
        .map(|at| Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(&bytes[at + 1..]) }));
    std::iter::once(Path::new(arg)).chain(value)
}

/// Component-wise, so `/p/target2` is not inside `/p/target`. `dirs` must already be
/// [`comparable`].
fn inside(path: &Path, dirs: &[PathBuf]) -> bool {
    let path = comparable(path);
    dirs.iter().any(|dir| path.starts_with(dir))
}

/// A path in the form two paths to the same place compare equal in, as far as can be told without
/// touching the disk.
///
/// macOS and Windows filesystems are case-insensitive by default, so case is folded there. That can
/// only ever make a different directory look like the target dir, which skips a sweep rather than
/// deleting something in use. Windows also drops the `\\?\` prefix `canonicalize` adds, which
/// process paths never carry, and treats `/` as `\`.
#[cfg(target_os = "macos")]
fn comparable(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(windows)]
fn comparable(path: &Path) -> PathBuf {
    let path = path.to_string_lossy().replace('/', "\\");
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
    };
    PathBuf::from(path.to_lowercase())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn comparable(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, exe: Option<&Path>, args: &[&OsStr]) -> Process {
        Process {
            pid,
            exe: exe.map(Path::to_path_buf),
            args: args.iter().map(|a| a.to_os_string()).collect(),
        }
    }

    /// A target dir path in this platform's shape. It never has to exist.
    fn target() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\jim\Dev\bandy\bandy-rust\target")
        } else {
            PathBuf::from("/home/jim/Dev/bandy/bandy-rust/target")
        }
    }

    fn find(processes: Vec<Process>, own_pid: u32) -> Option<LiveUse> {
        find_live_use(&Processes::Fixed(processes), &[target()], own_pid).unwrap()
    }

    #[test]
    fn nothing_running_from_the_target() {
        let processes = vec![
            process(
                1,
                Some(Path::new("/usr/lib/systemd/systemd")),
                &["/sbin/init".as_ref()],
            ),
            process(
                2,
                Some(Path::new("/home/jim/.cargo/bin/cargo")),
                &["cargo".as_ref(), "build".as_ref()],
            ),
            // A kernel thread: no executable, no arguments.
            process(3, None, &[]),
        ];
        assert_eq!(find(processes, 0), None);
    }

    #[test]
    fn an_executable_under_the_target() {
        let exe = target().join("release").join("bandy-mcp");
        let processes = vec![
            process(40, Some(Path::new("/usr/bin/bash")), &["bash".as_ref()]),
            process(41, Some(&exe), &["bandy-mcp".as_ref()]),
        ];
        assert_eq!(
            find(processes, 0),
            Some(LiveUse {
                pid: 41,
                via: Via::Exe,
                path: exe
            })
        );
    }

    #[test]
    fn an_executable_whose_file_was_replaced() {
        let exe = target().join("release").join("bandy-mcp");
        let mut deleted = exe.clone().into_os_string();
        deleted.push(" (deleted)");
        let found = find(vec![process(7, Some(Path::new(&deleted)), &[])], 0).unwrap();
        assert_eq!((found.pid, found.via, found.path), (7, Via::Exe, exe));
    }

    #[test]
    fn a_path_on_the_command_line() {
        let test_binary = target().join("debug").join("deps").join("test-1");
        let processes = vec![process(
            9,
            Some(Path::new("/usr/bin/valgrind")),
            &["valgrind".as_ref(), test_binary.as_os_str()],
        )];
        assert_eq!(
            find(processes, 0).map(|u| (u.pid, u.via)),
            Some((9, Via::Cmdline))
        );
    }

    #[test]
    fn a_path_as_the_value_of_a_flag() {
        let incremental = target().join("debug").join("incremental");
        let mut flag = OsString::from("-Cincremental=");
        flag.push(&incremental);
        let processes = vec![process(
            11,
            Some(Path::new("/home/jim/.rustup/toolchains/stable/bin/rustc")),
            &["rustc".as_ref(), &flag],
        )];
        assert_eq!(find(processes, 0).unwrap().path, incremental);
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_inside() {
        let mut sibling = target().into_os_string();
        sibling.push("2");
        let mut old = target().into_os_string();
        old.push("-old");
        let processes = vec![process(
            5,
            Some(&Path::new(&sibling).join("x")),
            &[Path::new(&old).join("x").as_os_str()],
        )];
        assert_eq!(find(processes, 0), None);
    }

    #[test]
    fn our_own_process_is_ignored() {
        let exe = target().join("release").join("cargo-sweep");
        let processes = || vec![process(99, Some(&exe), &[])];
        assert_eq!(find(processes(), 99), None);
        assert!(find(processes(), 100).is_some());
    }

    #[test]
    fn any_of_several_names_for_the_target() {
        let real = Path::new("/real/path/target");
        let processes = Processes::Fixed(vec![process(12, Some(&real.join("debug/app")), &[])]);
        let dirs = [target(), real.to_path_buf()];
        assert!(find_live_use(&processes, &dirs, 0).unwrap().is_some());
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn case_is_folded_where_the_filesystem_does() {
        let mut upper = target().into_os_string().to_string_lossy().to_uppercase();
        upper.push_str(if cfg!(windows) {
            r"\debug\app"
        } else {
            "/debug/app"
        });
        assert!(find(vec![process(3, Some(Path::new(&upper)), &[])], 0).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_target_matches_a_plain_process_path() {
        let processes = Processes::Fixed(vec![process(
            4,
            Some(Path::new(r"C:\Users\jim\Dev\p\target\debug\app.exe")),
            &[],
        )]);
        let dirs = [PathBuf::from(r"\\?\C:\Users\jim\Dev\p\target")];
        assert!(find_live_use(&processes, &dirs, 0).unwrap().is_some());
    }

    #[test]
    fn the_system_table_sees_this_test_binary() {
        let exe = std::env::current_exe().unwrap();
        let found = find_live_use(&Processes::System, std::slice::from_ref(&exe), 0)
            .unwrap()
            .unwrap();
        assert_eq!((found.pid, found.via), (std::process::id(), Via::Exe));
    }

    #[test]
    fn the_native_source_sees_this_test_binary() {
        let exe = std::env::current_exe().unwrap();
        let found = find_live_use(&Processes::native(), std::slice::from_ref(&exe), 0)
            .unwrap()
            .unwrap();
        assert_eq!(found.pid, std::process::id());
    }

    #[cfg(target_os = "linux")]
    mod proc {
        use super::super::*;
        use std::fs;
        use std::os::unix::fs::symlink;

        /// A fake `/proc`, with things the real one has that are not processes.
        fn fake_proc() -> tempfile::TempDir {
            let proc = tempfile::tempdir().unwrap();
            fs::create_dir_all(proc.path().join("sys")).unwrap();
            fs::write(proc.path().join("uptime"), "1 1").unwrap();
            proc
        }

        #[test]
        fn processes_are_read_from_exe_links_and_cmdline_files() {
            let proc = fake_proc();
            let dir = proc.path().join("41");
            fs::create_dir_all(&dir).unwrap();
            symlink("/t/release/bandy-mcp", dir.join("exe")).unwrap();
            fs::write(dir.join("cmdline"), b"bandy-mcp\0--port\08734\0").unwrap();
            // A kernel thread: no exe link, empty cmdline.
            fs::create_dir_all(proc.path().join("3")).unwrap();
            fs::write(proc.path().join("3/cmdline"), b"").unwrap();

            let processes = read_proc(proc.path()).unwrap();
            assert_eq!(processes.len(), 2);
            assert_eq!((processes[0].pid, &processes[0].exe), (3, &None));
            assert_eq!(
                processes[1].exe.as_deref(),
                Some(Path::new("/t/release/bandy-mcp"))
            );
            assert_eq!(processes[1].args[..3], ["bandy-mcp", "--port", "8734"]);
        }

        #[test]
        fn an_unreadable_proc_is_an_error() {
            let source = Processes::Proc(PathBuf::from("/nonexistent/proc"));
            assert!(find_live_use(&source, &[PathBuf::from("/t")], 0).is_err());
        }

        #[test]
        fn the_real_proc_sees_this_test_binary() {
            let exe = std::env::current_exe().unwrap();
            let source = Processes::Proc(PathBuf::from("/proc"));
            let found = find_live_use(&source, std::slice::from_ref(&exe), 0)
                .unwrap()
                .unwrap();
            assert_eq!(
                (found.pid, found.via, found.path),
                (std::process::id(), Via::Exe, exe)
            );
        }
    }
}
