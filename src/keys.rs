//! The localisation keys a mod's script files use: an event's `title`,
//! `desc` and option `name`s (`events/*.txt`), a focus's `id`
//! (`common/national_focus/*.txt`) and every technology
//! (`common/technologies/*.txt`).
//!
//! A value in square brackets (`title = "[GetTitle]"`) is not a key but
//! scripted localisation, which the game evaluates in place of the missing
//! key (vanilla does this), so it is skipped.
//!
//! Keys are read from the CST with the span that defines them. A file the
//! CST cannot parse is read with jomini instead, which accepts more malformed
//! input, so a stray brace does not hide a file's keys; those keys have no
//! span.

use crate::cst::{Block, Document, Scalar, Span, Value};
use crate::schema::{EVENT_TYPES, FileKind};

/// A localisation key a script file uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyUse {
    /// The key, quotes stripped.
    pub key: String,
    /// Where the file defines it (quotes excluded), if known.
    pub span: Option<Span>,
}

/// The blocks among `values`.
fn blocks<'block, I>(values: I) -> impl Iterator<Item = &'block Block>
where
    I: Iterator<Item = &'block Value>,
{
    values.filter_map(|value| match value {
        Value::Block(block) => Some(block),
        Value::Scalar(_) | Value::Tagged { .. } => None,
    })
}

/// The values of the entries of `block` whose key is `key`.
fn entries<'block>(
    src: &str,
    block: &'block Block,
    key: &str,
) -> impl Iterator<Item = &'block Value> {
    block
        .entries
        .iter()
        .filter(move |entry| entry.key_str(src) == Some(key))
        .map(|entry| &entry.value)
}

/// The localisation keys `doc` (a script file of kind `file`) uses, in
/// source order.
pub fn from_doc(doc: &Document<'_>, file: FileKind) -> Vec<KeyUse> {
    let src = doc.src;
    let mut keys = Vec::new();
    let mut push = |scalar: &Scalar| {
        if is_key(scalar.unquoted(src)) {
            keys.push(key_use(src, scalar));
        }
    };
    match file {
        FileKind::Events => {
            for entry in &doc.root.entries {
                let (Some(key), Value::Block(event)) = (entry.key_str(src), &entry.value) else {
                    continue;
                };
                if !EVENT_TYPES.contains(&key) {
                    continue;
                }
                for field in &event.entries {
                    match (field.key_str(src), &field.value) {
                        (Some("title" | "desc"), Value::Scalar(scalar)) => push(scalar),
                        (Some("option"), Value::Block(option)) => {
                            for name in entries(src, option, "name") {
                                if let Value::Scalar(scalar) = name {
                                    push(scalar);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        FileKind::NationalFocus => {
            for tree in blocks(entries(src, &doc.root, "focus_tree")) {
                for focus in blocks(entries(src, tree, "focus")) {
                    for id in entries(src, focus, "id") {
                        if let Value::Scalar(scalar) = id {
                            push(scalar);
                        }
                    }
                }
            }
        }
        FileKind::Technologies => {
            for technologies in blocks(entries(src, &doc.root, "technologies")) {
                for technology in &technologies.entries {
                    if let Some(key) = technology.key
                        && !key.unquoted(src).starts_with('@')
                    {
                        push(&key);
                    }
                }
            }
        }
        FileKind::Characters
        | FileKind::DecisionCategories
        | FileKind::Decisions
        | FileKind::Ideas
        | FileKind::Other => {}
    }
    keys
}

/// The localisation keys `text` (a script file of kind `file` that the CST
/// cannot parse) uses, read with jomini, or nothing if jomini cannot parse
/// it either. The keys have no span.
pub fn from_jomini(text: &str, file: FileKind) -> Vec<KeyUse> {
    let Ok(tape) = jomini::TextTape::from_slice(text.as_bytes()) else {
        return Vec::new();
    };
    let reader = tape.windows1252_reader();
    let mut keys = Vec::new();
    let mut push = |key: String| {
        if is_key(&key) {
            keys.push(KeyUse { key, span: None });
        }
    };
    match file {
        FileKind::Events => {
            for (key, _, value) in reader.fields() {
                if !EVENT_TYPES.contains(&key.read_str().as_ref()) {
                    continue;
                }
                let Ok(event) = value.read_object() else {
                    continue;
                };
                for (field, _, value) in event.fields() {
                    match field.read_str().as_ref() {
                        "title" | "desc" => {
                            if let Ok(scalar) = value.read_scalar() {
                                push(scalar.to_string());
                            }
                        }
                        "option" => {
                            let Ok(option) = value.read_object() else {
                                continue;
                            };
                            for (option_key, _, option_value) in option.fields() {
                                if option_key.read_str() == "name"
                                    && let Ok(scalar) = option_value.read_scalar()
                                {
                                    push(scalar.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        FileKind::NationalFocus => {
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
                            && let Ok(id) = focus_value.read_string()
                        {
                            push(id);
                        }
                    }
                }
            }
        }
        FileKind::Technologies => {
            for (key, _, value) in reader.fields() {
                if key.read_str() != "technologies" {
                    continue;
                }
                let Ok(block) = value.read_object() else {
                    continue;
                };
                for (technology, _, _) in block.fields() {
                    let technology = technology.read_str();
                    if !technology.starts_with('@') {
                        push(technology.into_owned());
                    }
                }
            }
        }
        FileKind::Characters
        | FileKind::DecisionCategories
        | FileKind::Decisions
        | FileKind::Ideas
        | FileKind::Other => {}
    }
    keys
}

/// Whether `value` (unquoted) is a localisation key: anything but
/// scripted localisation, `[...]` (see the module docs).
fn is_key(value: &str) -> bool {
    !value.starts_with('[')
}

/// The key a scalar holds, with the span of its text inside any quotes.
fn key_use(src: &str, scalar: &Scalar) -> KeyUse {
    let span = if scalar.quoted && scalar.span.end >= scalar.span.start + 2 {
        Span {
            end: scalar.span.end - 1,
            start: scalar.span.start + 1,
        }
    } else {
        scalar.span
    };
    KeyUse {
        key: scalar.unquoted(src).to_owned(),
        span: Some(span),
    }
}

/// Whether files of kind `file` use localisation keys.
pub const fn uses_keys(file: FileKind) -> bool {
    matches!(
        file,
        FileKind::Events | FileKind::NationalFocus | FileKind::Technologies
    )
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{KeyUse, from_doc, from_jomini, uses_keys};
    use crate::cst::{self, Span};
    use crate::schema::FileKind;

    /// The keys of `src` with the text at each span, which must be the key.
    fn keys(src: &str, file: FileKind) -> Vec<String> {
        let doc = cst::parse(src).expect("parses");
        from_doc(&doc, file)
            .into_iter()
            .map(|KeyUse { key, span }| {
                let span = span.expect("a span");
                assert_eq!(span.text(src), key, "{src:?}");
                key
            })
            .collect()
    }

    #[test]
    fn event_titles_descriptions_and_option_names() {
        let src = "add_namespace = e\n\
            country_event = {\n\tid = e.1\n\ttitle = e.1.t\n\tdesc = \"e.1.d\"\n\
            \tdesc = { text = e.1.d2 trigger = { always = yes } }\n\
            \toption = { name = e.1.a }\n\toption = { name = e.1.b name = e.1.c }\n\
            \toption = odd\n}\n\
            news_event = { id = e.2 title = e.2.t }\n\
            other = { title = no }\n";
        assert_eq!(
            keys(src, FileKind::Events),
            ["e.1.t", "e.1.d", "e.1.a", "e.1.b", "e.1.c", "e.2.t"]
        );
        // Only in event files.
        assert_eq!(keys(src, FileKind::Other), Vec::<String>::new());
    }

    #[test]
    fn focus_ids_and_technologies() {
        let src = "focus_tree = {\n\tid = tree\n\tfocus = {\n\t\tid = a\n\t\tprerequisite = { focus = b }\n\t}\n\
            \tfocus = { id = \"b\" }\n}\nshared_focus = { id = s }\nfocus_tree = odd\n";
        assert_eq!(keys(src, FileKind::NationalFocus), ["a", "b"]);
        let src = "technologies = {\n\t@cost = 1\n\ttech_a = { research_cost = @cost }\n\
            \t\"tech_b\" = { }\n\ttech_c = yes\n}\n";
        assert_eq!(
            keys(src, FileKind::Technologies),
            ["tech_a", "tech_b", "tech_c"]
        );
    }

    /// Scripted localisation in place of a key is not a key, quoted or not;
    /// jomini reads an unquoted one as `[`, which is skipped too.
    #[test]
    fn scripted_localisation_is_not_a_key() {
        let src = "country_event = {\n\tid = s.1\n\ttitle = \"[GetTitle]\"\n\tdesc = s.1.d\n\
            \toption = { name = [scripted_a] }\n\toption = { name = s.1.a }\n}\n";
        assert_eq!(keys(src, FileKind::Events), ["s.1.d", "s.1.a"]);
        let keys: Vec<String> = from_jomini(src, FileKind::Events)
            .into_iter()
            .map(|used| used.key)
            .collect();
        assert_eq!(keys, ["s.1.d", "s.1.a"]);
    }

    /// The span is where the key is defined, not where it first appears.
    #[test]
    fn spans_point_at_the_definition() {
        let src = "# e.1.t is the title\ncountry_event = {\n\tid = e.1\n\ttitle = e.1.t\n}\n";
        let doc = cst::parse(src).expect("parses");
        let found = from_doc(&doc, FileKind::Events);
        let start = src.rfind("e.1.t").expect("title");
        assert_eq!(
            found,
            [KeyUse {
                key: "e.1.t".to_owned(),
                span: Some(Span {
                    end: start + 5,
                    start
                })
            }]
        );
    }

    /// A file the CST rejects is read with jomini, which tolerates a
    /// missing closing brace.
    #[test]
    fn unparsable_files_fall_back_to_jomini() {
        let src =
            "country_event = {\n\tid = u.1\n\ttitle = u.1.t\n\toption = {\n\t\tname = u.1.a\n}\n";
        cst::parse(src).expect_err("the CST rejects it");
        let found: Vec<(String, Option<Span>)> = from_jomini(src, FileKind::Events)
            .into_iter()
            .map(|KeyUse { key, span }| (key, span))
            .collect();
        assert_eq!(
            found,
            [("u.1.t".to_owned(), None), ("u.1.a".to_owned(), None)]
        );
        let focus = "focus_tree = {\n\tfocus = {\n\t\tid = f\n\t}\n";
        assert_eq!(
            from_jomini(focus, FileKind::NationalFocus),
            [KeyUse {
                key: "f".to_owned(),
                span: None
            }]
        );
        let tech = "technologies = {\n\t@x = 1\n\tt = { }\n";
        assert_eq!(
            from_jomini(tech, FileKind::Technologies),
            [KeyUse {
                key: "t".to_owned(),
                span: None
            }]
        );
        assert_eq!(from_jomini("}}}", FileKind::Events), []);
        assert_eq!(from_jomini(focus, FileKind::Ideas), []);
    }

    /// Reads the keys of every events, focus and technology file of each
    /// `;`-separated root in `HEARTY_CORPUS` both from the CST and with
    /// jomini (as hearty did before it had a CST), and prints where they
    /// differ. Fails if a file jomini reads has keys the CST misses, other
    /// than jomini's misreadings: it splits `[scripted_loc]` scalars at `[`,
    /// and shows a non-ASCII scalar as `non-ascii string of N length`.
    ///
    /// Run with `cargo test --release corpus_keys -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_keys_match_jomini() {
        use rayon::prelude::*;
        use std::collections::BTreeSet;
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let mut missed = 0_usize;
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            let root = std::path::Path::new(root);
            let files = crate::files::scripts(root, &tracing::Span::none());
            let reports: Vec<(usize, String)> = files
                .par_iter()
                .filter(|file| uses_keys(file.kind))
                .filter_map(|file| {
                    let text = std::fs::read_to_string(&file.path).ok()?;
                    let old: BTreeSet<String> = from_jomini(&text, file.kind)
                        .into_iter()
                        .map(|used| used.key)
                        .collect();
                    let Ok(doc) = cst::parse(&text) else {
                        return Some((0, format!("{}: does not parse", file.relative.display())));
                    };
                    let new: BTreeSet<String> = from_doc(&doc, file.kind)
                        .into_iter()
                        .map(|used| used.key)
                        .collect();
                    let misread = |key: &&String| {
                        key.starts_with("non-ascii string of ") || key.contains(['[', ']'])
                    };
                    let only_jomini: Vec<&String> = old.difference(&new).collect();
                    let missed = only_jomini.iter().filter(|key| !misread(key)).count();
                    (old != new).then(|| {
                        (
                            missed,
                            format!(
                                "{}: only jomini {only_jomini:?}, only CST {:?}",
                                file.relative.display(),
                                new.difference(&old).collect::<Vec<_>>()
                            ),
                        )
                    })
                })
                .collect();
            println!("== {}: {} files differ", root.display(), reports.len());
            for (file_missed, report) in &reports {
                println!("    {report}");
                missed += file_missed;
            }
        }
        assert_eq!(missed, 0, "the CST misses keys jomini reads");
    }

    #[test]
    fn only_events_focuses_and_technologies_use_keys() {
        assert!(uses_keys(FileKind::Events));
        assert!(uses_keys(FileKind::NationalFocus));
        assert!(uses_keys(FileKind::Technologies));
        assert!(!uses_keys(FileKind::Decisions));
        assert!(!uses_keys(FileKind::Other));
    }
}
