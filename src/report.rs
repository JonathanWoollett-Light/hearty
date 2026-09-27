//! Summaries of what `--format`, `--check` and `--fix` changed: lines added
//! and removed per file, and totals per kind of change.

use similar::{DiffOp, DiffTag, TextDiff};
use std::collections::BTreeMap;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// A kind of change a formatter rule or fix makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Change {
    /// Multi-line blocks joined onto one line.
    BlocksJoined,
    /// Paragraphs of prose comments rewrapped to fit the line width.
    CommentsReflowed,
    /// Top-level events moved by the event sorter.
    EventsReordered,
    /// Definition blocks whose fields were sorted.
    FieldsReordered,
    /// Focuses moved by the focus sorter.
    FocusesReordered,
    /// Redundant fields removed by `--fix`.
    RedundantRemoved,
    /// Blank-line separators between focuses/events normalised.
    SeparatorsNormalised,
    /// Operators or single-line blocks re-spaced.
    SpacingFixed,
}

impl Change {
    /// `count` changes of this kind as a phrase, e.g. `3 focuses moved` or
    /// `1 spacing fix`.
    pub fn describe(self, count: usize) -> String {
        let (singular, plural) = match self {
            Self::BlocksJoined => ("block joined onto one line", "blocks joined onto one line"),
            Self::CommentsReflowed => ("comment reflowed", "comments reflowed"),
            Self::EventsReordered => ("event moved", "events moved"),
            Self::FieldsReordered => (
                "block with reordered fields",
                "blocks with reordered fields",
            ),
            Self::FocusesReordered => ("focus moved", "focuses moved"),
            Self::RedundantRemoved => ("redundant field removed", "redundant fields removed"),
            Self::SeparatorsNormalised => ("blank-line fix", "blank-line fixes"),
            Self::SpacingFixed => ("spacing fix", "spacing fixes"),
        };
        counted(count, singular, plural)
    }
}

/// Counts of changes by kind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes(BTreeMap<Change, usize>);

impl Changes {
    /// Records `count` changes of `kind` (no-op for 0).
    pub fn add(&mut self, kind: Change, count: usize) {
        if count > 0 {
            *self.0.entry(kind).or_default() += count;
        }
    }

    /// Every recorded kind as a phrase (see [`Change::describe`]), in
    /// [`Change`] order and comma-separated; empty if nothing was recorded.
    fn describe(&self) -> String {
        self.iter()
            .map(|(kind, count)| kind.describe(count))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The recorded `(kind, count)` pairs in [`Change`] order. Every count is
    /// non-zero.
    pub fn iter(&self) -> impl Iterator<Item = (Change, usize)> + '_ {
        self.0.iter().map(|(&kind, &count)| (kind, count))
    }
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Lines added.
    pub added: usize,
    /// Changes by kind.
    pub changes: Changes,
    /// Path relative to the mod root.
    pub path: PathBuf,
    /// Lines removed.
    pub removed: usize,
}

impl FileChange {
    /// Computes the line statistics of rewriting `before` as `after`.
    pub fn new(path: PathBuf, before: &str, after: &str, changes: Changes) -> Self {
        let (added, removed) =
            tracing::info_span!("line stats").in_scope(|| line_stats(before, after));
        Self {
            added,
            changes,
            path,
            removed,
        }
    }
}

/// One file's line of a [`summary`], before alignment.
struct Row {
    /// `+N`.
    added: String,
    /// The file's changes, see [`Changes::describe`].
    changes: String,
    /// See [`display_path`].
    path: String,
    /// `-N`.
    removed: String,
}

/// What happened to the files in a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `--fix` rewrote them.
    Fixed,
    /// `--format` rewrote them.
    Formatted,
    /// `--check` found they would be reformatted.
    WouldFormat,
}

impl Verb {
    /// The line introducing a list of `files` changed files.
    fn header(self, files: usize) -> String {
        let files = counted(files, "file", "files");
        match self {
            Self::Fixed => format!("Fixed {files}:"),
            Self::Formatted => format!("Formatted {files}:"),
            Self::WouldFormat => format!("{files} would be reformatted:"),
        }
    }

    /// The line saying that `files` files the action changed could not be
    /// written.
    fn not_written(self, files: usize) -> String {
        let files = counted(files, "file", "files");
        let action = match self {
            Self::Fixed => "Fixing",
            Self::Formatted => "Formatting",
            Self::WouldFormat => "Formatting check",
        };
        format!("{action}: {files} could not be written.")
    }

    /// The whole summary when no file changed.
    const fn unchanged(self) -> &'static str {
        match self {
            Self::Fixed => "Nothing to fix.\n",
            Self::Formatted => "Formatting: all files already formatted.\n",
            Self::WouldFormat => "Formatting check passed: no files would change.\n",
        }
    }
}

/// `count` followed by the `singular` or `plural` noun, e.g. `1 file` or
/// `1,234 files`.
fn counted(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{} {noun}", crate::fmt_commas(count as u64))
}

/// `path` with `/` separators, so a summary reads the same on every platform.
fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace(MAIN_SEPARATOR, "/")
}

/// The `git diff --stat`-style totals line, e.g. `3 files changed, 25
/// insertions(+), 33 deletions(-)`.
fn footer(files: &[FileChange]) -> String {
    let added: usize = files.iter().map(|file| file.added).sum();
    let removed: usize = files.iter().map(|file| file.removed).sum();
    let mut parts = vec![format!("{} changed", counted(files.len(), "file", "files"))];
    // Like git, leave out a zero count unless both are zero.
    if added > 0 || removed == 0 {
        parts.push(counted(added, "insertion(+)", "insertions(+)"));
    }
    if removed > 0 || added == 0 {
        parts.push(counted(removed, "deletion(-)", "deletions(-)"));
    }
    parts.join(", ")
}

/// Lines added and removed by rewriting `before` as `after`, counted from a
/// line diff. A line's terminator (`\n`, `\r\n` or a lone `\r`) is part of
/// the line, so a line whose ending changed counts as removed and re-added.
///
/// The diff is `similar`'s default, Myers with git's bounded-work heuristics
/// and no deadline, so the counts are deterministic and match `git diff
/// --stat` closely. Like git's, they can exceed the minimal diff when blocks
/// move far: sorting vanilla `germany.txt`'s focuses reports +28,495 -28,480
/// (git +28,326 -28,311; minimal +8,236 -8,221). The minimal diff is not
/// affordable: its worst case is quadratic, over 90 s for pairs of lines
/// joined in a 189k-line file that this diff handles in 25 ms. Patience is
/// no alternative either: it reported still more lines for moved blocks
/// (+31,079 for `germany.txt`) and took twice as long on the largest moves.
pub fn line_stats(before: &str, after: &str) -> (usize, usize) {
    TextDiff::from_lines(before, after)
        .ops()
        .iter()
        .map(DiffOp::as_tag_tuple)
        .filter(|(tag, ..)| *tag != DiffTag::Equal)
        .fold((0, 0), |(added, removed), (_, old, new)| {
            (added + new.len(), removed + old.len())
        })
}

/// A human-readable summary of `files` (sorted by path by the caller): a
/// header, one aligned line per file with its lines added and removed and
/// its changes, a `git diff --stat`-style totals line, and the changes
/// totalled across files. Then, if the action changed `unwritten` more
/// files that could not be written, a line saying so (in place of the line
/// saying nothing changed, if no file was written).
pub fn summary(verb: Verb, files: &[FileChange], unwritten: usize) -> String {
    let _span = tracing::info_span!("summary").entered();
    let not_written = (unwritten > 0).then(|| verb.not_written(unwritten));
    if files.is_empty() {
        return not_written.map_or_else(|| verb.unchanged().to_owned(), |line| format!("{line}\n"));
    }
    let rows: Vec<Row> = files
        .iter()
        .map(|file| Row {
            added: format!("+{}", crate::fmt_commas(file.added as u64)),
            changes: file.changes.describe(),
            path: display_path(&file.path),
            removed: format!("-{}", crate::fmt_commas(file.removed as u64)),
        })
        .collect();
    let width = |column: fn(&Row) -> &String| {
        rows.iter()
            .map(|row| column(row).chars().count())
            .max()
            .unwrap_or_default()
    };
    let path_width = width(|row| &row.path);
    let added_width = width(|row| &row.added);
    let removed_width = width(|row| &row.removed);

    let mut totals = Changes::default();
    for (kind, count) in files.iter().flat_map(|file| file.changes.iter()) {
        totals.add(kind, count);
    }

    let mut lines = vec![verb.header(files.len())];
    lines.extend(rows.iter().map(|row| {
        let Row {
            added,
            changes,
            path,
            removed,
        } = row;
        let stats =
            format!("  {path:<path_width$}  {added:>added_width$}  {removed:>removed_width$}");
        if changes.is_empty() {
            stats
        } else {
            format!("{stats}  ({changes})")
        }
    }));
    lines.push(footer(files));
    let totals = totals.describe();
    if !totals.is_empty() {
        lines.push(format!("Changes: {totals}"));
    }
    lines.extend(not_written);
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::{Change, Changes, FileChange, Verb, line_stats, summary};
    use crate::schema::{FileKind, file_kind};
    use rayon::prelude::*;
    use similar::DiffableStr as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// A [`FileChange`] at the `/`-separated `path`, built from components so
    /// the test also covers the platform's separator.
    fn file(path: &str, added: usize, removed: usize, changes: &[(Change, usize)]) -> FileChange {
        let mut recorded = Changes::default();
        for &(kind, count) in changes {
            recorded.add(kind, count);
        }
        FileChange {
            added,
            changes: recorded,
            path: path.split('/').collect(),
            removed,
        }
    }

    #[test]
    fn line_stats_identical() {
        assert_eq!(line_stats("a\nb\nc\n", "a\nb\nc\n"), (0, 0));
        assert_eq!(line_stats("a\r\nb", "a\r\nb"), (0, 0));
        let big = "focus = {\n\tid = x\n}\n".repeat(1_000);
        assert_eq!(line_stats(&big, &big), (0, 0));
    }

    #[test]
    fn line_stats_empty() {
        assert_eq!(line_stats("", ""), (0, 0));
        assert_eq!(line_stats("", "a\nb\n"), (2, 0));
        assert_eq!(line_stats("a\nb\n", ""), (0, 2));
    }

    #[test]
    fn line_stats_pure_insert() {
        assert_eq!(line_stats("a\nc\n", "a\nb\nc\n"), (1, 0));
        assert_eq!(line_stats("a\nb\n", "x\ny\na\nb\n"), (2, 0));
        assert_eq!(line_stats("a\nb\n", "a\nb\nc\n"), (1, 0));
    }

    #[test]
    fn line_stats_pure_delete() {
        assert_eq!(line_stats("a\nb\nc\n", "a\nc\n"), (0, 1));
        assert_eq!(line_stats("a\nb\nc\nd\n", "d\n"), (0, 3));
    }

    #[test]
    fn line_stats_replace() {
        assert_eq!(line_stats("a\nb\nc\n", "a\nB\nc\n"), (1, 1));
        // Joining a block onto one line.
        assert_eq!(
            line_stats(
                "x = {\n\ta = {\n\t\tb = c\n\t}\n}\n",
                "x = {\n\ta = { b = c }\n}\n"
            ),
            (1, 3)
        );
        // Moving a line counts as removing and re-adding it.
        assert_eq!(line_stats("a\nb\nc\nd\n", "b\nc\nd\na\n"), (1, 1));
    }

    #[test]
    fn line_stats_crlf() {
        assert_eq!(line_stats("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n"), (1, 1));
        assert_eq!(line_stats("a\r\nc\r\n", "a\r\nb\r\nc\r\n"), (1, 0));
        // The terminator is part of the line.
        assert_eq!(line_stats("a\r\nb\r\n", "a\nb\n"), (2, 2));
        assert_eq!(line_stats("a\nb", "a\nb\n"), (1, 1));
        // A lone `\r` ends a line too.
        assert_eq!(line_stats("a\rb\n", "a\rB\n"), (1, 1));
    }

    #[test]
    fn describe_singular_and_plural() {
        let cases = [
            (
                Change::BlocksJoined,
                "block joined onto one line",
                "blocks joined onto one line",
            ),
            (
                Change::CommentsReflowed,
                "comment reflowed",
                "comments reflowed",
            ),
            (Change::EventsReordered, "event moved", "events moved"),
            (
                Change::FieldsReordered,
                "block with reordered fields",
                "blocks with reordered fields",
            ),
            (Change::FocusesReordered, "focus moved", "focuses moved"),
            (
                Change::RedundantRemoved,
                "redundant field removed",
                "redundant fields removed",
            ),
            (
                Change::SeparatorsNormalised,
                "blank-line fix",
                "blank-line fixes",
            ),
            (Change::SpacingFixed, "spacing fix", "spacing fixes"),
        ];
        for (kind, singular, plural) in cases {
            assert_eq!(kind.describe(1), format!("1 {singular}"));
            assert_eq!(kind.describe(2), format!("2 {plural}"));
            assert_eq!(kind.describe(0), format!("0 {plural}"));
        }
        assert_eq!(
            Change::SpacingFixed.describe(1_234_567),
            "1,234,567 spacing fixes"
        );
    }

    #[test]
    fn changes_accumulate_in_change_order() {
        let mut changes = Changes::default();
        changes.add(Change::SpacingFixed, 1);
        changes.add(Change::BlocksJoined, 0);
        changes.add(Change::EventsReordered, 2);
        changes.add(Change::SpacingFixed, 3);
        assert_eq!(
            changes.iter().collect::<Vec<_>>(),
            [(Change::EventsReordered, 2), (Change::SpacingFixed, 4)]
        );
        assert_eq!(changes.describe(), "2 events moved, 4 spacing fixes");
        assert_eq!(Changes::default().describe(), "");
    }

    #[test]
    fn summary_empty() {
        assert_eq!(
            summary(Verb::Formatted, &[], 0),
            "Formatting: all files already formatted.\n"
        );
        assert_eq!(
            summary(Verb::WouldFormat, &[], 0),
            "Formatting check passed: no files would change.\n"
        );
        assert_eq!(summary(Verb::Fixed, &[], 0), "Nothing to fix.\n");
    }

    /// Files an action changed but could not write are counted after the
    /// files it wrote, or in place of the line saying nothing changed.
    #[test]
    fn summary_unwritten() {
        assert_eq!(
            summary(Verb::Formatted, &[], 1),
            "Formatting: 1 file could not be written.\n"
        );
        assert_eq!(
            summary(Verb::Fixed, &[], 2),
            "Fixing: 2 files could not be written.\n"
        );
        let files = [file("events/a.txt", 0, 1, &[(Change::RedundantRemoved, 1)])];
        assert_eq!(
            summary(Verb::Fixed, &files, 1_000),
            "Fixed 1 file:
  events/a.txt  +0  -1  (1 redundant field removed)
1 file changed, 1 deletion(-)
Changes: 1 redundant field removed
Fixing: 1,000 files could not be written.
"
        );
    }

    #[test]
    fn summary_formatted() {
        let files = [
            file(
                "common/decisions/MLT.txt",
                4,
                12,
                &[(Change::FieldsReordered, 2), (Change::BlocksJoined, 4)],
            ),
            file(
                "common/national_focus/bulgaria.txt",
                9,
                9,
                &[(Change::FocusesReordered, 3)],
            ),
            file(
                "events/germany.txt",
                12,
                12,
                &[(Change::EventsReordered, 4), (Change::SpacingFixed, 1)],
            ),
        ];
        assert_eq!(
            summary(Verb::Formatted, &files, 0),
            "Formatted 3 files:
  common/decisions/MLT.txt             +4  -12  (4 blocks joined onto one line, 2 blocks with reordered fields)
  common/national_focus/bulgaria.txt   +9   -9  (3 focuses moved)
  events/germany.txt                  +12  -12  (4 events moved, 1 spacing fix)
3 files changed, 25 insertions(+), 33 deletions(-)
Changes: 4 blocks joined onto one line, 4 events moved, 2 blocks with reordered fields, 3 focuses moved, 1 spacing fix
"
        );
    }

    #[test]
    fn summary_would_format_one_file() {
        let files = [file(
            "events/germany.txt",
            1,
            1,
            &[(Change::SeparatorsNormalised, 1)],
        )];
        assert_eq!(
            summary(Verb::WouldFormat, &files, 0),
            "1 file would be reformatted:
  events/germany.txt  +1  -1  (1 blank-line fix)
1 file changed, 1 insertion(+), 1 deletion(-)
Changes: 1 blank-line fix
"
        );
    }

    #[test]
    fn summary_would_format_many_files() {
        let files = [
            file(
                "common/national_focus/a.txt",
                1_234,
                1_234,
                &[(Change::FocusesReordered, 1_000)],
            ),
            file("events/b.txt", 2, 0, &[(Change::BlocksJoined, 1)]),
        ];
        assert_eq!(
            summary(Verb::WouldFormat, &files, 0),
            "2 files would be reformatted:
  common/national_focus/a.txt  +1,234  -1,234  (1,000 focuses moved)
  events/b.txt                     +2      -0  (1 block joined onto one line)
2 files changed, 1,236 insertions(+), 1,234 deletions(-)
Changes: 1 block joined onto one line, 1,000 focuses moved
"
        );
    }

    #[test]
    fn summary_fixed() {
        let files = [
            file(
                "common/decisions/a.txt",
                0,
                1,
                &[(Change::RedundantRemoved, 1)],
            ),
            file("common/ideas/b.txt", 0, 4, &[(Change::RedundantRemoved, 5)]),
        ];
        assert_eq!(
            summary(Verb::Fixed, &files, 0),
            "Fixed 2 files:
  common/decisions/a.txt  +0  -1  (1 redundant field removed)
  common/ideas/b.txt      +0  -4  (5 redundant fields removed)
2 files changed, 5 deletions(-)
Changes: 6 redundant fields removed
"
        );
        let one = [file(
            "common/decisions/a.txt",
            0,
            1,
            &[(Change::RedundantRemoved, 1)],
        )];
        assert_eq!(
            summary(Verb::Fixed, &one, 0),
            "Fixed 1 file:
  common/decisions/a.txt  +0  -1  (1 redundant field removed)
1 file changed, 1 deletion(-)
Changes: 1 redundant field removed
"
        );
    }

    /// Like `git diff --stat`, the footer leaves out a zero count unless both
    /// are zero, and a file with no recorded changes gets no breakdown.
    #[test]
    fn summary_zero_counts_and_no_changes() {
        let inserted = [file("events/a.txt", 3, 0, &[])];
        assert_eq!(
            summary(Verb::Formatted, &inserted, 0),
            "Formatted 1 file:
  events/a.txt  +3  -0
1 file changed, 3 insertions(+)
"
        );
        let nothing = [file("events/a.txt", 0, 0, &[])];
        assert_eq!(
            summary(Verb::Formatted, &nothing, 0),
            "Formatted 1 file:
  events/a.txt  +0  -0
1 file changed, 0 insertions(+), 0 deletions(-)
"
        );
    }

    #[test]
    fn file_change_new_computes_line_stats() {
        let change = FileChange::new(
            PathBuf::from("events/a.txt"),
            "a\nb\n",
            "a\nc\nd\n",
            Changes::default(),
        );
        assert_eq!((change.added, change.removed), (2, 1));
    }

    /// Every `*.txt` under `common/`, `events/` and `history/` of each
    /// `;`-separated root in `roots` that [`file_kind`] accepts, with its
    /// kind, sorted by path.
    fn corpus_files(roots: &str) -> Vec<(PathBuf, FileKind)> {
        let mut files = Vec::new();
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .map(Path::new)
        {
            for sub in ["common", "events", "history"] {
                for entry in walkdir::WalkDir::new(root.join(sub))
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|entry| !entry.file_type().is_dir())
                {
                    let kind = entry.path().strip_prefix(root).ok().and_then(file_kind);
                    if let Some(kind) = kind {
                        files.push((entry.into_path(), kind));
                    }
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// `lines` with every twelfth line (≈8%) deleted, and how many were
    /// deleted. A line after a lone `\r` is kept: deleting a `\n` line there
    /// would turn the `\r` into a `\r\n` and so change a kept line too.
    fn delete_lines(lines: &[&str]) -> (String, usize) {
        let mut out = String::new();
        let mut deleted = 0;
        let mut previous = "";
        for (index, &line) in lines.iter().enumerate() {
            if index % 12 == 11 && !previous.ends_with('\r') {
                deleted += 1;
            } else {
                out.push_str(line);
                previous = line;
            }
        }
        (out, deleted)
    }

    /// `lines` with each pair joined onto one line (as joining a block does),
    /// and how many pairs were joined.
    fn join_pairs(lines: &[&str]) -> (String, usize) {
        let mut out = String::new();
        let mut joined = 0;
        for pair in lines.chunks(2) {
            if let [first, second] = pair {
                out.push_str(first.trim_end_matches(['\r', '\n']));
                out.push(' ');
                out.push_str(second.trim_start_matches([' ', '\t']));
                joined += 1;
            } else {
                out.extend(pair.iter().copied());
            }
        }
        (out, joined)
    }

    /// Checks [`line_stats`] on rewrites of every file of the corpus and
    /// reports how long it took: the file against itself (no change), with
    /// every twelfth line deleted (≈8%; the diff must be exactly those
    /// deletions), with each pair of lines joined (at most one line added and
    /// two removed per pair), and as formatted by [`crate::pipeline::format_text`]. Every
    /// result must also balance: lines before + added = lines after +
    /// removed.
    ///
    /// Run with `cargo test --release corpus_line_stats -- --ignored
    /// --nocapture` and `HEARTY_CORPUS=<root>;<root>;...` pointing at HOI4 /
    /// mod directories.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_line_stats() {
        /// Names of the rewrites, indexing [`Timings`].
        const SCENARIOS: [&str; 4] = ["identical", "delete 8%", "join pairs", "formatted"];

        /// Time spent diffing one file, per scenario.
        type Timings = [Duration; SCENARIOS.len()];

        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let files = corpus_files(&roots);
        let start = Instant::now();
        let results: Vec<(PathBuf, usize, Timings, Vec<String>)> = files
            .into_par_iter()
            .filter_map(|(path, kind)| {
                // Non-UTF-8 files are never formatted, so never diffed.
                let before = std::fs::read_to_string(&path).ok()?;
                let lines = before.tokenize_lines();
                let (deleted_text, deleted) = delete_lines(&lines);
                let (joined_text, joined) = join_pairs(&lines);
                let formatted = crate::pipeline::format_text(kind, &before, 100).0;
                let expected = [
                    Some((0, 0)),
                    Some((0, deleted)),
                    None,
                    (formatted == before).then_some((0, 0)),
                ];
                let mut timings = Timings::default();
                let mut problems = Vec::new();
                for (index, after) in [&before, &deleted_text, &joined_text, &formatted]
                    .into_iter()
                    .enumerate()
                {
                    let scenario_start = Instant::now();
                    let (added, removed) = line_stats(&before, after);
                    if let Some(timing) = timings.get_mut(index) {
                        *timing = scenario_start.elapsed();
                    }
                    let scenario = SCENARIOS.get(index).copied().unwrap_or_default();
                    let after_lines = after.tokenize_lines().len();
                    if lines.len() + added != after_lines + removed {
                        problems.push(format!(
                            "{scenario}: +{added} -{removed} does not take {} lines to {after_lines}",
                            lines.len()
                        ));
                    }
                    if let Some(Some(want)) = expected.get(index)
                        && (added, removed) != *want
                    {
                        problems.push(format!(
                            "{scenario}: +{added} -{removed}, expected +{} -{}",
                            want.0, want.1
                        ));
                    }
                    if index == 2 && (added > joined || removed > 2 * joined) {
                        problems.push(format!(
                            "{scenario}: +{added} -{removed} for {joined} joined pairs"
                        ));
                    }
                }
                Some((path, lines.len(), timings, problems))
            })
            .collect();
        let wall = start.elapsed();

        let mut report = vec![format!(
            "files: {} | lines: {} | wall clock (all scenarios, parallel, incl. reading and \
             formatting): {wall:.2?}",
            results.len(),
            results.iter().map(|(_, lines, ..)| lines).sum::<usize>(),
        )];
        for (index, scenario) in SCENARIOS.iter().enumerate() {
            let timing = |(path, lines, timings, _): &(PathBuf, usize, Timings, Vec<String>)| {
                (
                    timings.get(index).copied().unwrap_or_default(),
                    path.display().to_string(),
                    *lines,
                )
            };
            let total: Duration = results.iter().map(|result| timing(result).0).sum();
            let mut slowest: Vec<_> = results.iter().map(timing).collect();
            slowest.sort_by_key(|(took, ..)| std::cmp::Reverse(*took));
            report.push(format!("{scenario}: total {total:.2?}; slowest:"));
            report.extend(
                slowest
                    .iter()
                    .take(3)
                    .map(|(took, path, lines)| format!("    {took:.2?}  {lines} lines  {path}")),
            );
        }
        let problems: Vec<String> = results
            .iter()
            .flat_map(|(path, _, _, problems)| {
                problems
                    .iter()
                    .map(move |problem| format!("{}: {problem}", path.display()))
            })
            .collect();
        report.push(format!("problems: {}", problems.len()));
        report.extend(
            problems
                .iter()
                .take(30)
                .map(|problem| format!("    {problem}")),
        );
        println!("{}", report.join("\n"));
        assert!(
            problems.is_empty(),
            "line_stats problems; see the report above"
        );
    }
}
