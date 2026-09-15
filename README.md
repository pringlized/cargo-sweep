# cargo-sweep

Cleans stale Rust build caches across a Linux dev machine, without touching `release/` or
anything a running process uses.

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

The exit status is non-zero if anything could not be removed or `/proc` could not be read.

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
3. **Checks for live processes.** Just before deleting anything in a target dir, it scans `/proc`.
   The target dir is skipped whole if any process's executable lives under it, or any
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
  binary survives its file being deleted, but whatever launches it next by path does not. If
  `/proc` can't be read, nothing is deleted. The tool ignores its own process.
- **Nothing is deleted without `--apply`.**
- `.cargo-lock` is never removed, so a running build keeps its lock.
- Symlinks are removed as links. Their targets are never followed.
- Before each delete, the path is checked again: it must sit strictly inside one of the target
  dir's cache directories, with no protected component.

## Requirements and limits

- Linux only. It needs `/proc` and a filesystem mounted `relatime` (the default) or `strictatime`.
  On `noatime` mounts, a file's last use is only its build time, so an old but still-used
  dependency will be removed and rebuilt.
- It does not clean `~/.cargo/registry` or `~/.cargo/git`, and does not honour a custom
  `CARGO_TARGET_DIR` or `build.target-dir`. It only looks at `target/` beside each `Cargo.toml`.
- Space is counted in allocated blocks. A hard-linked file counts only when all of its links are
  being removed, because removing fewer frees nothing.

## Development

```sh
cargo test
cargo clippy --all-targets
```

Tests build fake target dirs with controlled timestamps and a fake `/proc` of symlinks and cmdline
files. One test checks the real `/proc` for its own process.
