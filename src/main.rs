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

mod sort;

use clap::Parser;
use keyvalues_parser::{Value as VdfValue, Vdf};
use miette::{Diagnostic, NamedSource, SourceSpan};
use petgraph::graph::DiGraph;
use rayon::prelude::*;
use serde_json::{Map, Value as JsonValue};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::fs::read_to_string;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use walkdir::DirEntry;

/// Max number of diagnostics to print before truncating with ⋮.
const MAX_MISSING: u64 = 10;

const DEFAULT_CACHE_DIR: &str = ".hearty-cache";
/// Maximum bytes to read when downloading steamcmd (32 MiB).
const STEAMCMD_DOWNLOAD_MAX: u64 = 32 * 1_024 * 1_024;

/// HOI4 event block types whose fields require localisation.
const EVENT_TYPES: &[&str] = &[
    "country_event",
    "news_event",
    "operative_leader_event",
    "state_event",
    "unit_leader_event",
];

const HOI4_ID: &str = "394360";
/// Cache file name written inside the cache directory.
const HOI4_CACHE_FILE: &str = "hoi4-version-cache.json";
/// Maximum age of the on-disk cache before steamcmd is re-invoked (86 400 s = 24 h).
const HOI4_CACHE_MAX_AGE_SECS: u64 = 86_400;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("no files were processed (empty walk)")]
    EmptyWalk,
    #[error("formatting check failed: one or more files would be reformatted")]
    FormatDrift,
    #[error("missing localisations found")]
    MissingLocalisations,
    #[error("{0} is not a mod directory (no descriptor.mod found)")]
    NotModDir(std::path::PathBuf),
    #[error("failed to render diagnostic: {0}")]
    ReportRender(#[from] std::fmt::Error),
    #[error("walk processing failed")]
    WalkFailed,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown language: {0}")]
struct LanguageError(String);

#[derive(Debug, Hash, PartialEq, Eq, Clone, clap::ValueEnum, PartialOrd, Ord)]
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

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Language {
    fn as_str(&self) -> &'static str {
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

impl PartialEq<str> for Language {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl TryFrom<&str> for Language {
    type Error = LanguageError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "braz_por" => Ok(Self::BrazilianPortuguese),
            "simp_chinese" => Ok(Self::Chinese),
            "english" => Ok(Self::English),
            "french" => Ok(Self::French),
            "german" => Ok(Self::German),
            "japanese" => Ok(Self::Japanese),
            "korean" => Ok(Self::Korean),
            "polish" => Ok(Self::Polish),
            "russian" => Ok(Self::Russian),
            "spanish" => Ok(Self::Spanish),
            _ => Err(LanguageError(value.to_owned())),
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

    /// Verify focus-block ordering without modifying files; exits non-zero on drift.
    #[arg(long)]
    check: bool,

    /// Reorder focus blocks in national_focus files in place.
    #[arg(long)]
    format: bool,

    /// Languages to check. May be repeated: --lang english --lang german.
    /// Defaults to english if neither --lang nor --all is given.
    #[arg(long, value_enum, conflicts_with = "all")]
    lang: Vec<Language>,

    /// Run localisation/version checks. Enabled by default when no action flag is given.
    #[arg(long)]
    lint: bool,

    /// Path to the mod directory. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: std::path::PathBuf,
}

impl Args {
    /// Returns `(lint, format, check)` after applying the defaulting rule: if
    /// no action flag is set, `--lint` is implicitly enabled; otherwise only
    /// the explicitly set flags are enabled.
    fn actions(&self) -> (bool, bool, bool) {
        if self.lint || self.format || self.check {
            (self.lint, self.format, self.check)
        } else {
            (true, false, false)
        }
    }

    fn active_languages(&self) -> Vec<Language> {
        if self.all {
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
        }
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

/// duplicate key "{key}" in descriptor.mod.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct DuplicateDescriptorKey {
    key: String,
    #[label("duplicate key")]
    span: SourceSpan,
    #[source_code]
    src: NamedSource<Arc<str>>,
}

impl std::error::Error for DuplicateDescriptorKey {}

/// descriptor.mod supported_version "{supported_version}" does not match latest HOI4 {latest_version}.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct DescriptorVersionMismatch {
    latest_version: String,
    #[label("unsupported version")]
    span: SourceSpan,
    #[source_code]
    src: NamedSource<Arc<str>>,
    supported_version: String,
}

impl std::error::Error for DescriptorVersionMismatch {}

#[derive(Debug, Clone)]
struct State {
    args: Args,
    keys: BTreeMap<Language, BTreeSet<String>>,
    things: BTreeMap<String, (std::path::PathBuf, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatMode {
    Check,
    Write,
}

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

fn main() -> Result<(), String> {
    inner_main().map_err(|err| format!("{err}"))
}

fn inner_main() -> Result<(), AppError> {
    let start = std::time::Instant::now();
    let args = Args::parse();
    let (do_lint, do_format, do_check) = args.actions();

    if do_format {
        format(&args, FormatMode::Write);
    }

    let drift = if do_check {
        format(&args, FormatMode::Check)
    } else {
        false
    };

    if do_lint {
        lint(&args)?;
    }

    println!("Finished in {:.2?}.", start.elapsed());

    if drift {
        return Err(AppError::FormatDrift);
    }
    Ok(())
}

fn format(args: &Args, mode: FormatMode) -> bool {
    // Go through all focus tree files, sort their contents in order:
    // 1. Focuses within sub-trees are ordered breadth first with parent(s) before child focuses.
    // 2. Sub-trees are ordered based on the x and y coordinates of their roots.
    let national_focus_dir = args.path.join("common").join("national_focus");
    let drift = std::sync::atomic::AtomicBool::new(false);
    walkdir::WalkDir::new(&args.path)
        .into_iter()
        .par_bridge()
        .for_each(|dir_res| {
            let Ok(dir) = dir_res else {
                return;
            };
            let path = dir.path();
            if path.parent() == Some(national_focus_dir.as_path()) && format_focus_file(path, mode)
            {
                drift.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    drift.into_inner()
}

fn format_focus_file(path: &Path, mode: FormatMode) -> bool {
    let Ok(string) = read_to_string(path) else {
        return false;
    };

    // Collect ordered focus IDs across all focus_trees in the file. The tape
    // borrow is scoped so `string` is free to use for rewriting afterward.
    let all_ordered_ids: Vec<String> = {
        let Ok(tape) = jomini::TextTape::from_slice(string.as_bytes()) else {
            return false;
        };
        let reader = tape.windows1252_reader();
        let mut ids: Vec<String> = Vec::new();

        for (focus_tree_key, _, focus_tree_value) in reader.fields() {
            if focus_tree_key.read_str() != "focus_tree" {
                continue;
            }
            let Ok(tree) = focus_tree_value.read_object() else {
                continue;
            };

            // Focus IDs in file order, deduplicated. Order must be both
            // deterministic AND file-order-preserving: petgraph's DFS-based
            // toposort walks node_indices() in order, and the resulting order
            // becomes the `rank` that breaks ties in priority_toposort. An
            // alphabetical order (e.g. from BTreeSet) would assign leaf focuses
            // like `eight` the highest rank and push them to the end, instead
            // of keeping them next to their parent in file order.
            let mut node_ids: Vec<String> = Vec::new();
            let mut seen_ids: HashSet<String> = HashSet::new();
            let mut relative_position_edges = Vec::new();
            let mut prerequisite_edges = Vec::new();

            for (tree_key, _, tree_value) in tree.fields() {
                if tree_key.read_str() != "focus" {
                    continue;
                }
                let Ok(focus) = tree_value.read_object() else {
                    continue;
                };

                let mut focus_id: Option<String> = None;
                let mut focus_prerequisites = Vec::new();
                let mut focus_relative_positions = None;
                for (focus_key, _, focus_value) in focus.fields() {
                    if focus_key.read_str() == "id"
                        && let Ok(key_str) = focus_value.read_string()
                    {
                        focus_id = Some(key_str);
                    } else if focus_key.read_str() == "prerequisite"
                        && let Ok(preq) = focus_value.read_object()
                    {
                        for (preq_key, _, preq_value) in preq.fields() {
                            if preq_key.read_str() == "focus"
                                && let Ok(preq_str) = preq_value.read_string()
                            {
                                focus_prerequisites.push(preq_str);
                            }
                        }
                    } else if focus_key.read_str() == "relative_position_id"
                        && let Ok(relative) = focus_value.read_string()
                    {
                        focus_relative_positions = Some(relative);
                    }
                }
                let Some(a) = focus_id else {
                    continue;
                };
                if let Some(relative_position) = focus_relative_positions {
                    relative_position_edges.push((relative_position, a.clone()));
                }
                if seen_ids.insert(a.clone()) {
                    node_ids.push(a.clone());
                }
                prerequisite_edges.extend(focus_prerequisites.into_iter().map(|b| (b, a.clone())));
            }

            let mut relative_position_graph = DiGraph::<String, ()>::new();
            let mut prerequisite_graph = DiGraph::<String, ()>::new();
            let node_map: HashMap<String, _> = node_ids
                .iter()
                .map(|n| {
                    (
                        n.clone(),
                        (
                            relative_position_graph.add_node(n.clone()),
                            prerequisite_graph.add_node(n.clone()),
                        ),
                    )
                })
                .collect();

            // `prerequisite`s should contribute to the order, but should always be behind `relative_positions`
            for (a, b) in relative_position_edges {
                if let (Some(na), Some(nb)) = (node_map.get(&a), node_map.get(&b)) {
                    relative_position_graph.add_edge(na.0, nb.0, ());
                }
            }
            for (a, b) in prerequisite_edges {
                if let (Some(na), Some(nb)) = (node_map.get(&a), node_map.get(&b)) {
                    prerequisite_graph.add_edge(na.1, nb.1, ());
                }
            }
            let Ok(sorted) = sort::priority_toposort(&relative_position_graph, &prerequisite_graph)
            else {
                continue;
            };

            ids.extend(sorted);
        }

        ids
    };

    if all_ordered_ids.is_empty() {
        return false;
    }
    let new_content = reorder_focus_blocks(&string, &all_ordered_ids);
    if new_content == string {
        return false;
    }
    if mode == FormatMode::Write {
        drop(std::fs::write(path, new_content.as_bytes()));
    }
    true
}

// Handle linting (e.g. like `cargo clippy`).
fn lint(args: &Args) -> Result<(), AppError> {
    let descriptor = read_to_string(args.path.join("descriptor.mod"))
        .map_err(|_e| AppError::NotModDir(args.path.clone()))?;
    run_steamcmd_version_check(&descriptor);

    let walker = walkdir::WalkDir::new(&args.path);
    let State { args, keys, things } = walker
        .into_iter()
        .par_bridge()
        .try_fold(
            || State {
                args: args.clone(),
                keys: BTreeMap::new(),
                things: BTreeMap::new(),
            },
            iter_entry,
        )
        .try_reduce_with(
            |State {
                 args,
                 mut keys,
                 mut things,
             },
             State {
                 keys: item_keys,
                 things: item_things,
                 ..
             }| {
                for (lang, lang_keys) in item_keys {
                    if let Some(existing) = keys.get_mut(&lang) {
                        existing.extend(lang_keys);
                    } else {
                        keys.insert(lang, lang_keys);
                    }
                }
                things.extend(item_things);
                Ok(State { args, keys, things })
            },
        )
        .ok_or(AppError::EmptyWalk)?
        .map_err(|()| AppError::WalkFailed)?;

    let active: BTreeSet<Language> = args.active_languages().into_iter().collect();
    let mut keys: BTreeMap<Language, BTreeSet<String>> = keys
        .into_iter()
        .filter(|(lang, _)| active.contains(lang))
        .collect();
    // Ensure every active language has an entry. If a mod has no localisation
    // files for a language, that language won't appear in `keys` at all, which
    // would cause the missing-check loop to silently skip it.
    for lang in &active {
        keys.entry(lang.clone()).or_default();
    }

    let handler = miette::GraphicalReportHandler::new();
    let mut file_cache: BTreeMap<std::path::PathBuf, Arc<str>> = BTreeMap::new();
    let mut missing = 0u64;
    let mut printed = 0u64;
    for (thing, (source, offset)) in &things {
        let missing_langs: Vec<String> = keys
            .iter()
            .filter(|(_, lang_keys)| !lang_keys.contains(thing))
            .map(|(lang, _)| lang.to_string())
            .collect();
        if missing_langs.is_empty() {
            continue;
        }
        missing += 1;
        if printed >= MAX_MISSING {
            continue;
        }
        printed += 1;
        let content = file_cache.entry(source.clone()).or_insert_with(|| {
            let text = read_to_string(source).unwrap_or_default();
            Arc::from(text.as_str())
        });
        let diag = MissingLocalisation {
            key: thing.clone(),
            missing_langs: missing_langs.join(", "),
            span: (*offset, thing.len()).into(),
            src: NamedSource::new(source.display().to_string(), Arc::clone(content)),
        };
        let mut out = String::new();
        handler.render_report(&mut out, &diag)?;
        eprint!("{out}");
    }
    if missing > MAX_MISSING {
        eprintln!("⋮ ({} more not shown)", missing - MAX_MISSING);
    }

    println!(
        "\nFound {}/{} missing localisations.",
        fmt_commas(missing),
        fmt_commas(things.len() as u64)
    );

    if missing > 0 {
        Err(AppError::MissingLocalisations)
    } else {
        Ok(())
    }
}

fn iter_entry(
    State {
        args,
        mut keys,
        mut things,
    }: State,
    res: Result<DirEntry, walkdir::Error>,
) -> Result<State, ()> {
    let dir = res.map_err(drop)?;
    let path = dir.path();
    let prefix = path.file_prefix().ok_or(())?;
    let string = prefix.to_str().ok_or(())?;

    if let Some(index) = string.rfind("l_")
        && let Some(key) = string.get(index + 2..)
        && let Ok(language) = Language::try_from(key)
    {
        if let Some((lang, lang_keys)) = load_localisation(language, path) {
            if let Some(existing) = keys.get_mut(&lang) {
                existing.extend(lang_keys);
            } else {
                keys.insert(lang, lang_keys);
            }
        }
    } else if let Some(parent) = path.parent()
        && parent == args.path.join("events")
    {
        things.extend(read_events(path).unwrap_or_default());
    } else if let Some(parent) = path.parent()
        && parent == args.path.join("common").join("national_focus")
    {
        things.extend(read_focuses(path).unwrap_or_default());
    } else if let Some(parent) = path.parent()
        && parent == args.path.join("common").join("technologies")
    {
        things.extend(read_technologies(path).unwrap_or_default());
    }

    Ok(State { args, keys, things })
}

fn load_localisation(lang: Language, path: &Path) -> Option<(Language, BTreeSet<String>)> {
    let mut file = BufReader::new(OpenOptions::new().read(true).open(path).ok()?);
    file.skip_until(b'\n').ok()?; // skip language header line

    let mut line = String::new();
    let mut keys = BTreeSet::new();
    while let Ok(1..) = file.read_line(&mut line) {
        if let Some(key) = line
            .trim_start()
            .split(':')
            .next()
            .filter(|s| !s.is_empty())
        {
            keys.insert(key.to_owned());
        }
        line.clear();
    }

    Some((lang, keys))
}

fn find_offset(haystack: &str, needle: &str) -> usize {
    haystack.find(needle).unwrap_or(0)
}

fn read_events(path: &Path) -> Option<BTreeMap<String, (std::path::PathBuf, usize)>> {
    let string = read_to_string(path).ok()?;
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut events = BTreeMap::new();

    for (key, _, value) in reader.fields() {
        if !EVENT_TYPES.contains(&key.read_str().as_ref()) {
            continue;
        }
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (inner_key, _, inner_value) in obj.fields() {
            match inner_key.read_str().as_ref() {
                "title" | "desc" => {
                    if let Ok(scalar) = inner_value.read_scalar() {
                        let key_str = scalar.to_string();
                        let offset = find_offset(&string, &key_str);
                        events.insert(key_str, (path.to_path_buf(), offset));
                    }
                }
                "option" => {
                    let Ok(option) = inner_value.read_object() else {
                        continue;
                    };
                    for (opt_key, _, opt_value) in option.fields() {
                        if opt_key.read_str() == "name"
                            && let Ok(scalar) = opt_value.read_scalar()
                        {
                            let key_str = scalar.to_string();
                            let offset = find_offset(&string, &key_str);
                            events.insert(key_str, (path.to_path_buf(), offset));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Some(events)
}

fn read_focuses(path: &Path) -> Option<BTreeMap<String, (std::path::PathBuf, usize)>> {
    let string = read_to_string(path).ok()?;
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut focuses = BTreeMap::new();

    for (key, _, value) in reader.fields() {
        if key.read_str() != "focus_tree" {
            continue;
        }
        let Ok(tree) = value.read_object() else {
            continue;
        };
        for (tree_key, _, tree_value) in tree.fields() {
            if tree_key.read_str() != "focus" {
                continue;
            }
            let Ok(focus) = tree_value.read_object() else {
                continue;
            };
            for (focus_key, _, focus_value) in focus.fields() {
                if focus_key.read_str() == "id"
                    && let Ok(key_str) = focus_value.read_string()
                {
                    let offset = find_offset(&string, &key_str);
                    focuses.insert(key_str, (path.to_path_buf(), offset));
                }
            }
        }
    }

    Some(focuses)
}

fn read_technologies(path: &Path) -> Option<BTreeMap<String, (std::path::PathBuf, usize)>> {
    let string = read_to_string(path).ok()?;
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut techs = BTreeMap::new();

    for (key, _, value) in reader.fields() {
        if key.read_str() != "technologies" {
            continue;
        }
        let Ok(block) = value.read_object() else {
            continue;
        };
        for (tech_key, _, _) in block.fields() {
            let tech_str = tech_key.read_str();
            if tech_str.starts_with('@') {
                continue;
            }
            let offset = find_offset(&string, &tech_str);
            techs.insert(tech_str.into_owned(), (path.to_path_buf(), offset));
        }
    }

    Some(techs)
}

/// Returns the path to the HOI4 version cache file.
///
/// The directory is read from the `HEARTY_CACHE_DIR` environment variable when set.
/// In GitHub Actions, point `actions/cache` at that path (or at `.hearty-cache`) so
/// the file survives across workflow runs and steamcmd only runs when the cache is stale:
///
/// ```yaml
/// - uses: actions/cache@v4
///   with:
///     path: .hearty-cache
///     key: hoi4-version-cache
/// ```
fn cache_path() -> std::path::PathBuf {
    std::env::var_os("HEARTY_CACHE_DIR")
        .map_or_else(
            || std::path::PathBuf::from(DEFAULT_CACHE_DIR),
            std::path::PathBuf::from,
        )
        .join(HOI4_CACHE_FILE)
}

/// Reads the on-disk cache and returns the HOI4 app data if it is younger than
/// [`HOI4_CACHE_MAX_AGE_SECS`]. Returns `None` if the cache is missing, corrupt,
/// or expired.
fn read_cache() -> Option<serde_json::Value> {
    let content = read_to_string(cache_path()).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&content).ok()?;
    let fetched_at = cache.get("fetched_at_secs")?.as_u64()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let data = cache.get("data")?;
    (now.saturating_sub(fetched_at) < HOI4_CACHE_MAX_AGE_SECS).then(|| data.clone())
}

/// Writes the HOI4 app data and a `fetched_at_secs` Unix timestamp to the cache
/// file so [`read_cache`] can assess freshness on the next run. Failures are
/// silently ignored — the cache is best-effort.
fn write_cache(data: &serde_json::Value) {
    let path = cache_path();
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = serde_json::json!({
        "fetched_at_secs": now_secs,
        "data": data,
    });
    let Ok(serialized) = serde_json::to_string(&payload) else {
        return;
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }
    std::fs::write(path, serialized).unwrap_or_default();
}

/// Path where a downloaded steamcmd binary is cached alongside the version cache.
fn steamcmd_cache_path() -> std::path::PathBuf {
    let dir = cache_path().parent().map_or_else(
        || std::path::PathBuf::from(DEFAULT_CACHE_DIR),
        std::path::Path::to_path_buf,
    );
    #[cfg(windows)]
    return dir.join("steamcmd.exe");
    #[cfg(not(windows))]
    return dir.join("steamcmd.sh");
}

/// Returns `true` if a `steamcmd` binary is reachable via `PATH`.
fn steamcmd_in_path() -> bool {
    #[cfg(windows)]
    let name = "steamcmd.exe";
    #[cfg(not(windows))]
    let name = "steamcmd";
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).exists()))
}

/// Downloads steamcmd from the Steam CDN into the cache directory and returns
/// its path. Returns `None` if the download or extraction fails.
fn download_steamcmd() -> Option<std::path::PathBuf> {
    let dest = steamcmd_cache_path();
    let dir = dest.parent()?;
    std::fs::create_dir_all(dir).ok()?;

    #[cfg(windows)]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip";
    #[cfg(target_os = "macos")]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_osx.tar.gz";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz";

    println!("steamcmd not found; downloading to {} ...", dir.display());
    let mut bytes = Vec::new();
    ureq::get(url)
        .call()
        .ok()?
        .into_body()
        .into_with_config()
        .limit(STEAMCMD_DOWNLOAD_MAX)
        .reader()
        .read_to_end(&mut bytes)
        .ok()?;

    #[cfg(windows)]
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .ok()?
        .extract(dir)
        .ok()?;

    #[cfg(not(windows))]
    {
        let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
        tar::Archive::new(gz).unpack(dir).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
    }

    dest.exists().then_some(dest)
}

/// Returns the path to a usable steamcmd binary: system PATH first, then the
/// cache directory, downloading if neither is present.
fn resolve_steamcmd() -> Option<std::path::PathBuf> {
    if steamcmd_in_path() {
        #[cfg(windows)]
        return Some(std::path::PathBuf::from("steamcmd.exe"));
        #[cfg(not(windows))]
        return Some(std::path::PathBuf::from("steamcmd"));
    }
    let cached = steamcmd_cache_path();
    if cached.exists() {
        return Some(cached);
    }
    download_steamcmd()
}

fn run_steamcmd_version_check(descriptor: &str) {
    let Some(steamcmd) = resolve_steamcmd() else {
        return;
    };
    let hoi4_data_opt = read_cache().or_else(|| {
        let data = std::process::Command::new(&steamcmd)
            .args(["+login", "anonymous", "+app_info_print", HOI4_ID, "+quit"])
            .output()
            .ok()
            .and_then(|out| {
                let text = String::from_utf8_lossy(&out.stdout);
                let start = text.find(&format!("\"{HOI4_ID}\""))?;
                let end = text.rfind("Unloading Steam API")?;
                let values = Vdf::from(keyvalues_parser::parse(text.get(start..end)?).ok()?);
                Some(vdf_to_json(&values))
            })?;
        write_cache(&data);
        Some(data)
    });
    if let Some(hoi4_data) = hoi4_data_opt {
        let _: Option<()> = check_descriptor(descriptor, &hoi4_data);
    }
}

fn check_descriptor(string: &str, hoi4_data: &serde_json::Value) -> Option<()> {
    let branches = hoi4_data
        .as_object()?
        .get(HOI4_ID)?
        .as_object()?
        .get("depots")?
        .as_object()?
        .get("branches")?
        .as_object()?;
    let versions = branches
        .keys()
        .filter_map(|k| {
            semver::Version::parse(k).ok().or_else(|| {
                // HOI4 sometimes uses 4-part versions (e.g. "1.17.3.0"); drop the last component.
                let trimmed = k.rsplit_once('.')?.0;
                semver::Version::parse(trimmed).ok()
            })
        })
        .collect::<Vec<_>>();
    let latest_version = versions.into_iter().max()?;

    let src: Arc<str> = Arc::from(string);
    let handler = miette::GraphicalReportHandler::new();

    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut fields = HashSet::new();
    for (key, _, value) in reader.fields() {
        // Check for duplicate keys.
        let key = key.read_string();
        if fields.contains(&key) && key != "replace_path" {
            let key_str = key.clone();
            let first_end = string.find(&key_str).map_or(0, |p| p + key_str.len());
            let offset = string
                .get(first_end..)
                .and_then(|s| s.find(&key_str))
                .map_or(0, |o| o + first_end);
            let key_len = key_str.len();
            let diag = DuplicateDescriptorKey {
                key: key_str,
                span: (offset, key_len).into(),
                src: NamedSource::new("descriptor.mod", Arc::clone(&src)),
            };
            let mut out = String::new();
            if handler.render_report(&mut out, &diag).is_ok() {
                eprint!("{out}");
            }
            continue;
        }

        // Check supported_version field.
        if key == "supported_version" {
            let req_str = value.read_string().ok()?;
            let req = semver::VersionReq::parse(&req_str).ok()?;
            if !req.matches(&latest_version) {
                let offset = find_offset(string, &req_str);
                let req_len = req_str.len();
                let diag = DescriptorVersionMismatch {
                    supported_version: req_str,
                    latest_version: latest_version.to_string(),
                    span: (offset, req_len).into(),
                    src: NamedSource::new("descriptor.mod", Arc::clone(&src)),
                };
                let mut out = String::new();
                if handler.render_report(&mut out, &diag).is_ok() {
                    eprint!("{out}");
                }
            }
        }

        // Track fields for later duplicate key check.
        fields.insert(key);
    }
    Some(())
}

fn vdf_value_to_json(value: &VdfValue) -> JsonValue {
    match value {
        VdfValue::Str(s) => JsonValue::String(s.to_string()),
        VdfValue::Obj(obj) => {
            let mut map = Map::new();
            for (key, values) in obj.iter() {
                let mut converted: Vec<JsonValue> = values.iter().map(vdf_value_to_json).collect();
                // VDF allows duplicate keys at the same level, stored as Vec.
                // Collapse single-element vecs to the bare value; keep arrays for duplicates.
                let entry = if converted.len() == 1 {
                    converted.remove(0)
                } else {
                    JsonValue::Array(converted)
                };
                map.insert(key.to_string(), entry);
            }
            JsonValue::Object(map)
        }
    }
}

#[must_use]
#[inline]
pub fn vdf_to_json(vdf: &Vdf) -> JsonValue {
    // Wrap the root key/value into a single-entry object so the top-level
    // "394360" key is preserved.
    let mut root = Map::new();
    root.insert(vdf.key.to_string(), vdf_value_to_json(&vdf.value));
    JsonValue::Object(root)
}

/// Reorders `focus = { ... }` blocks in `content` so that position `i` in the
/// file holds the block whose ID is `ordered_ids[i]`. Works from the end of
/// the file backward so byte offsets stay valid while replacing.
///
/// Returns `content` unchanged if the number of focus blocks found does not
/// match `ordered_ids.len()` (safety bail-out for unexpected file shapes).
fn reorder_focus_blocks(content: &str, ordered_ids: &[String]) -> String {
    let blocks = find_focus_blocks(content);

    if blocks.len() != ordered_ids.len() {
        return content.to_owned();
    }

    // All indices in `blocks` came from scanning `content` byte-by-byte, so
    // every `start..end` range is guaranteed to land on a char boundary.
    #[expect(
        clippy::string_slice,
        reason = "indices from find_focus_blocks are guaranteed ASCII-boundary positions"
    )]
    let block_map: HashMap<&str, &str> = blocks
        .iter()
        .map(|(start, end, id)| (id.as_str(), &content[*start..*end]))
        .collect();

    let mut result = content.to_owned();
    for (pos, (start, end, _)) in blocks.iter().enumerate().rev() {
        if let Some(target_id) = ordered_ids.get(pos)
            && let Some(&new_block) = block_map.get(target_id.as_str())
        {
            result.replace_range(*start..*end, new_block);
        }
    }

    result
}

/// Scans `content` for `focus = { ... }` blocks, returning `(start, end, id)`
/// tuples where the range includes any immediately preceding comment lines.
///
/// All byte indices produced here land on ASCII character boundaries (the only
/// meaningful chars are single-byte tokens), so callers may slice `content`
/// with them safely.
#[expect(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "all byte accesses are guarded by i < bytes.len() checks; \
              string slice indices are derived from ASCII token scanning so they \
              are guaranteed to fall on char boundaries"
)]
fn find_focus_blocks(content: &str) -> Vec<(usize, usize, String)> {
    let bytes = content.as_bytes();
    let mut blocks: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip line comments.
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip quoted strings.
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        // Match `focus` keyword preceded by whitespace (avoids `focus_tree`, etc.).
        let preceded_by_ws = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if preceded_by_ws && bytes[i..].starts_with(b"focus") {
            let mut j = i + 5;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'{' {
                    // Start the block at the beginning of the `focus` line
                    // (to include its indentation), then extend backward over
                    // any immediately preceding comment lines so they travel
                    // with the focus they introduce. Blank lines stop the scan
                    // so they remain as separators between blocks.
                    let line_start = content[..i].rfind('\n').map_or(0, |p| p + 1);
                    let mut block_start = line_start;
                    let mut scan_pos = line_start;
                    loop {
                        if scan_pos == 0 {
                            break;
                        }
                        let prev_nl = scan_pos - 1;
                        let prev_line_start = content[..prev_nl].rfind('\n').map_or(0, |p| p + 1);
                        if content[prev_line_start..prev_nl].trim().starts_with('#') {
                            block_start = prev_line_start;
                            scan_pos = prev_line_start;
                        } else {
                            break;
                        }
                    }
                    // Brace-match to find the closing `}`.
                    let mut depth: u32 = 0;
                    let mut m = j;
                    loop {
                        if m >= bytes.len() {
                            break;
                        }
                        match bytes[m] {
                            b'#' => {
                                while m < bytes.len() && bytes[m] != b'\n' {
                                    m += 1;
                                }
                            }
                            b'"' => {
                                m += 1;
                                while m < bytes.len() && bytes[m] != b'"' {
                                    m += 1;
                                }
                                if m < bytes.len() {
                                    m += 1;
                                }
                            }
                            b'{' => {
                                depth += 1;
                                m += 1;
                            }
                            b'}' => {
                                depth -= 1;
                                m += 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {
                                m += 1;
                            }
                        }
                    }
                    // Consume the newline that follows the closing brace.
                    if m < bytes.len() && bytes[m] == b'\n' {
                        m += 1;
                    }
                    if let Some(id) = extract_focus_id(&content[block_start..m]) {
                        blocks.push((block_start, m, id));
                    }
                    i = m;
                    continue;
                }
            }
        }
        i += 1;
    }
    blocks
}

fn extract_focus_id(block: &str) -> Option<String> {
    for line in block.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("id") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let raw = rest.trim();
                // Strip inline comment (e.g. `id = one # comment`).
                let raw = raw.split_once('#').map_or(raw, |(v, _)| v.trim());
                let value = raw.trim_matches('"');
                if !value.is_empty() && !value.contains(|c: char| c == '{' || c.is_whitespace()) {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}
