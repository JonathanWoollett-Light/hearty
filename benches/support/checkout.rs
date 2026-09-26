//! Getting and updating the benchmark's git checkout. It is a module of its
//! own so `tests/bench_checkout.rs` can test it against a local repository.

use std::path::Path;
use std::time::{Duration, Instant};

pub use crate::git::{Result, git};

/// Marks a clone as made by the benchmark, so it may be reset.
pub const MARKER: &str = "hearty-bench";

/// What to check out.
pub struct Source<'a> {
    /// The branch.
    pub branch: &'a str,
    /// The directories checked out besides the root files.
    pub sparse_dirs: &'a [&'a str],
    /// The repository.
    pub url: &'a str,
}

/// Makes `dir` a fresh checkout of `source`: clones it if `dir` does not
/// exist (or is empty), else fetches into the benchmark's existing clone and
/// resets it. Returns how, and how long it took.
pub fn checkout(dir: &Path, source: &Source) -> Result<String> {
    let start = Instant::now();
    let marker = dir.join(".git").join(MARKER);
    if dir.join(".git").exists() {
        if !marker.exists() {
            return Err(format!(
                "{} is a git checkout without the benchmark's marker (.git/{MARKER}), so the \
                 benchmark did not clone it; refusing to reset it (point HEARTY_BENCH_DIR at a \
                 new directory)",
                dir.display()
            )
            .into());
        }
        println!("Fetching {} into {} ...", source.branch, dir.display());
        git(
            Some(dir),
            &[
                "fetch",
                "--depth",
                "1",
                "--filter=blob:none",
                "origin",
                source.branch,
            ],
        )?;
        git(Some(dir), &["reset", "--hard", "-q", "FETCH_HEAD"])?;
        // Cheap when nothing changed, and keeps the checkout in step with
        // `sparse_dirs` if they have.
        sparse_checkout(dir, source.sparse_dirs)?;
        // Also removes what a run interrupted before its restore added.
        git(Some(dir), &["clean", "-fdq"])?;
        return Ok(format!("fetched in {:.1?}", start.elapsed()));
    }
    if dir.exists() && dir.read_dir()?.next().is_some() {
        return Err(format!(
            "{} exists and is not a git checkout; remove it or set HEARTY_BENCH_DIR",
            dir.display()
        )
        .into());
    }
    // The clone is made next to `dir` and moved there once it is complete
    // and marked. A clone that fails or is interrupted (most likely in the
    // sparse checkout, which downloads most of the data) is then never left
    // at `dir`, where later runs would refuse to reset it, and the next run
    // clears it away and clones again.
    let name = dir
        .file_name()
        .ok_or("the benchmark directory has no name")?;
    let mut partial_name = name.to_owned();
    partial_name.push(".partial");
    let partial = dir.with_file_name(partial_name);
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    println!(
        "Cloning {} ({}) into {} ...",
        source.url,
        source.branch,
        dir.display()
    );
    let target = partial
        .to_str()
        .ok_or("the benchmark directory is not UTF-8")?;
    git(
        None,
        &[
            "clone",
            // Saves `core.longpaths` in the clone's config, for git commands
            // run in it by hand (`git` sets it for the benchmark's own).
            "--config",
            "core.longpaths=true",
            "--depth",
            "1",
            "--branch",
            source.branch,
            "--single-branch",
            "--filter=blob:none",
            "--sparse",
            source.url,
            target,
        ],
    )?;
    sparse_checkout(&partial, source.sparse_dirs)?;
    std::fs::write(
        partial.join(".git").join(MARKER),
        "cloned by hearty's millennium_dawn benchmark\n",
    )?;
    if dir.exists() {
        // It is empty (checked above), and a directory cannot be renamed
        // over another on Windows.
        std::fs::remove_dir(dir)?;
    }
    rename(&partial, dir)?;
    Ok(format!("cloned in {:.1?}", start.elapsed()))
}

/// Checks out only the root files and `sparse_dirs` of the clone at `dir`.
fn sparse_checkout(dir: &Path, sparse_dirs: &[&str]) -> Result<()> {
    // Cone mode keeps the root files (descriptor.mod) too.
    let mut args = vec!["sparse-checkout", "set"];
    args.extend(sparse_dirs);
    git(Some(dir), &args)?;
    Ok(())
}

/// Renames `from` to `to`, retrying for a few seconds: on Windows, renaming
/// a directory fails while another process, such as a virus scanner looking
/// at the files just checked out, has a file in it open.
fn rename(from: &Path, to: &Path) -> Result<()> {
    let mut attempts = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(_) if attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(err) => {
                return Err(format!(
                    "moving {} to {} failed: {err}",
                    from.display(),
                    to.display()
                )
                .into());
            }
        }
    }
}
