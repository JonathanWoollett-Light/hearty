//! Formatter rule: joins short blocks onto one line and normalises spacing
//! (`a = b`, `{ a = b }`).
//!
//! Every block the rule rewrites gets its canonical single-line form: `{ }`
//! when empty, else `{ ` + its entries joined by one space + ` }`, where an
//! entry is `key op value`, `key { .. }` or a bare value, a tagged value is
//! `tag { .. }`, and scalars (quotes included) are copied verbatim.
//!
//! The rule works in rounds until one changes nothing:
//!
//! 1. Spacing. Every single-line block whose text differs from its canonical
//!    form is replaced by it. Outside single-line blocks, the gaps between a
//!    key and its operator, an operator and its value, a key and the `{` of
//!    an operator-less block, and a tag and its `{` become exactly one space
//!    when they hold only spaces and tabs; a gap holding a line break or a
//!    comment is left alone.
//! 2. Joining. A multi-line block is replaced by its canonical form when it
//!    holds exactly one `key op scalar` entry or only bare scalars, has no
//!    comment and no multi-line string, is not a definition block (see
//!    [`schema::block_kind`]), and the line it then forms fits in
//!    `max_width` columns. Empty blocks are left alone: vanilla writes most
//!    of them over two lines.
//!
//! Each join is measured against the text as it was at the start of its
//! round, so joins in one round never share a line: in `} b = {` the block
//! that closes is joined in one round and the one that opens in the next,
//! measured on the line the first join produced. When a join leaves enclosing
//! blocks on one line (`a = {b = {` .. `}}`), the outermost of them is
//! rendered instead, so the line is canonical and measured as it will be.
//!
//! Both passes only rewrite whitespace between tokens and never insert a
//! line break, so the token sequence, comments, line endings and a leading
//! BOM are preserved. Joining is one-way: a single-line block is never split,
//! however long.

use crate::cst::{self, Block, Document, Edit, Entry, Scalar, Span, Value};
use crate::schema::{self, FileKind};

/// The UTF-8 byte order mark; a leading one takes no columns.
const BOM: char = '\u{feff}';

/// Most spacing/joining rounds [`apply_doc`] runs. Only a safety net: spacing
/// is idempotent and joins produce canonical text, so only the first round
/// respaces; every later round but the last joins at least one block; and a
/// chain of `n` blocks sharing lines (`} b = {`) takes about `log2(n) + 1`
/// joining rounds, as each round joins every other remaining link.
const MAX_ROUNDS: usize = 64;

/// A tab advances to the next multiple of this many columns.
const TAB_WIDTH: usize = 4;

/// What [`apply_doc`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Multi-line blocks joined onto one line.
    pub joined: usize,
    /// Entries whose spacing around an operator, or before the `{` of an
    /// operator-less or tagged block, was normalised, plus single-line
    /// blocks rewritten in their canonical form.
    pub respaced: usize,
}

/// A block joined in the current round.
#[derive(Debug)]
struct Join<'doc> {
    /// Key path of the joined block (as [`cst::visit_blocks`] reports it).
    path: Vec<&'doc str>,
    /// Canonical rendering of the replaced block.
    rendering: String,
    /// `{` ..= `}` of the replaced block: the joined block itself, or the
    /// outermost enclosing block that joining it leaves on a single line.
    span: Span,
}

/// What one spacing or joining round did.
#[derive(Debug)]
enum Round {
    /// It rewrote the text.
    Changed(String),
    /// There was nothing left to do.
    Settled,
}

/// Pass 2 of one round: finds the blocks to join.
struct Joiner<'doc> {
    file: FileKind,
    joins: Vec<Join<'doc>>,
    /// Where the last line rewritten by this round's previous join ends.
    last_line_end: Option<usize>,
    max_width: usize,
    src: &'doc str,
}

impl<'doc> Joiner<'doc> {
    /// Records a join of `block` (the value of the entry at `path`, inside
    /// the non-root `ancestors`, outermost first) if it may be joined.
    fn consider(&mut self, path: &[&'doc str], block: &'doc Block, ancestors: &[&'doc Block]) {
        let src = self.src;
        let Some(span) = block.span() else {
            return;
        };
        let joinable = has_joinable_entries(src, block)
            && !block.has_comments_recursive()
            && schema::block_kind(self.file, path).is_none();
        if !joinable {
            return;
        }
        let first_line = cst::line_start(src, span.start);
        if self
            .last_line_end
            .is_some_and(|last_line_end| first_line <= last_line_end)
        {
            // The previous join rewrites this block's first line, so this
            // block's width can only be known next round.
            return;
        }
        let last_line_end = cst::line_end(src, span.end - 1);
        // Enclosing blocks that open on this block's first line and close on
        // its last one are left on a single line by the join; render the
        // outermost of them so the result is canonical and measured exactly.
        let target = ancestors
            .iter()
            .rev()
            .take_while(|ancestor| {
                ancestor.span().is_some_and(|outer| {
                    cst::line_start(src, outer.start) == first_line
                        && cst::line_end(src, outer.end - 1) == last_line_end
                })
            })
            .last()
            .copied()
            .unwrap_or(block);
        let Some(target_span) = target.span() else {
            return;
        };
        let rendering = render(src, target);
        let prefix_start = first_line.max(bom_len(src));
        let (Some(prefix), Some(suffix)) = (
            src.get(prefix_start..target_span.start),
            src.get(target_span.end..last_line_end),
        ) else {
            return;
        };
        if columns(&[prefix, &rendering, suffix.trim_end()]) > self.max_width {
            return;
        }
        self.last_line_end = Some(last_line_end);
        self.joins.push(Join {
            path: path.to_vec(),
            rendering,
            span: target_span,
        });
    }

    /// Considers every multi-line block nested in `block`, in source order
    /// (which is what makes the line-sharing check against the previous join
    /// sufficient). `path` is `block`'s key path and `ancestors` the non-root
    /// blocks from the outermost down to `block` itself.
    fn visit(
        &mut self,
        block: &'doc Block,
        path: &mut Vec<&'doc str>,
        ancestors: &mut Vec<&'doc Block>,
    ) {
        for entry in &block.entries {
            let Some(inner) = entry.value.as_block() else {
                continue;
            };
            // Nothing inside a single-line block can be joined.
            if inner.is_single_line(self.src) {
                continue;
            }
            path.push(entry.key_str(self.src).unwrap_or_default());
            self.consider(path, inner, ancestors);
            ancestors.push(inner);
            self.visit(inner, path, ancestors);
            ancestors.pop();
            path.pop();
        }
    }
}

/// Joins short blocks and normalises spacing in `src`, keeping every line it
/// creates within `max_width` columns (tabs count as 4). Returns `src`
/// unchanged if it does not parse.
#[cfg(test)]
pub fn apply(src: &str, file: FileKind, max_width: usize) -> (String, Stats) {
    apply_observed(src, file, max_width, &mut |_| {})
}

/// Joins short blocks and normalises spacing in the parsed `doc`, keeping
/// every line it creates within `max_width` columns (tabs count as 4).
/// Returns `None` if that changes nothing; else the new text, what changed,
/// and the new text's parse when the last round made one (it does unless
/// [`MAX_ROUNDS`] ran out).
pub fn apply_doc(
    doc: &Document<'_>,
    file: FileKind,
    max_width: usize,
) -> Option<(String, Stats, Option<Block>)> {
    rounds(doc, file, max_width, &mut |_| {})
}

/// [`apply`], calling `on_join` with the key path of every block it joins.
#[cfg(test)]
fn apply_observed(
    src: &str,
    file: FileKind,
    max_width: usize,
    on_join: &mut dyn FnMut(&[&str]),
) -> (String, Stats) {
    cst::parse(src)
        .ok()
        .and_then(|doc| rounds(&doc, file, max_width, on_join))
        .map_or_else(
            || (src.to_owned(), Stats::default()),
            |(text, stats, _)| (text, stats),
        )
}

/// Byte length of a leading BOM (0 if there is none).
fn bom_len(src: &str) -> usize {
    if src.starts_with(BOM) {
        BOM.len_utf8()
    } else {
        0
    }
}

/// The display width of `pieces` laid end to end from column 0: a tab
/// advances to the next multiple of [`TAB_WIDTH`], any other char takes one
/// column.
fn columns(pieces: &[&str]) -> usize {
    pieces
        .iter()
        .flat_map(|piece| piece.chars())
        .fold(0, |column, ch| {
            if ch == '\t' {
                column / TAB_WIDTH * TAB_WIDTH + TAB_WIDTH
            } else {
                column + 1
            }
        })
}

/// Whether `block`'s contents may be joined: exactly one `key op scalar`
/// entry, or one or more bare scalars, with no scalar spanning lines.
fn has_joinable_entries(src: &str, block: &Block) -> bool {
    let one_line = |scalar: &Scalar| !scalar.text(src).contains('\n');
    match block.entries.as_slice() {
        [] => false,
        [
            Entry {
                key: Some(key),
                value: Value::Scalar(value),
                ..
            },
        ] => one_line(key) && one_line(value),
        entries => entries.iter().all(|entry| {
            entry.key.is_none() && matches!(&entry.value, Value::Scalar(value) if one_line(value))
        }),
    }
}

/// The canonical single-line rendering of `block`.
fn render(src: &str, block: &Block) -> String {
    let mut out = String::new();
    render_block(src, block, &mut out);
    out
}

/// Appends the canonical single-line rendering of `block` to `out`.
fn render_block(src: &str, block: &Block, out: &mut String) {
    if block.entries.is_empty() {
        out.push_str("{ }");
        return;
    }
    out.push('{');
    for entry in &block.entries {
        out.push(' ');
        if let Some(key) = entry.key {
            out.push_str(key.text(src));
            out.push(' ');
            if let Some(op) = entry.op {
                out.push_str(op.kind.as_str());
                out.push(' ');
            }
        }
        match &entry.value {
            Value::Scalar(scalar) => out.push_str(scalar.text(src)),
            Value::Block(inner) => render_block(src, inner, out),
            Value::Tagged { block: inner, tag } => {
                out.push_str(tag.text(src));
                out.push(' ');
                render_block(src, inner, out);
            }
        }
    }
    out.push_str(" }");
}

/// Pushes edits making each gap of `entry` that holds only spaces and tabs
/// (key–operator, operator–value, key–`{` of an operator-less block, tag–`{`)
/// exactly one space. Returns whether it pushed any.
fn respace_entry(src: &str, entry: &Entry, edits: &mut Vec<Edit>) -> bool {
    let gap = |start: usize, end: usize| Some(Span { end, start });
    let value_start = entry.value.span().start;
    let (before_op, after_op) = match (entry.key, entry.op) {
        (Some(key), Some(op)) => (
            gap(key.span.end, op.span.start),
            gap(op.span.end, value_start),
        ),
        (Some(key), None) => (gap(key.span.end, value_start), None),
        (None, _) => (None, None),
    };
    let before_brace = match &entry.value {
        Value::Tagged { block, tag } => block.open.and_then(|open| gap(tag.span.end, open)),
        Value::Block(_) | Value::Scalar(_) => None,
    };
    let pushed = edits.len();
    for span in [before_op, after_op, before_brace].into_iter().flatten() {
        let text = span.text(src);
        if text != " " && text.bytes().all(|byte| byte == b' ' || byte == b'\t') {
            edits.push(Edit {
                replacement: " ".to_owned(),
                span,
            });
        }
    }
    edits.len() > pushed
}

/// One spacing or joining round on `doc`. `None` if, defensively, an edit
/// cannot be applied.
fn round(
    doc: &Document<'_>,
    file: FileKind,
    max_width: usize,
    on_join: &mut dyn FnMut(&[&str]),
    stats: &mut Stats,
) -> Option<Round> {
    let (spacing, respaced) = spacing_edits(doc);
    if !spacing.is_empty() {
        stats.respaced += respaced;
        // The next round re-parses: joining measures lines as they are
        // after spacing.
        return cst::apply_edits(doc.src, spacing).map(Round::Changed);
    }
    let mut joiner = Joiner {
        file,
        joins: Vec::new(),
        last_line_end: None,
        max_width,
        src: doc.src,
    };
    joiner.visit(&doc.root, &mut Vec::new(), &mut Vec::new());
    if joiner.joins.is_empty() {
        return Some(Round::Settled);
    }
    stats.joined += joiner.joins.len();
    let edits = joiner
        .joins
        .into_iter()
        .map(|join| {
            on_join(&join.path);
            Edit {
                replacement: join.rendering,
                span: join.span,
            }
        })
        .collect();
    cst::apply_edits(doc.src, edits).map(Round::Changed)
}

/// Runs spacing and joining rounds on `doc` until one changes nothing (or
/// [`MAX_ROUNDS`] is reached). Returns `None` if the first round changes
/// nothing, or, defensively, if an edit cannot be applied or its result
/// does not parse; else the new text, what changed and its parse when the
/// last round made one.
fn rounds(
    doc: &Document<'_>,
    file: FileKind,
    max_width: usize,
    on_join: &mut dyn FnMut(&[&str]),
) -> Option<(String, Stats, Option<Block>)> {
    let mut stats = Stats::default();
    let Round::Changed(mut text) = round(doc, file, max_width, on_join, &mut stats)? else {
        return None;
    };
    for _ in 1..MAX_ROUNDS {
        let next = cst::parse(&text).ok()?;
        let Round::Changed(changed) = round(&next, file, max_width, on_join, &mut stats)? else {
            let root = next.root;
            return Some((text, stats, Some(root)));
        };
        text = changed;
    }
    Some((text, stats, None))
}

/// Pass 1: edits canonicalising every single-line block and respacing every
/// entry outside one, plus how many blocks and entries they change.
fn spacing_edits(doc: &Document<'_>) -> (Vec<Edit>, usize) {
    let src = doc.src;
    let mut edits = Vec::new();
    let mut changed: usize = 0;
    let mut stack = vec![&doc.root];
    // Most single-line blocks are canonical already; rendering each into one
    // reused buffer spares an allocation per block.
    let mut rendering = String::new();
    while let Some(block) = stack.pop() {
        for entry in &block.entries {
            if respace_entry(src, entry, &mut edits) {
                changed += 1;
            }
            let Some(inner) = entry.value.as_block() else {
                continue;
            };
            if !inner.is_single_line(src) {
                stack.push(inner);
                continue;
            }
            // The rendering covers everything nested in `inner`, so it is not
            // descended into. (It holds no comment: one would swallow the `}`.)
            let Some(span) = inner.span() else {
                continue;
            };
            rendering.clear();
            render_block(src, inner, &mut rendering);
            if span.text(src) != rendering {
                edits.push(Edit {
                    replacement: rendering.clone(),
                    span,
                });
                changed += 1;
            }
        }
    }
    (edits, changed)
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{Stats, apply, apply_observed, columns};
    use crate::cst::{self, Block, Document, Span, Value};
    use crate::schema::{self, FileKind};
    use rayon::prelude::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// Whitespace between tokens that may be glued together.
    const GAPS: &[&str] = &["", " ", "  ", "\t", " \t", "\n", "\r\n"];

    /// Keys the random documents use (some are definition keys somewhere).
    const KEYS: &[&str] = &[
        "k",
        "has_war",
        "\"quoted key\"",
        "focus",
        "option",
        "id",
        "1939.1.1",
    ];

    /// Operators the random documents use.
    const OPS: &[&str] = &["=", "<", ">=", "!=", "?="];

    /// Scalars the random documents use.
    const SCALARS: &[&str] = &[
        "a",
        "yes",
        "-1",
        "0.5",
        "GER",
        "\"q  q\"",
        "\"multi\nline\"",
        "@[ x + 1 ]",
        "[?v]",
    ];

    /// Whitespace between tokens that must be separated.
    const SEPARATORS: &[&str] = &[" ", "  ", "\t", "\n", "\r\n", "\n\t", "\n\n"];

    /// A width at which no test line is too long.
    const WIDE: usize = 100;

    /// What formatting one corpus file did.
    #[derive(Debug, Default)]
    struct FileReport {
        changed: bool,
        /// Key paths of the joined blocks.
        joined: Vec<Vec<String>>,
        lines_removed: usize,
        problems: Vec<String>,
        stats: Stats,
    }

    /// Xorshift generator of well-formed documents with irregular spacing.
    struct Generator(u64);

    impl Generator {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13_u32;
            self.0 ^= self.0 >> 7_u32;
            self.0 ^= self.0 << 17_u32;
            (self.0 % bound as u64).try_into().unwrap_or_default()
        }

        fn block(&mut self, out: &mut String, depth: usize) {
            out.push('{');
            for _ in 0..self.below(4) {
                out.push_str(self.pick(SEPARATORS));
                self.entry(out, depth + 1);
            }
            out.push_str(self.pick(GAPS));
            out.push('}');
        }

        fn document(&mut self) -> String {
            let mut out = String::new();
            for _ in 0..self.below(6) {
                self.entry(&mut out, 0);
                out.push_str(self.pick(SEPARATORS));
            }
            out
        }

        fn entry(&mut self, out: &mut String, depth: usize) {
            let kinds = if depth < 4 { 9 } else { 4 };
            match self.below(kinds) {
                0 => out.push_str(self.pick(SCALARS)),
                1 | 2 => {
                    self.key_op(out);
                    out.push_str(self.pick(SCALARS));
                }
                3 => out.push_str("# c\n"),
                4 | 5 => {
                    self.key_op(out);
                    self.block(out, depth);
                }
                6 => {
                    out.push_str(self.pick(KEYS));
                    out.push_str(self.pick(GAPS));
                    self.block(out, depth);
                }
                7 => {
                    self.key_op(out);
                    out.push_str("rgb");
                    out.push_str(self.pick(GAPS));
                    self.block(out, depth);
                }
                _ => self.block(out, depth),
            }
        }

        fn key_op(&mut self, out: &mut String) {
            out.push_str(self.pick(KEYS));
            out.push_str(self.pick(GAPS));
            out.push_str(self.pick(OPS));
            out.push_str(self.pick(GAPS));
        }

        fn pick(&mut self, items: &[&'static str]) -> &'static str {
            items
                .get(self.below(items.len()))
                .copied()
                .unwrap_or_default()
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// `apply`, asserting the output keeps every token, the BOM and the line
    /// endings, fits every joined line in `width`, and is a fixed point.
    fn format(src: &str, file: FileKind, width: usize) -> (String, Stats) {
        let (out, stats) = apply(src, file, width);
        if let Err(problem) =
            check_preserved(src, &out).and_then(|()| check_joined_widths(src, &out, file, width))
        {
            panic!("{src:?} -> {out:?}: {problem}");
        }
        assert_eq!(
            apply(&out, file, width),
            (out.clone(), Stats::default()),
            "not idempotent: {src:?} -> {out:?}"
        );
        (out, stats)
    }

    fn assert_format(
        src: &str,
        file: FileKind,
        width: usize,
        expected: &str,
        (joined, respaced): (usize, usize),
    ) {
        let (out, stats) = format(src, file, width);
        assert_eq!(out, expected, "{src:?}");
        assert_eq!(stats, Stats { joined, respaced }, "{src:?}");
    }

    fn assert_other(src: &str, expected: &str, counts: (usize, usize)) {
        assert_format(src, FileKind::Other, WIDE, expected, counts);
    }

    fn assert_unchanged(src: &str, file: FileKind, width: usize) {
        assert_format(src, file, width, src, (0, 0));
    }

    /// Whether `out` is a whitespace-only rewrite of `src`: same tokens in
    /// the same order (or `src` untouched if it does not parse), same BOM,
    /// and no line-ending style `src` did not already use.
    fn check_preserved(src: &str, out: &str) -> Result<(), String> {
        let Ok(before) = cst::parse(src) else {
            return if out == src {
                Ok(())
            } else {
                Err("unparseable input was changed".to_owned())
            };
        };
        let after = cst::parse(out).map_err(|err| format!("output does not parse: {err}"))?;
        let (ours, theirs) = (token_texts(&before), token_texts(&after));
        if let Some((index, (a, b))) = ours
            .iter()
            .zip(&theirs)
            .enumerate()
            .find(|(_, (a, b))| a != b)
        {
            return Err(format!("token {index} changed from {a:?} to {b:?}"));
        }
        if ours.len() != theirs.len() {
            return Err(format!("{} tokens became {}", ours.len(), theirs.len()));
        }
        if src.starts_with('\u{feff}') != out.starts_with('\u{feff}') {
            return Err("leading BOM changed".to_owned());
        }
        let (crlf, lone_lf) = line_endings(src);
        let (out_crlf, out_lone_lf) = line_endings(out);
        if (out_crlf && !crlf) || (out_lone_lf && !lone_lf) {
            return Err("introduced a new line-ending style".to_owned());
        }
        Ok(())
    }

    /// Whether every line of `out` (formatted from `src` at `width`) that a
    /// join produced fits in `width` columns. At width 0 nothing can be
    /// joined, so the lines a join produced are those `out` has beyond the
    /// spacing-only formatting of `src`.
    fn check_joined_widths(
        src: &str,
        out: &str,
        file: FileKind,
        width: usize,
    ) -> Result<(), String> {
        let (spaced, spaced_stats) = apply(src, file, 0);
        if spaced_stats.joined > 0 {
            return Err("joined a block at width 0".to_owned());
        }
        let bom = |text: &str| text.trim_start_matches('\u{feff}').to_owned();
        let spaced = bom(&spaced);
        let mut unjoined: BTreeMap<&str, usize> = BTreeMap::new();
        for line in spaced.lines() {
            *unjoined.entry(line).or_default() += 1;
        }
        for line in bom(out).lines() {
            match unjoined.get_mut(line) {
                Some(count) if *count > 0 => *count -= 1,
                Some(_) | None => {
                    if columns(&[line.trim_end()]) > width {
                        return Err(format!("joined line {line:?} is wider than {width}"));
                    }
                }
            }
        }
        Ok(())
    }

    fn collect_spans(block: &Block, out: &mut Vec<Span>) {
        let brace = |at: usize| Span {
            end: at + 1,
            start: at,
        };
        out.extend(block.open.map(brace));
        out.extend(block.close.map(brace));
        out.extend(block.comments.iter().copied());
        for entry in &block.entries {
            out.extend(entry.key.map(|key| key.span));
            out.extend(entry.op.map(|op| op.span));
            match &entry.value {
                Value::Scalar(scalar) => out.push(scalar.span),
                Value::Block(inner) => collect_spans(inner, out),
                Value::Tagged { block: inner, tag } => {
                    out.push(tag.span);
                    collect_spans(inner, out);
                }
            }
        }
    }

    /// `line: text` of the first line where `a` and `b` differ.
    fn first_difference(a: &str, b: &str) -> String {
        a.lines()
            .zip(b.lines())
            .enumerate()
            .find(|(_, (x, y))| x != y)
            .map_or_else(
                || "at the end".to_owned(),
                |(index, (x, y))| format!("line {}: {x:?} -> {y:?}", index + 1),
            )
    }

    /// Whether `text` holds a `\r\n`, and a `\n` without a `\r` before it.
    fn line_endings(text: &str) -> (bool, bool) {
        let crlf = text.matches("\r\n").count();
        (crlf > 0, text.matches('\n').count() > crlf)
    }

    /// Every token's text (scalars, operators, braces, comments) in source
    /// order.
    fn token_texts<'src>(doc: &Document<'src>) -> Vec<&'src str> {
        let mut spans = Vec::new();
        collect_spans(&doc.root, &mut spans);
        spans.sort_by_key(|span| span.start);
        spans.into_iter().map(|span| span.text(doc.src)).collect()
    }

    // ---------------------------------------------------------------------
    // Joining
    // ---------------------------------------------------------------------

    #[test]
    fn joins_the_user_examples() {
        assert_other(
            "prerequisite = {\n\tfocus = my_focus\n}\n",
            "prerequisite = { focus = my_focus }\n",
            (1, 0),
        );
        assert_other(
            "\tallowed = {\n\t\toriginal_tag = MLT\n\t}\n",
            "\tallowed = { original_tag = MLT }\n",
            (1, 0),
        );
        assert_format(
            "focus_tree = {\n\tfocus = {\n\t\tid = MLT_a\n\t\tprerequisite = {\n\t\t\tfocus = my_focus\n\t\t}\n\t}\n}\n",
            FileKind::NationalFocus,
            WIDE,
            "focus_tree = {\n\tfocus = {\n\t\tid = MLT_a\n\t\tprerequisite = { focus = my_focus }\n\t}\n}\n",
            (1, 0),
        );
        assert_format(
            "MLT_category = {\n\tMLT_decision = {\n\t\tallowed = {\n\t\t\toriginal_tag = MLT\n\t\t}\n\t}\n}\n",
            FileKind::Decisions,
            WIDE,
            "MLT_category = {\n\tMLT_decision = {\n\t\tallowed = { original_tag = MLT }\n\t}\n}\n",
            (1, 0),
        );
    }

    #[test]
    fn joins_a_single_entry_with_any_operator() {
        assert_other(
            "limit = {\n\thas_stability < 0.5\n}\n",
            "limit = { has_stability < 0.5 }\n",
            (1, 0),
        );
        for op in ["=", "==", "!=", "<", "<=", ">", ">=", "?="] {
            assert_other(
                &format!("t = {{\n\tx {op} 1\n}}"),
                &format!("t = {{ x {op} 1 }}"),
                (1, 0),
            );
        }
    }

    #[test]
    fn joins_bare_scalar_arrays() {
        assert_other(
            "tags = {\n\tGER\n\tFRA\n\tITA\n}\n",
            "tags = { GER FRA ITA }\n",
            (1, 0),
        );
        assert_other(
            "tags = {\n\t\tGER   FRA\n\tITA }\n",
            "tags = { GER FRA ITA }\n",
            (1, 0),
        );
        assert_other("a = {\n\tGER\n}", "a = { GER }", (1, 0));
        assert_other(
            "a = {\n\t{\n\t\t1 2\n\t}\n}\n",
            "a = {\n\t{ 1 2 }\n}\n",
            (1, 0),
        );
        assert_other(
            "color = rgb {\n\t1\n\t2\n\t3\n}\n",
            "color = rgb { 1 2 3 }\n",
            (1, 0),
        );
    }

    #[test]
    fn leaves_other_contents_multi_line() {
        for src in [
            // Two keyed entries.
            "a = {\n\tb = c\n\td = e\n}\n",
            // A keyed entry and a bare element.
            "a = {\n\tGER\n\tb = c\n}\n",
            // A nested block, even a single-line one.
            "a = {\n\tb = { c = d }\n}\n",
            "a = {\n\t{ 1 2 }\n}\n",
            // Empty blocks.
            "a = {\n}\n",
            "a = {\n\n}\n",
            "a = {\r\n}\r\n",
        ] {
            assert_unchanged(src, FileKind::Other, WIDE);
        }
    }

    #[test]
    fn joins_inner_blocks_but_not_their_parents() {
        assert_other(
            "available = {\n\tNOT = {\n\t\thas_war = yes\n\t}\n}\n",
            "available = {\n\tNOT = { has_war = yes }\n}\n",
            (1, 0),
        );
    }

    #[test]
    fn never_joins_definition_blocks() {
        for (src, file) in [
            (
                "focus_tree = {\n\tfocus = {\n\t\tid = x\n\t}\n}\n",
                FileKind::NationalFocus,
            ),
            ("shared_focus = {\n\tid = x\n}\n", FileKind::NationalFocus),
            (
                "country_event = {\n\tid = x.1\n\toption = {\n\t\tname = x.1.a\n\t}\n}\n",
                FileKind::Events,
            ),
            (
                "country_event = {\n\tid = x.1\n\tdesc = {\n\t\ttext = x.1.d\n\t}\n}\n",
                FileKind::Events,
            ),
            ("country_event = {\n\tid = x.1\n}\n", FileKind::Events),
            (
                "cat = {\n\tdec = {\n\t\ticon = x\n\t}\n}\n",
                FileKind::Decisions,
            ),
            (
                "ideas = {\n\tcountry = {\n\t\tMLT_idea = {\n\t\t\tpicture = x\n\t\t}\n\t}\n}\n",
                FileKind::Ideas,
            ),
        ] {
            assert_unchanged(src, file, WIDE);
        }
        // Only where the file kind makes them definitions.
        assert_other(
            "country_event = {\n\tid = x.1\n\toption = {\n\t\tname = x.1.a\n\t}\n}\n",
            "country_event = {\n\tid = x.1\n\toption = { name = x.1.a }\n}\n",
            (1, 0),
        );
        // Blocks inside definitions are not definitions themselves.
        assert_format(
            "country_event = {\n\tid = x.1\n\ttrigger = {\n\t\thas_war = yes\n\t}\n}\n",
            FileKind::Events,
            WIDE,
            "country_event = {\n\tid = x.1\n\ttrigger = { has_war = yes }\n}\n",
            (1, 0),
        );
    }

    #[test]
    fn never_joins_blocks_with_comments() {
        for src in [
            "a = {\n\tb = c # why\n}\n",
            "a = { # why\n\tb = c\n}\n",
            "a = {\n\t# why\n\tb = c\n}\n",
            "a = {\n\tb = c\n\t# why\n}\n",
            "a = {\n\tb # why\n\t= c\n}\n",
        ] {
            assert_unchanged(src, FileKind::Other, WIDE);
        }
    }

    #[test]
    fn never_joins_multi_line_strings() {
        for src in [
            "a = {\n\tb = \"x\ny\"\n}\n",
            "a = {\n\t\"x\ny\"\n}\n",
            "a = {\n\t\"x\ny\" = b\n}\n",
        ] {
            assert_unchanged(src, FileKind::Other, WIDE);
        }
    }

    #[test]
    fn keeps_quoted_strings_verbatim() {
        assert_other(
            "a = {\n\tb = \"x  =  {y} # z\"\n}\n",
            "a = { b = \"x  =  {y} # z\" }\n",
            (1, 0),
        );
        assert_other("a = {\"q\"=\"v  w\"}", "a = { \"q\" = \"v  w\" }", (0, 1));
        assert_other("\"k\"=\"v\"", "\"k\" = \"v\"", (0, 1));
    }

    #[test]
    fn width_limit_counts_tabs_to_the_next_stop() {
        assert_eq!(columns(&[""]), 0);
        assert_eq!(columns(&["\t"]), 4);
        assert_eq!(columns(&["abc\t"]), 4);
        assert_eq!(columns(&["abcd\t"]), 8);
        assert_eq!(columns(&["  \t", "\t"]), 8);
        assert_eq!(columns(&["é", "x"]), 2);

        // `\t\tallowed = { original_tag = MLT }` is 8 + 32 columns.
        let src = "\t\tallowed = {\n\t\t\toriginal_tag = MLT\n\t\t}\n";
        assert_format(
            src,
            FileKind::Other,
            40,
            "\t\tallowed = { original_tag = MLT }\n",
            (1, 0),
        );
        assert_unchanged(src, FileKind::Other, 39);
        // A tab after two spaces reaches column 4, not 6: `  \tk = { v }`.
        let src = "  \tk = {\n\t\tv\n\t}\n";
        assert_format(src, FileKind::Other, 13, "  \tk = { v }\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 12);
        // A tab inside a string, at column 8, advances to 12:
        // `x = { "a<tab>b" }` is 16 columns.
        let src = "x = {\n\t\"a\tb\"\n}\n";
        assert_format(src, FileKind::Other, 16, "x = { \"a\tb\" }\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 15);
    }

    #[test]
    fn width_limit_counts_the_rest_of_the_closing_line() {
        // `a = { b = c } # note` is 20 columns.
        let src = "a = {\n\tb = c\n} # note\n";
        assert_format(src, FileKind::Other, 20, "a = { b = c } # note\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 19);
        let src = "a = {\n\tb = c\n} d = e\n";
        assert_format(src, FileKind::Other, 19, "a = { b = c } d = e\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 18);
        // Trailing whitespace is not counted (and not removed).
        let src = "a = {\n\tb = c\n}  \t\n";
        assert_format(src, FileKind::Other, 13, "a = { b = c }  \t\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 12);
        // Nor is a leading BOM.
        let src = "\u{feff}a = {\n\tb = c\n}";
        assert_format(src, FileKind::Other, 13, "\u{feff}a = { b = c }", (1, 0));
        assert_unchanged(src, FileKind::Other, 12);
    }

    #[test]
    fn blocks_sharing_a_line_are_joined_in_turn() {
        assert_other(
            "a = {\n\tx = 1\n} b = {\n\ty = 2\n}\n",
            "a = { x = 1 } b = { y = 2 }\n",
            (2, 0),
        );
        // Measured separately, each side would fit in 26 columns (`a = { x =
        // 1 } b = {` and `} b = { y = 2 }`); together they take 27.
        assert_format(
            "a = {\n\tx = 1\n} b = {\n\ty = 2\n}\n",
            FileKind::Other,
            26,
            "a = { x = 1 } b = {\n\ty = 2\n}\n",
            (1, 0),
        );
        let src = "a = {\n\tx = 1\n} b = {\n\ty = 2\n} c = {\n\tz = 3\n} d = {\n\tw = 4\n}\n";
        assert_other(
            src,
            "a = { x = 1 } b = { y = 2 } c = { z = 3 } d = { w = 4 }\n",
            (4, 0),
        );
        let mut keys = Vec::new();
        apply_observed(src, FileKind::Other, WIDE, &mut |path| {
            keys.push(path.join("/"));
        });
        assert_eq!(keys, ["a", "c", "b", "d"], "joined a round at a time");
    }

    #[test]
    fn enclosing_blocks_left_on_one_line_become_canonical() {
        // Joining `b` leaves `a` on one line; `a` is rendered canonically and
        // its width decides: `a = { b = { c = d } }` is 21 columns.
        let src = "a = {b = {\n\tc = d\n}}\n";
        assert_format(src, FileKind::Other, 21, "a = { b = { c = d } }\n", (1, 0));
        assert_unchanged(src, FileKind::Other, 20);
        assert_other(
            "x = { a = {\n\tb\n} c = {\n\td\n} }\n",
            "x = { a = { b } c = { d } }\n",
            (2, 0),
        );
        assert_format(
            "focus_tree = { focus = { id = a prerequisite = {\n\tfocus = b\n} } }\n",
            FileKind::NationalFocus,
            WIDE,
            "focus_tree = { focus = { id = a prerequisite = { focus = b } } }\n",
            (1, 0),
        );
    }

    #[test]
    fn preserves_crlf() {
        let src = "a = {\r\n\tb = c\r\n}\r\nd = {\r\n\te = f\r\n\tg = h\r\n}\r\n";
        assert_format(
            src,
            FileKind::Other,
            13,
            "a = { b = c }\r\nd = {\r\n\te = f\r\n\tg = h\r\n}\r\n",
            (1, 0),
        );
        assert_other("x={a=b}\r\n", "x = { a = b }\r\n", (0, 2));
    }

    #[test]
    fn never_splits_long_single_line_blocks() {
        assert_unchanged("a = { b = c d = e f = g h = i }\n", FileKind::Other, 10);
    }

    #[test]
    fn reports_joined_key_paths() {
        let mut paths = Vec::new();
        apply_observed(
            "focus_tree = {\n\tfocus = {\n\t\tid = a\n\t\tprerequisite = {\n\t\t\tfocus = b\n\t\t}\n\t}\n}\n",
            FileKind::NationalFocus,
            WIDE,
            &mut |path| paths.push(path.join("/")),
        );
        assert_eq!(paths, ["focus_tree/focus/prerequisite"]);
    }

    // ---------------------------------------------------------------------
    // Spacing
    // ---------------------------------------------------------------------

    #[test]
    fn respaces_operators() {
        assert_other("x=-1", "x = -1", (0, 1));
        assert_other("a  =\tb\n", "a = b\n", (0, 1));
        assert_other("a<=1 b!=2 c?=3 d = 4", "a <= 1 b != 2 c ?= 3 d = 4", (0, 3));
        assert_other(
            "key={\n\ta = b\n\tc = d\n}",
            "key = {\n\ta = b\n\tc = d\n}",
            (0, 1),
        );
        assert_unchanged("a = b\n", FileKind::Other, WIDE);
    }

    #[test]
    fn leaves_gaps_with_line_breaks_or_comments() {
        for src in [
            "key =\n{\n\ta = b\n\tc = d\n}\n",
            "key\n= v\n",
            "key # c\n= v\n",
            "key =  # c\n\tv\n",
            "color = hsv # c\n{ 1 2 3 }\n",
            "key\r\n{\r\n\ta = b\r\n\tc = d\r\n}\r\n",
        ] {
            assert_unchanged(src, FileKind::Other, WIDE);
        }
        // The block is still joined; the gap before it is kept.
        assert_other("key =\n{\n\ta = b\n}\n", "key =\n{ a = b }\n", (1, 0));
    }

    #[test]
    fn respaces_operator_less_and_tagged_blocks() {
        assert_other(
            "key{\n\ta = b\n\tc = d\n}",
            "key {\n\ta = b\n\tc = d\n}",
            (0, 1),
        );
        assert_other("key  {a=b}", "key { a = b }", (0, 2));
        assert_other("key\t{\n\ta = b\n}", "key { a = b }", (1, 1));
        assert_other("color = rgb{1 2 3}", "color = rgb { 1 2 3 }", (0, 2));
        assert_other("color=rgb  {  1  2 3}", "color = rgb { 1 2 3 }", (0, 2));
        assert_other("color = hsv{ 1 2 3 }", "color = hsv { 1 2 3 }", (0, 1));
    }

    #[test]
    fn canonicalises_single_line_blocks() {
        assert_other("a = {b=c d=e}\n", "a = { b = c d = e }\n", (0, 1));
        assert_other("x={a=b c=d}", "x = { a = b c = d }", (0, 2));
        assert_other("a = {b={c=d}}", "a = { b = { c = d } }", (0, 1));
        assert_other("a = {}\n", "a = { }\n", (0, 1));
        assert_other("a = {   }\n", "a = { }\n", (0, 1));
        assert_other("a = { {1 2}   {3 4} }", "a = { { 1 2 } { 3 4 } }", (0, 1));
        assert_other("a = { b = rgb{1} }", "a = { b = rgb { 1 } }", (0, 1));
        // A lone `\r` is not a line break.
        assert_other("a = {\rb = c\r}", "a = { b = c }", (0, 1));
        assert_unchanged("a = { }\n", FileKind::Other, WIDE);
        assert_unchanged("a = { b = c }\n", FileKind::Other, WIDE);
    }

    #[test]
    fn unparseable_input_is_returned_unchanged() {
        for src in ["a = {", "a = { b=c", "a=b }", "a = = b"] {
            assert_eq!(
                apply(src, FileKind::Other, WIDE),
                (src.to_owned(), Stats::default())
            );
        }
    }

    /// Pseudo-random well-formed documents with irregular spacing: the
    /// output parses, keeps every token and line ending, and is a fixed
    /// point, at several widths and file kinds.
    #[test]
    fn random_documents_are_preserved_and_idempotent() {
        let mut generator = Generator(0x9e37_79b9_7f4a_7c15);
        let mut joined = 0;
        for _ in 0_u32..5_000 {
            let src = generator.document();
            if let Err(err) = cst::parse(&src) {
                panic!("generated an unparseable document {src:?}: {err}");
            }
            let width = [8, 16, 30, 60, 100]
                .get(generator.below(5))
                .copied()
                .unwrap_or(WIDE);
            let file = [FileKind::Other, FileKind::NationalFocus, FileKind::Events]
                .get(generator.below(3))
                .copied()
                .unwrap_or(FileKind::Other);
            joined += format(&src, file, width).1.joined;
        }
        assert!(joined > 100, "the documents exercise joining ({joined})");
    }

    // ---------------------------------------------------------------------
    // Corpus
    // ---------------------------------------------------------------------

    /// Every script file under `root` with its kind, sorted by path.
    fn corpus_files(root: &Path) -> Vec<(PathBuf, FileKind)> {
        let mut files: Vec<(PathBuf, FileKind)> = ["common", "events", "history"]
            .iter()
            .flat_map(|sub| walkdir::WalkDir::new(root.join(sub)))
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

    /// Formats one corpus file and checks the result; `None` for files that
    /// are not UTF-8 or do not parse (which `apply` leaves alone).
    fn check_corpus_file(path: &Path, kind: FileKind) -> Option<FileReport> {
        let src = std::fs::read_to_string(path).ok()?;
        cst::parse(&src).ok()?;
        let mut joined = Vec::new();
        let (out, stats) = apply_observed(&src, kind, WIDE, &mut |key_path| {
            joined.push(key_path.iter().map(|key| (*key).to_owned()).collect());
        });
        let mut problems = Vec::new();
        if let Err(problem) =
            check_preserved(&src, &out).and_then(|()| check_joined_widths(&src, &out, kind, WIDE))
        {
            problems.push(problem);
        }
        let (again, again_stats) = apply(&out, kind, WIDE);
        if again != out || again_stats != Stats::default() {
            problems.push(format!(
                "not idempotent ({again_stats:?}), {}",
                first_difference(&out, &again)
            ));
        }
        Some(FileReport {
            changed: out != src,
            joined,
            lines_removed: src
                .matches('\n')
                .count()
                .saturating_sub(out.matches('\n').count()),
            problems,
            stats,
        })
    }

    /// Formats every script file of each `;`-separated root in
    /// `HEARTY_CORPUS` at width 100 and checks that the output parses, keeps
    /// every token (scalars, operators, braces, comments) in order along with
    /// the BOM and line endings, and is a fixed point. Prints per-corpus
    /// statistics.
    ///
    /// Run with `cargo test --release inline::tests::corpus -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_formatting_is_lossless_and_idempotent() {
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let mut failures = Vec::new();
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            let files = corpus_files(Path::new(root));
            let reports: Vec<(PathBuf, Option<FileReport>)> = files
                .into_par_iter()
                .map(|(path, kind)| {
                    let report = check_corpus_file(&path, kind);
                    (path, report)
                })
                .collect();
            let mut skipped = 0_usize;
            let mut changed = 0_usize;
            let mut lines_removed = 0_usize;
            let mut total = Stats::default();
            let mut top_level = 0_usize;
            let mut keys: BTreeMap<&str, usize> = BTreeMap::new();
            for (path, report) in &reports {
                let Some(report) = report else {
                    skipped += 1;
                    continue;
                };
                changed += usize::from(report.changed);
                lines_removed += report.lines_removed;
                total.joined += report.stats.joined;
                total.respaced += report.stats.respaced;
                for key_path in &report.joined {
                    top_level += usize::from(key_path.len() == 1);
                    let key = key_path.last().map_or("", String::as_str);
                    *keys.entry(key).or_default() += 1;
                }
                failures.extend(
                    report
                        .problems
                        .iter()
                        .map(|problem| format!("{}: {problem}", path.display())),
                );
            }
            let mut ranked: Vec<(&str, usize)> = keys.into_iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let top: Vec<String> = ranked
                .iter()
                .take(10)
                .map(|(key, count)| format!("{key} {count}"))
                .collect();
            println!(
                "{root}\n    files: {} (skipped, not UTF-8 or unparseable: {skipped}) | changed: \
                 {changed} | joined: {} (top-level: {top_level}) | respaced: {} | lines removed: \
                 {lines_removed}\n    most joined keys: {}",
                reports.len(),
                total.joined,
                total.respaced,
                top.join(", "),
            );
        }
        for failure in failures.iter().take(30) {
            println!("FAILURE {failure}");
        }
        assert!(
            failures.is_empty(),
            "{} files failed; see the report above",
            failures.len()
        );
    }
}
