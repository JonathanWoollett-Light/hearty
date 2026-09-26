//! Lint: fields set to their default value, or otherwise without effect
//! (e.g. `fire_only_once = no` in a decision). See
//! [`crate::schema::redundant_rules`].
//!
//! The fix deletes the field. A field alone on its line(s) goes with its
//! whole line(s); the blank lines around it are then tidied so the deletion
//! never leaves two blank lines in a row, or a blank line just inside a brace,
//! that were not there before. A field sharing a line goes with the
//! whitespace on one side of it, so `{ cost = 5 fire_only_once = no }` becomes
//! `{ cost = 5 }`; that never removes a line break, so blank lines next to
//! the line stay, even if only a brace is left on it. A field is never deleted together with a comment: a comment
//! inside it, or right after it on its line (which may well describe it),
//! leaves the finding without a fix. Comment lines above a field are kept.
//!
//! Each finding's fix is valid applied on its own or with any of the others
//! (say, by a caller fixing only some rules): it never joins the text around
//! the field into one token. The whitespace is tidiest when all of them are
//! applied together, as [`fix`] does with everything [`find_doc`] returns.

use crate::cst::{self, Block, Document, Edit, Entry, OpKind, Span, Value};
use crate::schema::{self, FileKind, Redundant, RedundantRule};

/// A redundant field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Why the field has no effect.
    pub explanation: &'static str,
    /// The edit that removes the field, or `None` if removing it would also
    /// delete a comment. It never overlaps the fixes of the other findings
    /// of the same [`find_doc`] call, and is valid with or without any of them;
    /// applied without the others, it may leave the whitespace around the
    /// field less tidy (see the module docs).
    pub fix: Option<Edit>,
    /// The redundant entry, from its key to the end of its value.
    pub span: Span,
    /// The entry's text with whitespace collapsed, e.g. `fire_only_once = no`.
    pub text: String,
}

/// Whole lines deleted by the fixes of one or more consecutive findings of a
/// block (one finding when entries have their own lines; a run of findings
/// sharing lines otherwise).
#[derive(Debug, Clone, Copy)]
struct LineRemoval {
    /// Slot (index among the block's findings) of the first finding.
    first: usize,
    /// Slot of the last finding.
    last: usize,
    /// From the start of the first line to just past the last line's
    /// terminator.
    span: Span,
}

/// Finds redundant fields in `src`, in source order. Returns nothing if `src`
/// does not parse.
#[cfg(test)]
pub fn find(src: &str, file: FileKind) -> Vec<Finding> {
    cst::parse(src).map_or_else(|_err| Vec::new(), |doc| find_doc(&doc, file))
}

/// Finds redundant fields in the parsed `doc`, in source order.
pub fn find_doc(doc: &Document<'_>, file: FileKind) -> Vec<Finding> {
    let src = doc.src;
    let mut findings = Vec::new();
    cst::visit_blocks(doc, &mut |path, entry, block| {
        if let Some(kind) = schema::block_kind(file, path) {
            block_findings(
                src,
                entry.key_str(src),
                block,
                schema::redundant_rules(kind),
                &mut findings,
            );
        }
    });
    findings.sort_by_key(|finding| finding.span.start);
    findings
}

/// Applies the fixes of `findings` to `src`, returning the new text and the
/// number of findings fixed.
pub fn fix(src: &str, findings: &[Finding]) -> (String, usize) {
    let edits: Vec<Edit> = accepted(findings)
        .into_iter()
        .filter_map(|finding| finding.fix.clone())
        .collect();
    let count = edits.len();
    cst::apply_edits(src, edits).map_or_else(|| (src.to_owned(), 0), |text| (text, count))
}

/// The findings whose fixes [`fix`] applies, in source order: every finding
/// with a fix, except one whose fix overlaps a fix already taken.
fn accepted(findings: &[Finding]) -> Vec<&Finding> {
    let mut fixable: Vec<(Span, &Finding)> = findings
        .iter()
        .filter_map(|finding| Some((finding.fix.as_ref()?.span, finding)))
        .collect();
    fixable.sort_by_key(|&(span, _)| (span.start, span.end));
    let mut taken = Vec::with_capacity(fixable.len());
    let mut cursor = 0_usize;
    for (span, finding) in fixable {
        if span.start >= cursor {
            cursor = span.end;
            taken.push(finding);
        }
    }
    taken
}

/// End of the run of blank lines starting at line start `pos`: the start of
/// the first non-blank line at or after `pos`.
fn blank_lines_after(src: &str, pos: usize) -> usize {
    let mut end = pos;
    loop {
        let line_end = cst::line_end(src, end);
        // A last line without a terminator is never inside a block.
        if line_end == src.len() || !is_blank(src, end, line_end) {
            return end;
        }
        end = next_line(src, end);
    }
}

/// Start of the run of blank lines ending at line start `pos` (`pos` itself
/// if the line above is not blank).
fn blank_lines_before(src: &str, pos: usize) -> usize {
    let mut start = pos;
    while let Some(newline) = start.checked_sub(1) {
        let line = cst::line_start(src, newline);
        if !is_blank(src, line, newline) {
            break;
        }
        start = line;
    }
    start
}

/// Appends the findings of one definition `block`, whose own key is `owner`,
/// against the `rules` of its kind.
fn block_findings(
    src: &str,
    owner: Option<&str>,
    block: &Block,
    rules: &[&'static RedundantRule],
    out: &mut Vec<Finding>,
) {
    let matched: Vec<(usize, &'static RedundantRule)> = block
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| Some((index, matching_rule(src, owner, block, entry, rules)?)))
        .collect();
    if matched.is_empty() {
        return;
    }
    let targets: Vec<usize> = matched.iter().map(|&(index, _)| index).collect();
    let removals = removals(src, block, &targets);
    for ((index, rule), removal) in matched.into_iter().zip(removals) {
        let Some(entry) = block.entries.get(index) else {
            continue;
        };
        out.push(Finding {
            explanation: rule.explanation,
            fix: removal.map(|span| Edit {
                replacement: String::new(),
                span,
            }),
            span: entry.span,
            text: collapse_whitespace(entry.span.text(src)),
        });
    }
}

/// `text` with every whitespace run collapsed to one space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a `#` comment directly follows `pos` on its line (only whitespace
/// in between). After an entry, a `#` always starts a comment.
fn comment_follows(src: &str, pos: usize) -> bool {
    src.get(pos..cst::line_end(src, pos))
        .is_some_and(|rest| rest.trim_start().starts_with('#'))
}

/// `text` as a plain decimal number (an optional `-`, digits, and optionally
/// `.` and more digits), normalized so that numbers with the same value, and
/// only those, compare equal: `(negative, integer digits without leading
/// zeros, fraction digits without trailing zeros)`, zero never negative.
///
/// `None` for anything else, including forms that `f64` parsing accepts
/// (`1e3`, `+5`, `5.`, `inf`): script numbers have no such syntax (jomini
/// rejects exponents too), so the game may read them as something else.
/// Comparing digits rather than `f64`s keeps values that differ only past
/// `f64` precision apart.
fn decimal(text: &str) -> Option<(bool, &str, &str)> {
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (integer, fraction) = match digits.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (digits, ""),
    };
    if integer.is_empty()
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let integer = integer.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let zero = integer.is_empty() && fraction.is_empty();
    Some((negative && !zero, integer, fraction))
}

/// Whether line start `pos` is the line just after the one holding `open`,
/// and nothing but whitespace or a comment follows `open` on its line. (A `{`
/// line that also holds an entry is an ordinary line.)
fn follows_open(src: &str, open: usize, pos: usize) -> bool {
    pos.checked_sub(1)
        .is_some_and(|newline| cst::line_start(src, newline) == cst::line_start(src, open))
        && src
            .get(open + 1..cst::line_end(src, open))
            .is_some_and(|rest| {
                let rest = rest.trim_start();
                rest.is_empty() || rest.starts_with('#')
            })
}

/// Whether `entry` (to be removed with `span`, which contains it) holds a
/// comment, or `span` covers one of `block`'s comments.
fn has_comment(block: &Block, entry: &Entry, span: Span) -> bool {
    block
        .comments
        .iter()
        .any(|comment| span.start <= comment.start && comment.start < span.end)
        || entry
            .value
            .as_block()
            .is_some_and(Block::has_comments_recursive)
}

/// Whether the portion of `src` between `start` and `end` is whitespace only
/// (`false` if the range is invalid).
fn is_blank(src: &str, start: usize, end: usize) -> bool {
    src.get(start..end)
        .is_some_and(|text| text.trim_start().is_empty())
}

/// Which of the blank-line runs around and between the removals of one gap
/// to keep (the others are deleted with the removals). `counts` holds the
/// number of blank lines of each run, from the run above the first removal
/// to the run below the last; `open` / `close` say whether the gap starts
/// right after the block's `{` line / ends right before its `}` line.
///
/// Blank lines that were already next to a brace stay; blank lines that the
/// deletion would bring next to a brace go; between two kept lines the
/// largest separator survives (the first, on a tie).
fn kept_run(counts: &[usize], open: bool, close: bool) -> usize {
    let last = counts.len().saturating_sub(1);
    match (open, close) {
        (true, true) => {
            if counts.first() <= counts.last() {
                0
            } else {
                last
            }
        }
        (true, false) => 0,
        (false, true) => last,
        (false, false) => {
            let most = counts.iter().max();
            counts
                .iter()
                .position(|count| Some(count) == most)
                .unwrap_or_default()
        }
    }
}

/// How many entries of `block` have key `key`.
fn key_count(src: &str, block: &Block, key: &str) -> usize {
    block
        .entries
        .iter()
        .filter(|entry| entry.key_str(src) == Some(key))
        .count()
}

/// The first of `rules` that `entry` of the definition `block` (whose own
/// key is `owner`) matches.
fn matching_rule(
    src: &str,
    owner: Option<&str>,
    block: &Block,
    entry: &Entry,
    rules: &[&'static RedundantRule],
) -> Option<&'static RedundantRule> {
    let key = entry.key_str(src)?;
    if entry.op?.kind != OpKind::Eq {
        return None;
    }
    rules.iter().copied().find(|rule| {
        rule.key == key
            // Removing one of several occurrences could change which one the
            // game uses.
            && (rule.repeatable || key_count(src, block, key) == 1)
            && value_matches(src, rule.value, &entry.value, owner, block)
    })
}

/// Start of the line after the one holding `pos` (`src.len()` on the last
/// line).
fn next_line(src: &str, pos: usize) -> usize {
    let end = cst::line_end(src, pos);
    match src.as_bytes().get(end) {
        // `line_end` only stops at a `\r` that starts a `\r\n`.
        Some(b'\r') => end + 2,
        Some(_) => end + 1,
        None => end,
    }
}

/// Whether `block` holds exactly `pairs`, in any order: one `key = scalar`
/// entry per pair, keys distinct, values compared by [`scalar_eq`].
fn pairs_match(src: &str, block: &Block, pairs: &[(&str, &str)]) -> bool {
    let mut seen: Vec<&str> = Vec::with_capacity(block.entries.len());
    for entry in &block.entries {
        let (Some(key), Some(op), Value::Scalar(value)) =
            (entry.key_str(src), entry.op, &entry.value)
        else {
            return false;
        };
        let expected = pairs
            .iter()
            .find_map(|&(name, expected)| (name == key).then_some(expected));
        if op.kind != OpKind::Eq
            || seen.contains(&key)
            || !expected.is_some_and(|expected| scalar_eq(value.unquoted(src), expected))
        {
            return false;
        }
        seen.push(key);
    }
    seen.len() == pairs.len()
}

/// Whether line start `pos` is the start of the line holding `close`, with
/// only whitespace before the `}`.
fn precedes_close(src: &str, close: usize, pos: usize) -> bool {
    cst::line_start(src, close) == pos && is_blank(src, pos, close)
}

/// The spans that remove each of `targets` (ascending indices of entries of
/// `block`), or `None` where that would delete a comment.
fn removals(src: &str, block: &Block, targets: &[usize]) -> Vec<Option<Span>> {
    let mut spans = vec![None; targets.len()];
    let mut lines = Vec::new();
    if let Some(units) = cst::line_units(src, block) {
        for (slot, &index) in targets.iter().enumerate() {
            let (Some(unit), Some(entry), Some(removal)) = (
                units.get(index),
                block.entries.get(index),
                spans.get_mut(slot),
            ) else {
                continue;
            };
            // Attached comment lines above the entry stay; a trailing
            // comment on its line would go, so it blocks the fix.
            let span = Span {
                end: unit.end,
                start: unit.entry_line_start,
            };
            if !has_comment(block, entry, span) {
                *removal = Some(span);
                lines.push(LineRemoval {
                    first: slot,
                    last: slot,
                    span,
                });
            }
        }
    } else {
        shared_line_removals(src, block, targets, &mut spans, &mut lines);
    }
    tidy_blank_lines(src, block, &lines, &mut spans);
    spans
}

/// Removal spans for `run`, targets (with their slots) that are consecutive
/// entries on one line, removed as a whole: with its line(s) if nothing else
/// is on them, else with the whitespace before it if it ends its line, else
/// with the whitespace after it.
///
/// The removal is split into one piece per target, each of which is also
/// valid applied on its own, or with any other pieces: it never takes the
/// whitespace on both sides of its field, so it cannot join the text around
/// it into one token. Every piece takes its field and the whitespace on one
/// side: before it when the run ends its line after kept text (the kept text
/// keeps the whitespace after it), else after it. When the run fills its
/// line(s), the first piece also takes the indentation and the last the line
/// break, so applied alone they leave the line untidy but valid.
fn run_removals(
    src: &str,
    run: &[(usize, &Entry)],
    spans: &mut [Option<Span>],
    lines: &mut Vec<LineRemoval>,
) {
    let (Some(&(first_slot, first)), Some(&(last_slot, last))) = (run.first(), run.last()) else {
        return;
    };
    let line_start = cst::line_start(src, first.span.start);
    let line_end = cst::line_end(src, last.span.end);
    let alone_before = is_blank(src, line_start, first.span.start);
    let alone_after = is_blank(src, last.span.end, line_end);
    let whole = alone_before && alone_after;
    // Whether the whitespace between two fields goes with the later one.
    let gap_to_later = alone_after && !alone_before;
    let span = if whole {
        Span {
            end: next_line(src, last.span.end),
            start: line_start,
        }
    } else if alone_after {
        let before = src.get(line_start..first.span.start).unwrap_or_default();
        Span {
            end: last.span.end,
            start: first.span.start - (before.len() - before.trim_end().len()),
        }
    } else {
        let after = src.get(last.span.end..line_end).unwrap_or_default();
        Span {
            end: last.span.end + (after.len() - after.trim_start().len()),
            start: first.span.start,
        }
    };
    let mut start = span.start;
    for (position, &(slot, entry)) in run.iter().enumerate() {
        let end = match run.get(position + 1) {
            None => span.end,
            Some(_) if gap_to_later => entry.span.end,
            Some(&(_, next)) => next.span.start,
        };
        if let Some(removal) = spans.get_mut(slot) {
            *removal = Some(Span { end, start });
        }
        start = end;
    }
    if whole {
        lines.push(LineRemoval {
            first: first_slot,
            last: last_slot,
            span,
        });
    }
}

/// Whether `start` and `end` (the end of one entry and the start of the next)
/// are on the same line with only whitespace between them.
fn same_line(src: &str, start: usize, end: usize) -> bool {
    src.get(start..end)
        .is_some_and(|gap| !gap.contains('\n') && gap.trim_start().is_empty())
}

/// Numeric equality when both are plain decimals (see [`decimal`]; `0.0` is
/// `0`), else ASCII case-insensitive equality (`No` is `no`).
fn scalar_eq(actual: &str, expected: &str) -> bool {
    match (decimal(actual), decimal(expected)) {
        (Some(number), Some(expected_number)) => number == expected_number,
        (Some(_) | None, Some(_) | None) => actual.eq_ignore_ascii_case(expected),
    }
}

/// Removal spans for a block whose entries share lines. Each run of
/// removable targets that are consecutive entries on one line is deleted as
/// a whole (see [`run_removals`]).
///
/// A target directly followed by a comment on its line is not removable, as
/// when entries have lines of their own: the comment may describe it.
fn shared_line_removals(
    src: &str,
    block: &Block,
    targets: &[usize],
    spans: &mut [Option<Span>],
    lines: &mut Vec<LineRemoval>,
) {
    let mut runs: Vec<Vec<(usize, &Entry)>> = Vec::new();
    let mut previous: Option<(usize, Span)> = None;
    for (slot, &index) in targets.iter().enumerate() {
        let Some(entry) = block.entries.get(index) else {
            continue;
        };
        if has_comment(block, entry, entry.span) || comment_follows(src, entry.span.end) {
            previous = None;
            continue;
        }
        let joins = previous.is_some_and(|(before, span)| {
            before + 1 == index && same_line(src, span.end, entry.span.start)
        });
        match runs.last_mut() {
            Some(run) if joins => run.push((slot, entry)),
            _ => runs.push(vec![(slot, entry)]),
        }
        previous = Some((index, entry.span));
    }

    for run in &runs {
        let (Some(&(_, first)), Some(&(_, last))) = (run.first(), run.last()) else {
            continue;
        };
        let line_start = cst::line_start(src, first.span.start);
        let ends_line_after_kept = !is_blank(src, line_start, first.span.start)
            && is_blank(src, last.span.end, cst::line_end(src, last.span.end));
        if ends_line_after_kept {
            // Each piece of such a run takes the whitespace before its
            // field (see `run_removals`), so a piece applied alone relies on
            // the whitespace after its field to keep the text before it apart
            // from the next field. Where fields touch (`available = {}visible
            // = {}`) there is none: split the run there. The parts before
            // the last then end at a field and take the whitespace after
            // them, which leaves the whitespace before the run in place.
            for part in run.chunk_by(|(_, earlier), (_, later)| earlier.span.end < later.span.start)
            {
                run_removals(src, part, spans, lines);
            }
        } else {
            run_removals(src, run, spans, lines);
        }
    }
}

/// Extends the whole-line removals of `block` over the blank lines they would
/// otherwise leave doubled, or stranded next to a brace (see [`kept_run`]).
///
/// Removals separated only by blank lines form one gap, bounded by the kept
/// lines (or braces) around it. Deleting the gap's removals merges its blank
/// runs into one; all but one run are deleted along with an adjacent
/// removal: the one on its far side from the kept run (the removal below a
/// run above the kept one, the removal above a run below it), so that each
/// removal also leaves tidy blank lines when applied alone wherever it can.
/// Handling a gap as a whole (rather than each removal on its own) keeps two
/// removals from both claiming the blank line between them. `lines` must be
/// in source order.
fn tidy_blank_lines(src: &str, block: &Block, lines: &[LineRemoval], spans: &mut [Option<Span>]) {
    let (Some(open), Some(close)) = (block.open, block.close) else {
        return;
    };
    let mut index = 0_usize;
    while let Some(first) = lines.get(index) {
        // Blank runs of the gap: `runs[0]` is above `members[0]`, and
        // `runs[k]` below `members[k - 1]`.
        let mut runs = vec![Span {
            end: first.span.start,
            start: blank_lines_before(src, first.span.start),
        }];
        let mut members = vec![*first];
        let mut end = first.span.end;
        index += 1;
        loop {
            let after = blank_lines_after(src, end);
            runs.push(Span {
                end: after,
                start: end,
            });
            match lines.get(index) {
                Some(next) if next.span.start == after => {
                    members.push(*next);
                    end = next.span.end;
                    index += 1;
                }
                _ => break,
            }
        }

        let counts: Vec<usize> = runs
            .iter()
            .map(|run| run.text(src).matches('\n').count())
            .collect();
        let starts_at_open = runs
            .first()
            .is_some_and(|run| follows_open(src, open, run.start));
        let ends_at_close = runs
            .last()
            .is_some_and(|run| precedes_close(src, close, run.end));
        let keep = kept_run(&counts, starts_at_open, ends_at_close);
        for (position, run) in runs.iter().enumerate() {
            if position == keep || run.start == run.end {
                continue;
            }
            // `runs[position]` lies between `members[position - 1]` and
            // `members[position]`.
            let extended = if position < keep {
                members
                    .get(position)
                    .and_then(|below| spans.get_mut(below.first))
            } else {
                position
                    .checked_sub(1)
                    .and_then(|above| members.get(above))
                    .and_then(|above| spans.get_mut(above.last))
            };
            if let Some(Some(span)) = extended {
                span.start = span.start.min(run.start);
                span.end = span.end.max(run.end);
            }
        }
    }
}

/// Whether `value` is `expected`. `block` is the definition block holding
/// the field and `owner` its own key (for [`Redundant::OwnName`]).
fn value_matches(
    src: &str,
    expected: Redundant,
    value: &Value,
    owner: Option<&str>,
    block: &Block,
) -> bool {
    match (expected, value) {
        (Redundant::Scalar(text), Value::Scalar(scalar)) => scalar_eq(scalar.unquoted(src), text),
        (Redundant::EmptyBlock, Value::Block(inner)) => inner.entries.is_empty(),
        (Redundant::Block(pairs), Value::Block(inner)) => pairs_match(src, inner, pairs),
        (Redundant::OwnName, Value::Scalar(scalar)) => {
            owner.is_some_and(|name| scalar.unquoted(src) == name)
                && key_count(src, block, "name") == 0
        }
        (
            Redundant::Block(_) | Redundant::EmptyBlock | Redundant::OwnName | Redundant::Scalar(_),
            Value::Block(_) | Value::Scalar(_) | Value::Tagged { .. },
        ) => false,
    }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{Finding, accepted, find, fix, matching_rule};
    use crate::cst::{self, Block, Span, Value};
    use crate::schema::{self, BlockKind, FileKind, RedundantRule};
    use rayon::prelude::*;
    use std::collections::{BTreeMap, HashSet};
    use std::path::{Path, PathBuf};

    /// Per-rule finding counts: `(kind, key, value) -> (fixable, unfixable)`.
    type RuleCounts = BTreeMap<(String, &'static str, String), (usize, usize)>;

    /// What checking one corpus file found.
    #[derive(Debug, Default)]
    struct FileReport {
        counts: RuleCounts,
        fixed: usize,
        /// Fixes that remove part of a line (fields sharing lines).
        mid_line: usize,
        not_utf8: bool,
        parse_failed: bool,
        violations: Vec<String>,
    }

    /// The texts of the findings in `src`.
    fn texts(src: &str, file: FileKind) -> Vec<String> {
        find(src, file)
            .into_iter()
            .map(|finding| finding.text)
            .collect()
    }

    /// `src` with every fix applied, checking that every fixable finding was
    /// fixed and that the result has nothing left to fix. Also checks every
    /// subset of the fixes with [`check_fix_subsets`].
    fn fixed(src: &str, file: FileKind) -> String {
        let findings = find(src, file);
        let fixable = findings.iter().filter(|f| f.fix.is_some()).count();
        let (text, count) = fix(src, &findings);
        assert_eq!(count, fixable, "not every fix applied in {src:?}");
        assert!(
            find(&text, file).iter().all(|f| f.fix.is_none()),
            "fixable findings remain in {text:?}"
        );
        check_fix_subsets(src, file);
        text
    }

    /// Checks that applying any subset of the fixes of `src` (as a caller
    /// fixing only some rules would) leaves text that parses to the original
    /// entries minus the fixed ones, with every comment kept.
    fn check_fix_subsets(src: &str, file: FileKind) {
        let Ok(doc) = cst::parse(src) else {
            panic!("{src:?} does not parse");
        };
        let fixable: Vec<Finding> = find(src, file)
            .into_iter()
            .filter(|finding| finding.fix.is_some())
            .collect();
        assert!(fixable.len() <= 8, "too many subsets of {src:?}");
        for mask in 0_u32..1 << fixable.len() {
            let subset: Vec<Finding> = fixable
                .iter()
                .enumerate()
                .filter(|&(bit, _)| (mask >> bit) & 1 == 1)
                .map(|(_, finding)| finding.clone())
                .collect();
            let (text, count) = fix(src, &subset);
            assert_eq!(count, subset.len(), "fixes {mask:b} of {src:?}");
            let Ok(fixed_doc) = cst::parse(&text) else {
                panic!("fixes {mask:b} of {src:?} give unparsable {text:?}");
            };
            let removed: HashSet<Span> = subset.iter().map(|finding| finding.span).collect();
            let (mut expected, mut actual) = (String::new(), String::new());
            shape(src, &doc.root, &removed, &mut expected);
            shape(&text, &fixed_doc.root, &HashSet::new(), &mut actual);
            assert_eq!(expected, actual, "fixes {mask:b} of {src:?} give {text:?}");
            assert_eq!(
                comments(src, &doc.root),
                comments(&text, &fixed_doc.root),
                "fixes {mask:b} of {src:?} give {text:?}"
            );
        }
    }

    /// For each finding in `src` with a fix, the text with only that fix
    /// applied.
    fn fixed_alone(src: &str, file: FileKind) -> Vec<String> {
        find(src, file)
            .into_iter()
            .filter(|finding| finding.fix.is_some())
            .map(|finding| fix(src, &[finding]).0)
            .collect()
    }

    /// A decision file with one decision whose body is `body`.
    fn decision(body: &str) -> String {
        format!("cat = {{\n\tdec = {{\n{body}\t}}\n}}\n")
    }

    /// A national focus file with one focus whose body is `body`.
    fn focus(body: &str) -> String {
        format!("focus_tree = {{\n\tid = tree\n\tfocus = {{\n{body}\t}}\n}}\n")
    }

    #[test]
    fn scalar_defaults_are_flagged() {
        let src = decision("\t\tcost = 5\n\t\tfire_only_once = no\n");
        let findings = find(&src, FileKind::Decisions);
        let [finding] = findings.as_slice() else {
            panic!("one finding expected: {findings:?}");
        };
        assert_eq!(finding.text, "fire_only_once = no");
        assert_eq!(finding.explanation, "`fire_only_once` defaults to `no`");
        assert_eq!(finding.span.text(&src), "fire_only_once = no");
        assert_eq!(fixed(&src, FileKind::Decisions), decision("\t\tcost = 5\n"));

        // ASCII case-insensitive, quotes stripped.
        for value in ["NO", "No", "\"no\""] {
            let src = decision(&format!("\t\tfire_only_once = {value}\n"));
            assert_eq!(texts(&src, FileKind::Decisions).len(), 1, "{value}");
        }
        for value in ["yes", "noo", "\"no \"", "0"] {
            let src = decision(&format!("\t\tfire_only_once = {value}\n"));
            assert!(texts(&src, FileKind::Decisions).is_empty(), "{value}");
        }
        // A focus's `cancel_if_invalid` defaults to `yes`.
        let src = focus("\t\tid = f\n\t\tcancel_if_invalid = yes\n\t\tcontinue_if_invalid = no\n");
        assert_eq!(
            texts(&src, FileKind::NationalFocus),
            ["cancel_if_invalid = yes", "continue_if_invalid = no"]
        );
        let src = focus("\t\tid = f\n\t\tcancel_if_invalid = no\n");
        assert!(texts(&src, FileKind::NationalFocus).is_empty());
    }

    #[test]
    fn numbers_compare_numerically() {
        for offset in [
            "x = 0 y = 0",
            "x = 0.0 y = 0",
            "y = 0.000 x = -0",
            "x = \"0\" y = 0",
        ] {
            let src = focus(&format!("\t\tid = f\n\t\toffset = {{ {offset} }}\n"));
            assert_eq!(
                texts(&src, FileKind::NationalFocus),
                [format!("offset = {{ {offset} }}")],
                "{offset}"
            );
        }
        for offset in ["x = 0.5 y = 0", "x = 0", "x = 0 y = 0 trigger = { }"] {
            let src = focus(&format!("\t\tid = f\n\t\toffset = {{ {offset} }}\n"));
            assert!(texts(&src, FileKind::NationalFocus).is_empty(), "{offset}");
        }
        let src = focus("\t\tid = f\n\t\tai_will_do = { factor = 1.00 }\n");
        assert_eq!(
            texts(&src, FileKind::NationalFocus),
            ["ai_will_do = { factor = 1.00 }"]
        );
    }

    #[test]
    fn only_plain_decimals_are_numbers() {
        let tree = |position: &str| {
            format!(
                "focus_tree = {{\n\tid = t\n\tcontinuous_focus_position = {{ {position} }}\n}}\n"
            )
        };
        // Script numbers have no exponent (jomini rejects `5e1` too), so the
        // game may not read `5e1` as 50: not the default as far as we know.
        // The same goes for other forms `f64` parsing accepts.
        for position in [
            "x = 5e1 y = 1e3",
            "x = 50 y = 1E3",
            "x = 50 y = 1000e0",
            "x = .5e2 y = 1000",
            "x = +50 y = 1000",
            "x = 50. y = 1000",
            "x = 50 y = 1000.0f",
            "x = 50 y = inf",
            "x = 50 y = 1_000",
        ] {
            assert!(
                texts(&tree(position), FileKind::NationalFocus).is_empty(),
                "{position}"
            );
        }
        let src = focus("\t\tid = f\n\t\tai_will_do = { factor = 1e0 }\n");
        assert!(texts(&src, FileKind::NationalFocus).is_empty());
        // Plain decimals compare by exact value: leading and trailing zeros
        // and the sign of zero do not matter, but every digit does (`f64`
        // would round the last one away).
        for position in ["x = 050 y = 1000.000", "x = 50.0 y = 01000"] {
            assert_eq!(
                texts(&tree(position), FileKind::NationalFocus).len(),
                1,
                "{position}"
            );
        }
        for position in [
            "x = 50.0000000000000000001 y = 1000",
            "x = 5 y = 1000",
            "x = -50 y = 1000",
        ] {
            assert!(
                texts(&tree(position), FileKind::NationalFocus).is_empty(),
                "{position}"
            );
        }
        let src = focus("\t\tid = f\n\t\toffset = { x = -0.0 y = -000 }\n");
        assert_eq!(texts(&src, FileKind::NationalFocus).len(), 1);
    }

    #[test]
    fn empty_blocks_are_flagged() {
        let src = decision("\t\tavailable = { }\n\t\tvisible = {}\n\t\tallowed = {\n\t\t}\n");
        assert_eq!(
            texts(&src, FileKind::Decisions),
            ["available = { }", "visible = {}", "allowed = { }"]
        );
        assert_eq!(fixed(&src, FileKind::Decisions), decision(""));

        // Comments do not change what an empty block means, but removing
        // the field would delete them.
        let src = decision("\t\tavailable = { # always\n\t\t}\n");
        let findings = find(&src, FileKind::Decisions);
        assert_eq!(findings.len(), 1);
        assert!(findings.iter().all(|finding| finding.fix.is_none()));

        for value in ["{ always = yes }", "rgb { }", "{ { } }", "{ a }"] {
            let src = decision(&format!("\t\tavailable = {value}\n"));
            assert!(texts(&src, FileKind::Decisions).is_empty(), "{value}");
        }
    }

    #[test]
    fn block_patterns_match_in_any_order() {
        let tree = |body: &str| format!("focus_tree = {{\n\tid = tree\n{body}}}\n");
        for position in ["x = 50 y = 1000", "y = 1000 x = 50", "y = 1000.0 x = 50"] {
            let src = tree(&format!("\tcontinuous_focus_position = {{ {position} }}\n"));
            assert_eq!(texts(&src, FileKind::NationalFocus).len(), 1, "{position}");
            assert_eq!(fixed(&src, FileKind::NationalFocus), tree(""));
        }
        for position in [
            "x = 50",
            "x = 50 y = 1000 z = 1",
            "x = 50 x = 50",
            "x = 50 y > 1000",
            "x = 50 y = { }",
            "x = 50 y = 1001",
            "X = 50 y = 1000",
            "x = 50 y = 1000 1",
        ] {
            let src = tree(&format!("\tcontinuous_focus_position = {{ {position} }}\n"));
            assert!(
                texts(&src, FileKind::NationalFocus).is_empty(),
                "{position}"
            );
        }
        let src = decision("\t\tallowed = { always = yes }\n\t\tvisible = { ALWAYS = yes }\n");
        assert_eq!(
            texts(&src, FileKind::Decisions),
            ["allowed = { always = yes }"]
        );
    }

    #[test]
    fn own_name_pictures() {
        let ideas = |idea: &str| {
            format!("ideas = {{\n\tcountry = {{\n\t\tGER_idea = {{\n{idea}\t\t}}\n\t}}\n}}\n")
        };
        let src = ideas("\t\t\tpicture = GER_idea\n\t\t\tmodifier = { x = 1 }\n");
        assert_eq!(texts(&src, FileKind::Ideas), ["picture = GER_idea"]);
        assert_eq!(
            fixed(&src, FileKind::Ideas),
            ideas("\t\t\tmodifier = { x = 1 }\n")
        );
        let src = ideas("\t\t\tpicture = \"GER_idea\"\n");
        assert_eq!(texts(&src, FileKind::Ideas).len(), 1);
        // Case-sensitive, another picture, or a `name` that changes the
        // sprite the idea would use.
        for body in [
            "\t\t\tpicture = ger_idea\n",
            "\t\t\tpicture = GER_idea_2\n",
            "\t\t\tname = other\n\t\t\tpicture = GER_idea\n",
        ] {
            assert!(texts(&ideas(body), FileKind::Ideas).is_empty(), "{body}");
        }
    }

    #[test]
    fn only_plain_assignments_are_flagged() {
        for entry in [
            "fire_only_once != no",
            "fire_only_once ?= no",
            "fire_only_once == no",
            "fire_only_once < no",
        ] {
            let src = decision(&format!("\t\t{entry}\n"));
            assert!(texts(&src, FileKind::Decisions).is_empty(), "{entry}");
        }
        let src = decision("\t\tavailable { }\n");
        assert!(texts(&src, FileKind::Decisions).is_empty());
        // Keys are case-sensitive.
        let src = decision("\t\tFire_only_once = no\n");
        assert!(texts(&src, FileKind::Decisions).is_empty());
    }

    #[test]
    fn repeated_keys() {
        // A duplicated key is left alone: which occurrence wins is unclear.
        for body in [
            "\t\tfire_only_once = no\n\t\tfire_only_once = no\n",
            "\t\tfire_only_once = no\n\t\tfire_only_once = yes\n",
            "\t\tavailable = { }\n\t\tavailable = { has_war = yes }\n",
        ] {
            assert!(
                texts(&decision(body), FileKind::Decisions).is_empty(),
                "{body}"
            );
        }
        // Repeatable keys are judged one occurrence at a time.
        let src = focus(
            "\t\tid = f\n\t\tmutually_exclusive = { }\n\t\tmutually_exclusive = { focus = g }\n\t\tmutually_exclusive = { }\n",
        );
        assert_eq!(
            texts(&src, FileKind::NationalFocus),
            ["mutually_exclusive = { }", "mutually_exclusive = { }"]
        );
        assert_eq!(
            fixed(&src, FileKind::NationalFocus),
            focus("\t\tid = f\n\t\tmutually_exclusive = { focus = g }\n")
        );
    }

    #[test]
    fn only_listed_block_kinds_are_checked() {
        // Not a decision rule for focuses.
        let src = focus("\t\tid = f\n\t\tfire_only_once = no\n");
        assert!(texts(&src, FileKind::NationalFocus).is_empty());
        // A category is not a decision.
        let src = "cat = {\n\tfire_only_once = no\n}\n";
        assert!(texts(src, FileKind::Decisions).is_empty());
        // Nor is a block nested inside a decision.
        let src = decision("\t\tcomplete_effect = {\n\t\t\tfire_only_once = no\n\t\t}\n");
        assert!(texts(&src, FileKind::Decisions).is_empty());
        // The file kind decides.
        let src = decision("\t\tfire_only_once = no\n");
        assert!(texts(&src, FileKind::Other).is_empty());
        assert!(texts(&src, FileKind::Events).is_empty());
    }

    #[test]
    fn event_options() {
        let src = "country_event = {\n\tid = e.1\n\ttrigger = { always = yes }\n\toption = { }\n\toption = {\n\t\tname = e.1.a\n\t\ttrigger = { }\n\t\tai_chance = { base = 1 }\n\t}\n}\n";
        assert_eq!(
            texts(src, FileKind::Events),
            [
                "trigger = { always = yes }",
                "trigger = { }",
                "ai_chance = { base = 1 }"
            ]
        );
        assert_eq!(
            fixed(src, FileKind::Events),
            "country_event = {\n\tid = e.1\n\toption = { }\n\toption = {\n\t\tname = e.1.a\n\t}\n}\n"
        );
        // An empty option is kept whatever else is in the event.
        let src = "country_event = {\n\tid = e.1\n\toption = { }\n}\n";
        assert!(texts(src, FileKind::Events).is_empty());
    }

    #[test]
    fn comments_are_never_deleted() {
        // A trailing comment may describe the field.
        let src = decision("\t\tcost = 5\n\t\tfire_only_once = no # on purpose\n");
        let findings = find(&src, FileKind::Decisions);
        assert_eq!(findings.len(), 1);
        assert!(findings.iter().all(|finding| finding.fix.is_none()));
        assert_eq!(fix(&src, &findings), (src.clone(), 0));

        // A comment between key and value.
        let src = decision("\t\tfire_only_once = # why\n\t\t\tno\n");
        assert!(
            find(&src, FileKind::Decisions)
                .iter()
                .all(|finding| finding.fix.is_none())
        );

        // Comment lines above the field stay where they are.
        let src = decision("\t\tcost = 5\n\t\t# Fires once.\n\t\tfire_only_once = no\n");
        assert_eq!(
            fixed(&src, FileKind::Decisions),
            decision("\t\tcost = 5\n\t\t# Fires once.\n")
        );

        // Also when entries share lines.
        let src = decision("\t\tcost = 5 fire_only_once = no # on purpose\n\t\tis_good = no\n");
        assert_eq!(
            fixed(&src, FileKind::Decisions),
            decision("\t\tcost = 5 fire_only_once = no # on purpose\n")
        );
    }

    #[test]
    fn blank_lines_are_tidied() {
        let dec = FileKind::Decisions;
        // (before, after) pairs of decision bodies.
        for (before, after) in [
            // Right after `{`.
            (
                "\t\tfire_only_once = no\n\n\t\tcost = 5\n",
                "\t\tcost = 5\n",
            ),
            // Right before `}`.
            (
                "\t\tcost = 5\n\n\t\tfire_only_once = no\n",
                "\t\tcost = 5\n",
            ),
            // Between blank lines.
            (
                "\t\tcost = 5\n\n\t\tfire_only_once = no\n\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\t\ticon = x\n",
            ),
            // One blank line on one side only: it stays.
            (
                "\t\tcost = 5\n\t\tfire_only_once = no\n\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\t\ticon = x\n",
            ),
            (
                "\t\tcost = 5\n\n\t\tfire_only_once = no\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\t\ticon = x\n",
            ),
            // Several fields, adjacent and apart.
            (
                "\t\tcost = 5\n\n\t\tfire_only_once = no\n\t\tis_good = no\n\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\t\ticon = x\n",
            ),
            (
                "\t\tcost = 5\n\n\t\tfire_only_once = no\n\n\t\tis_good = no\n\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\t\ticon = x\n",
            ),
            ("\t\tfire_only_once = no\n\n\t\tis_good = no\n", ""),
            ("\n\t\tfire_only_once = no\n\t\tis_good = no\n", ""),
            // Blank lines that were already there stay.
            (
                "\n\t\tfire_only_once = no\n\t\tcost = 5\n",
                "\n\t\tcost = 5\n",
            ),
            (
                "\t\tcost = 5\n\t\tfire_only_once = no\n\n",
                "\t\tcost = 5\n\n",
            ),
            (
                "\t\tcost = 5\n\n\n\t\tfire_only_once = no\n\t\ticon = x\n",
                "\t\tcost = 5\n\n\n\t\ticon = x\n",
            ),
            // Blank lines holding whitespace count as blank.
            (
                "\t\tcost = 5\n  \t\n\t\tfire_only_once = no\n\t\t\n\t\ticon = x\n",
                "\t\tcost = 5\n  \t\n\t\ticon = x\n",
            ),
            // A comment line is not blank: it bounds the gap.
            (
                "\t\tcost = 5\n\t\t# note\n\n\t\tfire_only_once = no\n\n\t\ticon = x\n",
                "\t\tcost = 5\n\t\t# note\n\n\t\ticon = x\n",
            ),
        ] {
            assert_eq!(fixed(&decision(before), dec), decision(after), "{before:?}");
        }
        // A comment after `{` still counts as the `{` line.
        let src = "cat = {\n\tdec = { # note\n\t\tfire_only_once = no\n\n\t\tcost = 5\n\t}\n}\n";
        assert_eq!(
            fixed(src, dec),
            "cat = {\n\tdec = { # note\n\t\tcost = 5\n\t}\n}\n"
        );
    }

    #[test]
    fn shared_lines() {
        let dec = FileKind::Decisions;
        let one_line = |body: &str| format!("cat = {{\n\tdec = {{ {body} }}\n}}\n");
        for (before, after) in [
            ("cost = 5 fire_only_once = no", "cost = 5"),
            ("fire_only_once = no cost = 5", "cost = 5"),
            ("cost = 5 fire_only_once = no icon = x", "cost = 5 icon = x"),
            ("cost = 5 fire_only_once = no is_good = no", "cost = 5"),
            ("fire_only_once = no is_good = no cost = 5", "cost = 5"),
            ("fire_only_once = no cost = 5 is_good = no", "cost = 5"),
            ("available = {\n\t} cost = 5", "cost = 5"),
        ] {
            assert_eq!(fixed(&one_line(before), dec), one_line(after), "{before:?}");
        }
        assert_eq!(
            fixed(&one_line("fire_only_once = no"), dec),
            "cat = {\n\tdec = { }\n}\n"
        );
        assert_eq!(
            fixed("cat = {\n\tdec = {fire_only_once = no}\n}\n", dec),
            "cat = {\n\tdec = {}\n}\n"
        );

        // Lines shared with the braces: an entry ending its line takes the
        // whitespace before it, and whole lines go with their tidied blank
        // lines. A `{` line holding an entry is an ordinary kept line, so
        // the blank line separating it from the next survives.
        let src = "cat = {\n\tdec = { cost = 5 fire_only_once = no\n\t\tis_good = no\n\n\t\ticon = x }\n}\n";
        assert_eq!(
            fixed(src, dec),
            "cat = {\n\tdec = { cost = 5\n\n\t\ticon = x }\n}\n"
        );
        let src = "cat = {\n\tdec = {\n\t\tfire_only_once = no\n\n\t\tcost = 5 icon = x }\n}\n";
        assert_eq!(
            fixed(src, dec),
            "cat = {\n\tdec = {\n\t\tcost = 5 icon = x }\n}\n"
        );
        let src = "cat = {\n\tdec = { cost = 5 icon = x\n\n\t\tfire_only_once = no\n\t}\n}\n";
        assert_eq!(
            fixed(src, dec),
            "cat = {\n\tdec = { cost = 5 icon = x\n\t}\n}\n"
        );
        let src = "cat = {\n\tdec = { cost = 5\n\t\tfire_only_once = no is_good = no\n\t\ticon = x }\n}\n";
        assert_eq!(
            fixed(src, dec),
            "cat = {\n\tdec = { cost = 5\n\t\ticon = x }\n}\n"
        );
        let src = "cat = {\n\tdec = { cost = 5\n\t\tfire_only_once = no\n\t}\n}\n";
        assert_eq!(fixed(src, dec), "cat = {\n\tdec = { cost = 5\n\t}\n}\n");
    }

    #[test]
    fn each_fix_is_valid_on_its_own() {
        let dec = FileKind::Decisions;
        // Fields ending a line shared with a kept field: each fix takes the
        // whitespace before its field only, so applied alone it never joins
        // the fields around it (`cost = 5is_good = no`).
        let src = "cat = {\n\tdec = { cost = 5 fire_only_once = no is_good = no\n\t}\n}\n";
        assert_eq!(
            fixed_alone(src, dec),
            [
                "cat = {\n\tdec = { cost = 5 is_good = no\n\t}\n}\n",
                "cat = {\n\tdec = { cost = 5 fire_only_once = no\n\t}\n}\n",
            ]
        );
        assert_eq!(fixed(src, dec), "cat = {\n\tdec = { cost = 5\n\t}\n}\n");
        // Fields followed by a kept field: each fix takes the whitespace
        // after its field.
        let src = "cat = {\n\tdec = { fire_only_once = no is_good = no cost = 5 }\n}\n";
        assert_eq!(
            fixed_alone(src, dec),
            [
                "cat = {\n\tdec = { is_good = no cost = 5 }\n}\n",
                "cat = {\n\tdec = { fire_only_once = no cost = 5 }\n}\n",
            ]
        );
        // Fields not separated by whitespace: no way to split the removal so
        // that each part is valid on its own and all of them leave no stray
        // whitespace, so validity wins and a trailing space is left.
        let src = "cat = {\n\tdec = { cost = 5 available = {}fire_only_once = no\n\t}\n}\n";
        assert_eq!(
            fixed_alone(src, dec),
            [
                "cat = {\n\tdec = { cost = 5 fire_only_once = no\n\t}\n}\n",
                "cat = {\n\tdec = { cost = 5 available = {}\n\t}\n}\n",
            ]
        );
        assert_eq!(fixed(src, dec), "cat = {\n\tdec = { cost = 5 \n\t}\n}\n");
        let src = "cat = {\n\tdec = { cost = 5 available = {}visible = {}fire_only_once = no is_good = no\n\t}\n}\n";
        assert_eq!(fixed(src, dec), "cat = {\n\tdec = { cost = 5 \n\t}\n}\n");
        let src = "cat = {\n\tdec = {available = {}fire_only_once = no cost = 5}\n}\n";
        assert_eq!(fixed(src, dec), "cat = {\n\tdec = {cost = 5}\n}\n");
        // Several fields alone on their line: applied alone, the first fix
        // takes the indentation and the last the line break (valid, if
        // untidy); together they take the whole line.
        let src = decision("\t\tfire_only_once = no is_good = no\n\t\tcost = 5\n");
        assert_eq!(fixed(&src, dec), decision("\t\tcost = 5\n"));
        // Blank lines deleted along with a field go with the one on the far
        // side from the blank line that stays, so each fix alone also leaves
        // tidy blank lines here.
        let src = decision("\t\tcost = 5\n\n\t\tfire_only_once = no\n\n\t\tis_good = no\n");
        assert_eq!(
            fixed_alone(&src, dec),
            [
                decision("\t\tcost = 5\n\n\t\tis_good = no\n"),
                decision("\t\tcost = 5\n\n\t\tfire_only_once = no\n"),
            ]
        );
        assert_eq!(fixed(&src, dec), decision("\t\tcost = 5\n"));
    }

    #[test]
    fn every_small_layout_fixes_cleanly() {
        // Every arrangement of up to three fields (kept and redundant) with
        // every kind of separator: each subset of the fixes must leave valid,
        // equivalent script (see `fixed`), and all of them together must not
        // add doubled or brace-adjacent blank lines.
        let fields = [
            "cost = 5",
            "fire_only_once = no",
            "is_good = no",
            "available = {}",
        ];
        let separators = ["", " ", "\n\t\t", "\n\n\t\t"];
        let mut bodies: Vec<String> = fields.iter().map(ToString::to_string).collect();
        let mut last = bodies.clone();
        for _ in 1_u8..3 {
            let mut longer = Vec::new();
            for body in &last {
                for separator in separators {
                    for field in fields {
                        longer.push(format!("{body}{separator}{field}"));
                    }
                }
            }
            bodies.extend(longer.iter().cloned());
            last = longer;
        }
        let mut checked = 0_usize;
        for body in &bodies {
            for open in ["", " ", "\n\t\t", "\n\n\t\t"] {
                for close in ["", " ", "\n\t", "\n\n\t"] {
                    let src = format!("cat = {{\n\tdec = {{{open}{body}{close}}}\n}}\n");
                    if cst::parse(&src).is_err() {
                        continue;
                    }
                    let text = fixed(&src, FileKind::Decisions);
                    let (before, after) = (blank_line_stats(&src), blank_line_stats(&text));
                    // A fix within a line never removes or adds a line
                    // break, so it can only turn a line holding a brace
                    // and the field into one holding just the brace, next
                    // to a blank line that was already there.
                    let whole_lines = find(&src, FileKind::Decisions)
                        .iter()
                        .filter_map(|finding| finding.fix.as_ref())
                        .all(|edit| removes_whole_lines(&src, edit.span));
                    let checked_stats = if whole_lines { 3 } else { 1 };
                    assert!(
                        after
                            .iter()
                            .zip(before)
                            .take(checked_stats)
                            .all(|(after, before)| *after <= before),
                        "{src:?} -> {text:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 5000, "only {checked} layouts parse");
    }

    #[test]
    fn crlf_and_bom_are_preserved() {
        let src = "\u{feff}cat = {\r\n\tdec = {\r\n\t\tcost = 5\r\n\r\n\t\tfire_only_once = no\r\n\r\n\t\ticon = x\r\n\t\tis_good = no\r\n\t}\r\n}\r\n";
        assert_eq!(
            fixed(src, FileKind::Decisions),
            "\u{feff}cat = {\r\n\tdec = {\r\n\t\tcost = 5\r\n\r\n\t\ticon = x\r\n\t}\r\n}\r\n"
        );
        let src = "cat = {\r\n\tdec = { cost = 5 fire_only_once = no\r\n\t}\r\n}";
        assert_eq!(
            fixed(src, FileKind::Decisions),
            "cat = {\r\n\tdec = { cost = 5\r\n\t}\r\n}"
        );
    }

    #[test]
    fn text_collapses_whitespace() {
        let src = decision("\t\tavailable = {\r\n\r\n\t\t}\n\t\tfire_only_once\t=\n\t\t\tno\n");
        assert_eq!(
            texts(&src, FileKind::Decisions),
            ["available = { }", "fire_only_once = no"]
        );
    }

    #[test]
    fn unparsable_files_have_no_findings() {
        for src in [
            "cat = {\n\tdec = {\n\t\tfire_only_once = no\n\t}\n",
            "cat = {\n\tdec = {\n\t\tfire_only_once = no\n\t}\n}\n}\n",
            "cat = {\n\tdec = {\n\t\tfire_only_once =\n\t}\n}\n",
        ] {
            assert!(find(src, FileKind::Decisions).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn overlapping_fixes_are_skipped() {
        let src = "a = 1 b = 2";
        let finding = |start, end| Finding {
            explanation: "",
            fix: Some(crate::cst::Edit {
                replacement: String::new(),
                span: Span { end, start },
            }),
            span: Span { end, start },
            text: String::new(),
        };
        let findings = [finding(0, 6), finding(4, 8), finding(6, 11)];
        assert_eq!(fix(src, &findings), (String::new(), 2));
        let unfixable = Finding {
            fix: None,
            ..finding(0, 1)
        };
        assert_eq!(fix(src, &[unfixable]), (src.to_owned(), 0));
        // An invalid edit leaves the text alone.
        assert_eq!(fix(src, &[finding(0, 99)]), (src.to_owned(), 0));
    }

    #[test]
    fn findings_are_in_source_order() {
        let src = "country_event = {\n\tid = e.1\n\toption = {\n\t\ttrigger = { }\n\t}\n\thidden = no\n}\n";
        let starts: Vec<usize> = find(src, FileKind::Events)
            .iter()
            .map(|finding| finding.span.start)
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(starts.is_sorted());
    }

    // ---------------------------------------------------------------------
    // Corpus
    // ---------------------------------------------------------------------

    /// Every script file under `root` with its kind.
    fn corpus_files(root: &Path) -> Vec<(PathBuf, FileKind)> {
        let mut files: Vec<(PathBuf, FileKind)> = ["common", "events", "history"]
            .iter()
            .flat_map(|dir| walkdir::WalkDir::new(root.join(dir)))
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_type().is_dir())
            .filter_map(|entry| {
                let kind = schema::file_kind(entry.path().strip_prefix(root).ok()?)?;
                Some((entry.into_path(), kind))
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// Structural rendering of `block` without the entries in `removed`:
    /// keys, operators, scalars and nesting, as written.
    fn shape(src: &str, block: &Block, removed: &HashSet<Span>, out: &mut String) {
        for entry in &block.entries {
            if removed.contains(&entry.span) {
                continue;
            }
            if let Some(key) = entry.key {
                out.push_str(key.text(src));
            }
            if let Some(op) = entry.op {
                out.push_str(op.kind.as_str());
            }
            match &entry.value {
                Value::Scalar(scalar) => out.push_str(scalar.text(src)),
                Value::Block(inner) => {
                    out.push('{');
                    shape(src, inner, removed, out);
                    out.push('}');
                }
                Value::Tagged { block: inner, tag } => {
                    out.push_str(tag.text(src));
                    out.push('{');
                    shape(src, inner, removed, out);
                    out.push('}');
                }
            }
            out.push('\u{1f}');
        }
    }

    /// Every comment of `block` and its descendants, in source order.
    fn comments<'src>(src: &'src str, block: &Block) -> Vec<&'src str> {
        let mut spans = Vec::new();
        let mut stack = vec![block];
        while let Some(current) = stack.pop() {
            spans.extend(current.comments.iter().copied());
            stack.extend(
                current
                    .entries
                    .iter()
                    .filter_map(|entry| entry.value.as_block()),
            );
        }
        spans.sort_by_key(|span| span.start);
        spans.into_iter().map(|span| span.text(src)).collect()
    }

    /// Whether `span` runs from a line start to a line start (or the end of
    /// `src`).
    fn removes_whole_lines(src: &str, span: Span) -> bool {
        cst::line_start(src, span.start) == span.start
            && (span.end == src.len() || cst::line_start(src, span.end) == span.end)
    }

    /// Doubled blank lines, blank lines after a `{` line and blank lines
    /// before a `}` line.
    fn blank_line_stats(text: &str) -> [usize; 3] {
        let lines: Vec<&str> = text.lines().collect();
        let blank = |line: &str| line.trim().is_empty();
        let mut stats = [0; 3];
        for pair in lines.windows(2) {
            let [above, below] = pair else { continue };
            stats[0] += usize::from(blank(above) && blank(below));
            stats[1] += usize::from(above.trim_end().ends_with('{') && blank(below));
            stats[2] += usize::from(blank(above) && below.trim() == "}");
        }
        stats
    }

    /// The kind and rule behind each finding in `src`, by span.
    fn rules_by_span(
        src: &str,
        file: FileKind,
    ) -> BTreeMap<usize, (BlockKind, &'static RedundantRule)> {
        let mut out = BTreeMap::new();
        let Ok(doc) = cst::parse(src) else {
            return out;
        };
        cst::visit_blocks(&doc, &mut |path, owner, block| {
            let Some(kind) = schema::block_kind(file, path) else {
                return;
            };
            let rules = schema::redundant_rules(kind);
            for entry in &block.entries {
                if let Some(rule) = matching_rule(src, owner.key_str(src), block, entry, rules) {
                    out.insert(entry.span.start, (kind, rule));
                }
            }
        });
        out
    }

    /// Whether applying only `finding`'s fix to `src` (parsed as `doc`)
    /// leaves text that parses to the original entries minus the field, with
    /// every comment kept.
    fn fixes_alone_cleanly(src: &str, doc: &cst::Document<'_>, finding: &Finding) -> bool {
        let (text, count) = fix(src, std::slice::from_ref(finding));
        let Ok(fixed_doc) = cst::parse(&text) else {
            return false;
        };
        let (mut expected, mut actual) = (String::new(), String::new());
        shape(
            src,
            &doc.root,
            &HashSet::from([finding.span]),
            &mut expected,
        );
        shape(&text, &fixed_doc.root, &HashSet::new(), &mut actual);
        count == 1
            && expected == actual
            && comments(src, &doc.root) == comments(&text, &fixed_doc.root)
    }

    /// Finds and fixes `path`, checking that the fix is lossless apart from
    /// the removed fields, also when each fix is applied on its own.
    fn check_corpus_file(path: &Path, file: FileKind) -> FileReport {
        let mut report = FileReport::default();
        let Ok(src) = std::fs::read_to_string(path) else {
            report.not_utf8 = true;
            return report;
        };
        let name = path.display();
        let Ok(doc) = cst::parse(&src) else {
            report.parse_failed = true;
            if !find(&src, file).is_empty() {
                report
                    .violations
                    .push(format!("{name}: findings in an unparsable file"));
            }
            return report;
        };
        let findings = find(&src, file);
        let rules = rules_by_span(&src, file);
        if rules.len() != findings.len()
            || findings
                .iter()
                .any(|finding| !rules.contains_key(&finding.span.start))
        {
            report
                .violations
                .push(format!("{name}: findings do not match their rules"));
        }
        for finding in &findings {
            if let Some((kind, rule)) = rules.get(&finding.span.start) {
                let counts = report
                    .counts
                    .entry((format!("{kind:?}"), rule.key, format!("{:?}", rule.value)))
                    .or_default();
                if finding.fix.is_some() {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
            }
        }

        let taken = accepted(&findings);
        let fixable = findings.iter().filter(|f| f.fix.is_some()).count();
        if taken.len() != fixable {
            report.violations.push(format!(
                "{name}: {} of {fixable} fixes overlap",
                fixable - taken.len()
            ));
        }
        let (text, count) = fix(&src, &findings);
        report.fixed = count;
        if count != taken.len() {
            report.violations.push(format!(
                "{name}: fix applied {count} of {} fixes",
                taken.len()
            ));
        }

        // (a) The result parses.
        let fixed_doc = match cst::parse(&text) {
            Ok(fixed_doc) => fixed_doc,
            Err(err) => {
                report
                    .violations
                    .push(format!("{name}: fixed text does not parse: {err}"));
                return report;
            }
        };
        // (b) Only the fixed fields went; every comment stayed.
        let removed: HashSet<Span> = taken.iter().map(|finding| finding.span).collect();
        let (mut expected, mut actual) = (String::new(), String::new());
        shape(&src, &doc.root, &removed, &mut expected);
        shape(&text, &fixed_doc.root, &HashSet::new(), &mut actual);
        if expected != actual {
            report
                .violations
                .push(format!("{name}: entries changed beyond the removed fields"));
        }
        if comments(&src, &doc.root) != comments(&text, &fixed_doc.root) {
            report.violations.push(format!("{name}: comments changed"));
        }
        // (c) Nothing fixable is left, and nothing new appeared.
        let after = find(&text, file);
        if after.iter().any(|finding| finding.fix.is_some()) {
            report
                .violations
                .push(format!("{name}: fixable findings remain after fixing"));
        }
        if after.len() != findings.len() - count {
            report.violations.push(format!(
                "{name}: {} findings after fixing, expected {}",
                after.len(),
                findings.len() - count
            ));
        }
        // Each fix is also valid applied alone (as by a caller fixing only
        // some rules).
        let broken: Vec<&Finding> = taken
            .par_iter()
            .copied()
            .filter(|finding| !fixes_alone_cleanly(&src, &doc, finding))
            .collect();
        if let Some(first) = broken.first() {
            report.violations.push(format!(
                "{name}: {} fixes break the file applied alone, e.g. `{}` at {}",
                broken.len(),
                first.text,
                first.span.start
            ));
        }
        report.mid_line = taken
            .iter()
            .filter_map(|finding| finding.fix.as_ref())
            .filter(|edit| !removes_whole_lines(&src, edit.span))
            .count();
        // Tidying never adds a doubled blank line or one next to a brace.
        let (before_stats, after_stats) = (blank_line_stats(&src), blank_line_stats(&text));
        if after_stats
            .iter()
            .zip(before_stats)
            .any(|(after, before)| *after > before)
        {
            report.violations.push(format!(
                "{name}: blank lines worsened: {before_stats:?} -> {after_stats:?}"
            ));
        }
        report
    }

    /// For each `;`-separated root in `HEARTY_CORPUS`: finds and fixes every
    /// script file in memory, checking that the fixed text parses, that it
    /// differs from the original only by the removed fields (entries compared
    /// structurally, comments exactly), that nothing fixable is left, and that
    /// no doubled or brace-adjacent blank line was added; and that each fix
    /// applied on its own passes the same parse and lossless checks. Prints
    /// per-rule counts per root.
    ///
    /// Run with `cargo test --release corpus_fixes -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_fixes_are_lossless() {
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let mut violations = Vec::new();
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            let files = corpus_files(Path::new(root));
            let reports: Vec<FileReport> = files
                .par_iter()
                .map(|(path, file)| check_corpus_file(path, *file))
                .collect();
            let mut counts = RuleCounts::new();
            let mut by_key: BTreeMap<(&'static str, String), (usize, usize)> = BTreeMap::new();
            for ((kind, key, value), (fixable, unfixable)) in
                reports.iter().flat_map(|report| report.counts.clone())
            {
                let total = counts.entry((kind, key, value.clone())).or_default();
                total.0 += fixable;
                total.1 += unfixable;
                let total = by_key.entry((key, value)).or_default();
                total.0 += fixable;
                total.1 += unfixable;
            }
            let fixable: usize = counts.values().map(|counts| counts.0).sum();
            let unfixable: usize = counts.values().map(|counts| counts.1).sum();
            println!(
                "\n== {root}\nfiles: {} | not UTF-8: {} | parse failures: {} | findings: {} \
                 (fixable {fixable}, unfixable {unfixable}) | fixed: {} (mid-line: {}) | \
                 files changed: {}",
                reports.len(),
                reports.iter().filter(|report| report.not_utf8).count(),
                reports.iter().filter(|report| report.parse_failed).count(),
                fixable + unfixable,
                reports.iter().map(|report| report.fixed).sum::<usize>(),
                reports.iter().map(|report| report.mid_line).sum::<usize>(),
                reports.iter().filter(|report| report.fixed > 0).count(),
            );
            println!("  by kind (fixable + unfixable):");
            for ((kind, key, value), (fixable, unfixable)) in &counts {
                println!("    {kind:<16} {key:<26} {value:<28} {fixable:>6} + {unfixable}");
            }
            println!("  by rule (fixable + unfixable):");
            for ((key, value), (fixable, unfixable)) in &by_key {
                println!("    {key:<26} {value:<28} {fixable:>6} + {unfixable}");
            }
            violations.extend(reports.into_iter().flat_map(|report| report.violations));
        }
        println!("\nVIOLATIONS: {}", violations.len());
        for violation in violations.iter().take(50) {
            println!("    {violation}");
        }
        assert!(violations.is_empty(), "see the violations above");
    }
}
