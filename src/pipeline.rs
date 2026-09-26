//! The script pass: every script file processed end to end in memory, on
//! every core (see [`in_parallel`], which also schedules loading the
//! localisation alongside).
//!
//! Each file is read once, then (as the actions ask) fixed, formatted,
//! written once if that changed it, checked, and linted. Every stage after
//! the write sees the text on disk: the rewritten text, or the original one
//! if the write failed. A file's CST is parsed once and shared by the stages
//! until one changes the text; each rule that rewrites the text hands on the
//! parse of its result when it made one.

use crate::cst::{self, Block, Document};
use crate::files::ScriptFile;
use crate::keys::{self, KeyUse};
use crate::redundant::{self, Finding};
use crate::report::{Change, Changes, FileChange};
use crate::schema::FileKind;
use crate::{field_order, inline, sort};
use std::cmp::Reverse;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// What the formatter rules made of a file.
#[derive(Debug)]
struct Formatted {
    changes: Changes,
    /// The parse of `text`, when a rule made it.
    root: Option<Block>,
    text: String,
}

/// What processing one script file did and found.
#[derive(Debug, Default)]
pub struct Outcome {
    /// What `--fix` changed, if the fixed file was written.
    pub fixed: Option<FileChange>,
    /// What `--format` changed, if the formatted file was written.
    pub formatted: Option<FileChange>,
    /// The localisation keys the file uses, when linting.
    pub keys: Vec<KeyUse>,
    /// The redundant fields of the file, when linting.
    pub redundant: Vec<Finding>,
    /// Redundant fields `--fix` left because removing them would delete a
    /// comment.
    pub unfixed: usize,
    /// What `--check` would change.
    pub would_format: Option<FileChange>,
    /// Why writing the fixed or formatted file failed, if it did.
    pub write_failure: Option<WriteFailure>,
}

/// A file's text and its CST, parsed on first use. A tree is only ever
/// paired with the text it was parsed from.
#[derive(Debug)]
struct Script {
    text: String,
    tree: Tree,
}

impl Script {
    /// The text, and its tree if parsed.
    fn into_parts(self) -> (String, Option<Block>) {
        match self.tree {
            Tree::Parsed(root) => (self.text, Some(root)),
            Tree::Unparsable | Tree::Unparsed => (self.text, None),
        }
    }

    /// The text with its tree, `root`, when it is known.
    fn new(text: String, root: Option<Block>) -> Self {
        Self {
            text,
            tree: root.map_or(Tree::Unparsed, Tree::Parsed),
        }
    }

    /// Runs `f` on the parse of the text (parsing it if need be); `None` if
    /// the text does not parse.
    fn with_doc<R, F>(&mut self, f: F) -> Option<R>
    where
        F: FnOnce(&Document<'_>) -> R,
    {
        let root = match std::mem::replace(&mut self.tree, Tree::Unparsable) {
            Tree::Parsed(root) => root,
            Tree::Unparsable => return None,
            Tree::Unparsed => cst::parse(&self.text).ok()?.root,
        };
        let doc = Document {
            root,
            src: &self.text,
        };
        let result = f(&doc);
        self.tree = Tree::Parsed(doc.root);
        Some(result)
    }
}

/// What is known of a [`Script`]'s CST.
#[derive(Debug)]
enum Tree {
    Parsed(Block),
    /// The text does not parse.
    Unparsable,
    Unparsed,
}

/// A fixed or formatted file that could not be written.
#[derive(Debug)]
pub struct WriteFailure {
    /// Why.
    pub error: std::io::Error,
    /// Whether `--fix` changed the file.
    pub fixed: bool,
    /// Whether `--format` changed the file.
    pub formatted: bool,
}

/// What the pass does to each file.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "fields are independent CLI actions, not a state machine"
)]
pub struct Settings {
    pub check: bool,
    pub fix: bool,
    pub format: bool,
    /// Find redundant fields and the localisation keys used.
    pub lint: bool,
    /// See `--max-width`.
    pub max_width: usize,
}

/// Field order and inline (the formatter rules after sorting) on `doc`,
/// recording what they change in `changes`. Returns the new text and its
/// parse (if known), or `None` if neither changes anything.
fn fields_and_spacing(
    kind: FileKind,
    doc: &Document<'_>,
    max_width: usize,
    changes: &mut Changes,
) -> Option<(String, Option<Block>)> {
    let inline = |doc: &Document<'_>, changes: &mut Changes| {
        let (text, stats, root) =
            tracing::info_span!("inline").in_scope(|| inline::apply_doc(doc, kind, max_width))?;
        changes.add(Change::BlocksJoined, stats.joined);
        changes.add(Change::SpacingFixed, stats.respaced);
        Some((text, root))
    };
    let Some((text, blocks, root)) =
        tracing::info_span!("field order").in_scope(|| field_order::apply_doc(doc, kind))
    else {
        return inline(doc, changes);
    };
    changes.add(Change::FieldsReordered, blocks);
    let mut reordered = Script::new(text, root);
    match reordered.with_doc(|doc| inline(doc, changes)).flatten() {
        Some(inlined) => Some(inlined),
        None => Some(reordered.into_parts()),
    }
}

/// Runs every formatter rule over `doc`, a script file of kind `kind`:
/// 1. Focus files: sort focuses within each tree; event files: sort the
///    events so chains of events stay together (see [`sort`]).
/// 2. Sort the fields of definition blocks (see [`field_order`]).
/// 3. Join short blocks onto one line and normalise spacing (see
///    [`inline`]).
///
/// Returns `None` if the file is formatted already.
fn format_doc(kind: FileKind, doc: &Document<'_>, max_width: usize) -> Option<Formatted> {
    let mut changes = Changes::default();
    let sorted = match kind {
        FileKind::NationalFocus => tracing::info_span!("sort focuses")
            .in_scope(|| sort::focuses(doc))
            .map(|(text, moved)| (text, moved, Change::FocusesReordered)),
        FileKind::Events => tracing::info_span!("sort events")
            .in_scope(|| sort::events(doc))
            .map(|(text, moved)| (text, moved, Change::EventsReordered)),
        FileKind::Characters
        | FileKind::DecisionCategories
        | FileKind::Decisions
        | FileKind::Ideas
        | FileKind::Other
        | FileKind::Technologies => None,
    };
    let Some((text, moved, change)) = sorted else {
        let (text, root) = fields_and_spacing(kind, doc, max_width, &mut changes)?;
        return Some(Formatted {
            changes,
            root,
            text,
        });
    };
    if moved > 0 {
        changes.add(change, moved);
    } else {
        changes.add(Change::SeparatorsNormalised, 1);
    }
    let mut sorted = Script::new(text, None);
    let rest = sorted
        .with_doc(|doc| fields_and_spacing(kind, doc, max_width, &mut changes))
        .flatten();
    let (text, root) = rest.unwrap_or_else(|| sorted.into_parts());
    Some(Formatted {
        changes,
        root,
        text,
    })
}

/// [`format_doc`] on `text`: the formatted text (`text` itself if it does
/// not parse or is formatted already) and what changed.
#[cfg(test)]
pub fn format_text(kind: FileKind, text: &str, max_width: usize) -> (String, Changes) {
    cst::parse(text)
        .ok()
        .and_then(|doc| format_doc(kind, &doc, max_width))
        .map_or_else(
            || (text.to_owned(), Changes::default()),
            |formatted| (formatted.text, formatted.changes),
        )
}

/// The order in which [`in_parallel`] hands out the files of `sizes` to
/// `threads` workers.
///
/// First come the files big enough to finish last if started late, those
/// over half a worker's fair share of all the bytes, largest first. (A
/// parallel iterator would not do: the thread that splits the list keeps the
/// largest files and runs them one after another.) The rest follow
/// interleaved from across the list: the first file of each of `threads`
/// equal runs, then the second of each, and so on. So the files in progress
/// at any moment come from all over the mod, and differ in size: on Windows,
/// writing files of one directory together made `--format` much slower, as
/// did formatting large files together (which contend for memory), so that
/// strictly largest-first took 25% longer on Millennium Dawn.
fn work_order(sizes: &[u64], threads: usize) -> Vec<usize> {
    let threads = threads.max(1);
    let total: u64 = sizes.iter().sum();
    let big_from = total / (2 * threads as u64);
    let size = |index: usize| sizes.get(index).copied().unwrap_or_default();
    let (mut order, others): (Vec<usize>, Vec<usize>) =
        (0..sizes.len()).partition(|&index| size(index) > big_from);
    order.sort_by_key(|&index| Reverse(size(index)));
    let run = others.len().div_ceil(threads);
    let others = others.as_slice();
    order.extend((0..run).flat_map(|offset| {
        (0..threads).filter_map(move |start| others.get(start * run + offset).copied())
    }));
    order
}

/// Locks `mutex`, carrying on if a panicking thread poisoned it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Processes one script file (see the module docs).
pub fn process(file: &ScriptFile, settings: Settings) -> Outcome {
    let mut outcome = Outcome::default();
    let Some(text) =
        tracing::info_span!("read").in_scope(|| std::fs::read_to_string(&file.path).ok())
    else {
        // Unreadable, or not UTF-8: left alone.
        return outcome;
    };
    let mut disk = Script::new(text, None);
    // The fixed or formatted text, not yet written.
    let mut rewritten: Option<Script> = None;
    // The redundant fields of `disk`, once found.
    let mut disk_findings: Option<Vec<Finding>> = None;

    if settings.fix {
        let _span = tracing::info_span!("fix").entered();
        let findings = disk
            .with_doc(|doc| {
                tracing::info_span!("redundant find")
                    .in_scope(|| redundant::find_doc(doc, file.kind))
            })
            .unwrap_or_default();
        if !findings.is_empty() {
            let (after, fixed) = tracing::info_span!("redundant fix")
                .in_scope(|| redundant::fix(&disk.text, &findings));
            outcome.unfixed = findings.len() - fixed;
            if after != disk.text {
                let mut changes = Changes::default();
                changes.add(Change::RedundantRemoved, fixed);
                outcome.fixed = Some(FileChange::new(
                    file.relative.clone(),
                    &disk.text,
                    &after,
                    changes,
                ));
                rewritten = Some(Script::new(after, None));
            }
        }
        disk_findings = Some(findings);
    }

    if settings.format {
        let _span = tracing::info_span!("format").entered();
        let current = rewritten.as_mut().unwrap_or(&mut disk);
        if let Some(formatted) = current
            .with_doc(|doc| format_doc(file.kind, doc, settings.max_width))
            .flatten()
        {
            // The old text's tree is not needed again (should the write fail,
            // the text is parsed anew), so it is freed now rather than held
            // through the line stats, the longest step.
            current.tree = Tree::Unparsed;
            outcome.formatted = Some(FileChange::new(
                file.relative.clone(),
                &current.text,
                &formatted.text,
                formatted.changes,
            ));
            rewritten = Some(Script::new(formatted.text, formatted.root));
        }
    }

    if let Some(new) = rewritten {
        match tracing::info_span!("write").in_scope(|| std::fs::write(&file.path, &new.text)) {
            Ok(()) => {
                disk = new;
                disk_findings = None;
            }
            Err(error) => {
                outcome.write_failure = Some(WriteFailure {
                    error,
                    fixed: outcome.fixed.take().is_some(),
                    formatted: outcome.formatted.take().is_some(),
                });
            }
        }
    }

    if settings.check {
        let _span = tracing::info_span!("check").entered();
        if let Some(Formatted {
            changes,
            root,
            text,
        }) = disk
            .with_doc(|doc| format_doc(file.kind, doc, settings.max_width))
            .flatten()
        {
            // Neither tree is needed for the line stats, the longest step, so
            // they are freed first: the formatted text's is never used, and
            // the file's only by the lint.
            drop(root);
            if !settings.lint {
                disk.tree = Tree::Unparsed;
            }
            outcome.would_format = Some(FileChange::new(
                file.relative.clone(),
                &disk.text,
                &text,
                changes,
            ));
        }
    }

    if settings.lint {
        let _span = tracing::info_span!("lint").entered();
        let uses_keys = keys::uses_keys(file.kind);
        let linted = disk.with_doc(|doc| {
            let findings = disk_findings.take().unwrap_or_else(|| {
                tracing::info_span!("redundant find")
                    .in_scope(|| redundant::find_doc(doc, file.kind))
            });
            let keys = if uses_keys {
                tracing::info_span!("keys").in_scope(|| keys::from_doc(doc, file.kind))
            } else {
                Vec::new()
            };
            (findings, keys)
        });
        if let Some((findings, keys)) = linted {
            outcome.redundant = findings;
            outcome.keys = keys;
        } else if uses_keys {
            outcome.keys =
                tracing::info_span!("keys").in_scope(|| keys::from_jomini(&disk.text, file.kind));
        }
    }
    outcome
}

/// Runs `work` on every index of `sizes` (the sizes of the files to work
/// on, in path order) on every core, and returns the results in index
/// order. Each worker takes the next index of [`work_order`] until none is
/// left.
pub fn in_parallel<T, F>(sizes: &[u64], work: F) -> Vec<T>
where
    T: Default + Send,
    F: Fn(usize) -> T + Sync,
{
    let order = work_order(sizes, rayon::current_num_threads());
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::with_capacity(sizes.len()));
    rayon::scope(|scope| {
        for _ in 0..rayon::current_num_threads().min(sizes.len()) {
            scope.spawn(|_| {
                let mut mine = Vec::new();
                while let Some(&index) = order.get(next.fetch_add(1, Ordering::Relaxed)) {
                    mine.push((index, work(index)));
                }
                lock(&done).extend(mine);
            });
        }
    });
    let mut results: Vec<T> = std::iter::repeat_with(T::default)
        .take(sizes.len())
        .collect();
    for (index, result) in done.into_inner().unwrap_or_else(PoisonError::into_inner) {
        if let Some(slot) = results.get_mut(index) {
            *slot = result;
        }
    }
    results
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{Outcome, Settings, format_text, in_parallel, process, work_order};
    use crate::files::ScriptFile;
    use crate::report::Change;
    use crate::schema::FileKind;
    use std::path::{Path, PathBuf};

    /// An unformatted decision with a redundant field.
    const DECISION: &str = "cat = {\n\tdec = {\n\t\tcomplete_effect = {\n\t\t\tadd_political_power = 1\n\t\t}\n\t\tfire_only_once = no\n\t\ticon = x\n\t}\n}\n";

    /// [`DECISION`] fixed and formatted.
    const DONE: &str = "cat = {\n\tdec = {\n\t\ticon = x\n\t\tcomplete_effect = { add_political_power = 1 }\n\t}\n}\n";

    const EVERYTHING: Settings = Settings {
        check: true,
        fix: true,
        format: true,
        lint: true,
        max_width: 100,
    };

    fn script(root: &Path, relative: &str, kind: FileKind, text: &str) -> ScriptFile {
        let path = root.join(relative);
        std::fs::write(&path, text).expect("written");
        ScriptFile {
            kind,
            path,
            relative: PathBuf::from(relative),
            size: text.len() as u64,
        }
    }

    /// Processes `files` as the pass does.
    fn run(files: &[ScriptFile], settings: Settings) -> Vec<Outcome> {
        let sizes: Vec<u64> = files.iter().map(|file| file.size).collect();
        in_parallel(&sizes, |index| {
            files
                .get(index)
                .map(|file| process(file, settings))
                .unwrap_or_default()
        })
    }

    /// The outcome of running the pass on `file` alone.
    fn run_one(file: &ScriptFile, settings: Settings) -> Outcome {
        let mut outcomes = run(std::slice::from_ref(file), settings);
        assert_eq!(outcomes.len(), 1, "one outcome per file");
        outcomes.pop().expect("an outcome")
    }

    fn read(file: &ScriptFile) -> String {
        std::fs::read_to_string(&file.path).expect("read")
    }

    /// Each file goes through every action in order, is written once, and
    /// is linted as written.
    #[test]
    fn every_action_in_order() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = script(dir.path(), "d.txt", FileKind::Decisions, DECISION);
        let outcome = run_one(&file, EVERYTHING);
        assert_eq!(read(&file), DONE);
        let fixed = outcome.fixed.expect("fixed");
        assert_eq!(
            fixed.changes.iter().collect::<Vec<_>>(),
            [(Change::RedundantRemoved, 1)]
        );
        assert!(outcome.formatted.is_some());
        assert!(outcome.would_format.is_none(), "formatted text is checked");
        assert!(outcome.redundant.is_empty(), "the fixed text is linted");
        assert!(outcome.write_failure.is_none());
        assert_eq!(outcome.unfixed, 0);
    }

    /// Without `--fix` and `--format` nothing is written; the check and the
    /// lint see the file as it is.
    #[test]
    fn check_and_lint_leave_files_alone() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = script(dir.path(), "d.txt", FileKind::Decisions, DECISION);
        let outcome = run_one(
            &file,
            Settings {
                fix: false,
                format: false,
                ..EVERYTHING
            },
        );
        assert_eq!(read(&file), DECISION);
        assert!(outcome.fixed.is_none() && outcome.formatted.is_none());
        assert!(outcome.would_format.is_some());
        assert_eq!(outcome.redundant.len(), 1);
    }

    /// A file that cannot be written counts as neither fixed nor formatted,
    /// but as a failure of the actions that changed it; the check and lint
    /// see it as it is on disk.
    #[test]
    fn a_failed_write_leaves_the_file_as_it_was() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = script(dir.path(), "d.txt", FileKind::Decisions, DECISION);
        let mut permissions = std::fs::metadata(&file.path)
            .expect("metadata")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file.path, permissions.clone()).expect("read-only");
        // A privileged user writes read-only files anyway.
        let writable = std::fs::OpenOptions::new()
            .append(true)
            .open(&file.path)
            .is_ok();
        let outcome = run_one(&file, EVERYTHING);
        let format_only = run_one(
            &file,
            Settings {
                fix: false,
                ..EVERYTHING
            },
        );
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "only undoes the read-only flag set above"
        )]
        permissions.set_readonly(false);
        std::fs::set_permissions(&file.path, permissions).expect("writable");
        if writable {
            return;
        }
        let failure = outcome.write_failure.expect("a failed write");
        assert!(failure.fixed && failure.formatted, "{failure:?}");
        assert!(outcome.fixed.is_none() && outcome.formatted.is_none());
        assert!(outcome.would_format.is_some());
        assert_eq!(outcome.redundant.len(), 1);
        assert_eq!(read(&file), DECISION);
        let failure = format_only.write_failure.expect("a failed write");
        assert!(!failure.fixed && failure.formatted, "{failure:?}");
    }

    /// Outcomes come in the order of the files, whatever their sizes. A file
    /// that cannot be read is left alone, as is one that does not parse,
    /// whose keys are read with jomini.
    #[test]
    fn outcomes_follow_the_files() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let events = "country_event = {\n\tid = e.2\n\ttitle = e.2.t\n}\n\n\
            country_event = {\n\tid = e.1\n\ttitle = e.1.t\n}\n";
        let unclosed =
            "country_event = {\n\tid = u.2\n}\ncountry_event = {\n\tid = u.1\n\ttitle = u.1.t\n";
        let missing = ScriptFile {
            kind: FileKind::Other,
            path: dir.path().join("missing.txt"),
            relative: PathBuf::from("missing.txt"),
            size: 0,
        };
        let files = [
            script(dir.path(), "a.txt", FileKind::Other, "x={a=b}\n"),
            script(dir.path(), "b.txt", FileKind::Events, events),
            script(dir.path(), "c.txt", FileKind::Events, unclosed),
            script(
                dir.path(),
                "d.txt",
                FileKind::Other,
                &"x = 1\n".repeat(1_000),
            ),
            missing,
        ];
        let outcomes = run(&files, EVERYTHING);
        let formatted: Vec<bool> = outcomes.iter().map(|o| o.formatted.is_some()).collect();
        assert_eq!(formatted, [true, true, false, false, false]);
        let keys: Vec<Vec<&str>> = outcomes
            .iter()
            .map(|outcome| outcome.keys.iter().map(|used| used.key.as_str()).collect())
            .collect();
        // The events are sorted before their keys are read.
        assert_eq!(
            keys,
            [
                vec![],
                vec!["e.1.t", "e.2.t"],
                vec!["u.1.t"],
                vec![],
                vec![]
            ]
        );
        let [_, _, unclosed_file, ..] = &files;
        assert_eq!(read(unclosed_file), unclosed);
    }

    /// Files that could finish last go first, largest first; the others
    /// are dealt out from across the list.
    #[test]
    fn big_files_first_then_the_rest_interleaved() {
        // 1,000 bytes in all: over 125 is big for 4 workers. The other 9
        // make runs of 3: [0, 2, 3], [5, 6, 8] and [9, 10, 11].
        let sizes = [10, 400, 10, 10, 200, 10, 10, 126, 10, 10, 10, 10, 184];
        assert_eq!(
            work_order(&sizes, 4),
            [1, 4, 12, 7, 0, 5, 9, 2, 6, 10, 3, 8, 11]
        );
        assert_eq!(work_order(&[], 4), Vec::<usize>::new());
        // Every file once, whatever the sizes and workers.
        for threads in [0, 1, 3, 24] {
            let sizes: Vec<u64> = (0..50).map(|size| size * size % 17).collect();
            let mut order = work_order(&sizes, threads);
            order.sort_unstable();
            assert_eq!(order, (0..50).collect::<Vec<_>>());
        }
    }

    #[test]
    fn format_text_leaves_unparsable_text_alone() {
        let unclosed = "country_event = {\n\tid = u.2\n}\ncountry_event = {\n\tid = u.1\n";
        assert_eq!(format_text(FileKind::Events, unclosed, 100).0, unclosed);
        let (formatted, changes) = format_text(FileKind::Decisions, DECISION, 100);
        assert_ne!(formatted, DECISION);
        assert!(
            changes
                .iter()
                .any(|(change, _)| change == Change::FieldsReordered)
        );
    }
}
