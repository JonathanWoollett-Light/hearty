#![warn(clippy::restriction)]
#![allow(clippy::single_call_fn)]

use clap::Parser;
use miette::{Diagnostic, NamedSource, SourceSpan};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs::OpenOptions;
use std::fs::read_to_string;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use walkdir::DirEntry;

#[derive(Debug, Hash, PartialEq, Eq, Clone, clap::ValueEnum, PartialOrd, Ord)]
#[clap(rename_all = "snake_case")]
enum Language {
    English,
    German,
    French,
    Spanish,
    Russian,
    Chinese,
    Japanese,
    BrazilianPortuguese,
    Polish,
    Korean,
}
impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Language {
    fn as_str(&self) -> &'static str {
        match self {
            Language::English => "english",
            Language::German => "german",
            Language::French => "french",
            Language::Spanish => "spanish",
            Language::Russian => "russian",
            Language::Chinese => "simp_chinese",
            Language::Japanese => "japanese",
            Language::BrazilianPortuguese => "braz_por",
            Language::Polish => "polish",
            Language::Korean => "korean",
        }
    }
}

impl PartialEq<str> for Language {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl TryFrom<&str> for Language {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "english" => Ok(Language::English),
            "german" => Ok(Language::German),
            "french" => Ok(Language::French),
            "spanish" => Ok(Language::Spanish),
            "russian" => Ok(Language::Russian),
            "simp_chinese" => Ok(Language::Chinese),
            "japanese" => Ok(Language::Japanese),
            "braz_por" => Ok(Language::BrazilianPortuguese),
            "polish" => Ok(Language::Polish),
            "korean" => Ok(Language::Korean),
            _ => Err(format!("Unknown language: {value}")),
        }
    }
}

const MAX_MISSING: u64 = 10;

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Path to the mod directory. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: std::path::PathBuf,

    /// Languages to check. May be repeated: --lang english --lang german.
    /// Defaults to english if neither --lang nor --all is given.
    #[arg(long, value_enum, conflicts_with = "all")]
    lang: Vec<Language>,

    /// Check all languages.
    #[arg(long, conflicts_with = "lang")]
    all: bool,
}

impl Args {
    fn active_languages(&self) -> Vec<Language> {
        if self.all {
            vec![
                Language::English,
                Language::German,
                Language::French,
                Language::Spanish,
                Language::Russian,
                Language::Chinese,
                Language::Japanese,
                Language::BrazilianPortuguese,
                Language::Polish,
                Language::Korean,
            ]
        } else if self.lang.is_empty() {
            vec![Language::English]
        } else {
            self.lang.clone()
        }
    }
}

fn main() -> Result<(), String> {
    if let Err(e) = inner_main() {
        Err(format!("{e}"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct MissingLocalisation {
    #[source_code]
    src: NamedSource<Arc<String>>,
    #[label("missing localisation")]
    span: SourceSpan,
    key: String,
    missing: Vec<String>,
}

impl std::fmt::Display for MissingLocalisation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "\"{}\" not localised in: {}", self.key, self.missing.join(", "))
    }
}

impl std::error::Error for MissingLocalisation {}

fn fmt_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn inner_main() -> Result<(), Box<dyn Error>> {
    let start = std::time::Instant::now();
    let args = Args::parse();

    if !args.path.join("descriptor.mod").exists() {
        return Err(format!(
            "{} does not appear to be a mod directory (no descriptor.mod found)",
            args.path.display()
        )
        .into());
    }

    let walker = walkdir::WalkDir::new(&args.path);
    let State { keys, things, .. } = walker
        .into_iter()
        .par_bridge()
        .try_fold(
            || State {
                keys: BTreeMap::new(),
                things: BTreeMap::new(),
                args: args.clone(),
            },
            iter_entry,
        )
        .try_reduce_with(
            |State {
                 mut keys,
                 mut things,
                 args,
             },
             State {
                 keys: item_keys,
                 things: item_things,
                 ..
             }| {
                for (lang, lang_keys) in item_keys {
                    if let Some(x) = keys.get_mut(&lang) {
                        x.extend(lang_keys);
                    } else {
                        keys.insert(lang, lang_keys);
                    }
                }
                things.extend(item_things);
                Ok(State { keys, things, args })
            },
        )
        .unwrap()
        .unwrap();

    let active: BTreeSet<Language> = args.active_languages().into_iter().collect();
    let keys: BTreeMap<Language, BTreeSet<String>> = keys
        .into_iter()
        .filter(|(lang, _)| active.contains(lang))
        .collect();

    let handler = miette::GraphicalReportHandler::new();
    let mut file_cache: BTreeMap<std::path::PathBuf, Arc<String>> = BTreeMap::new();
    let mut missing = 0u64;
    let mut printed = 0u64;
    for (thing, (source, offset)) in &things {
        let missing_langs: Vec<String> = keys.iter()
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
        let content = file_cache
            .entry(source.clone())
            .or_insert_with(|| Arc::new(read_to_string(source).unwrap_or_default()));
        let diag = MissingLocalisation {
            src: NamedSource::new(source.display().to_string(), Arc::clone(content)),
            span: (*offset, thing.len()).into(),
            key: thing.clone(),
            missing: missing_langs,
        };
        let mut out = String::new();
        handler.render_report(&mut out, &diag).unwrap();
        eprint!("{out}");
    }
    if missing > MAX_MISSING {
        eprintln!("⋮ ({} more not shown)", missing - MAX_MISSING);
    }

    println!(
        "\nFound {} missing localisations (out of {}) in {:.2?} seconds.",
        fmt_commas(missing),
        fmt_commas(things.len() as u64),
        start.elapsed()
    );
    if missing > 0 {
        Err("Missing localisations found".into())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct State {
    keys: BTreeMap<Language, BTreeSet<String>>,
    things: BTreeMap<String, (std::path::PathBuf, usize)>,
    args: Args,
}

fn iter_entry(
    State {
        mut keys,
        mut things,
        args,
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
            if let Some(x) = keys.get_mut(&lang) {
                x.extend(lang_keys);
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
    // Note: things.extend() on BTreeMap silently overwrites on duplicate keys,
    // which is fine — same key in multiple files just keeps one source path.

    Ok(State { keys, things, args })
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
            keys.insert(key.to_string());
            // TODO Check for duplicates.
        }
        line.clear();
    }

    Some((lang, keys))
}

const EVENT_TYPES: &[&str] = &[
    "country_event",
    "news_event",
    "state_event",
    "unit_leader_event",
    "operative_leader_event",
];

fn find_offset(haystack: &str, needle: &str) -> usize {
    haystack.find(needle).unwrap_or(0)
}

fn read_events(path: &Path) -> Option<BTreeMap<String, (std::path::PathBuf, usize)>> {
    let string = read_to_string(path).ok()?;
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut events = BTreeMap::new();

    for (key, _op, value) in reader.fields() {
        if !EVENT_TYPES.contains(&key.read_str().as_ref()) {
            continue;
        }
        let Ok(obj) = value.read_object() else { continue };
        for (inner_key, _op, inner_value) in obj.fields() {
            match inner_key.read_str().as_ref() {
                "title" | "desc" => {
                    if let Ok(scalar) = inner_value.read_scalar() {
                        let s = scalar.to_string();
                        let offset = find_offset(&string, &s);
                        events.insert(s, (path.to_path_buf(), offset));
                    }
                }
                "option" => {
                    let Ok(option) = inner_value.read_object() else { continue };
                    for (opt_key, _op, opt_value) in option.fields() {
                        if opt_key.read_str() == "name"
                            && let Ok(scalar) = opt_value.read_scalar()
                        {
                            let s = scalar.to_string();
                            let offset = find_offset(&string, &s);
                            events.insert(s, (path.to_path_buf(), offset));
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

    for (key, _op, value) in reader.fields() {
        if key.read_str() != "focus_tree" {
            continue;
        }
        let Ok(tree) = value.read_object() else { continue };
        for (tree_key, _op, tree_value) in tree.fields() {
            if tree_key.read_str() != "focus" {
                continue;
            }
            let Ok(focus) = tree_value.read_object() else { continue };
            for (focus_key, _op, focus_value) in focus.fields() {
                if focus_key.read_str() == "id"
                    && let Ok(scalar) = focus_value.read_scalar()
                {
                    let s = scalar.to_string();
                    let offset = find_offset(&string, &s);
                    focuses.insert(s, (path.to_path_buf(), offset));
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

    for (key, _op, value) in reader.fields() {
        if key.read_str() != "technologies" {
            continue;
        }
        let Ok(block) = value.read_object() else {
            continue;
        };
        for (tech_key, _op, _) in block.fields() {
            let s = tech_key.read_str();
            if s.starts_with('@') {
                continue;
            }
            let offset = find_offset(&string, &s);
            techs.insert(s.into_owned(), (path.to_path_buf(), offset));
        }
    }

    Some(techs)
}
