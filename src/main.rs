#![warn(clippy::pedantic)]
#![warn(clippy::restriction)]
#![allow(
    clippy::single_call_fn,
    clippy::implicit_return,
    clippy::absolute_paths,
    clippy::std_instead_of_core,
    clippy::std_instead_of_alloc,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::missing_trait_methods,
    clippy::unseparated_literal_suffix,
    clippy::separated_literal_suffix,
    clippy::blanket_clippy_restriction_lints,
    clippy::else_if_without_else,
    clippy::question_mark_used,              // conflicts with unwrap_used; ? is idiomatic
    clippy::missing_docs_in_private_items,   // binary crate, no public API to document
    clippy::shadow_reuse,                    // intentional re-binding of `keys` after filtering
    clippy::shadow_unrelated,                // `_` reuse across nested loop levels is fine
    clippy::arithmetic_side_effects,         // checked arithmetic everywhere would be noise
    clippy::integer_division,                // intentional integer division in fmt_commas
    clippy::integer_division_remainder_used, // intentional modulo in fmt_commas
    clippy::non_ascii_literal,               // ⋮ truncation indicator is intentional
    clippy::use_debug,                       // Duration's Debug IS the human-readable format
    clippy::min_ident_chars,                 // |s| closure params are idiomatic Rust
    clippy::unwrap_in_result,                // unwrap_used already covers this
    clippy::as_conversions,                  // usize→u64 cast is safe on all target platforms
    clippy::pattern_type_mismatch,           // match ergonomics are idiomatic Rust
    clippy::doc_markdown,                    // displaydoc format strings use {field} syntax, not code
    clippy::too_many_lines,                  // line counts are a noisy proxy for complexity
    reason = "Mitigates excessive and sometimes conflicting warnings from `clippy::restriction`."
)]

mod cst;
mod field_order;
mod files;
mod game;
mod inline;
mod keys;
mod localisation;
mod pipeline;
mod redundant;
mod report;
mod schema;
mod sort;
mod timings;
mod version;

use clap::Parser;
use files::ScriptFile;
use localisation::LocFile;
use miette::{Diagnostic, NamedSource, SourceSpan};
use pipeline::{Outcome, Settings, WriteFailure};
use rayon::prelude::*;
use report::{FileChange, Verb};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Max number of diagnostics of each lint to print before truncating with ⋮.
const MAX_MISSING: u64 = 10;

/// mimalloc copes far better than the system allocator with every core
/// allocating at once, as the script pass does.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("failed to write the flamegraph: {0}")]
    Flamegraph(std::io::Error),
    #[error("formatting check failed: one or more files would be reformatted")]
    FormatDrift,
    #[error("lint found {0} problem(s)")]
    LintFindings(u64),
    #[error("{0} is not a mod directory (no descriptor.mod found)")]
    NotModDir(std::path::PathBuf),
    #[error("failed to render diagnostic: {0}")]
    ReportRender(#[from] std::fmt::Error),
    #[error("failed to write {0} file(s)")]
    WriteFailed(usize),
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy, clap::ValueEnum, PartialOrd, Ord)]
#[clap(rename_all = "snake_case")]
enum Language {
    BrazilianPortuguese,
    Chinese,
    English,
    French,
    German,
    Japanese,
    Korean,
    Polish,
    Russian,
    Spanish,
}

impl Language {
    /// The language's name in localisation file names (`*l_<name>.yml`).
    const fn as_str(self) -> &'static str {
        match self {
            Self::BrazilianPortuguese => "braz_por",
            Self::Chinese => "simp_chinese",
            Self::English => "english",
            Self::French => "french",
            Self::German => "german",
            Self::Japanese => "japanese",
            Self::Korean => "korean",
            Self::Polish => "polish",
            Self::Russian => "russian",
            Self::Spanish => "spanish",
        }
    }
}

#[derive(Debug, Clone, Parser)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "fields are independent CLI flags, not a state machine"
)]
struct Args {
    /// Check all languages.
    #[arg(long, conflicts_with = "lang")]
    all: bool,

    /// Verify formatting without modifying files; exits non-zero on drift.
    #[arg(long)]
    check: bool,

    /// Automatically fix lint findings that support it (e.g. remove fields
    /// set to their default value).
    #[arg(long)]
    fix: bool,

    /// Write an SVG flamegraph of where the run's time went to FILE: each
    /// frame's width is the time threads spent in that part (see --timings).
    #[arg(long, value_name = "FILE")]
    flamegraph: Option<std::path::PathBuf>,

    /// Apply formatting across all supported file types.
    #[arg(long)]
    format: bool,

    /// Hearts of Iron IV's folder. The lint counts the localisation keys
    /// the base game defines as defined, since a mod may use them. Defaults
    /// to HEARTY_GAME_DIR, else the game's folder in a Steam library; an
    /// empty value leaves the base game out.
    #[arg(long, value_name = "DIR")]
    game_dir: Option<std::path::PathBuf>,

    /// Languages to check. May be repeated: --lang english --lang german.
    /// Defaults to english if neither --lang nor --all is given.
    #[arg(long, value_enum, conflicts_with = "all")]
    lang: Vec<Language>,

    /// Run linting checks. Enabled by default when no action flag is given.
    #[arg(long)]
    lint: bool,

    /// Maximum line width (tabs count as 4 columns) up to which formatting
    /// joins a short block onto one line.
    #[arg(long, default_value_t = 100, value_name = "COLUMNS")]
    max_width: usize,

    /// Path to the mod directory. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: std::path::PathBuf,

    /// Print a breakdown of where the run's time went to stderr: for each
    /// part (action, file, formatter rule, parse, lint step), the time
    /// threads spent in it summed over threads, its calls and its share.
    #[arg(long)]
    timings: bool,
}

impl Args {
    /// Returns the actions to run after applying the defaulting rule: if no
    /// action flag is set, `--lint` is implicitly enabled; otherwise only the
    /// explicitly set flags are enabled.
    const fn actions(&self) -> Actions {
        if self.lint || self.format || self.check || self.fix {
            Actions {
                check: self.check,
                fix: self.fix,
                format: self.format,
                lint: self.lint,
            }
        } else {
            Actions {
                check: false,
                fix: false,
                format: false,
                lint: true,
            }
        }
    }

    /// The languages to check, sorted and without repeats.
    fn active_languages(&self) -> Vec<Language> {
        let mut languages = if self.all {
            vec![
                Language::BrazilianPortuguese,
                Language::Chinese,
                Language::English,
                Language::French,
                Language::German,
                Language::Japanese,
                Language::Korean,
                Language::Polish,
                Language::Russian,
                Language::Spanish,
            ]
        } else if self.lang.is_empty() {
            vec![Language::English]
        } else {
            self.lang.clone()
        };
        languages.sort_unstable();
        languages.dedup();
        languages
    }
}

/// "{key}" not localised in: {missing_langs}.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct MissingLocalisation {
    /// The localisation key.
    key: String,
    /// Languages missing this key, comma-separated.
    missing_langs: String,
    /// Byte span of the key in the source file.
    #[label("missing localisation")]
    span: SourceSpan,
    /// Source file containing the key reference.
    #[source_code]
    src: NamedSource<Arc<str>>,
}

impl std::error::Error for MissingLocalisation {}

/// `{text}` has no effect: {explanation}.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct RedundantField {
    /// Why the field has no effect.
    explanation: &'static str,
    #[help]
    help: Option<&'static str>,
    #[label("redundant")]
    span: SourceSpan,
    #[source_code]
    src: NamedSource<Arc<str>>,
    /// The field as written, whitespace collapsed.
    text: String,
}

impl std::error::Error for RedundantField {}

/// Which actions a run performs; see [`Args::actions`].
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "fields are independent CLI actions, not a state machine"
)]
struct Actions {
    check: bool,
    fix: bool,
    format: bool,
    lint: bool,
}

/// What processing one file of the pass produced.
#[derive(Debug, Default)]
enum Done {
    /// The keys a localisation file defines.
    Localisation(Vec<Box<str>>),
    #[default]
    Nothing,
    Script(Box<Outcome>),
}

/// What the lint needs besides the script pass's outcomes.
struct LintInputs<'run> {
    /// The keys each file of `localisation` defines.
    defined: Vec<Vec<Box<str>>>,
    /// The base game's folder, if found.
    game: Option<std::path::PathBuf>,
    /// The checked languages.
    languages: &'run [Language],
    /// The localisation files of the checked languages, the mod's and then
    /// the base game's.
    localisation: &'run [LocFile],
    /// The version check of `descriptor.mod`, running on its own thread.
    version: std::thread::JoinHandle<version::Report>,
}

/// A missing localisation to print: the key, the file defining it (an index
/// into the script files), where, and the languages it is missing from.
type MissingKey<'run> = (&'run str, usize, Option<cst::Span>, String);

fn fmt_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn inner_main() -> Result<(), AppError> {
    let start = std::time::Instant::now();
    let args = Args::parse();
    // Only a profiled run installs a subscriber: without one, every span
    // callsite is disabled and costs next to nothing.
    let profiler = if args.timings || args.flamegraph.is_some() {
        timings::Profiler::install(start)
    } else {
        None
    };
    let result = run(&args, start);
    // Report timings even when the actions found problems, as a lint run
    // with findings is still worth profiling; the actions' error wins.
    let reported = profiler.map_or(Ok(()), |profiler| {
        profiler
            .finish(args.timings, args.flamegraph.as_deref())
            .map_err(AppError::Flamegraph)
    });
    result.and(reported)
}

/// Prints the lint's findings: `descriptor.mod`'s version check, missing
/// localisations and redundant fields. `report` is the span of the report.
fn lint(
    files: &[ScriptFile],
    outcomes: &[Outcome],
    inputs: LintInputs<'_>,
    report: &tracing::Span,
) -> Result<(), AppError> {
    // Joined outside any span: waiting for the check is not work.
    if let Ok(version) = inputs.version.join() {
        print!("{}", version.stdout);
        eprint!("{}", version.stderr);
    }

    // The keys the scripts use, each with the file (in path order) and
    // place that first defines it: every use in path order, then source
    // order, stably sorted by key, keeping each key's first use. (A serial
    // `BTreeMap` of the ~70k keys of a large mod took a tenth of a lint.)
    let mut things: Vec<(&str, usize, Option<cst::Span>)> = outcomes
        .iter()
        .enumerate()
        .flat_map(|(index, outcome)| {
            outcome
                .keys
                .iter()
                .map(move |used| (used.key.as_str(), index, used.span))
        })
        .collect();
    things.par_sort_by(|a, b| a.0.cmp(b.0));
    things.dedup_by(|later, first| later.0 == first.0);
    let wanted: Vec<&str> = things.iter().map(|&(key, ..)| key).collect();
    let masks = localisation::defined(
        &wanted,
        inputs.localisation,
        &inputs.defined,
        &tracing::info_span!(parent: report, "missing keys"),
    );
    let mut missing = 0_u64;
    let mut shown: Vec<MissingKey<'_>> = Vec::new();
    report.in_scope(|| {
        let _span = tracing::info_span!("missing keys").entered();
        for (&(key, file, span), mask) in things.iter().zip(masks) {
            let missing_langs: Vec<&str> = inputs
                .languages
                .iter()
                .enumerate()
                .filter(|&(bit, _)| (mask >> bit) & 1 == 0)
                .map(|(_, language)| language.as_str())
                .collect();
            if missing_langs.is_empty() {
                continue;
            }
            missing += 1;
            if missing <= MAX_MISSING {
                shown.push((key, file, span, missing_langs.join(", ")));
            }
        }
    });

    let redundant: u64 = outcomes
        .iter()
        .map(|outcome| outcome.redundant.len() as u64)
        .sum();
    let fixable = outcomes
        .iter()
        .flat_map(|outcome| &outcome.redundant)
        .filter(|finding| finding.fix.is_some())
        .count() as u64;
    let shown_redundant: Vec<(usize, &redundant::Finding)> = outcomes
        .iter()
        .enumerate()
        .flat_map(|(index, outcome)| {
            outcome
                .redundant
                .iter()
                .map(move |finding| (index, finding))
        })
        .take(usize::try_from(MAX_MISSING).unwrap_or(usize::MAX))
        .collect();

    // Only the files of the diagnostics printed are kept, read again here.
    let wanted: BTreeSet<usize> = shown
        .iter()
        .map(|&(_, file, ..)| file)
        .chain(shown_redundant.iter().map(|&(file, _)| file))
        .collect();
    let texts: BTreeMap<usize, Arc<str>> = wanted
        .into_par_iter()
        .filter_map(|index| {
            let file = files.get(index)?;
            let _span = tracing::info_span!(parent: report, "read").entered();
            let text = std::fs::read_to_string(&file.path).unwrap_or_default();
            Some((index, Arc::from(text)))
        })
        .collect();

    report.in_scope(|| {
        let handler = miette::GraphicalReportHandler::new();
        tracing::info_span!("missing keys").in_scope(|| {
            for (key, index, span, missing_langs) in shown {
                let (Some(file), Some(text)) = (files.get(index), texts.get(&index)) else {
                    continue;
                };
                let diag = MissingLocalisation {
                    key: key.to_owned(),
                    missing_langs,
                    span: key_span(text, key, span).into(),
                    src: NamedSource::new(file.path.display().to_string(), Arc::clone(text)),
                };
                let mut out = String::new();
                handler.render_report(&mut out, &diag)?;
                eprint!("{out}");
            }
            if missing > MAX_MISSING {
                eprintln!("⋮ ({} more not shown)", missing - MAX_MISSING);
            }
            match &inputs.game {
                Some(game) => println!(
                    "\nRead {} localisation files of the base game from {}.",
                    fmt_commas(
                        inputs
                            .localisation
                            .iter()
                            .filter(|file| file.game)
                            .count() as u64
                    ),
                    game.display()
                ),
                None => println!(
                    "\nThe base game's localisation was not read, so keys only it defines are reported missing (pass --game-dir or set {} to Hearts of Iron IV's folder).",
                    game::GAME_DIR_VAR
                ),
            }
            println!(
                "Found {}/{} missing localisations.",
                fmt_commas(missing),
                fmt_commas(things.len() as u64)
            );
            Ok::<(), AppError>(())
        })?;

        tracing::info_span!("redundant fields").in_scope(|| {
            for (index, finding) in shown_redundant {
                let (Some(file), Some(text)) = (files.get(index), texts.get(&index)) else {
                    continue;
                };
                let span = finding.span;
                let fits = text.get(span.start..span.end).is_some();
                let diag = RedundantField {
                    explanation: finding.explanation,
                    help: Some(if finding.fix.is_some() {
                        "remove it, or run `hearty --fix`"
                    } else {
                        "remove it (not auto-fixed because a comment would be lost)"
                    }),
                    span: if fits {
                        (span.start, span.end - span.start).into()
                    } else {
                        (0, 0).into()
                    },
                    src: NamedSource::new(file.relative.display().to_string(), Arc::clone(text)),
                    text: finding.text.clone(),
                };
                let mut out = String::new();
                handler.render_report(&mut out, &diag)?;
                eprint!("{out}");
            }
            if redundant > MAX_MISSING {
                eprintln!("⋮ ({} more not shown)", redundant - MAX_MISSING);
            }
            println!(
                "\nFound {} redundant fields ({} fixable with --fix).",
                fmt_commas(redundant),
                fmt_commas(fixable)
            );
            Ok::<(), AppError>(())
        })
    })?;

    // The keys of every localisation file, millions with --all, took tens of
    // milliseconds to free one by one on this thread; the pool shares that.
    inputs.defined.into_par_iter().for_each(drop);

    let problems = missing + redundant;
    if problems > 0 {
        Err(AppError::LintFindings(problems))
    } else {
        Ok(())
    }
}

/// Where to point a missing key's diagnostic in `text`: at its definition if
/// that is where `span` says, else at its first appearance (or the start of
/// the file).
fn key_span(text: &str, key: &str, span: Option<cst::Span>) -> (usize, usize) {
    let offset = span
        .filter(|span| text.get(span.start..span.end) == Some(key))
        .map_or_else(|| text.find(key).unwrap_or(0), |span| span.start);
    if text.get(offset..offset + key.len()).is_some() {
        (offset, key.len())
    } else {
        (0, 0)
    }
}

fn main() -> Result<(), String> {
    inner_main().map_err(|err| format!("{err}"))
}

/// Prints the summaries of `--fix`, `--format` and `--check` (as `actions`
/// asks), and reports every file that could not be written. Returns whether
/// `--check` found drift, and how many files could not be written.
fn print_summaries(files: &[ScriptFile], outcomes: &[Outcome], actions: Actions) -> (bool, usize) {
    let mut write_failures = 0;
    for (file, outcome) in files.iter().zip(outcomes) {
        if let Some(failure) = &outcome.write_failure {
            eprintln!("failed to write {}: {}", file.path.display(), failure.error);
            write_failures += 1;
        }
    }
    let changes = |change: fn(&Outcome) -> Option<&FileChange>| -> Vec<FileChange> {
        outcomes.iter().filter_map(change).cloned().collect()
    };
    // How many files an action changed that could not be written.
    let unwritten = |lost: fn(&WriteFailure) -> bool| -> usize {
        outcomes
            .iter()
            .filter_map(|outcome| outcome.write_failure.as_ref())
            .filter(|&failure| lost(failure))
            .count()
    };
    if actions.fix {
        print!(
            "{}",
            report::summary(
                Verb::Fixed,
                &changes(|o| o.fixed.as_ref()),
                unwritten(|failure| failure.fixed)
            )
        );
        let unfixed: usize = outcomes.iter().map(|outcome| outcome.unfixed).sum();
        if unfixed > 0 {
            println!(
                "{} redundant field(s) left in place because removing them would delete a comment; run --lint to see them.",
                fmt_commas(unfixed as u64)
            );
        }
    }
    if actions.format {
        print!(
            "{}",
            report::summary(
                Verb::Formatted,
                &changes(|o| o.formatted.as_ref()),
                unwritten(|failure| failure.formatted)
            )
        );
    }
    let drift = actions.check && {
        let would_format = changes(|o| o.would_format.as_ref());
        print!("{}", report::summary(Verb::WouldFormat, &would_format, 0));
        !would_format.is_empty()
    };
    (drift, write_failures)
}

/// Runs the actions `args` asks for. Every script file is read once and
/// taken through the actions in order (fix, format, check, lint) in one
/// parallel pass, alongside loading the localisation; the results are then
/// printed in that order.
fn run(args: &Args, start: std::time::Instant) -> Result<(), AppError> {
    let actions = args.actions();
    let languages = args.active_languages();
    // The lint needs a `descriptor.mod`. Its version check may wait on the
    // network or steamcmd, so it starts now, on a thread of its own.
    let descriptor = actions
        .lint
        .then(|| std::fs::read_to_string(args.path.join("descriptor.mod")).ok())
        .flatten();
    // Without a `descriptor.mod` the lint fails at once, so the files are
    // only linted with one.
    let lint_files = descriptor.is_some();
    // The base game's localisation, minus the folders the mod replaces,
    // counts as the mod's.
    let game = lint_files
        .then(|| game::find(args.game_dir.as_deref()))
        .flatten()
        .map(|(dir, _)| dir);
    let replaced = descriptor
        .as_deref()
        .map(game::replaced_folders)
        .unwrap_or_default();
    let version = descriptor.map(version::spawn);

    let found = tracing::info_span!("find files");
    let (files, localisation_files) = rayon::join(
        || files::scripts(&args.path, &found),
        || {
            if lint_files {
                let mut files = localisation::files(&args.path, &languages, &found);
                if let Some(game) = &game {
                    files.extend(localisation::game_files(
                        game, &languages, &replaced, &found,
                    ));
                }
                files
            } else {
                Vec::new()
            }
        },
    );
    drop(found);

    let settings = Settings {
        check: actions.check,
        fix: actions.fix,
        format: actions.format,
        lint: lint_files,
        max_width: args.max_width,
    };
    // One pass over the script files and, alongside, the localisation
    // files, all on every core.
    let scripts = tracing::info_span!("scripts");
    let loading = lint_files.then(|| tracing::info_span!("localisation load"));
    let sizes: Vec<u64> = files
        .iter()
        .map(|file| file.size)
        .chain(localisation_files.iter().map(|file| file.size))
        .collect();
    let done = pipeline::in_parallel(&sizes, |index| {
        if let Some(file) = files.get(index) {
            let _span =
                tracing::info_span!(parent: &scripts, "file", path = %file.relative.display())
                    .entered();
            Done::Script(Box::new(pipeline::process(file, settings)))
        } else if let (Some(file), Some(loading)) = (
            index
                .checked_sub(files.len())
                .and_then(|index| localisation_files.get(index)),
            &loading,
        ) {
            let _span =
                tracing::info_span!(parent: loading, "file", path = %file.relative.display())
                    .entered();
            Done::Localisation(localisation::load(file))
        } else {
            Done::Nothing
        }
    });
    drop(scripts);
    drop(loading);
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(files.len());
    let mut defined: Vec<Vec<Box<str>>> = Vec::with_capacity(localisation_files.len());
    for result in done {
        match result {
            Done::Localisation(keys) => defined.push(keys),
            Done::Nothing => {}
            Done::Script(outcome) => outcomes.push(*outcome),
        }
    }

    let report = tracing::info_span!("report");
    let (drift, write_failures) = report.in_scope(|| {
        let _span = tracing::info_span!("summary").entered();
        print_summaries(&files, &outcomes, actions)
    });
    let linted = match version {
        _ if !actions.lint => Ok(()),
        None => Err(AppError::NotModDir(args.path.clone())),
        Some(version) => lint(
            &files,
            &outcomes,
            LintInputs {
                defined,
                game,
                languages: &languages,
                localisation: &localisation_files,
                version,
            },
            &report,
        ),
    };
    drop(report);

    if linted.is_ok() {
        println!("Finished in {:.2?}.", start.elapsed());
    }
    if write_failures > 0 {
        return Err(AppError::WriteFailed(write_failures));
    }
    linted?;
    if drift {
        return Err(AppError::FormatDrift);
    }
    Ok(())
}
