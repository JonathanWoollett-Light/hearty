//! Running hearty's benchmark scenarios on a mod held in a git working copy:
//! timing runs, restoring the files after runs that change them, and
//! collecting the `--timings` breakdowns. Shared by the benchmarks.

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::git::{Result, git};

/// The directories holding the script files that `--format`, `--check` and
/// `--fix` handle.
pub const SCRIPT_DIRS: &[&str] = &["common", "events", "history"];

/// Timed runs per scenario, after one warm-up run.
pub const ITERATIONS: usize = 3;

/// The release binary, built by `cargo bench`.
pub const HEARTY: &str = env!("CARGO_BIN_EXE_hearty");

/// A way to run hearty.
pub struct Scenario {
    /// hearty's flags.
    pub args: &'static [&'static str],
    /// Whether it changes files, which are restored after each run.
    pub mutates: bool,
    /// Short name, used for file names and in results.
    pub name: &'static str,
    /// Whether to also make an instrumented run for `--timings` and
    /// `--flamegraph`.
    pub profile: bool,
}

/// What a scenario took.
pub struct Measured {
    /// The median run's output.
    pub median: Output,
    /// The instrumented run: its wall time, `--timings` report and
    /// flamegraph path.
    pub profile: Option<(Duration, String, PathBuf)>,
    /// Wall time of each timed run, sorted.
    pub walls: Vec<Duration>,
}

impl Measured {
    /// The fastest, median and slowest timed runs.
    pub fn min_median_max(&self) -> [Option<Duration>; 3] {
        [
            self.walls.first().copied(),
            self.walls.get(self.walls.len() / 2).copied(),
            self.walls.last().copied(),
        ]
    }
}

/// The scenarios every benchmark runs: the two combined runs a mod's CI
/// and a developer would make, then each action alone.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        args: &["--check", "--lint"],
        mutates: false,
        name: "check-lint",
        profile: true,
    },
    Scenario {
        args: &["--fix", "--format"],
        mutates: true,
        name: "fix-format",
        profile: true,
    },
    Scenario {
        args: &["--lint"],
        mutates: false,
        name: "lint",
        profile: false,
    },
    Scenario {
        args: &["--check"],
        mutates: false,
        name: "check",
        profile: false,
    },
    Scenario {
        args: &["--fix"],
        mutates: true,
        name: "fix",
        profile: false,
    },
    Scenario {
        args: &["--format"],
        mutates: true,
        name: "format",
        profile: false,
    },
];

/// Discards every change a run made to the working copy at `dir`.
pub fn restore(dir: &Path) -> Result<()> {
    git(Some(dir), &["reset", "--hard", "-q", "HEAD"])?;
    git(Some(dir), &["clean", "-fdq"])?;
    settle(dir);
    Ok(())
}

/// Reads every file under `dir` (but not `.git`) once. On Windows the first
/// read of a file after it is written waits for the antivirus to scan it,
/// which made the first `--check` after restoring a `--format` run spend
/// 23 s of its 41 s of active time reading (1.4 s on the next run). Without
/// this, the runs after a restore would time the scan rather than hearty.
pub fn settle(dir: &Path) {
    let files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.par_iter().for_each(|file| {
        // Only the read matters; a file that cannot be read is hearty's
        // problem to report.
        let _ = std::fs::read(file);
    });
}

/// Runs hearty on `dir` with `args`, returning its wall time and output.
/// Lint findings and formatting drift exit with 1, which is expected; any
/// other failure (a panic exits with 101) is an error.
pub fn hearty(dir: &Path, cache: &Path, args: &[&str]) -> Result<(Duration, Output)> {
    let start = Instant::now();
    let output = Command::new(HEARTY)
        .arg(dir)
        .args(args)
        .env("HEARTY_CACHE_DIR", cache)
        .output()?;
    let wall = start.elapsed();
    if matches!(output.status.code(), Some(0 | 1)) {
        return Ok((wall, output));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
    Err(format!(
        "hearty {} failed ({}):\n{}",
        args.join(" "),
        output.status,
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    )
    .into())
}

/// Runs a scenario on the working copy at `dir`: a warm-up run,
/// [`ITERATIONS`] timed runs and, if it is profiled, an instrumented run
/// whose flamegraph is written to `out` as `flamegraph-<prefix><name>.svg`.
pub fn measure(
    scenario: &Scenario,
    dir: &Path,
    cache: &Path,
    out: &Path,
    prefix: &str,
) -> Result<Measured> {
    let (warm_up, _) = hearty(dir, cache, scenario.args)?;
    println!("warm-up   {warm_up:>9.2?}");
    if scenario.mutates {
        restore(dir)?;
    }
    let mut runs = Vec::new();
    for iteration in 1..=ITERATIONS {
        let (wall, output) = hearty(dir, cache, scenario.args)?;
        println!("run {iteration}     {wall:>9.2?}");
        runs.push((wall, output));
        if scenario.mutates {
            restore(dir)?;
        }
    }
    runs.sort_by_key(|(wall, _)| *wall);
    let walls: Vec<Duration> = runs.iter().map(|(wall, _)| *wall).collect();
    let median = runs
        .into_iter()
        .nth(ITERATIONS / 2)
        .map(|(_, output)| output)
        .ok_or("no timed runs")?;

    let profile = if scenario.profile {
        let flamegraph = out.join(format!("flamegraph-{prefix}{}.svg", scenario.name));
        let flamegraph_arg = flamegraph
            .to_str()
            .ok_or("the flamegraph path is not UTF-8")?;
        let mut args = scenario.args.to_vec();
        args.extend(["--timings", "--flamegraph", flamegraph_arg]);
        let (wall, output) = hearty(dir, cache, &args)?;
        println!("--timings {wall:>9.2?}");
        if scenario.mutates {
            restore(dir)?;
        }
        Some((wall, timings(&output.stderr), flamegraph))
    } else {
        None
    };
    Ok(Measured {
        median,
        profile,
        walls,
    })
}

/// hearty's stdout without the per-file lines (which are indented), which
/// number in the thousands on a large mod: the headers and totals.
pub fn summarise(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("  "))
        .map(str::to_owned)
        .collect()
}

/// The `--timings` report from hearty's stderr, which starts with the
/// lint's diagnostics.
pub fn timings(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .skip_while(|line| !line.starts_with("Timings:"))
        .take_while(|line| !line.starts_with("Wrote flamegraph to "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Number and total size of the files under `root` that `keep` accepts,
/// not looking into the directories it rejects.
pub fn dir_size(root: &Path, keep: &dyn Fn(&walkdir::DirEntry) -> bool) -> (u64, u64) {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || keep(entry))
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .fold((0, 0), |(files, bytes), entry| {
            let size = entry.metadata().map_or(0, |metadata| metadata.len());
            (files + 1, bytes + size)
        })
}

/// Number and total size of the files with extension `extension` under the
/// `dirs` of `root`.
pub fn count_files(root: &Path, dirs: &[&str], extension: &str) -> (u64, u64) {
    dirs.iter()
        .map(|dir| {
            dir_size(&root.join(dir), &|entry| {
                entry.file_type().is_dir()
                    || entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
            })
        })
        .fold((0, 0), |(files, bytes), (dir_files, dir_bytes)| {
            (files + dir_files, bytes + dir_bytes)
        })
}

/// `bytes` in MiB, e.g. `12.3 MiB`.
pub fn mib(bytes: u64) -> String {
    let tenths = bytes * 10 / (1024 * 1024);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

/// Prints each scenario's min/median/max wall time and its instrumented
/// run's wall time as a table.
pub fn print_table(results: &[(&Scenario, Measured)]) {
    println!(
        "\n{:<24} {:>9} {:>9} {:>9} {:>14}",
        "hearty", "min", "median", "max", "--timings run"
    );
    for (scenario, measured) in results {
        let [min, median, max] = measured
            .min_median_max()
            .map(|wall| wall.map_or_else(String::new, |wall| format!("{wall:.2?}")));
        let profiled = measured
            .profile
            .as_ref()
            .map_or_else(String::new, |(wall, ..)| format!("{wall:.2?}"));
        println!(
            "{:<24} {min:>9} {median:>9} {max:>9} {profiled:>14}",
            scenario.args.join(" ")
        );
    }
}

/// Prints the median run's summary and the `--timings` report of each
/// scenario.
pub fn print_details(results: &[(&Scenario, Measured)]) {
    for (scenario, measured) in results {
        println!(
            "\n-- hearty {}: output of the median run --",
            scenario.args.join(" ")
        );
        for line in summarise(&measured.median.stdout) {
            println!("  {line}");
        }
        if let Some((_, timings, flamegraph)) = &measured.profile {
            println!("\n{timings}");
            println!("Flamegraph: {}", flamegraph.display());
        }
    }
}
