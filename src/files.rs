//! Finding a mod's files: its script files and its localisation.

use crate::schema::{self, FileKind};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

/// Top-level mod directories holding the script files that `--format`,
/// `--check`, `--fix` and the lint handle.
const SCRIPT_DIRS: &[&str] = &["common", "events", "history"];

/// A file found by [`walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Path used for I/O.
    pub path: PathBuf,
    /// Size in bytes, as the directory listing gave it.
    pub size: u64,
}

/// A script file to format, fix or lint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptFile {
    pub kind: FileKind,
    /// Absolute (or `PATH`-relative) path used for I/O.
    pub path: PathBuf,
    /// Path relative to the mod root, used for display.
    pub relative: PathBuf,
    /// Size in bytes, to process the largest files first.
    pub size: u64,
}

/// Every `*.txt` file under the mod's [`SCRIPT_DIRS`] that
/// [`schema::file_kind`] classifies, sorted by relative path. `span` is the
/// parent of the spans timing the walk.
pub fn scripts(root: &Path, span: &tracing::Span) -> Vec<ScriptFile> {
    let mut files: Vec<ScriptFile> = SCRIPT_DIRS
        .par_iter()
        .flat_map_iter(|dir| walk(&root.join(dir), span))
        .filter_map(|found| {
            let relative = found.path.strip_prefix(root).ok()?.to_path_buf();
            let kind = schema::file_kind(&relative)?;
            Some(ScriptFile {
                kind,
                path: found.path,
                relative,
                size: found.size,
            })
        })
        .collect();
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    files
}

/// Every entry under `dir` that is not a directory, in no particular order,
/// skipping what cannot be read (a missing `dir`, an unreadable
/// subdirectory). Symbolic links are not followed. The subdirectories of
/// `dir` are walked in parallel. `span` is the parent of the spans timing
/// the walk.
pub fn walk(dir: &Path, span: &tracing::Span) -> Vec<Found> {
    let found = |entry: &walkdir::DirEntry| Found {
        path: entry.path().to_path_buf(),
        size: entry.metadata().map_or(0, |metadata| metadata.len()),
    };
    let (subdirectories, mut files): (Vec<walkdir::DirEntry>, Vec<walkdir::DirEntry>) =
        tracing::info_span!(parent: span, "walk").in_scope(|| {
            walkdir::WalkDir::new(dir)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .filter_map(Result::ok)
                .partition(|entry| entry.file_type().is_dir())
        });
    let nested: Vec<Found> = subdirectories
        .par_iter()
        .flat_map_iter(|subdirectory| {
            let _span = tracing::info_span!(parent: span, "walk").entered();
            walkdir::WalkDir::new(subdirectory.path())
                .min_depth(1)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| !entry.file_type().is_dir())
                .map(|entry| found(&entry))
                .collect::<Vec<_>>()
        })
        .collect();
    files.retain(|entry| !entry.file_type().is_dir());
    let mut all: Vec<Found> = files.iter().map(found).collect();
    all.extend(nested);
    all
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{scripts, walk};
    use crate::schema::FileKind;
    use std::path::{Path, PathBuf};

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("created");
        std::fs::write(path, text).expect("written");
    }

    #[test]
    fn walks_every_file_at_any_depth() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        for file in ["a.txt", "sub/b.txt", "sub/deeper/c.yml", "other/d"] {
            write(dir.path(), file, "12345");
        }
        let mut found: Vec<(PathBuf, u64)> = walk(dir.path(), &tracing::Span::none())
            .into_iter()
            .map(|found| {
                let relative = found.path.strip_prefix(dir.path()).expect("inside");
                (relative.to_path_buf(), found.size)
            })
            .collect();
        found.sort();
        let expected: Vec<(PathBuf, u64)> = ["a.txt", "other/d", "sub/b.txt", "sub/deeper/c.yml"]
            .iter()
            .map(|file| (file.split('/').collect(), 5))
            .collect();
        assert_eq!(found, expected);
        assert_eq!(
            walk(&dir.path().join("missing"), &tracing::Span::none()),
            []
        );
    }

    #[test]
    fn scripts_are_classified_and_sorted() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        for file in [
            "events/b.txt",
            "events/a.TXT",
            "events/nested/c.txt",
            "events/readme.md",
            "common/national_focus/f.txt",
            "history/states/1.txt",
            "interface/x.txt",
        ] {
            write(dir.path(), file, "x = 1\n");
        }
        let found: Vec<(String, FileKind)> = scripts(dir.path(), &tracing::Span::none())
            .into_iter()
            .map(|file| {
                (
                    file.relative.to_string_lossy().replace('\\', "/"),
                    file.kind,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (
                    "common/national_focus/f.txt".to_owned(),
                    FileKind::NationalFocus
                ),
                ("events/a.TXT".to_owned(), FileKind::Events),
                ("events/b.txt".to_owned(), FileKind::Events),
                ("events/nested/c.txt".to_owned(), FileKind::Other),
                ("history/states/1.txt".to_owned(), FileKind::Other),
            ]
        );
    }
}
