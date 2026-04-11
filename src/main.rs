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
    reason = "Mitigates excessive and sometimes conflicting warnings from `clippy::restriction`."
)]

use clap::Parser;
use miette::{Diagnostic, NamedSource, SourceSpan};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::fs::read_to_string;
use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::sync::Arc;
use walkdir::DirEntry;

/// Max number of diagnostics to print before truncating with ⋮.
const MAX_MISSING: u64 = 10;

/// HOI4 event block types whose fields require localisation.
const EVENT_TYPES: &[&str] = &[
    "country_event",
    "news_event",
    "operative_leader_event",
    "state_event",
    "unit_leader_event",
];

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("no files were processed (empty walk)")]
    EmptyWalk,
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
struct Args {
    /// Check all languages.
    #[arg(long, conflicts_with = "lang")]
    all: bool,

    /// Languages to check. May be repeated: --lang english --lang german.
    /// Defaults to english if neither --lang nor --all is given.
    #[arg(long, value_enum, conflicts_with = "all")]
    lang: Vec<Language>,

    /// Path to the mod directory. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: std::path::PathBuf,
}

impl Args {
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

/// "{key}" not localised in: {missing_langs}
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

#[derive(Debug, Clone)]
struct State {
    args: Args,
    keys: BTreeMap<Language, BTreeSet<String>>,
    things: BTreeMap<String, (std::path::PathBuf, usize)>,
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

    if !args.path.join("descriptor.mod").exists() {
        return Err(AppError::NotModDir(args.path.clone()));
    }

    println!("Checking path: {}", args.path.display());

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
    let keys: BTreeMap<Language, BTreeSet<String>> = keys
        .into_iter()
        .filter(|(lang, _)| active.contains(lang))
        .collect();

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
        "\nFound {} missing localisations (out of {}) in {:.2?}.",
        fmt_commas(missing),
        fmt_commas(things.len() as u64),
        start.elapsed()
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
                    && let Ok(scalar) = focus_value.read_scalar()
                {
                    let key_str = scalar.to_string();
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
