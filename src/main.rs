use clap::Parser;
use rayon::prelude::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fs::OpenOptions;
use std::fs::read_to_string;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::path::Path;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};
use walkdir::DirEntry;

#[derive(Debug, Hash, PartialEq, Eq, Clone, clap::ValueEnum)]
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

const MAX_MISSING: u64 = 200;

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

    println!("Checking path: {}", args.path.display());

    let walker = walkdir::WalkDir::new(&args.path);
    let State { keys, things, .. } = walker
        .into_iter()
        .par_bridge()
        .try_fold(
            || State {
                keys: HashMap::new(),
                things: HashMap::new(),
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
                for (lang, lang_keys) in item_keys.into_iter() {
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

    let active: HashSet<Language> = args.active_languages().into_iter().collect();
    let keys: HashMap<Language, HashSet<String>> = keys
        .into_iter()
        .filter(|(lang, _)| active.contains(lang))
        .collect();

    let bufwtr = BufferWriter::stdout(ColorChoice::Always);
    let path_width = things
        .values()
        .map(|(p, line)| format!("{}:{line}", p.display()).len())
        .max()
        .unwrap_or(0);
    let thing_width = things.keys().map(|t| t.len()).max().unwrap_or(0);
    let lang_width = keys.keys().map(|l| l.as_str().len()).max().unwrap_or(0);
    let mut missing = 0u64;
    for (thing, (source, line)) in &things {
        if keys.values().all(|lang_keys| lang_keys.contains(thing)) {
            continue;
        }
        missing += 1;
        if missing > MAX_MISSING {
            continue;
        }
        let mut buffer = bufwtr.buffer();
        write!(
            &mut buffer,
            "{:<path_width$}  {thing:<thing_width$}",
            format!("{}:{line}", source.display())
        )
        .unwrap();
        for (lang, lang_keys) in &keys {
            let color = if lang_keys.contains(thing) {
                Color::Green
            } else {
                Color::Red
            };
            buffer
                .set_color(ColorSpec::new().set_fg(Some(color)))
                .unwrap();
            write!(&mut buffer, "  {lang:<lang_width$}").unwrap();
        }
        buffer.reset().unwrap();
        writeln!(&mut buffer).unwrap();
        bufwtr.print(&buffer).unwrap();
    }
    if missing > MAX_MISSING {
        let mut buffer = bufwtr.buffer();
        let more_msg = format!("⋮ ({} more)", missing - MAX_MISSING);
        write!(&mut buffer, "{more_msg:<path_width$}  {:<thing_width$}", "").unwrap();
        for _ in &keys {
            buffer.reset().unwrap();
            write!(&mut buffer, "  {:<lang_width$}", "⋮").unwrap();
        }
        buffer.reset().unwrap();
        writeln!(&mut buffer).unwrap();
        bufwtr.print(&buffer).unwrap();
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
    keys: HashMap<Language, HashSet<String>>,
    things: HashMap<String, (std::path::PathBuf, u32)>,
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
    // Note: things.extend() on HashMap silently overwrites on duplicate keys,
    // which is fine — same key in multiple files just keeps one source path.

    Ok(State { keys, things, args })
}

fn load_localisation(lang: Language, path: &Path) -> Option<(Language, HashSet<String>)> {
    let mut file = BufReader::new(OpenOptions::new().read(true).open(path).ok()?);
    file.skip_until(b'\n').ok()?; // skip language header line

    let mut line = String::new();
    let mut keys = HashSet::new();
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

/// Builds a vec of byte offsets where each line starts. Built once per file.
fn build_line_index(s: &str) -> Vec<usize> {
    let mut idx = vec![0];
    idx.extend(s.match_indices('\n').map(|(i, _)| i + 1));
    idx
}

/// Returns the 1-based line number for a byte offset via binary search.
fn line_of_offset(index: &[usize], offset: usize) -> u32 {
    index.partition_point(|&o| o <= offset) as u32
}

fn find_line(haystack: &str, index: &[usize], needle: &str) -> u32 {
    haystack
        .find(needle)
        .map(|offset| line_of_offset(index, offset))
        .unwrap_or(1)
}

fn read_events(path: &Path) -> Option<HashMap<String, (std::path::PathBuf, u32)>> {
    let string = read_to_string(path).ok()?;
    let line_index = build_line_index(&string);
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut events = HashMap::new();

    for (key, _op, value) in reader.fields() {
        if !EVENT_TYPES.contains(&key.read_str().as_ref()) {
            continue;
        }
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (inner_key, _op, inner_value) in obj.fields() {
            match inner_key.read_str().as_ref() {
                "title" | "desc" => {
                    if let Ok(scalar) = inner_value.read_scalar() {
                        let s = scalar.to_string();
                        let line = find_line(&string, &line_index, &s);
                        events.insert(s, (path.to_path_buf(), line));
                    }
                }
                "option" => {
                    let Ok(option) = inner_value.read_object() else {
                        continue;
                    };
                    for (opt_key, _op, opt_value) in option.fields() {
                        if opt_key.read_str() == "name"
                            && let Ok(scalar) = opt_value.read_scalar()
                        {
                            let s = scalar.to_string();
                            let line = find_line(&string, &line_index, &s);
                            events.insert(s, (path.to_path_buf(), line));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Some(events)
}

fn read_focuses(path: &Path) -> Option<HashMap<String, (std::path::PathBuf, u32)>> {
    let string = read_to_string(path).ok()?;
    let line_index = build_line_index(&string);
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut focuses = HashMap::new();

    for (key, _op, value) in reader.fields() {
        if key.read_str() != "focus_tree" {
            continue;
        }
        let Ok(tree) = value.read_object() else {
            continue;
        };
        for (tree_key, _op, tree_value) in tree.fields() {
            if tree_key.read_str() != "focus" {
                continue;
            }
            let Ok(focus) = tree_value.read_object() else {
                continue;
            };
            for (focus_key, _op, focus_value) in focus.fields() {
                if focus_key.read_str() == "id"
                    && let Ok(scalar) = focus_value.read_scalar()
                {
                    let s = scalar.to_string();
                    let line = find_line(&string, &line_index, &s);
                    focuses.insert(s, (path.to_path_buf(), line));
                }
            }
        }
    }

    Some(focuses)
}

fn read_technologies(path: &Path) -> Option<HashMap<String, (std::path::PathBuf, u32)>> {
    let string = read_to_string(path).ok()?;
    let line_index = build_line_index(&string);
    let tape = jomini::TextTape::from_slice(string.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut techs = HashMap::new();

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
            let line = find_line(&string, &line_index, &s);
            techs.insert(s.into_owned(), (path.to_path_buf(), line));
        }
    }

    Some(techs)
}
