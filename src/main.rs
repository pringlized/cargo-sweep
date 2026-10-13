use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::Parser;

use cargo_sweep::discover;
use cargo_sweep::live::{Processes, Via};
use cargo_sweep::sweep::{self, Options, Outcome};

/// Clean stale Rust build caches under a directory.
///
/// Finds every Cargo project under --root and removes build output in each target dir's debug/,
/// llvm-cov-target/, flycheck<N>/, tmp/ and sqlx-tmp/ that nothing has used for --days days.
/// release/ is never touched, and a target dir a running process executes from, or names on its
/// command line, is skipped whole. Without --apply this is a dry run.
#[derive(Parser)]
#[command(name = "cargo-sweep", version)]
struct Cli {
    /// Directory to search for Cargo projects [default: ~/Dev]
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Remove only build output unused for this many days
    #[arg(long, value_name = "N", default_value_t = 14)]
    days: u32,

    /// Delete. Without this, only report what would be removed
    #[arg(long)]
    apply: bool,

    /// List every file and directory that is (or would be) removed, and every read warning
    #[arg(short, long)]
    verbose: bool,
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn main() -> ExitCode {
    // `cargo sweep …` runs `cargo-sweep sweep …`.
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "sweep") {
        args.remove(1);
    }
    match run(Cli::parse_from(args)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("cargo-sweep: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether everything that should have happened did.
fn run(cli: Cli) -> Result<bool> {
    let root = match cli.root {
        Some(root) => root,
        None => std::env::home_dir()
            .context("no home directory found; pass --root")?
            .join("Dev"),
    };
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let options = Options {
        cutoff: SystemTime::now() - DAY * cli.days,
        apply: cli.apply,
        processes: Processes::native(),
        own_pid: std::process::id(),
    };

    if cli.apply {
        println!(
            "Removing build output unused for {} days under {}\n",
            cli.days,
            root.display()
        );
    } else {
        println!(
            "Dry run: build output unused for {} days under {} (pass --apply to delete)\n",
            cli.days,
            root.display()
        );
    }

    let found = discover::find_targets(&root);
    let mut ok = true;
    let (mut total, mut units, mut cleaned, mut live, mut warnings) = (0u64, 0, 0, 0, 0);
    let verb = if cli.apply { "freed" } else { "would free" };
    // Projects are shown relative to the root, entries relative to their target dir.
    let relative = |path: &Path, base: &Path| {
        path.strip_prefix(base)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let width = found
        .targets
        .iter()
        .map(|t| relative(t, &root).len())
        .max()
        .unwrap_or(0);

    for target in &found.targets {
        let label = format!("{:width$}", relative(target, &root));
        let report = match sweep::sweep_target(target, &options) {
            Ok(report) => report,
            Err(err) => {
                println!("  {label}  error: {err}");
                ok = false;
                continue;
            }
        };
        match &report.outcome {
            Outcome::Live(process) => {
                live += 1;
                let via = match process.via {
                    Via::Exe => "running",
                    Via::Cmdline => "with an argument in",
                };
                println!(
                    "  {label}  skipped: live process {} {via} {}",
                    process.pid,
                    process.path.display()
                );
            }
            Outcome::Stale { units: stale, .. } if stale.is_empty() => {
                println!("  {label}  nothing unused for {} days", cli.days);
            }
            Outcome::Stale {
                units: stale,
                bytes,
                failures,
            } => {
                cleaned += 1;
                units += stale.len();
                total += bytes;
                println!(
                    "  {label}  {verb} {} ({})",
                    human_bytes(*bytes),
                    count(stale.len(), "entry", "entries")
                );
                if cli.verbose {
                    for unit in stale {
                        println!(
                            "      {} (unused {} days)",
                            relative(&unit.path, target),
                            days_since(unit.last_used)
                        );
                    }
                }
                for (path, err) in failures {
                    println!("      failed to remove {}: {err}", relative(path, target));
                    ok = false;
                }
            }
        }
        warnings += report.warnings.len();
        if cli.verbose {
            for warning in &report.warnings {
                println!("      warning: {warning}");
            }
        }
    }
    warnings += found.warnings.len();
    if cli.verbose {
        for warning in &found.warnings {
            println!("  warning: {warning}");
        }
    }

    let mut summary = format!(
        "\n{} {} in {} across {cleaned} of {}",
        capitalise(verb),
        human_bytes(total),
        count(units, "entry", "entries"),
        count(found.targets.len(), "project", "projects"),
    );
    if live > 0 {
        summary += &format!(", {live} skipped for live processes");
    }
    if warnings > 0 && !cli.verbose {
        summary += &format!(
            ", {} (see --verbose)",
            count(warnings, "read warning", "read warnings")
        );
    }
    println!("{summary}");
    Ok(ok)
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn days_since(time: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs()
        / DAY.as_secs()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(108 * 1024 * 1024 * 1024), "108.0 GiB");
    }

    #[test]
    fn cli_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
