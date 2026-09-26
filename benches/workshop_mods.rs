//! Benchmark: hearty on five large Steam Workshop mods: The Road to 56,
//! Kaiserreich, Old World Blues, The Fire Rises and Millennium Dawn (its
//! Workshop release; `benches/millennium_dawn.rs` runs its `main` branch).
//!
//! Run it with `cargo bench --bench workshop_mods`, then draw its results
//! with `python scripts/plot_benchmarks.py`. `HEARTY_BENCH_MODS` (a comma
//! separated list of slugs or Workshop ids, e.g. `kaiserreich,2265420196`)
//! runs a subset.
//!
//! Each mod is taken from, in order: the folder in its
//! `HEARTY_BENCH_MOD_<id>` environment variable; the Workshop folder of any
//! Steam library (so subscribing to a mod is enough; `HEARTY_BENCH_STEAM_DIR`
//! adds a Steam installation to search); or, when `HEARTY_BENCH_STEAM_USER`
//! names a Steam account that owns HOI4, a steamcmd download (steamcmd's
//! anonymous login cannot download HOI4's Workshop items). A mod found in
//! none of these is skipped, saying how to provide it.
//!
//! The files hearty reads (`descriptor.mod`, `common`, `events`, `history`
//! and `localisation`) are synced into a git working copy under
//! `target/tmp/workshop-mods/<slug>` (or `HEARTY_BENCH_WORKSHOP_DIR`), so
//! the Steam folder is never written to and runs that change files are
//! undone with `git reset`. Later runs only copy files that changed.
//!
//! Every mod gets the same scenarios as the Millennium Dawn benchmark. The
//! problems hearty reports (from `--check --lint`) and every timing are
//! written to `results.json` in that directory, next to the flamegraphs.

#[path = "support/git.rs"]
mod git;
#[path = "support/results.rs"]
mod results;
#[path = "support/run.rs"]
mod run;
#[path = "support/workshop.rs"]
mod workshop;

use git::Result;
use results::{Problems, Timings};
use run::{
    HEARTY, ITERATIONS, Measured, SCENARIOS, SCRIPT_DIRS, Scenario, count_files, measure, mib,
    settle,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};
use workshop::{Found, MODS, WorkshopMod, locate, sync};

/// A benchmarked mod's results.
struct ModResult {
    /// Where it was found.
    found: Found,
    /// Size of its inputs.
    inputs: Inputs,
    /// What the scenarios took, in [`SCENARIOS`] order.
    measured: Vec<(&'static Scenario, Measured)>,
    /// The mod.
    workshop_mod: WorkshopMod,
}

/// A mod's input files.
struct Inputs {
    localisation_bytes: u64,
    localisation_files: u64,
    script_bytes: u64,
    script_files: u64,
}

fn main() -> ExitCode {
    // `cargo bench` passes `--bench`. `cargo test --benches` (or
    // `--all-targets`) does not, and should not copy gigabytes of mods.
    if !std::env::args().any(|arg| arg == "--bench") {
        println!("workshop_mods: skipped; run it with `cargo bench --bench workshop_mods`");
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("workshop_mods benchmark failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let out = std::env::var_os("HEARTY_BENCH_WORKSHOP_DIR")
        .map_or_else(|| tmp.join("workshop-mods"), PathBuf::from);
    let cache = tmp.join("hearty-cache");
    std::fs::create_dir_all(&out)?;
    std::fs::create_dir_all(&cache)?;
    let threads = std::thread::available_parallelism().map_or(0, usize::from);
    println!("== hearty benchmark: Steam Workshop mods ==");
    println!("hearty:  {HEARTY}, {threads} hardware threads");
    println!("output:  {}", out.display());

    let mut benchmarked = Vec::new();
    let mut skipped = Vec::new();
    for workshop_mod in selected_mods()? {
        println!("\n==== {} ({}) ====", workshop_mod.name, workshop_mod.id);
        let Some(found) = locate(&workshop_mod, &out.join("steamcmd"))? else {
            let reason = format!(
                "not found: subscribe to it on the Steam Workshop \
                 (https://steamcommunity.com/sharedfiles/filedetails/?id={}), set {} to a copy \
                 of it, or set HEARTY_BENCH_STEAM_USER to download it with steamcmd",
                workshop_mod.id,
                workshop_mod.env_var()
            );
            println!("skipped: {reason}");
            skipped.push((workshop_mod, reason));
            continue;
        };
        println!("source:  {}", found.describe());
        let dir = out.join(workshop_mod.slug);
        let synced = sync(found.path(), &dir)?;
        println!(
            "copy:    {} ({}; {} files copied, {} deleted)",
            dir.display(),
            if synced.created { "created" } else { "updated" },
            synced.copied,
            synced.deleted
        );
        settle(&dir);
        let (script_files, script_bytes) = count_files(&dir, SCRIPT_DIRS, "txt");
        let (localisation_files, localisation_bytes) = count_files(&dir, &["localisation"], "yml");
        println!(
            "inputs:  {script_files} script files ({}), {localisation_files} localisation files ({})",
            mib(script_bytes),
            mib(localisation_bytes)
        );

        let mut measured = Vec::new();
        for scenario in SCENARIOS {
            println!(
                "\n-- {}: hearty {} --",
                workshop_mod.name,
                scenario.args.join(" ")
            );
            let prefix = format!("{}-", workshop_mod.slug);
            measured.push((scenario, measure(scenario, &dir, &cache, &out, &prefix)?));
        }
        benchmarked.push(ModResult {
            found,
            inputs: Inputs {
                localisation_bytes,
                localisation_files,
                script_bytes,
                script_files,
            },
            measured,
            workshop_mod,
        });
    }

    let results = results_json(&benchmarked, &skipped, threads);
    let results_path = out.join("results.json");
    std::fs::write(&results_path, serde_json::to_string_pretty(&results)?)?;

    for result in &benchmarked {
        println!(
            "\n== {} ({}), {ITERATIONS} runs each after a warm-up ==",
            result.workshop_mod.name, result.workshop_mod.id
        );
        run::print_table(&result.measured);
        run::print_details(&result.measured);
    }
    print_overview(&benchmarked);
    for (workshop_mod, reason) in &skipped {
        println!("skipped {}: {reason}", workshop_mod.name);
    }
    println!("\nResults: {}", results_path.display());
    println!(
        "Charts:  python scripts/plot_benchmarks.py {}",
        results_path.display()
    );
    if benchmarked.is_empty() {
        return Err("none of the mods was found; see the messages above".into());
    }
    Ok(())
}

/// The mods to run: those `HEARTY_BENCH_MODS` names (by slug or Workshop
/// id), or all of them.
fn selected_mods() -> Result<Vec<WorkshopMod>> {
    let Some(names) = std::env::var_os("HEARTY_BENCH_MODS") else {
        return Ok(MODS.to_vec());
    };
    let names = names.to_str().ok_or("HEARTY_BENCH_MODS is not UTF-8")?;
    names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            MODS.iter()
                .find(|workshop_mod| workshop_mod.slug == name || workshop_mod.id == name)
                .copied()
                .ok_or_else(|| {
                    let known: Vec<&str> =
                        MODS.iter().map(|workshop_mod| workshop_mod.slug).collect();
                    format!(
                        "HEARTY_BENCH_MODS: unknown mod {name:?}; known: {}",
                        known.join(", ")
                    )
                    .into()
                })
        })
        .collect()
}

/// The median-run output of the scenario named `name`, as text.
fn median_output<'a>(result: &'a ModResult, name: &str) -> Option<&'a Measured> {
    result
        .measured
        .iter()
        .find(|(scenario, _)| scenario.name == name)
        .map(|(_, measured)| measured)
}

/// The problems hearty found in a mod, read from its `--check --lint`
/// run.
fn problems(result: &ModResult) -> Problems {
    median_output(result, "check-lint").map_or_else(Problems::default, |measured| {
        Problems::parse(
            &String::from_utf8_lossy(&measured.median.stdout),
            &String::from_utf8_lossy(&measured.median.stderr),
        )
    })
}

/// Everything the benchmark measured, for `scripts/plot_benchmarks.py`.
fn results_json(
    benchmarked: &[ModResult],
    skipped: &[(WorkshopMod, String)],
    threads: usize,
) -> Value {
    let generated = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    json!({
        "generated_at_unix": generated,
        "hardware_threads": threads,
        "hearty": HEARTY,
        "iterations": ITERATIONS,
        "mods": benchmarked.iter().map(|result| json!({
            "id": result.workshop_mod.id,
            "inputs": {
                "localisation_bytes": result.inputs.localisation_bytes,
                "localisation_files": result.inputs.localisation_files,
                "script_bytes": result.inputs.script_bytes,
                "script_files": result.inputs.script_files,
            },
            "name": result.workshop_mod.name,
            "problems": problems(result).to_json(),
            "scenarios": result.measured.iter().map(|(scenario, measured)| {
                let [min, median, max] = measured
                    .min_median_max()
                    .map(|wall| wall.map(|wall| wall.as_secs_f64()));
                json!({
                    "args": scenario.args.join(" "),
                    "flamegraph": measured.profile.as_ref().map(|(_, _, path)| path.display().to_string()),
                    "max_secs": max,
                    "median_secs": median,
                    "min_secs": min,
                    "name": scenario.name,
                    "runs_secs": measured.walls.iter().map(|wall| wall.as_secs_f64()).collect::<Vec<_>>(),
                    "timings": measured.profile.as_ref().and_then(|(_, report, _)| Timings::parse(report)).map(|timings| timings.to_json()),
                })
            }).collect::<Vec<_>>(),
            "slug": result.workshop_mod.slug,
            "source": result.found.describe(),
        })).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|(workshop_mod, reason)| json!({
            "id": workshop_mod.id,
            "name": workshop_mod.name,
            "reason": reason,
        })).collect::<Vec<_>>(),
    })
}

/// A table comparing the mods: their size, the time of the two combined
/// scenarios, and the problems hearty found.
fn print_overview(benchmarked: &[ModResult]) {
    println!("\n== Overview (median of {ITERATIONS} runs) ==");
    println!(
        "{:<18} {:>8} {:>11} {:>12} {:>13} {:>13} {:>11} {:>11}",
        "mod",
        "scripts",
        "script MiB",
        "check+lint",
        "fix+format",
        "to reformat",
        "missing loc",
        "redundant"
    );
    for result in benchmarked {
        let median = |name: &str| {
            median_output(result, name)
                .and_then(|measured| measured.min_median_max()[1])
                .map_or_else(String::new, |wall| format!("{wall:.2?}"))
        };
        let problems = problems(result);
        println!(
            "{:<18} {:>8} {:>11} {:>12} {:>13} {:>13} {:>11} {:>11}",
            result.workshop_mod.name,
            result.inputs.script_files,
            mib(result.inputs.script_bytes).trim_end_matches(" MiB"),
            median("check-lint"),
            median("fix-format"),
            problems.files_to_reformat,
            format!(
                "{}/{}",
                problems.missing_localisations, problems.localisation_keys
            ),
            problems.redundant_fields,
        );
    }
}
