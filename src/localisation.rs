//! The localisation a mod defines: the keys of the `*l_<language>.yml`
//! files anywhere under its `localisation/` directory (`replace/` included),
//! which is where the game reads them from, plus those of the base game's
//! own localisation (see [`crate::game`]).

use crate::Language;
use crate::files;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

/// A localisation file of one of the checked languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocFile {
    /// Whether the file is the base game's rather than the mod's.
    pub game: bool,
    /// Index of the file's language among the checked languages.
    pub language: usize,
    /// Path used for I/O.
    pub path: PathBuf,
    /// Path relative to the mod root (or, for the game's files, to the
    /// game's folder, under `[game]`), for timings.
    pub relative: PathBuf,
    /// Size in bytes, to load the largest files first.
    pub size: u64,
}

/// The keys `text` (a localisation file) defines: the text before the first
/// `:` of every line after the first (the `l_<language>:` header), leading
/// whitespace trimmed. Reading stops at the first line that is not UTF-8.
fn file_keys(text: &[u8]) -> Vec<Box<str>> {
    let Some(header) = text.iter().position(|&byte| byte == b'\n') else {
        return Vec::new();
    };
    let body = text.get(header + 1..).unwrap_or_default();
    let mut keys = Vec::new();
    for line in body.split_inclusive(|&byte| byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            break;
        };
        if let Some(key) = line
            .trim_start()
            .split(':')
            .next()
            .filter(|key| !key.is_empty())
        {
            keys.push(key.into());
        }
    }
    keys
}

/// Every localisation file of `languages` (see the module docs) under
/// `root`, sorted by path. `span` is the parent of the spans timing the
/// walk.
pub fn files(root: &Path, languages: &[Language], span: &tracing::Span) -> Vec<LocFile> {
    let mut found: Vec<LocFile> = files::walk(&root.join("localisation"), span)
        .into_iter()
        .filter_map(|file| {
            let language = language_of(file.path.file_name()?.to_str()?, languages)?;
            let relative = file.path.strip_prefix(root).ok()?.to_path_buf();
            Some(LocFile {
                game: false,
                language,
                path: file.path,
                relative,
                size: file.size,
            })
        })
        .collect();
    found.sort_by(|a, b| a.relative.cmp(&b.relative));
    found
}

/// Every localisation file of `languages` in the base game at `game`,
/// except those in the folders the mod replaces (`replaced`, relative to the
/// game's folder, subfolders not included), sorted by path. `span` is the
/// parent of the spans timing the walk.
pub fn game_files(
    game: &Path,
    languages: &[Language],
    replaced: &[PathBuf],
    span: &tracing::Span,
) -> Vec<LocFile> {
    files(game, languages, span)
        .into_iter()
        .filter(|file| {
            file.relative
                .parent()
                .is_none_or(|folder| !replaced.iter().any(|replaced| replaced == folder))
        })
        .map(|file| LocFile {
            game: true,
            relative: Path::new("[game]").join(&file.relative),
            ..file
        })
        .collect()
}

/// Which of the checked languages define each of `wanted` (sorted, without
/// repeats): bit `l` of a key's mask is set if language `l` does. `defined`
/// holds the keys each of `files` defines (see [`load`]). `span` is the
/// parent of the spans timing the lookup.
pub fn defined(
    wanted: &[&str],
    files: &[LocFile],
    defined: &[Vec<Box<str>>],
    span: &tracing::Span,
) -> Vec<u16> {
    let masks: Vec<AtomicU16> = std::iter::repeat_with(AtomicU16::default)
        .take(wanted.len())
        .collect();
    files.par_iter().zip(defined).for_each(|(file, keys)| {
        let _span = tracing::info_span!(parent: span, "lookup").entered();
        let bit = 1_u16.checked_shl(u32::try_from(file.language).unwrap_or(u32::MAX));
        for key in keys {
            if let (Ok(found), Some(bit)) = (wanted.binary_search(&&**key), bit)
                && let Some(mask) = masks.get(found)
            {
                mask.fetch_or(bit, Ordering::Relaxed);
            }
        }
    });
    masks.into_iter().map(AtomicU16::into_inner).collect()
}

/// The keys `file` defines (see [`file_keys`]); none if it cannot be read.
pub fn load(file: &LocFile) -> Vec<Box<str>> {
    let text = tracing::info_span!("read")
        .in_scope(|| std::fs::read(&file.path))
        .unwrap_or_default();
    tracing::info_span!("keys").in_scope(|| file_keys(&text))
}

/// Which of `languages` a file named `name` holds the localisation of: its
/// name ends with `l_<language>.yml`. Like the game, whatever comes before
/// is not checked: Kaiserreich names most of its files like
/// `ACC - American Constitutional Coalition l_english.yml`.
fn language_of(name: &str, languages: &[Language]) -> Option<usize> {
    let stem = name.strip_suffix(".yml")?;
    languages.iter().position(|language| {
        stem.strip_suffix(language.as_str())
            .is_some_and(|rest| rest.ends_with("l_"))
    })
}

#[cfg(test)]
mod tests {
    use super::{LocFile, defined, file_keys, language_of};
    use crate::Language;
    use std::path::PathBuf;

    /// The keys files of each language define mark those languages in each
    /// wanted key's mask; other keys are ignored.
    #[test]
    fn masks_mark_the_languages_defining_each_key() {
        let file = |language: usize| LocFile {
            game: false,
            language,
            path: PathBuf::new(),
            relative: PathBuf::new(),
            size: 0,
        };
        let keys =
            |keys: &[&str]| -> Vec<Box<str>> { keys.iter().map(|&key| key.into()).collect() };
        let files = [file(0), file(1), file(0), file(2)];
        let found = [
            keys(&["a", "b", "unwanted"]),
            keys(&["b"]),
            keys(&["c"]),
            keys(&["a", "a"]),
        ];
        assert_eq!(
            defined(
                &["a", "b", "c", "d"],
                &files,
                &found,
                &tracing::Span::none()
            ),
            [0b101, 0b011, 0b001, 0b000]
        );
    }

    #[test]
    fn keys_are_the_text_before_the_first_colon() {
        let text = "\u{feff}l_english:\n KEY_A:0 \"A: a\"\n\tKEY_B: \"B\"\r\n # comment: here\n\n  \nno_colon\nKEY_C :0 \"C\"\nlast:1 \"no newline\"";
        let keys: Vec<Box<str>> = [
            "KEY_A",
            "KEY_B",
            "# comment",
            "no_colon\n",
            "KEY_C ",
            "last",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        assert_eq!(file_keys(text.as_bytes()), keys);
    }

    #[test]
    fn the_header_line_is_skipped() {
        assert_eq!(
            file_keys(b"KEY:0 \"only a header\""),
            Vec::<Box<str>>::new()
        );
        assert_eq!(file_keys(b""), Vec::<Box<str>>::new());
        assert_eq!(
            file_keys(b"l_english:\nA:0 \"a\"\n"),
            vec![Box::<str>::from("A")]
        );
    }

    #[test]
    fn reading_stops_at_the_first_line_that_is_not_utf8() {
        let text = b"l_english:\nA:0 \"a\"\nB:0 \"\xff\"\nC:0 \"c\"\n";
        assert_eq!(file_keys(text), vec![Box::<str>::from("A")]);
    }

    #[test]
    fn file_names_give_the_language() {
        let languages = [Language::English, Language::German];
        assert_eq!(language_of("hearty_l_english.yml", &languages), Some(0));
        assert_eq!(language_of("_l_german.yml", &languages), Some(1));
        assert_eq!(
            language_of("hearty_replace_l_german.yml", &languages),
            Some(1)
        );
        // Whatever precedes `l_<language>` is not checked, as in the game.
        for name in [
            "ACC - American Constitutional Coalition l_english.yml",
            "00 Bookmarks l_english.yml",
            "l_english.yml",
        ] {
            assert_eq!(language_of(name, &languages), Some(0), "{name}");
        }
        for name in [
            // Not a checked language.
            "hearty_l_french.yml",
            // Not `.yml`.
            "hearty_l_english.txt",
            "hearty_l_english.yml.bak",
            "hearty_l_english",
            // No `l_` before the language.
            "hearty_english.yml",
            "hearty_l_panel.yml",
        ] {
            assert_eq!(language_of(name, &languages), None, "{name}");
        }
    }
}
