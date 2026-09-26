//! Benchmark: hearty on the `main` branch of Millennium Dawn, a large
//! HOI4 mod.
//!
//! Run it with `cargo bench --bench millennium_dawn`. The first run clones
//! the mod (shallow, blobless and sparse: only `common`, `events`, `history`,
//! `localisation` and the root files) into `target/tmp/Millennium-Dawn`, or
//! into `HEARTY_BENCH_DIR` if set; later runs fetch `main` into that clone
//! and reset it. The clone is made next to that directory and moved into it
//! once complete, so a clone that fails or is interrupted is cleared away
//! and made again by the next run. The clone is marked as the benchmark's,
//! and a git checkout without the mark is never reset, so pointing
//! `HEARTY_BENCH_DIR` at a working copy fails instead of discarding its
//! changes.
//!
//! Each scenario runs the release `hearty` binary once to warm up (the first
//! `--lint` also fills the HOI4 version cache in `target/tmp/hearty-cache`,
//! running steamcmd), then [`ITERATIONS`] timed times, restoring the files
//! after every run that changes them (and reading them all once, see
//! [`settle`]). The two combined scenarios then run
//! once more with `--timings` and `--flamegraph`, and their breakdown is
//! printed and their flamegraphs written to `target/tmp`.

#[path = "support/checkout.rs"]
mod checkout;
#[path = "support/git.rs"]
mod git;
#[path = "support/run.rs"]
mod run;

use checkout::{Result, Source, checkout, git};
use run::{
    HEARTY, ITERATIONS, SCENARIOS, SCRIPT_DIRS, count_files, dir_size, measure, mib, settle,
};
use std::path::PathBuf;
use std::process::ExitCode;

/// The mod's repository.
const REPO: &str = "https://github.com/MillenniumDawn/Millennium-Dawn";

/// The branch benchmarked.
const BRANCH: &str = "main";

/// The directories checked out besides the root files: everything hearty
/// reads.
const SPARSE_DIRS: &[&str] = &["common", "events", "history", "localisation"];

fn main() -> ExitCode {
    // `cargo bench` passes `--bench`. `cargo test --benches` (or
    // `--all-targets`) does not, and should not clone a large repository.
    if !std::env::args().any(|arg| arg == "--bench") {
        println!("millennium_dawn: skipped; run it with `cargo bench --bench millennium_dawn`");
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("millennium_dawn benchmark failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let dir = std::env::var_os("HEARTY_BENCH_DIR")
        .map_or_else(|| tmp.join("Millennium-Dawn"), PathBuf::from);
    let cache = tmp.join("hearty-cache");
    std::fs::create_dir_all(&cache)?;

    println!("== hearty benchmark: Millennium Dawn ({BRANCH}) ==");
    let source = Source {
        branch: BRANCH,
        sparse_dirs: SPARSE_DIRS,
        url: REPO,
    };
    let obtained = checkout(&dir, &source)?;
    settle(&dir);
    for required in std::iter::once("descriptor.mod").chain(SPARSE_DIRS.iter().copied()) {
        if !dir.join(required).exists() {
            return Err(format!("{} has no {required}", dir.display()).into());
        }
    }
    let commit = git(Some(&dir), &["log", "-1", "--format=%H (%cs) %s"])?;
    let (tree_files, tree_bytes) = dir_size(&dir, &|entry| entry.file_name() != ".git");
    let (_, git_bytes) = dir_size(&dir.join(".git"), &|_| true);
    let (scripts, script_bytes) = count_files(&dir, SCRIPT_DIRS, "txt");
    let (locs, loc_bytes) = count_files(&dir, &["localisation"], "yml");

    let header = [
        format!("checkout:  {} ({obtained})", dir.display()),
        format!("commit:    {}", commit.trim()),
        format!(
            "size:      {} in {tree_files} files checked out, {} of .git",
            mib(tree_bytes),
            mib(git_bytes)
        ),
        format!(
            "inputs:    {scripts} script files ({}) in {}, {locs} localisation files ({})",
            mib(script_bytes),
            SCRIPT_DIRS.join("/"),
            mib(loc_bytes)
        ),
        format!(
            "hearty:    {HEARTY}, {} hardware threads",
            std::thread::available_parallelism().map_or(0, usize::from)
        ),
    ];
    for line in &header {
        println!("{line}");
    }

    let mut results = Vec::new();
    for scenario in SCENARIOS {
        println!(
            "
-- hearty {} --",
            scenario.args.join(" ")
        );
        let measured = measure(scenario, &dir, &cache, &tmp, "")?;
        results.push((scenario, measured));
    }

    println!(
        "
== Results: Millennium Dawn {BRANCH}, {ITERATIONS} runs each after a warm-up =="
    );
    for line in &header {
        println!("{line}");
    }
    run::print_table(&results);
    run::print_details(&results);
    Ok(())
}
