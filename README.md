# cargo-sweep

Cleans stale Rust build caches across a dev machine, without touching `release/` or anything a
running process uses.

Built and used on Linux. It should work on Mac and Windows, but it has not been tested there. See
[Platforms](#platforms).

Cargo never cleans up after itself. Every changed feature flag, dependency bump or toolchain update
leaves another copy of compiled artifacts in `target/debug`, and incremental caches grow without
bound. A busy week on one workspace can add 100 GB. `cargo clean` wipes everything, including the
cache for the project you're working on today. This tool removes only what nobody has used lately.

This is not [holmgr/cargo-sweep](https://github.com/holmgr/cargo-sweep), which is unmaintained.
It borrows that tool's recursive project discovery and age-based cleanup, but doesn't parse Cargo's
fingerprint files. It adds the two safety rules below.

## Usage

```sh
cargo-sweep                      # dry run over ~/Dev, 14 days
cargo-sweep -v                   # ...listing every file and directory it would remove
cargo-sweep --apply              # actually delete
cargo-sweep --root ~/src --days 30 --apply
```

With the binary on `PATH` (for example `cargo install --path .`), `cargo sweep …` works too.

Sample output:

```
Dry run: build output unused for 14 days under /home/jim/Dev (pass --apply to delete)

  agito/prototypes/agito_v3/target        would free 41.3 GiB (212 entries)
  bandy/bandy-rust/target                 skipped: live process 1449504 running /home/jim/Dev/bandy/bandy-rust/target/release/bandy-mcp
  rustlings/target                        nothing unused for 14 days

Would free 41.3 GiB in 212 entries across 1 of 3 projects, 1 skipped for live processes
```

The exit status is non-zero if anything could not be removed or the list of running processes
could not be read.

## What it does

1. **Finds projects.** Walks `--root` (default `~/Dev`) for `Cargo.toml` files, without following
   symlinks and without descending into `.git`, `node_modules`, or a project's own `target/`.
   A `target/` beside a `Cargo.toml` counts only if Cargo's marker files (`CACHEDIR.TAG` or
   `.rustc_info.json`) are in it.
2. **Plans.** Inside each target dir, it looks only at the throwaway build output: `debug/`,
   `llvm-cov-target/`, `flycheck<N>/`, `tmp/` and `sqlx-tmp/`. Anything there not used for `--days`
   days is stale.
   - A file was last used at the later of its access and modification times. An rlib compiled two
     months ago but linked yesterday is still in use. A directory counts only its modification
     time, because just listing it (which this tool does) updates its access time.
   - A directory whose entire contents are stale is removed as one entry.
   - Each crate's entry under `build/`, `.fingerprint/` and `incremental/` is removed whole or not
     at all.
3. **Checks for live processes.** Just before deleting anything in a target dir, it lists running
   processes (from `/proc` on Linux, from the OS through the `sysinfo` crate elsewhere). The
   target dir is skipped whole if any process's executable lives under it, or any
   command-line argument (including the value in `--flag=/path`) points into it. An executable
   that was replaced while running (`… (deleted)`) still counts.
4. **Deletes**, only with `--apply`.

Cargo rebuilds whatever is missing on the next build, so removing a stale entry costs compile time,
never correctness.

## Safety rules

- **`release/` is never touched**, at any depth, including `llvm-cov-target/release`. No flag
  overrides this. Release binaries get installed, symlinked, and referenced by editor and MCP
  configs.
- **A target dir in use by a running process is skipped entirely**, `debug/` included. A running
  binary survives its file being deleted, but whatever launches it next by path does not. If the
  process list can't be read, nothing is deleted. The tool ignores its own process.
- **Nothing is deleted without `--apply`.**
- `.cargo-lock` is never removed, so a running build keeps its lock.
- Symlinks are removed as links. Their targets are never followed.
- Before each delete, the path is checked again: it must sit strictly inside one of the target
  dir's cache directories, with no protected component.

## Platforms

**Linux** is where this was built and is used. Everything below is tested there.

**It should work on Mac and Windows, but it has not been tested.** It compiles and passes `clippy`
for both, and nothing more has been checked. If you try it, start with a dry run (no `--apply`)
and read the output before deleting anything. What differs:

- **Process check.** Linux reads `/proc`. Mac and Windows ask the OS through the
  [`sysinfo`](https://crates.io/crates/sysinfo) crate. If that returns nothing, the tool refuses to
  delete, as it does when `/proc` is unreadable. Processes owned by other users may not show
  their paths without admin rights on any platform, so run it as the user who runs your builds.
- **Path matching.** Mac and Windows paths are compared ignoring case, because their default
  filesystems are case-insensitive. This can only make the tool skip more, never delete more.
- **Last-used times.** Staleness depends on the filesystem updating access times:
  - Linux: mounted `relatime` (the default) or `strictatime`.
  - Mac: not checked. If `mount` shows `noatime` on the volume, access times are off.
  - Windows: NTFS often has access-time updates turned off. Check with
    `fsutil behavior query disablelastaccess`. If the output says "Disabled", they are off.

  Without access times, a file's last use is only its build time, so an old but still-used
  dependency will be removed and rebuilt. That costs compile time, not correctness.
- **Space reported.** On Windows the size shown is each file's apparent size, counted once per
  hard link, so the "would free" figure can be higher than what you get back.
- **Running binaries.** On Windows a running `.exe` is locked and can't be deleted. If one slips
  past the process check, deleting that file fails and is reported, and the file stays.
- **Default root.** `~/Dev` is your home directory's `Dev` folder on every platform
  (`%USERPROFILE%\Dev` on Windows). Pass `--root` to change it.

## Limits

- It does not clean `~/.cargo/registry` or `~/.cargo/git`, and does not honour a custom
  `CARGO_TARGET_DIR` or `build.target-dir`. It only looks at `target/` beside each `Cargo.toml`.
- On Linux and Mac, space is counted in allocated blocks. A hard-linked file counts only when all of its links are
  being removed, because removing fewer frees nothing.

## Development

```sh
cargo test
cargo clippy --all-targets
```

Tests build fake target dirs with controlled timestamps and feed the live-process check fixed
process lists. On Linux, more tests use a fake `/proc` of symlinks and cmdline files, and the real
`/proc`. On every platform, one test checks that the OS process table includes the test itself.

To check that the code still builds for the other platforms without a Mac or Windows machine:

```sh
rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin
cargo clippy --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings
```

That proves it compiles, not that it works. Running the tests there needs the real OS (for
example a GitHub Actions `macos-latest` or `windows-latest` runner).
