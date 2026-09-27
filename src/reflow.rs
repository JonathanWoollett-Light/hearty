//! Formatter rule: rewraps prose comments wider than the line width.
//!
//! Like rustfmt's `wrap_comments`, the rule is conservative: it only changes
//! paragraphs of prose with a line wider than `max_width`, never touches
//! commented-out script or data, decorations or tables, which HOI4 mods are
//! full of, and leaves a paragraph as it is when it cannot tell where the
//! paragraph ends.
//!
//! 1. Only full-line comments count: those with nothing but spaces and tabs
//!    before the `#` on their line, found through the CST, so a `#` in a
//!    quoted string is never one. A comment after code on its line (`x = y #
//!    why`) is left alone: wrapping it would take a new comment line whose
//!    indentation only the code could tell.
//! 2. A comment line is its indent (the whitespace before the `#`), its
//!    marker (the run of `#`), its gap (the spaces and tabs after the marker,
//!    maybe none) and its text (the rest, trailing spaces and tabs left out).
//! 3. A line is prose unless it is
//!    - code-like (see [`is_code`]): commented-out script or data, such as
//!      `#has_war = yes`, `# NOT = {`, `#num_of_factories > 50`, a
//!      localisation entry (`#key:0 "Text"`), a list of quoted names, a
//!      line of numbers or script keywords (`# 3542 6555`, `# if limit`),
//!      or a single word that looks like a token of script (`#
//!      GER_focus_x`, `# events.12`; see [`is_token`]);
//!    - inside a block of commented-out script: after a comment line
//!      opening a `{` that no comment line has closed yet, with no line
//!      between them other than full-line comments (a names list under `#
//!      surnames = {`);
//!    - a decoration: fewer than half the chars of its text other than spaces
//!      are alphanumeric, or its text starts or ends with three or more of the
//!      same punctuation char (`# ----`, `### Focus tree ###`; four for `.`,
//!      so that an ellipsis is prose), or its marker is a banner of more than
//!      [`MAX_MARKER`] `#`s (`########## NOTE`);
//!    - aligned: its text holds a tab, or two spaces in a row other than after
//!      the end of a sentence, as a table or text lined up in columns does;
//!      and a cell of such a table carried onto the lines below it, which
//!      start at least [`MIN_CELL_INDENT`] columns further in;
//!    - empty (a paragraph break), or holding a lone `\r`, which editors show
//!      as a line break.
//!
//!    A line that is not prose is never changed, and ends a paragraph.
//! 4. A paragraph is a run of prose lines on consecutive lines with the same
//!    indent and marker and their text starting in the same column, each
//!    carrying on the text of the line above rather than starting a note of
//!    its own (see [`boundary`]): HOI4 comments are as often a stack of
//!    one-line notes as wrapped prose, and running notes together would
//!    garble them. A list item starts a new paragraph: its text starts with a
//!    bullet (`- `, `* `, `+ `, `• `, a number of up to three digits followed
//!    by `. ` or `) `, or, at most a space after the marker, a term being
//!    defined followed by ` - `, as in vanilla's `# division_types - is a
//!    list of tokens ..`), unless it carries on the prose above it (see
//!    [`items_in_prose`]). Its continuation lines are the prose lines after it
//!    whose text starts under the item's text, after its bullet, or anywhere
//!    between its bullet and its text, as long as they all start in the same
//!    column (a hanging indent).
//! 5. A paragraph is left as it is if where it ends is in doubt (see
//!    [`Boundary`] and [`next_line`]): if the line after it, with the same
//!    indent and marker, may carry on its last sentence but may as well be a
//!    note of its own, or is not prose (commented-out script, text lined up
//!    in columns, a list item) but in the same column after a sentence left
//!    open. Refilling the paragraph could leave a scrap of that sentence on a
//!    line of its own, or run two notes together. Where the doubt rests on
//!    how wide the lines are, the next line's paragraph is left as it is
//!    too, as refilling it would change the answer.
//! 6. A paragraph with no line wider than `max_width` columns (a tab advances
//!    to the next multiple of 4, as for [`inline`](crate::inline)) is left as
//!    it is: lines that fit are never joined. Nor is a line wider only for
//!    holding a single word, such as a URL, reason to change anything.
//!    Otherwise, from its first line that is too wide to its end, its words
//!    are refilled greedily, each line taking as many as fit after the
//!    paragraph's prefix: the indent, marker and gap of its first line, but
//!    on an item's continuation lines those of its first continuation line,
//!    or spaces up to its text if it has none. A word too wide for any line
//!    gets a line of its own, and no new line starts with a lone bullet (`-`),
//!    which would read as an item. The lines before the first one too wide are
//!    left as they are. A paragraph is left alone if its prefix leaves fewer
//!    than [`MIN_TEXT_COLUMNS`] columns for text, if a line to be refilled
//!    ends in a word or path broken after a `-` or `/` (`state-` / `owned`,
//!    `common/` / `ideas`), which refilling would put a space in, or if its
//!    new first line would not read as the line it replaces (cut short to a
//!    lone token of script, say), as the lines around it were read by that
//!    line.
//!
//! Only the spaces and line breaks between the words of a paragraph change,
//! so every token and every word of every comment is kept, in order. The new
//! lines end like the first line they replace (a `\r\n` stays a `\r\n`), and
//! a leading BOM is kept. Running the rule on its output changes nothing:
//! every line it writes fits, unless it holds a single word, so no line is
//! left that would make it reflow a paragraph again; and what leaves a
//! paragraph as it is rests on lines also left as they are, or on the words
//! of the lines around it but not where they break, so it leaves it as it is
//! again.

use crate::cst::{self, Block, Document, Edit, Span};
use crate::inline::columns;

/// Words ending with `.` that end no sentence at the end of a line.
const ABBREVIATIONS: &[&str] = &["cf.", "e.g.", "eg.", "i.e.", "ie.", "vs."];

/// Words of script that a line of prose is unlikely to be made of alone: a
/// comment line holding only these, numbers and country tags is
/// commented-out script (`# if limit`, `# yes`).
const KEYWORDS: &[&str] = &[
    "AND", "NOT", "OR", "always", "else", "false", "if", "limit", "no", "true", "yes",
];

/// Most digits a numbered list item's number may have: more is likely a
/// year starting a sentence (`# 1936. The war begins`).
pub const MAX_ITEM_DIGITS: usize = 3;

/// Most `#`s a comment of prose starts with: more make a banner, a heading
/// or a note shouted (`#################### NATIONAL FOCUS TREE EVENTS`,
/// `########## MAYBE add a wargoal`).
pub const MAX_MARKER: usize = 4;

/// How many columns further in than a line lined up in columns (see
/// [`is_aligned`]) the lines below it must start to be taken as a cell of it
/// carried on, rather than prose.
const MIN_CELL_INDENT: usize = 6;

/// The fewest columns a paragraph's prefix must leave for its text for the
/// paragraph to be reflowed. Deeply nested comments would otherwise come out a
/// word or two per line.
pub const MIN_TEXT_COLUMNS: usize = 20;

/// Operators of script that, after a token (see [`is_token`]), mark a line as
/// commented-out script: `#num_of_factories > 50`, `# GER_x * 2`. Not `/`,
/// which prose puts between tokens to mean "or" (`GER_x_tt / _y_tt`).
const OPERATORS: &[&str] = &["*", "<", "<=", ">", ">="];

/// How much wider than `max_width` a line must be, at the least, for it not
/// to have been wrapped by hand (see [`UNWRAPPED_WIDTH`]).
pub const UNWRAPPED_MARGIN: usize = 40;

/// A line wider than this (or than `max_width` and [`UNWRAPPED_MARGIN`],
/// if that is wider) was not wrapped by hand, so its line break is no
/// evidence of a paragraph going on. People wrap prose at up to about 130
/// columns (vanilla's documentation comments, and mods wrapping at 120); in
/// HOI4 and three large mods most lines wider than 140 that another line of
/// prose followed were notes of their own (`# deleted Panzer-Division Tatra
/// as ..` above `# deleted Panzer-Divisions Jüterbog ..`), but some were
/// not (Old World Blues' `.. the inner rockies` above `are more regionally
/// ..`).
pub const UNWRAPPED_WIDTH: usize = 140;

/// How a full-line comment relates to the paragraph right above it; see
/// [`next_line`] and [`boundary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// It carries on the paragraph.
    Continues,
    /// It is not part of the paragraph, which ends before it.
    Separate,
    /// It may or may not carry on the paragraph, so both it (with its own
    /// paragraph) and the paragraph are left as they are: refilling the
    /// paragraph could leave a scrap of a sentence on a line of its own, and
    /// refilling either could change the answer.
    Uncertain,
    /// It is not part of the paragraph, but the paragraph's last sentence
    /// may run on into it, so the paragraph is left as it is.
    Unfinished,
}

/// What a full-line comment is to the rule; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind<'src> {
    /// A list item, whose text starts with this bullet and the spaces after
    /// it.
    Item(&'src str),
    /// Not prose: never changed, and ends a paragraph.
    Other(Reason),
    Prose,
}

/// A full-line comment, split into its parts (see the module docs).
#[derive(Debug, Clone, Copy)]
struct Line<'src> {
    /// The column the text starts at: the display width of the indent,
    /// marker and gap.
    column: usize,
    /// Where the line's content ends: at its terminator, or the end of the
    /// file.
    end: usize,
    gap: &'src str,
    indent: &'src str,
    kind: Kind<'src>,
    marker: &'src str,
    /// Where the line starts (after a leading BOM).
    start: usize,
    /// The comment after its gap, trailing spaces and tabs left out.
    text: &'src str,
    /// The display width of the line, trailing spaces and tabs left out.
    width: usize,
}

impl<'src> Line<'src> {
    /// The full-line comment at `span` of `src` (whose leading BOM, if any,
    /// takes `bom` bytes); `None` if something other than spaces and tabs
    /// comes before it on its line.
    fn new(src: &'src str, span: Span, bom: usize) -> Option<Self> {
        let start = cst::line_start(src, span.start).max(bom);
        let indent = src.get(start..span.start)?;
        if !indent.bytes().all(|byte| byte == b' ' || byte == b'\t') {
            return None;
        }
        let comment = span.text(src);
        let after_marker = comment.trim_start_matches('#');
        let marker = comment.get(..comment.len() - after_marker.len())?;
        let text = after_marker.trim_start_matches([' ', '\t']);
        let gap = after_marker.get(..after_marker.len() - text.len())?;
        let text = text.trim_end_matches([' ', '\t']);
        // A term being defined starts at most a space after the marker;
        // further in, it is more likely a cell of a table.
        let mut kind = kind(text);
        if let Kind::Item(bullet) = kind
            && is_definition(bullet)
            && gap.chars().count() > 1
        {
            kind = Kind::Prose;
        }
        // A long run of `#`s makes a banner.
        if marker.len() > MAX_MARKER && !matches!(kind, Kind::Other(_)) {
            kind = Kind::Other(Reason::Decoration);
        }
        Some(Self {
            column: columns(&[indent, marker, gap]),
            end: span.end,
            gap,
            indent,
            kind,
            marker,
            start,
            text,
            width: columns(&[indent, marker, gap, text]),
        })
    }

    /// Whether the line is prose wider than `max_width` that could be
    /// broken: it has more than one word (after an item's bullet).
    fn overflows(&self, max_width: usize) -> bool {
        !matches!(self.kind, Kind::Other(_))
            && self.width > max_width
            && self.words().nth(1).is_some()
    }

    /// The indent, marker and gap of the line.
    fn prefix(&self) -> String {
        [self.indent, self.marker, self.gap].concat()
    }

    /// Whether `other` has the same indent and marker, so could be part of
    /// the same paragraph.
    fn same_margin(&self, other: &Line<'_>) -> bool {
        (self.indent, self.marker) == (other.indent, other.marker)
    }

    /// The words of the line's text, after the bullet of an item.
    fn words(&self) -> impl Iterator<Item = &'src str> {
        let text = match self.kind {
            Kind::Item(bullet) => self.text.get(bullet.len()..).unwrap_or_default(),
            Kind::Other(_) | Kind::Prose => self.text,
        };
        text.split([' ', '\t']).filter(|word| !word.is_empty())
    }
}

/// A paragraph of [`Line`]s, as indices into them.
#[derive(Debug, Clone, Copy)]
struct Paragraph {
    /// The first line.
    first: usize,
    /// Whether the paragraph is left as it is, as where it ends, or starts,
    /// is in doubt (see [`Boundary`]).
    frozen: bool,
    /// The last line.
    last: usize,
}

/// Why a full-line comment is not prose; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// Lined up in columns, as a table is.
    Aligned,
    /// Inside a block of commented-out script.
    Block,
    /// A cell of a table carried onto a line of its own.
    Cell,
    /// Commented-out script or data.
    Code,
    Decoration,
    /// Empty, or holding a lone `\r`: a paragraph break.
    Empty,
}

/// Rewraps the prose comments of `doc` wider than `max_width` (see the module
/// docs). Returns `None` if that changes nothing, else the new text and how
/// many paragraphs were reflowed.
pub fn apply_doc(doc: &Document<'_>, max_width: usize) -> Option<(String, usize)> {
    let edits = edits(doc, max_width);
    if edits.is_empty() {
        return None;
    }
    let reflowed = edits.len();
    cst::apply_edits(doc.src, edits).map(|text| (text, reflowed))
}

/// [`apply_doc`] on `src`: the new text (`src` itself if it does not parse or
/// nothing changes) and how many paragraphs were reflowed.
#[cfg(test)]
pub fn apply(src: &str, max_width: usize) -> (String, usize) {
    cst::parse(src)
        .ok()
        .and_then(|doc| apply_doc(&doc, max_width))
        .unwrap_or_else(|| (src.to_owned(), 0))
}

/// Whether `later` is on the line right after `earlier`'s.
fn adjacent(src: &str, earlier: &Line<'_>, later: &Line<'_>) -> bool {
    let rest = src.get(earlier.end..).unwrap_or_default();
    let terminator = if rest.starts_with("\r\n") {
        2
    } else if rest.starts_with('\n') {
        1
    } else {
        return false;
    };
    later.start == earlier.end + terminator
}

/// Marks as [`Reason::Block`] each line of prose or list item of `lines` (the
/// full-line comments of `src`) inside a block of commented-out script: after
/// a comment line that opens a `{` (outside quotes) that no comment line has
/// closed yet, with nothing but full-line comments on the lines between
/// them.
fn blocks(src: &str, lines: &mut [Line<'_>]) {
    let mut depth = 0_usize;
    let mut previous: Option<Line<'_>> = None;
    for line in lines.iter_mut() {
        if !previous.is_some_and(|previous| adjacent(src, &previous, line)) {
            depth = 0;
        }
        if depth > 0 && !matches!(line.kind, Kind::Other(_)) {
            line.kind = Kind::Other(Reason::Block);
        }
        let mut quoted = false;
        for ch in line.text.chars() {
            match ch {
                '"' => quoted = !quoted,
                '{' if !quoted => depth += 1,
                '}' if !quoted => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        previous = Some(*line);
    }
}

/// How the prose line `line` relates to the paragraph `above` (its lines so
/// far), whose last line, `previous`, is right above it with the prefix of
/// the paragraph:
/// - [`Separate`](Boundary::Separate) if it starts with a label (`TODO:`,
///   `Low agitation if accepted:`; see [`is_label`]) or `previous` ends a
///   sentence (see [`ends_sentence`]);
/// - else [`Unfinished`](Boundary::Unfinished) if it starts with an
///   uppercase letter, a digit or a token (see [`is_token`]): it may be a
///   note of its own after a note with no full stop (`Divide the seats`,
///   `var:scientist is ..`), or a sentence going on with a name (`the war
///   with` / `Germany ends ..`). It is the sentence going on, though, in
///   prose wrapped by hand over more than one line whose author ends
///   sentences with full stops (see [`is_punctuated`]), broken where the
///   word would not have fitted within its widest line: such an author leaves
///   none out;
/// - else (it goes on as a sentence does) [`Uncertain`](Boundary::Uncertain)
///   if either line is wider than [`UNWRAPPED_WIDTH`], as then nobody
///   wrapped the text by hand, or if `previous` could have held its first
///   word and still been no wider than it or `max_width`: whoever wrapped the
///   text would have put the word there, unless the lines are notes of their
///   own;
/// - else [`Continues`](Boundary::Continues).
fn boundary(above: &[Line<'_>], line: &Line<'_>, max_width: usize) -> Boundary {
    let (Some(previous), Some(first)) = (above.last(), line.words().next()) else {
        return Boundary::Separate;
    };
    if is_label(line.words()) || ends_sentence(previous.text) {
        return Boundary::Separate;
    }
    let starts_note =
        first.starts_with(|ch: char| ch.is_uppercase() || ch.is_ascii_digit()) || is_token(first);
    let wrapped = || {
        let widest = above.iter().map(|line| line.width).max();
        above.len() > 1
            && widest.is_some_and(|widest| previous.width + 1 + first.chars().count() > widest)
            && is_punctuated(above)
    };
    if starts_note && !wrapped() {
        return Boundary::Unfinished;
    }
    let unwrapped = UNWRAPPED_WIDTH.max(max_width + UNWRAPPED_MARGIN);
    let full = previous.width + 1 + first.chars().count() > line.width.min(max_width);
    if previous.width <= unwrapped && line.width <= unwrapped && full {
        Boundary::Continues
    } else {
        Boundary::Uncertain
    }
}

/// Whether a line ending with `word` was broken inside a word or a path, so
/// that joining it to the next line with a space would change the text: it
/// ends with a `-` or `/` right after a letter or digit (`state-`,
/// `common/`).
fn breaks_inside(word: &str) -> bool {
    let mut chars = word.chars().rev();
    matches!(chars.next(), Some('-' | '/')) && chars.next().is_some_and(char::is_alphanumeric)
}

/// The bullet starting `text`, with the spaces after it, if `text` is a list
/// item: `- `, `* `, `+ `, `• `, a number of up to [`MAX_ITEM_DIGITS`] digits
/// followed by `. ` or `) `, or a term (see [`is_term`]) followed by ` - `.
fn bullet(text: &str) -> Option<&str> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let symbol = if digits == 0 {
        text.chars()
            .next()
            .filter(|ch| matches!(ch, '-' | '*' | '+' | '•'))
            .map_or(0, char::len_utf8)
    } else if digits <= MAX_ITEM_DIGITS && matches!(text.as_bytes().get(digits), Some(b'.' | b')'))
    {
        digits + 1
    } else {
        0
    };
    let symbol = if symbol > 0 {
        symbol
    } else {
        // `term -`, the spaces after it following below.
        text.split_once(' ')
            .filter(|&(term, rest)| is_term(term) && rest.starts_with("- "))
            .map_or(0, |(term, _)| term.len() + " -".len())
    };
    let after = text.get(symbol..)?;
    let words = after.trim_start_matches(' ');
    if symbol == 0 || words.len() == after.len() || words.is_empty() {
        return None;
    }
    text.get(..text.len() - words.len())
}

/// Marks as [`Reason::Cell`] each line of prose or list item of `lines` (the
/// full-line comments of `src`) that carries a cell of a table onto a line of
/// its own:
/// right below a line lined up in columns (see [`is_aligned`]) with the same
/// indent and marker, starting at least [`MIN_CELL_INDENT`] columns further
/// in, or right below such a line and starting in the same column.
fn cells(src: &str, lines: &mut [Line<'_>]) {
    for index in 1..lines.len() {
        let (Some(above), Some(line)) = (lines.get(index - 1), lines.get(index)) else {
            continue;
        };
        let cell = !matches!(line.kind, Kind::Other(_))
            && above.same_margin(line)
            && adjacent(src, above, line)
            && match above.kind {
                Kind::Other(Reason::Aligned) => line.column >= above.column + MIN_CELL_INDENT,
                Kind::Other(Reason::Cell) => line.column == above.column,
                Kind::Item(_) | Kind::Other(_) | Kind::Prose => false,
            };
        if cell && let Some(line) = lines.get_mut(index) {
            line.kind = Kind::Other(Reason::Cell);
        }
    }
}

/// Every comment of `root` and the blocks nested in it, in source order.
fn comments(root: &Block) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut stack = vec![root];
    while let Some(block) = stack.pop() {
        spans.extend_from_slice(&block.comments);
        stack.extend(
            block
                .entries
                .iter()
                .filter_map(|entry| entry.value.as_block()),
        );
    }
    spans.sort_unstable_by_key(|span| span.start);
    spans
}

/// The edits reflowing every paragraph of `doc` that needs it.
fn edits(doc: &Document<'_>, max_width: usize) -> Vec<Edit> {
    let mut lines = full_line_comments(doc);
    if !lines.iter().any(|line| line.overflows(max_width)) {
        return Vec::new();
    }
    blocks(doc.src, &mut lines);
    cells(doc.src, &mut lines);
    items_in_prose(doc.src, &mut lines, max_width);
    paragraphs(doc.src, &lines, max_width)
        .into_iter()
        .filter(|paragraph| !paragraph.frozen)
        .filter_map(|paragraph| {
            reflow(
                doc.src,
                lines.get(paragraph.first..=paragraph.last)?,
                max_width,
            )
        })
        .collect()
}

/// Whether `text` ends a sentence: with a `.`, `!` or `?`, before any
/// closing brackets or quotes, and not in an abbreviation such as `e.g.`. A
/// colon ends none: what it introduces may well go on on the next line.
fn ends_sentence(text: &str) -> bool {
    let last = text.rsplit([' ', '\t']).next().unwrap_or_default();
    let end = last.trim_end_matches([')', ']', '"', '\'', '*']);
    end.ends_with(['.', '!', '?']) && !ABBREVIATIONS.contains(&end.to_lowercase().as_str())
}

/// `words` filled greedily onto lines of at most `max_width` columns, the
/// first after a prefix `first_prefix` columns wide and the others after one
/// `prefix` columns wide: each line takes words while they fit, and a word
/// too wide for any line gets one of its own. No line but the first starts
/// with a bullet on its own (see [`is_bullet`]), which would read as a list
/// item: the word before it goes along, if the line has room and that word
/// is not a bullet too (a run of them stays together on the new line rather
/// than empty the lines before it one word at a time).
fn fill<'src, I>(
    words: I,
    first_prefix: usize,
    prefix: usize,
    max_width: usize,
) -> Vec<Vec<&'src str>>
where
    I: Iterator<Item = &'src str>,
{
    let mut lines = Vec::new();
    let mut line: Vec<&'src str> = Vec::new();
    let mut line_width = first_prefix;
    for word in words {
        let word_width = word.chars().count();
        if line.is_empty() || line_width + 1 + word_width <= max_width {
            line_width += usize::from(!line.is_empty()) + word_width;
            line.push(word);
            continue;
        }
        let mut next = vec![word];
        line_width = prefix + word_width;
        if is_bullet(word)
            && line.len() > 1
            && let Some(&last) = line.last()
            && !is_bullet(last)
            && line_width + 1 + last.chars().count() <= max_width
        {
            line.pop();
            next.insert(0, last);
            line_width += 1 + last.chars().count();
        }
        lines.push(std::mem::replace(&mut line, next));
    }
    lines.push(line);
    lines
}

/// The full-line comments of `doc`, in source order.
fn full_line_comments<'src>(doc: &Document<'src>) -> Vec<Line<'src>> {
    let src = doc.src;
    let bom = if src.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    comments(&doc.root)
        .into_iter()
        .filter_map(|span| Line::new(src, span, bom))
        .collect()
}

/// Whether `text` is lined up in columns: it holds a tab, three spaces in a
/// row, or two other than after the end of a sentence (`.  Next`).
fn is_aligned(text: &str) -> bool {
    text.contains('\t')
        || text.contains("   ")
        || text.match_indices("  ").any(|(at, _)| {
            !text
                .get(..at)
                .is_some_and(|before| before.ends_with(['.', '!', '?', ':']))
        })
}

/// Whether `word` is a bullet on its own: `-`, `*`, `+`, `•`, or a number of
/// up to [`MAX_ITEM_DIGITS`] digits followed by `.` or `)`.
fn is_bullet(word: &str) -> bool {
    matches!(word, "-" | "*" | "+" | "•")
        || word.strip_suffix(['.', ')']).is_some_and(|number| {
            (1..=MAX_ITEM_DIGITS).contains(&number.len())
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether the comment text `text` is commented-out script or data rather
/// than prose. It is if it
/// - holds `=`, `{`, `}` or `#` (`#has_war = yes`, `# NOT = {`, or a
///   comment in a comment);
/// - is a single word that looks like a token (see [`is_token`]);
/// - has a token followed by an operator (`#num_of_factories > 50`, `#
///   GER_x * 2`; see [`OPERATORS`]);
/// - has a localisation key followed by a quoted string (`#key:0 "Text"`,
///   `# key: "Text"`; see [`is_loc_key`]);
/// - is mostly quoted strings, as a list of names is (`# "Zara" "Pola"`);
/// - is made only of numbers, script keywords (see [`KEYWORDS`]) and country
///   tags (`# 3542 6555 11481`, `# if limit`, `# GER ITA`).
fn is_code(text: &str) -> bool {
    let words: Vec<&str> = text
        .split([' ', '\t'])
        .filter(|word| !word.is_empty())
        .collect();
    let pairs = || words.iter().zip(words.iter().skip(1));
    text.contains(['=', '{', '}', '#'])
        || matches!(words.as_slice(), [word] if is_token(word))
        || pairs().any(|(word, next)| {
            (is_token(word) && OPERATORS.contains(next))
                || (is_loc_key(word) && next.starts_with('"'))
        })
        || is_mostly_quoted(text)
        || words.iter().all(|word| {
            KEYWORDS.contains(word)
                || word.bytes().all(|byte| byte.is_ascii_digit())
                || is_tag(word)
        })
}

/// Whether `text` is a decoration: mostly punctuation, or starting or ending
/// with a run of one punctuation char (see the module docs).
fn is_decoration(text: &str) -> bool {
    let (alphanumeric, other) = text.chars().filter(|ch| !ch.is_whitespace()).fold(
        (0_usize, 0_usize),
        |(alphanumeric, other), ch| {
            if ch.is_alphanumeric() {
                (alphanumeric + 1, other)
            } else {
                (alphanumeric, other + 1)
            }
        },
    );
    alphanumeric < other || starts_with_run(text.chars()) || starts_with_run(text.chars().rev())
}

/// Whether `bullet` (see [`bullet`]) is a term being defined and its dash.
fn is_definition(bullet: &str) -> bool {
    bullet
        .split_once(' ')
        .is_some_and(|(term, _)| is_term(term))
}

/// Whether `words`, the words of a line, start with a label of a note: a
/// capitalised word of letters and a colon, such as `TODO:`, `Note:` or
/// `Example:`, or up to four words of letters, the first capitalised and the
/// last ending with a colon (`Low agitation if accepted:`).
fn is_label<'src, I>(words: I) -> bool
where
    I: Iterator<Item = &'src str>,
{
    for (index, word) in words.take(4).enumerate() {
        let (name, colon) = word
            .strip_suffix(':')
            .map_or((word, false), |name| (name, true));
        let letters = !name.is_empty() && name.chars().all(char::is_alphabetic);
        if !letters || (index == 0 && !name.starts_with(char::is_uppercase)) {
            return false;
        }
        if colon {
            return index > 0 || name.chars().count() > 1;
        }
    }
    false
}

/// Whether `word` is a localisation key and its version as a localisation
/// file writes them (`GER_focus_desc:0`, `malaysia.100.d:`): a key of
/// letters, digits, `_`, `.` and `-`, a colon and maybe a number. A key
/// without a number must hold a `_` or `.`, so that a word of prose and a
/// colon (`says:`) is not one.
fn is_loc_key(word: &str) -> bool {
    word.rsplit_once(':').is_some_and(|(key, version)| {
        !key.is_empty()
            && version.bytes().all(|byte| byte.is_ascii_digit())
            && key
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-'))
            && (!version.is_empty() || key.contains(['_', '.']))
    })
}

/// Whether more than half the chars of `text` other than whitespace are in
/// quoted strings (their quotes included), as in a list of names.
fn is_mostly_quoted(text: &str) -> bool {
    let mut quoted_chars = 0_usize;
    let mut chars = 0_usize;
    let mut quoted = false;
    for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
        if ch == '"' {
            quoted = !quoted;
        }
        chars += 1;
        quoted_chars += usize::from(quoted || ch == '"');
    }
    quoted_chars * 2 > chars
}

/// Whether the words of `lines` end a sentence (see [`ends_sentence`]) and
/// go on with a capitalised word: their author ends sentences with full
/// stops. Only the words count, not where the lines break, so reflowing the
/// lines does not change the answer.
fn is_punctuated(lines: &[Line<'_>]) -> bool {
    let mut words = lines.iter().flat_map(Line::words).peekable();
    while let Some(word) = words.next() {
        if ends_sentence(word)
            && words
                .peek()
                .is_some_and(|next| next.starts_with(char::is_uppercase))
        {
            return true;
        }
    }
    false
}

/// Whether `word` looks like a country tag: three ASCII uppercase letters
/// or digits, the first a letter (`GER`, `D01`).
fn is_tag(word: &str) -> bool {
    word.len() == 3
        && word.starts_with(|ch: char| ch.is_ascii_uppercase())
        && word
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// Whether `word` could be a term that a list defines, as in `hardness - it
/// is ..`: an identifier or a word in lower case. A capitalised word is left
/// out, as wrapped prose may well start a line with a name and a dash (`MLT -
/// the whole of ..`).
fn is_term(word: &str) -> bool {
    (word.contains('_') || word.starts_with(char::is_lowercase))
        && word.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
}

/// Whether `word` looks like an identifier or a token of script
/// (`GER_focus_x`, `events.12`, `FROM:owner`, `@cost`, `$PARAM$`,
/// `[Root.GetName]`) rather than a word of prose. Punctuation ending a
/// sentence does not count, so `sentence.` is prose.
fn is_token(word: &str) -> bool {
    word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
        .contains(['_', '.', ':', '@', '$', '['])
}

/// Makes prose of each list item of `lines` (the full-line comments of
/// `src`) that carries on the prose line right above it, whose text starts
/// in the same column, rather than starting a list: wrapped prose may well
/// break a line before `word - ` or before a dash. It does so for
/// - a term being defined (`groups - the one ..`) that the line above goes
///   on into (see [`boundary`]), the line above being too full to have held
///   the term within `max_width`;
/// - a lone `-`, `*`, `+` or `•` item after a line that does not end a
///   sentence but a word, its text starting in lower case or closing a
///   bracket opened above it (`- because ..`, `- Gotchas).`), with no item
///   of the same bullet right below it or its continuation lines, as a list
///   would have.
fn items_in_prose(src: &str, lines: &mut [Line<'_>], max_width: usize) {
    for index in 1..lines.len() {
        let (Some(above), Some(line)) = (lines.get(index - 1), lines.get(index)) else {
            continue;
        };
        let Kind::Item(bullet) = line.kind else {
            continue;
        };
        let prose = Line {
            kind: Kind::Prose,
            ..*line
        };
        let after_prose = above.kind == Kind::Prose
            && above.same_margin(line)
            && above.column == line.column
            && adjacent(src, above, line);
        let dash = || {
            let text = line.text.get(bullet.len()..).unwrap_or_default();
            let closes_bracket = text
                .chars()
                .scan(0_isize, |depth, ch| {
                    *depth += match ch {
                        '(' => 1,
                        ')' => -1,
                        _ => 0,
                    };
                    Some(*depth)
                })
                .any(|depth| depth < 0);
            // The list's next item comes after this one's continuation
            // lines, which start further in.
            let mut list_below = false;
            let mut previous = line;
            for below in lines.iter().skip(index + 1) {
                if !below.same_margin(line) || !adjacent(src, previous, below) {
                    break;
                }
                if below.column <= line.column {
                    list_below = below.kind == Kind::Item(bullet) && below.column == line.column;
                    break;
                }
                previous = below;
            }
            matches!(bullet.trim_end(), "-" | "*" | "+" | "•")
                && !ends_sentence(above.text)
                && above
                    .text
                    .ends_with(|ch: char| ch.is_alphanumeric() || ch == '_')
                && (text.starts_with(char::is_lowercase) || closes_bracket)
                && !list_below
        };
        // Taken as wide as the limit, so that refilling the item's own line
        // cannot change the answer.
        let as_wide_as_the_limit = Line {
            width: max_width,
            ..prose
        };
        let carried_on = after_prose
            && if is_definition(bullet) {
                boundary(
                    std::slice::from_ref(above),
                    &as_wide_as_the_limit,
                    max_width,
                ) == Boundary::Continues
            } else {
                dash()
            };
        if carried_on && let Some(line) = lines.get_mut(index) {
            *line = prose;
        }
    }
}

/// What the comment line with `text` is; see the module docs.
fn kind(text: &str) -> Kind<'_> {
    // Editors show a lone `\r` as a line break.
    if text.is_empty() || text.contains('\r') {
        Kind::Other(Reason::Empty)
    } else if is_decoration(text) {
        Kind::Other(Reason::Decoration)
    } else if is_code(text) {
        Kind::Other(Reason::Code)
    } else {
        let bullet = bullet(text);
        let words = bullet
            .and_then(|bullet| text.get(bullet.len()..))
            .unwrap_or(text);
        if is_aligned(words) {
            Kind::Other(Reason::Aligned)
        } else {
            bullet.map_or(Kind::Prose, Kind::Item)
        }
    }
}

/// How `line`, a full-line comment of `lines` (those of `src`), relates to
/// `paragraph`, which ends right above it, or before it. A prose line carries
/// the paragraph on if it is on the next line, has the same indent and
/// marker, starts in the paragraph's column and its text carries on (see
/// [`boundary`]). The paragraph's column is its first line's, but for an
/// item it is its first continuation line's, or, with none yet, any column
/// after the item's bullet up to its text. A list item, commented-out script
/// or text lined up in columns in the same column as the paragraph's last
/// line leaves it [`Unfinished`](Boundary::Unfinished) if that line ends no
/// sentence, as its sentence may run on into it (though not a list item
/// after an item: that is a list).
fn next_line(
    src: &str,
    lines: &[Line<'_>],
    paragraph: &Paragraph,
    line: &Line<'_>,
    max_width: usize,
) -> Boundary {
    let (Some(head), Some(previous)) = (lines.get(paragraph.first), lines.get(paragraph.last))
    else {
        return Boundary::Separate;
    };
    if !head.same_margin(line) || !adjacent(src, previous, line) {
        return Boundary::Separate;
    }
    let in_column = match (head.kind, lines.get(paragraph.first + 1)) {
        (Kind::Item(_), Some(continuation)) if paragraph.last > paragraph.first => {
            line.column == continuation.column
        }
        (Kind::Item(bullet), _) => {
            line.column > head.column && line.column <= head.column + columns(&[bullet])
        }
        (Kind::Other(_) | Kind::Prose, _) => line.column == head.column,
    };
    let run_on = (in_column || line.column == previous.column) && !ends_sentence(previous.text);
    match line.kind {
        Kind::Item(_) if run_on && head.kind == Kind::Prose => Boundary::Unfinished,
        Kind::Other(Reason::Aligned | Reason::Code) if run_on => Boundary::Unfinished,
        Kind::Item(_) | Kind::Other(_) => Boundary::Separate,
        Kind::Prose => match lines.get(paragraph.first..=paragraph.last) {
            Some(above) if in_column => boundary(above, line, max_width),
            Some(_) | None => Boundary::Separate,
        },
    }
}

/// The paragraphs of `lines`, the full-line comments of `src` in order.
fn paragraphs(src: &str, lines: &[Line<'_>], max_width: usize) -> Vec<Paragraph> {
    let mut paragraphs = Vec::new();
    let mut current: Option<Paragraph> = None;
    for (index, line) in lines.iter().enumerate() {
        let mut frozen = false;
        if let Some(mut paragraph) = current.take() {
            match next_line(src, lines, &paragraph, line, max_width) {
                Boundary::Continues => {
                    paragraph.last = index;
                    current = Some(paragraph);
                    continue;
                }
                Boundary::Separate => {}
                Boundary::Uncertain => {
                    paragraph.frozen = true;
                    frozen = true;
                }
                Boundary::Unfinished => paragraph.frozen = true,
            }
            paragraphs.push(paragraph);
        }
        current = match line.kind {
            Kind::Item(_) | Kind::Prose => Some(Paragraph {
                first: index,
                frozen,
                last: index,
            }),
            Kind::Other(_) => None,
        };
    }
    paragraphs.extend(current);
    paragraphs
}

/// The edit reflowing `paragraph`, lines of `src`; `None` if it is left as
/// it is.
fn reflow(src: &str, paragraph: &[Line<'_>], max_width: usize) -> Option<Edit> {
    let first = paragraph
        .iter()
        .position(|line| line.overflows(max_width))?;
    let (head, rest) = (paragraph.first()?, paragraph.get(first..)?);
    let (start, end) = (rest.first()?, rest.last()?);
    // The prefix of the lines after the first.
    let prefix = match (head.kind, paragraph.get(1)) {
        (Kind::Item(_), Some(continuation)) => continuation.prefix(),
        (Kind::Item(bullet), None) => {
            let mut prefix = head.prefix();
            prefix.extend(std::iter::repeat_n(' ', columns(&[bullet])));
            prefix
        }
        (Kind::Other(_) | Kind::Prose, _) => head.prefix(),
    };
    let first_prefix = match head.kind {
        Kind::Item(bullet) if first == 0 => [&head.prefix(), bullet].concat(),
        Kind::Item(_) | Kind::Other(_) | Kind::Prose => prefix.clone(),
    };
    let (first_width, width) = (columns(&[&first_prefix]), columns(&[&prefix]));
    if first_width.max(width) + MIN_TEXT_COLUMNS > max_width {
        return None;
    }
    // Refilling a word or a path broken across lines (`state-` / `owned`)
    // would put a space in it.
    let (_, carried) = rest.split_last()?;
    if carried
        .iter()
        .any(|line| line.words().last().is_some_and(breaks_inside))
    {
        return None;
    }

    let filled: Vec<String> = fill(
        rest.iter().flat_map(Line::words),
        first_width,
        width,
        max_width,
    )
    .into_iter()
    .enumerate()
    .map(|(index, words)| {
        let prefix = if index == 0 { &first_prefix } else { &prefix };
        [prefix.clone(), words.join(" ")].concat()
    })
    .collect();
    // A new first line must read as the line it replaces, which the lines
    // around it were read by: cut short, `- - ... - https://..` would read as
    // a decoration, and `GER_focus_with_a_long_name starts ..` as a lone
    // token of script.
    if first == 0
        && filled
            .first()
            .and_then(|line| line.get(head.prefix().len()..))
            .is_none_or(|text| kind(text) != kind(head.text))
    {
        return None;
    }

    let unchanged = filled.len() == rest.len()
        && filled.iter().zip(rest).all(|(new, old)| {
            src.get(old.start..old.end)
                .is_some_and(|old| old.trim_end_matches([' ', '\t']) == new)
        });
    if unchanged {
        return None;
    }
    Some(Edit {
        replacement: filled.join(terminator(src, start.end)),
        span: Span {
            end: end.end,
            start: start.start,
        },
    })
}

/// Whether `chars` start with three or more of the same punctuation char
/// (four for `.`).
fn starts_with_run<I>(mut chars: I) -> bool
where
    I: Iterator<Item = char>,
{
    let Some(first) = chars.next() else {
        return false;
    };
    let more = if first == '.' { 3 } else { 2 };
    !first.is_alphanumeric()
        && !first.is_whitespace()
        && chars.take(more).filter(|&ch| ch == first).count() == more
}

/// The line terminator at `offset` of `src`, else the first in `src`, else
/// `\n`.
fn terminator(src: &str, offset: usize) -> &'static str {
    let rest = src.get(offset..).unwrap_or_default();
    if rest.starts_with("\r\n") {
        "\r\n"
    } else if rest.starts_with('\n') {
        "\n"
    } else if src.find('\n').is_some_and(|newline| {
        src.get(..newline)
            .is_some_and(|before| before.ends_with('\r'))
    }) {
        "\r\n"
    } else {
        "\n"
    }
}
#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{
        Kind, Line, MIN_TEXT_COLUMNS, Paragraph, Reason, apply, blocks, bullet, cells, columns,
        comments, edits, full_line_comments, is_decoration, items_in_prose, next_line, paragraphs,
    };
    use crate::cst::{self, Block, Document, Span, Value};
    use rayon::prelude::*;
    use std::path::{Path, PathBuf};

    /// Words the random documents' comments use: prose, bullets, tokens,
    /// code, decorations, a non-ASCII char and a word too wide for any line.
    const COMMENT_WORDS: &[&str] = &[
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "the",
        "a",
        "and",
        "dog.",
        "cat!",
        "Germany",
        "The",
        "-",
        "1.",
        "3",
        "(x)",
        "x)",
        "e.g.",
        "Label:",
        "GER_focus",
        "GER",
        "yes",
        ">",
        "key:0",
        "\"Zara\"",
        "state-",
        "=",
        "{",
        "}",
        "----",
        "...",
        "é",
        "https://example.com/a/very/long/path/that/fits/on/no/line/at/all",
    ];

    /// Gaps after a `#` the random documents use.
    const GAPS: &[&str] = &["", " ", "  ", "   ", "\t"];

    /// Indents the random documents use.
    const INDENTS: &[&str] = &["", "\t", "  ", "\t\t", " \t"];

    /// Why a full-line comment wider than the limit is (or would be) left
    /// as it is, else what prose it is; see [`reason`].
    const REASONS: [&str; 11] = [
        "code-like",
        "decoration",
        "aligned",
        "block",
        "cell",
        "one word",
        "too deep",
        "in doubt",
        "kept whole",
        "prose",
        "item",
    ];

    /// Line terminators the random documents use.
    const TERMINATORS: &[&str] = &["\n", "\n", "\r\n"];

    /// The width most tests wrap at: 28 columns of text after `# `.
    const WIDTH: usize = 30;

    /// What reflowing one corpus file did.
    #[derive(Debug, Default)]
    struct FileReport {
        /// Each paragraph with a line wider than the limit that is left as it
        /// is for doubt about where it ends, with the line around it.
        frozen: Vec<String>,
        /// Each reflowed paragraph as a diff, with a line of context around.
        hunks: Vec<String>,
        /// Full-line comments wider than the limit after reflowing, by
        /// [`reason`].
        left: [usize; REASONS.len()],
        /// Full-line comments wider than the limit before reflowing, by
        /// [`reason`].
        overflowing: [usize; REASONS.len()],
        problems: Vec<String>,
        reflowed: usize,
    }

    /// Xorshift generator of documents mixing script and comment lines.
    struct Generator(u64);

    impl Generator {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13_u32;
            self.0 ^= self.0 >> 7_u32;
            self.0 ^= self.0 << 17_u32;
            (self.0 % bound as u64).try_into().unwrap_or_default()
        }

        fn document(&mut self) -> String {
            let terminator = self.pick(TERMINATORS);
            let mut out = String::new();
            if self.below(8) == 0 {
                out.push('\u{feff}');
            }
            // Comment lines often share the prefix of the line above.
            let mut prefix: Option<String> = None;
            for _ in 0..self.below(12) {
                match self.below(8) {
                    0 => out.push_str("x = { a = b }"),
                    1 => {
                        out.push_str("y = z # ");
                        self.words(&mut out);
                    }
                    2 => {}
                    _ => {
                        if prefix.is_none() || self.below(3) == 0 {
                            prefix = Some(
                                [
                                    self.pick(INDENTS),
                                    self.pick(&["#", "#", "##"]),
                                    self.pick(GAPS),
                                ]
                                .concat(),
                            );
                        }
                        out.push_str(prefix.as_deref().unwrap_or_default());
                        self.words(&mut out);
                    }
                }
                out.push_str(terminator);
            }
            out
        }

        fn pick(&mut self, items: &[&'static str]) -> &'static str {
            items
                .get(self.below(items.len()))
                .copied()
                .unwrap_or_default()
        }

        fn words(&mut self, out: &mut String) {
            for index in 0..self.below(20) {
                if index > 0 {
                    out.push_str(self.pick(&[" ", " ", " ", " ", " ", " ", "  ", "\t"]));
                }
                out.push_str(self.pick(COMMENT_WORDS));
            }
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// `apply`, asserting [`check`] and that a second run changes nothing.
    fn reflow(src: &str, width: usize) -> (String, usize) {
        let (out, reflowed) = apply(src, width);
        if let Err(problem) = check(src, &out, width) {
            panic!("{src:?} -> {out:?}: {problem}");
        }
        assert_eq!(
            apply(&out, width),
            (out.clone(), 0),
            "not idempotent: {src:?} -> {out:?}"
        );
        (out, reflowed)
    }

    fn assert_reflow(src: &str, width: usize, expected: &str, reflowed: usize) {
        assert_eq!(
            reflow(src, width),
            (expected.to_owned(), reflowed),
            "{src:?}"
        );
    }

    fn assert_unchanged(src: &str, width: usize) {
        assert_reflow(src, width, src, 0);
    }

    /// Whether `out`, `src` reflowed at `width`, keeps every token other
    /// than comments, every comment's words in order, the BOM and the line
    /// endings, and fits every line it rewrote in `width` unless it holds a
    /// single word (or `src` does not parse and `out` is `src`).
    fn check(src: &str, out: &str, width: usize) -> Result<(), String> {
        let Ok(before) = cst::parse(src) else {
            return if out == src {
                Ok(())
            } else {
                Err("unparseable input was changed".to_owned())
            };
        };
        let after = cst::parse(out).map_err(|err| format!("output does not parse: {err}"))?;
        if code_tokens(&before) != code_tokens(&after) {
            return Err("the tokens other than comments changed".to_owned());
        }
        if comment_words(&before) != comment_words(&after) {
            return Err("the words of the comments changed".to_owned());
        }
        if src.starts_with('\u{feff}') != out.starts_with('\u{feff}') {
            return Err("leading BOM changed".to_owned());
        }
        // A file without line breaks may gain `\n`s; any other only the
        // terminators it has.
        let (crlf, lone_lf) = line_endings(src);
        let (out_crlf, out_lone_lf) = line_endings(out);
        if (out_crlf && !crlf) || (out_lone_lf && !lone_lf && crlf) {
            return Err("introduced a new line-ending style".to_owned());
        }
        for edit in edits(&before, width) {
            for line in edit.replacement.split('\n') {
                let line = line.trim_end_matches('\r');
                if columns(&[line]) > width && words_after_prefix(line) > 1 {
                    return Err(format!("line {line:?} is wider than {width}"));
                }
            }
        }
        Ok(())
    }

    /// The text of every token of `doc` but its comments, in source order.
    fn code_tokens<'src>(doc: &Document<'src>) -> Vec<&'src str> {
        let mut spans = Vec::new();
        collect_code_spans(&doc.root, &mut spans);
        spans.sort_by_key(|span| span.start);
        spans.into_iter().map(|span| span.text(doc.src)).collect()
    }

    fn collect_code_spans(block: &Block, out: &mut Vec<Span>) {
        let brace = |at: usize| Span {
            end: at + 1,
            start: at,
        };
        out.extend(block.open.map(brace));
        out.extend(block.close.map(brace));
        for entry in &block.entries {
            out.extend(entry.key.map(|key| key.span));
            out.extend(entry.op.map(|op| op.span));
            match &entry.value {
                Value::Scalar(scalar) => out.push(scalar.span),
                Value::Block(inner) => collect_code_spans(inner, out),
                Value::Tagged { block: inner, tag } => {
                    out.push(tag.span);
                    collect_code_spans(inner, out);
                }
            }
        }
    }

    /// The words of every comment of `doc` after its `#`s, in source order.
    fn comment_words<'src>(doc: &Document<'src>) -> Vec<&'src str> {
        comments(&doc.root)
            .into_iter()
            .flat_map(|span| {
                span.text(doc.src)
                    .trim_start_matches('#')
                    .split_whitespace()
            })
            .collect()
    }

    /// Whether `text` holds a `\r\n`, and a `\n` without a `\r` before it.
    fn line_endings(text: &str) -> (bool, bool) {
        let crlf = text.matches("\r\n").count();
        (crlf > 0, text.matches('\n').count() > crlf)
    }

    /// How many words a comment line has after its indent, `#`s, gap and
    /// bullet.
    fn words_after_prefix(line: &str) -> usize {
        let text = line
            .trim_start_matches([' ', '\t'])
            .trim_start_matches('#')
            .trim_start_matches([' ', '\t']);
        let text = bullet(text).map_or(text, |bullet| text.get(bullet.len()..).unwrap_or(text));
        text.split_whitespace().count()
    }

    // ---------------------------------------------------------------------
    // Wrapping
    // ---------------------------------------------------------------------

    #[test]
    fn wraps_a_line_too_wide() {
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
        // Several times over, at the default width.
        let long = format!("#{}\n", " word".repeat(50));
        let wrapped = format!(
            "#{}\n#{}\n#{}\n",
            " word".repeat(19),
            " word".repeat(19),
            " word".repeat(12)
        );
        assert_reflow(&long, 100, &wrapped, 1);
        // Inside blocks, at the comment's indent; the code is left alone.
        assert_reflow(
            "a = {\n\t# The quick brown fox jumps over the lazy dog\n\tb = c\n}\n",
            34,
            "a = {\n\t# The quick brown fox jumps\n\t# over the lazy dog\n\tb = c\n}\n",
            1,
        );
    }

    /// The words after the break carry over into the rest of the paragraph,
    /// which is refilled.
    #[test]
    fn carries_the_overflow_into_the_rest_of_the_paragraph() {
        assert_reflow(
            "# The quick brown fox jumps over\n# the lazy dog. It barks at\n# the moon.\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog. It barks\n# at the moon.\n",
            1,
        );
        // Each paragraph with a line too wide counts once.
        assert_reflow(
            "# The quick brown fox jumps over\n# the lazy dog.\n\n# A second paragraph much too wide.\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog.\n\n# A second paragraph much too\n# wide.\n",
            2,
        );
    }

    /// Lines that fit are never joined, and the lines before the first one
    /// too wide stay as they are.
    #[test]
    fn leaves_lines_that_fit() {
        assert_unchanged("# Short\n# lines\n# stay.\n", WIDTH);
        // 30 columns exactly.
        assert_unchanged("# The quick brown fox jumps ov\n", WIDTH);
        assert_reflow(
            "# A\n# short line\n# The quick brown fox jumps over the lazy dog\n# ok\n",
            WIDTH,
            "# A\n# short line\n# The quick brown fox jumps\n# over the lazy dog ok\n",
            1,
        );
        // Trailing whitespace takes no columns, and is only removed from the
        // lines rewritten.
        assert_unchanged("# The quick brown fox jumps ov   \t\n", WIDTH);
        assert_reflow(
            "# Fits \n# The quick brown fox jumps over the lazy dog  \n",
            WIDTH,
            "# Fits \n# The quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
    }

    /// No new line starts with a bullet on its own, which would read as a
    /// list item: the word before it comes along.
    #[test]
    fn no_line_starts_with_a_bullet() {
        for bullet in ["-", "*", "3)", "12."] {
            assert_reflow(
                &format!("# The quick brown fox leapt up {bullet} over the lazy dog\n"),
                WIDTH,
                &format!("# The quick brown fox leapt\n# up {bullet} over the lazy dog\n"),
                1,
            );
        }
        // Unless the line has no room for it.
        assert_reflow(
            "# a abcdefghijklmnopqrstuvwxyz 12. more\n",
            WIDTH,
            "# a abcdefghijklmnopqrstuvwxyz\n# 12. more\n",
            1,
        );
        // A run of bullet-like words stays together on the new line, rather
        // than empty the lines before it one word at a time.
        assert_reflow(
            "# a 1. 2. 3. 4. 5. 6. 7. 8. 9. 10. 11. 12. done\n",
            WIDTH,
            "# a 1. 2. 3. 4. 5. 6. 7. 8. 9.\n# 10. 11. 12. done\n",
            1,
        );
        let levels: Vec<String> = (1_u32..=40).map(|level| format!("{level}.")).collect();
        let (out, _) = reflow(&format!("# Levels: {} done\n", levels.join(" ")), 100);
        assert_eq!(out.lines().count(), 2, "{out}");
    }

    /// A word too wide for any line gets a line of its own.
    #[test]
    fn a_word_too_wide_gets_a_line_of_its_own() {
        assert_reflow(
            "# See https://example.com/a/very/long/path for more\n",
            WIDTH,
            "# See\n# https://example.com/a/very/long/path\n# for more\n",
            1,
        );
        // First, it would leave a line that reads as a token of script on its
        // own (see `keeps_how_the_first_line_reads`).
        assert_unchanged("# https://example.com/a/very/long/path and more\n", WIDTH);
        assert_reflow(
            "# - https://example.com/a/very/long/path and more\n",
            WIDTH,
            "# - https://example.com/a/very/long/path\n#   and more\n",
            1,
        );
        // Alone on its line, it is left as it is, trailing space and all, and
        // it is no reason to refill the lines after it.
        for src in [
            "# https://example.com/a/very/long/path \n",
            "# words\n# https://example.com/a/very/long/path\n",
            "# https://example.com/a/very/long/path\n# short line one\n# short line two\n",
            "# - https://example.com/a/very/long/path\n#   short line one\n#   short line two\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
    }

    /// After the end of a sentence, or at a label, a line starts a note of
    /// its own, which is not part of the paragraph above it; but wrapped
    /// prose goes on in lower case or after a bracket.
    #[test]
    fn notes_are_not_run_together() {
        for (end, note) in [
            ("dog.", "Divide the seats"),
            ("dog!", "GER_x is set"),
            ("dog?", "var:scientist is set"),
            ("dog.)", "3 times."),
            ("dog.", "and a note in lower case"),
            ("dog", "TODO: fix it"),
            ("dog", "Note: see above"),
        ] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy {end}\n# {note}\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy {end}\n# {note}\n"),
                1,
            );
        }
        // A label starts a note even on a line too wide, and may be of a few
        // words.
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n# TODO: jump over the fox as well\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog\n# TODO: jump over the fox as\n# well\n",
            2,
        );
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n# Low agitation if accepted: devolve\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog\n# Low agitation if accepted:\n# devolve\n",
            2,
        );
        // An abbreviation ends no sentence; a colon ends none either.
        for end in ["e.g.", "is:"] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy dog {end}\n# cat.\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy dog {end} cat.\n"),
                1,
            );
        }
        for next in ["and runs.", "(twice)."] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy dog\n# {next}\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy dog {next}\n"),
                1,
            );
        }
    }

    /// A line starting with a capital, a digit or a token after a line that
    /// ends no sentence may be a note of its own after a note with no full
    /// stop, or the sentence going on with a name: the paragraph above is
    /// left as it is, and the line starts one of its own.
    #[test]
    fn a_paragraph_that_may_run_on_is_left_alone() {
        for note in [
            "Divide the seats",
            "GER_x is set",
            "var:scientist is set",
            "3 times.",
            "5th Guards Tank Division",
        ] {
            assert_unchanged(
                &format!("# The quick brown fox jumps over the lazy dog\n# {note}\n"),
                WIDTH,
            );
        }
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n# Germany and the fox jump over it\n",
            WIDTH,
            "# The quick brown fox jumps over the lazy dog\n# Germany and the fox jump\n# over it\n",
            1,
        );
        // In prose wrapped by hand whose author ends sentences with full
        // stops, a line broken where the next word would not fit goes on
        // with a name.
        assert_reflow(
            "# The fox jumps. The dog sleeps in\n# the sun by the river bank near\n# Germany where the sun shines bright\n",
            WIDTH,
            "# The fox jumps. The dog\n# sleeps in the sun by the\n# river bank near Germany\n# where the sun shines bright\n",
            1,
        );
        // Not after a single line, a note as likely as not, nor after a line
        // that had room for the name.
        for src in [
            "# The fox jumps. The dog sleeps in the sun by\n# Germany shines\n",
            "# The fox jumps. The dog sleeps in\n# the sun by the river\n# Germany shines\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
    }

    /// A lower-case line that the line above had room for, or after a line
    /// wider than 140 columns (which nobody wrapped by hand), may carry on the
    /// paragraph above or be a note of its own: both are left as they are.
    #[test]
    fn a_paragraph_that_may_end_is_left_alone() {
        assert_unchanged(
            "# The quick brown fox jumps over the lazy dog\n# and runs\n# and a much longer line that is a note\n",
            WIDTH,
        );
        // 140 columns exactly may have been wrapped by hand.
        let wrapped = |last: &str| format!("#{} {last}\n# and more\n", " word".repeat(27));
        let (joined, one) = reflow(&wrapped("abc"), 100);
        assert_eq!(one, 1);
        assert!(joined.ends_with(" word abc and more\n"), "{joined}");
        assert_unchanged(&wrapped("abcd"), 100);
        assert_unchanged("# and more\n# a b c d e f g h i j k l m n o\n", WIDTH);
    }

    /// Hand-wrapped prose is ragged: a line may be a little shorter than the
    /// next though the next one's first word would have fitted on it. When
    /// it is wider than the limit anyway, that is no sign of a note of its
    /// own.
    #[test]
    fn ragged_prose_carries_on() {
        assert_reflow(
            "\t\t\t\t# Under military rule, legitimacy decays unless it is bought. Let it fall far enough and\n\t\t\t\t# the tribes answer; let it fall further with a weak grip and the officers turn on each other.\n",
            100,
            "\t\t\t\t# Under military rule, legitimacy decays unless it is bought. Let it fall far enough\n\t\t\t\t# and the tribes answer; let it fall further with a weak grip and the officers turn\n\t\t\t\t# on each other.\n",
            1,
        );
    }

    /// With a limit near 140 columns or above, a line must be wider still
    /// not to have been wrapped by hand, so a paragraph wrapped wider than
    /// the limit carries its overflow on into its next line.
    #[test]
    fn wide_limits_carry_the_overflow_on() {
        let src = format!(
            "#{} end\n#{} last\n",
            " word".repeat(30),
            " word".repeat(30)
        );
        assert!(src.lines().all(|line| columns(&[line]) > 150));
        let (out, reflowed) = reflow(&src, 150);
        assert_eq!(reflowed, 1);
        // 62 words fill 3 lines; each line on its own would take 2.
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.lines().all(|line| columns(&[line]) <= 150), "{out}");
    }

    /// A line with a tab or two spaces in a row in its text is lined up in
    /// columns, and is left as it is, as are the lines carrying a cell of it
    /// on further in.
    #[test]
    fn leaves_aligned_lines() {
        for src in [
            "# is_central_america    COUNTRY    TRUE if the country is in Central America\n",
            "#   row 25                [The Deep Ones Walk]              national (override)\n",
            "# name\tthe name of the country, as it is shown to the player\n",
            "#   The Chained Are Given to the Tide  -> event mltd.51, option a (round 28b)\n",
            "# - The quick brown  fox jumps over the lazy dog\n",
            // A cell carried onto the lines below its row, starting further
            // in.
            "#   The Drowned Frumentarius   -> the river carries it (Silanus the Drowned and\n#                                 CES -6 % stability, a token army and the\n#                                 converts)\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
        // Two spaces after the end of a sentence are prose, as are those
        // after a bullet.
        assert_reflow(
            "# The quick brown fox jumps.  Over the lazy dog\n",
            WIDTH,
            "# The quick brown fox jumps.\n# Over the lazy dog\n",
            1,
        );
        assert_reflow(
            "# -  The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# -  The quick brown fox jumps\n#    over the lazy dog\n",
            1,
        );
        // A line only a little further in than a row is prose.
        assert_reflow(
            "#   a  b\n#     The quick brown fox jumps over\n",
            WIDTH,
            "#   a  b\n#     The quick brown fox\n#     jumps over\n",
            1,
        );
    }

    // ---------------------------------------------------------------------
    // What is not prose
    // ---------------------------------------------------------------------

    /// Commented-out script and data are never changed, and end a paragraph.
    #[test]
    fn leaves_code_like_lines() {
        for src in [
            "#\tset_variable = { a_rather_long_variable_name = 1 }\n",
            "# has_war = yes and some more words to make it wide\n",
            "# NOT = { has_war_with = a_country_with_a_long_tag }\n",
            "# } closing a block commented out a long time ago\n",
            "# # A commented-out comment that is far too wide to fit\n",
            "# A note #and another note in it, far too wide to fit\n",
            // A token and an operator.
            "#num_of_factories > 50 is what the AI waits for\n",
            "#naval_base > 0 Better to have it build some first\n",
            "# ANQ_anger: ANQ_anger_multiplier * ANQ_anger_factor\n",
            "# GER_x <= 10 or the war never starts at all\n",
            // Localisation entries.
            "#malaysia.100.d:0 \"The Kramat Pulai Mine, the chief source\"\n",
            "#\tSIN_temasek_holdings_desc: \"Temasek Holdings is a fund\"\n",
            "# key:12 \"A word\" and more words after the entry\n",
            // Lists of quoted names.
            "\t\t# \"Calabria\" \"Basilicata\" \"Puglia\" \"Campania\" \"Zara\"\n",
            "# DD: G/H class (\"HMS Gallant\" \"HMS Garland\" \"HMS Gipsy\")\n",
            // Numbers, script keywords and country tags.
            "# 3542 6555 11481 12000 13000 14000 15000 16000\n",
            "# if limit always yes no AND OR NOT else true false\n",
            "# GER ITA JAP SOV ENG FRA USA CHI D01 POL CZE HUN\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
        // The vanilla trigger commented out in CHI_decisions.txt.
        let trigger = "\t\t\t#faction_influence_ratio > var:FROM.press_warlord_to_become_puppet_influence_points_trigger\n";
        assert!(columns(&[trigger.trim_end()]) > 100);
        assert_unchanged(trigger, 100);
        // An operator after a word of prose, a key after a word of prose and a
        // quoted word or two are prose.
        for src in [
            "# Disloyal > Deep State > Loyal, in order of rank\n",
            "# He says: \"no\" to the war and more words\n",
            "# Note: \"x\" is what the fox jumps over\n",
            "# GER_x_tt / GER_y_tt is what the fox jumps over\n",
        ] {
            assert_eq!(reflow(src, WIDTH).1, 1, "{src}");
        }
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n#has_war = yes\n# NOT = {\n# }\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog\n#has_war = yes\n# NOT = {\n# }\n",
            1,
        );
    }

    /// A paragraph whose last sentence may run on into the line below it
    /// that is not prose, in the same column, or into a term being defined,
    /// is left as it is: refilling it would leave a scrap of the sentence
    /// above a line that stays as it was.
    #[test]
    fn leaves_prose_running_on_into_code() {
        for next in [
            "license_purchase_cost.",
            "transaction_caps (= add_caps with its tooltip)",
            "3542 6555 11481",
            "if limit",
            "mltd_x_tt - change both.",
            "a table  row",
        ] {
            assert_unchanged(
                &format!("# The quick brown fox jumps over the lazy dog's\n# {next}\n"),
                WIDTH,
            );
        }
        // Not once the sentence has ended, nor into a line in another
        // column, a decoration or a paragraph break.
        for (end, next) in [
            ("dog.", "license_purchase_cost"),
            ("dog", "\tlicense_purchase_cost"),
            ("dog", "-----"),
            ("dog", ""),
        ] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy {end}\n# {next}\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy {end}\n# {next}\n"),
                1,
            );
        }
        // An item's sentence may run on into its continuation column.
        assert_unchanged(
            "# - The quick brown fox jumps over the lazy dog and\n#   GER_x_tt = 5 goes on\n",
            WIDTH,
        );
        // The vanilla notes above a list of provinces.
        let provinces = "\t\t# These provinces were moved to the new Saar state in the last update, together with the victory points they held\n\t\t# 3542 6555 11481\n";
        assert_unchanged(provinces, 100);
        assert_eq!(reflow(&provinces.replace("held", "held."), 100).1, 1);
    }

    /// The lines inside a block of commented-out script are never changed,
    /// whatever they hold: a list of names in it is data, not prose.
    #[test]
    fn leaves_commented_out_blocks() {
        let block = "#\tsurnames = {\n#\t\tAguirre Ajuriaguerra Aldecoa Arana Aranzadi Arizmendiarrieta\n#\t\tBilbao Campión \"de Amésquita\" \"de Rentería\" Echave\n#\t}\n";
        // The block ends at its closing brace.
        assert_reflow(
            &format!("{block}# The quick brown fox jumps over the lazy dog\n"),
            WIDTH,
            &format!("{block}# The quick brown fox jumps\n# over the lazy dog\n"),
            1,
        );
        // Or, left open, at the first line that is not a full-line comment.
        assert_reflow(
            "# old = {\n# The quick brown fox jumps over the lazy dog\nx = y\n# The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# old = {\n# The quick brown fox jumps over the lazy dog\nx = y\n# The quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
        // A brace in a quoted string opens no block.
        assert_reflow(
            "# x = \"{\"\n# The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# x = \"{\"\n# The quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
    }

    /// A paragraph whose new first line would read as something other than
    /// the line it replaces is left as it is: the lines around it were read
    /// by what that line was, and would be read differently next time.
    #[test]
    fn keeps_how_the_first_line_reads() {
        // Cut short, the item would be `- - ... -`, a decoration, and the
        // line above it, left open because an item follows, would not be.
        assert_unchanged(
            "# The quick brown fox jumps over the lazy dog\n# - - ... - https://example.com/a/very/long/path and more\n",
            WIDTH,
        );
        // A lone token of script, or mostly quoted strings.
        for src in [
            "# GER_focus_with_a_rather_long_name starts at 0.01, +0.02 per level\n",
            "# The \"Tankova Dyviziia\" is created in the oob so another is made\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
    }

    /// A word or a path broken across lines is left as it is: refilling it
    /// would put a space in it.
    #[test]
    fn leaves_words_broken_across_lines() {
        for (end, next) in [
            ("state-", "owned lazy dog"),
            ("common/", "ideas folder of the dog"),
        ] {
            assert_unchanged(
                &format!("# The quick brown fox jumps over the {end}\n# {next}\n"),
                WIDTH,
            );
        }
        // A break before the first line too wide is kept anyway.
        assert_reflow(
            "# the quick brown fox's state-\n# owned quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# the quick brown fox's state-\n# owned quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
        // A dash on its own is no broken word.
        assert_reflow(
            "# The quick brown fox jumps over the lazy -\n# dog\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy - dog\n",
            1,
        );
    }

    /// A single word that looks like a token of script is not prose, but a
    /// word ending a sentence is.
    #[test]
    fn identifier_like_words_are_not_prose() {
        for word in [
            "GER_focus_x",
            "events.12",
            "FROM:owner",
            "@cost",
            "$PARAM$",
            "[Root.GetName]",
        ] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy dog.\n# {word}\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy dog.\n# {word}\n"),
                1,
            );
        }
        for word in ["dog.", "dog:", "(dog)", "dog!"] {
            assert_reflow(
                &format!("# The quick brown fox jumps over the lazy\n# {word}\n"),
                WIDTH,
                &format!("# The quick brown fox jumps\n# over the lazy {word}\n"),
                1,
            );
        }
        // Tokens among other words are prose.
        assert_reflow(
            "# GER_focus_x gives GER.1 to FROM:owner\n",
            WIDTH,
            "# GER_focus_x gives GER.1 to\n# FROM:owner\n",
            1,
        );
    }

    /// Decorations are never changed, and end a paragraph.
    #[test]
    fn leaves_decorations() {
        for src in [
            "# ---------------------------------------------\n",
            "#############################################\n",
            "### Focus tree #################################\n",
            "# -- a -- b -- c -- d -- e -- f -- g -- h -- i --\n",
            "# ##  ##  ########  ##   ##  ########  ##  ##\n",
            "# **** The quick brown fox jumps over the lazy dog\n",
            "# The quick brown fox jumps over the lazy dog ~~~\n",
            "#.... The quick brown fox jumps over the lazy dog\n",
            "##### A BANNER OF A HEADING TOO WIDE TO FIT HERE\n",
            "##########MAYBE add a wargoal here but this is Tuva\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
        // Up to four `#`s start prose.
        assert_reflow(
            "#### The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "#### The quick brown fox jumps\n#### over the lazy dog\n",
            1,
        );
        assert!(is_decoration("-- a --"));
        assert!(!is_decoration("a - b"));
        // An ellipsis is prose.
        assert_reflow(
            "# ...and the quick brown fox jumps over the lazy dog...\n",
            WIDTH,
            "# ...and the quick brown fox\n# jumps over the lazy dog...\n",
            1,
        );
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n# ----\n# more\n",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog\n# ----\n# more\n",
            1,
        );
    }

    /// A paragraph ends at an empty comment, a blank line, code, or a change
    /// of indent, marker or gap.
    #[test]
    fn paragraph_breaks() {
        for between in [
            "#\n",
            "#   \n",
            "\n",
            "x = y\n",
            "  # \n",
            "#two words\n",
            "  # two words\n",
            " #two words\n",
            "## two words\n",
            "#  two words\n",
            "#\ttwo words\n",
        ] {
            let src =
                format!("# The quick brown fox jumps over the lazy dog\n{between}# Next line.\n");
            assert_reflow(
                &src,
                WIDTH,
                &format!(
                    "# The quick brown fox jumps\n# over the lazy dog\n{between}# Next line.\n"
                ),
                1,
            );
        }
        // Editors show a lone `\r` as a line break.
        assert_unchanged("# The quick brown fox\rjumps over the lazy dog\n", WIDTH);
        // A gap of spaces or of a tab, starting the text in the same column,
        // makes no break; the new lines take the first line's.
        assert_reflow(
            "#   The quick brown fox jumps over the\n#\tlazy dog\n",
            WIDTH,
            "#   The quick brown fox jumps\n#   over the lazy dog\n",
            1,
        );
    }

    // ---------------------------------------------------------------------
    // List items
    // ---------------------------------------------------------------------

    /// A list item starts a paragraph, whose continuation lines hang under
    /// its text.
    #[test]
    fn list_items_hang() {
        assert_reflow(
            "# - The quick brown fox jumps over the lazy dog\n#   and runs away.\n# - Second item.\n",
            WIDTH,
            "# - The quick brown fox jumps\n#   over the lazy dog and runs\n#   away.\n# - Second item.\n",
            1,
        );
        for bullet in ["*", "+", "•"] {
            assert_reflow(
                &format!("# {bullet} The quick brown fox jumps over the lazy dog\n"),
                WIDTH,
                &format!("# {bullet} The quick brown fox jumps\n#   over the lazy dog\n"),
                1,
            );
        }
        assert_reflow(
            "# 1. The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# 1. The quick brown fox jumps\n#    over the lazy dog\n",
            1,
        );
        assert_reflow(
            "# 10) The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# 10) The quick brown fox\n#     jumps over the lazy dog\n",
            1,
        );
        // Several spaces after the bullet hang the text under them.
        assert_reflow(
            "# -  The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# -  The quick brown fox jumps\n#    over the lazy dog\n",
            1,
        );
        // An item's overflow never reaches the next item, nor a line that
        // does not hang under it.
        assert_reflow(
            "# - The quick brown fox jumps over the lazy dog\n# - Second.\n# Not hanging.\n",
            WIDTH,
            "# - The quick brown fox jumps\n#   over the lazy dog\n# - Second.\n# Not hanging.\n",
            1,
        );
        // A continuation line too wide is refilled from there.
        assert_reflow(
            "# - Item\n#   The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# - Item\n#   The quick brown fox jumps\n#   over the lazy dog\n",
            1,
        );
        // Not items: a year, a number, no space after the bullet, a name
        // and a dash.
        for text in [
            "1936. The",
            "1.5 The",
            "12345) The",
            "-The",
            "- ",
            "-",
            "MLT - the",
            "term -the",
            "term  - the",
            "(term) - the",
        ] {
            assert_eq!(bullet(text), None, "{text}");
        }
        assert_eq!(bullet("- The"), Some("- "));
        assert_eq!(bullet("12.   The"), Some("12.   "));
        assert_eq!(bullet("can_use - is"), Some("can_use - "));
        assert_eq!(bullet("hardness -  it"), Some("hardness -  "));
        assert_reflow(
            "# 1936. The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "# 1936. The quick brown fox\n# jumps over the lazy dog\n",
            1,
        );
    }

    /// A term being defined is an item too, as in vanilla's
    /// `names_divisions` files: its description hangs under itself, and the
    /// next term starts its own paragraph.
    #[test]
    fn definitions_hang() {
        assert_reflow(
            "# can_use - is a trigger that locks the group\n#           when it is false.\n",
            40,
            "# can_use - is a trigger that locks the\n#           group when it is false.\n",
            1,
        );
        assert_reflow(
            "#hardness - it is in the script, but set to 0\n#armor_value - it is in the script as well, set to 0\n",
            40,
            "#hardness - it is in the script, but set\n#           to 0\n#armor_value - it is in the script as\n#              well, set to 0\n",
            2,
        );
        // Continuation lines may start anywhere after the bullet up to the
        // text, with tabs or spaces: the new ones take the first one's.
        assert_reflow(
            "# can_use - is a trigger that locks the group\n#\t\t\twhen it is false.\n",
            40,
            "# can_use - is a trigger that locks the\n#\t\t\tgroup when it is false.\n",
            1,
        );
        // As in Millennium Dawn's copies of vanilla's header.
        assert_reflow(
            "# division_types - is a list of tokens to corresponding unit types. A player can in fact use any group of names for a div.template\n#\t\t\t\t  however this tag is a helper for an automated choice (for AI, or if the group must switch on it's own, because\n#\t\t\t\t  for example the current one is no longer available due to the can_use trigger saying so).\n#\t\t\t\t  In automated choice, the division template must have at least 1 of the following types for it to be chosen.\n",
            100,
            "# division_types - is a list of tokens to corresponding unit types. A player can in fact use any\n#\t\t\t\t  group of names for a div.template however this tag is a helper for an automated\n#\t\t\t\t  choice (for AI, or if the group must switch on it's own, because for example the\n#\t\t\t\t  current one is no longer available due to the can_use trigger saying so).\n#\t\t\t\t  In automated choice, the division template must have at least 1 of the following\n#\t\t\t\t  types for it to be chosen.\n",
            2,
        );
        assert_reflow(
            "# fallback_name - Is going to be used if we run out of the scripted historical names. If you want to use the old division naming\n#\t\t\t\t mechanics to be used for fallbacks, then just skip this option.\n",
            100,
            "# fallback_name - Is going to be used if we run out of the scripted historical names. If you want to\n#\t\t\t\t use the old division naming mechanics to be used for fallbacks, then just skip this\n#\t\t\t\t option.\n",
            1,
        );
        // A line further in than the item's text is not part of it.
        assert_reflow(
            "# - The quick brown fox jumps over the lazy dog\n#      an example\n",
            WIDTH,
            "# - The quick brown fox jumps\n#   over the lazy dog\n#      an example\n",
            1,
        );
        // Deeper in, a term and a dash are more likely a cell of a table.
        assert_eq!(super::kind("tag - one grant per tag"), Kind::Item("tag - "));
        let doc = cst::parse("#   tag - one grant per tag\n").unwrap_or_else(|err| panic!("{err}"));
        assert!(
            full_line_comments(&doc)
                .iter()
                .all(|line| line.kind == Kind::Prose)
        );
        // Wrapped prose that breaks a line before `word - ` goes on.
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog in two\n# groups - the one and the other\n",
            40,
            "# The quick brown fox jumps over the\n# lazy dog in two groups - the one and\n# the other\n",
            1,
        );
    }

    /// A dash that wrapped prose happens to start a line with is no list
    /// item: after a line ending in a word but no sentence, it goes on in
    /// lower case or closes a bracket, with no item right below it.
    #[test]
    fn dashes_in_prose_are_not_items() {
        assert_reflow(
            "# For MLT the cap is the floor plus special_forces_min\n# - because the other term would need more of them to\n# pass it.\n",
            40,
            "# For MLT the cap is the floor plus\n# special_forces_min - because the other\n# term would need more of them to pass\n# it.\n",
            1,
        );
        assert_reflow(
            "# (see the note below, for a reload\n# - Gotchas). A subject is never moved again.\n",
            WIDTH,
            "# (see the note below, for a\n# reload - Gotchas). A subject\n# is never moved again.\n",
            1,
        );
        // A list stays a list, and the line introducing it is left as it is.
        assert_reflow(
            "# The options are\n# - one thing that is long enough to wrap\n# - another\n",
            WIDTH,
            "# The options are\n# - one thing that is long\n#   enough to wrap\n# - another\n",
            1,
        );
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\n# - One thing that is long enough to wrap\n",
            WIDTH,
            "# The quick brown fox jumps over the lazy dog\n# - One thing that is long\n#   enough to wrap\n",
            1,
        );
    }

    /// What the text of a comment line makes of it; see the module docs.
    #[test]
    fn classifies_comment_text() {
        use Reason::{Aligned, Code, Decoration, Empty};
        for (text, expected) in [
            ("", Kind::Other(Empty)),
            ("a\rb", Kind::Other(Empty)),
            ("----- a -----", Kind::Other(Decoration)),
            ("has_war = yes", Kind::Other(Code)),
            ("faction_influence_ratio > var:X", Kind::Other(Code)),
            ("naval_base > 0 Better", Kind::Other(Code)),
            ("ANQ_x: ANQ_y * ANQ_z", Kind::Other(Code)),
            ("malaysia.100.d:0 \"The mine\"", Kind::Other(Code)),
            ("SIN_desc: \"Temasek\"", Kind::Other(Code)),
            ("\"Zara\" \"Pola\" and", Kind::Other(Code)),
            ("3542 6555 11481", Kind::Other(Code)),
            ("if limit", Kind::Other(Code)),
            ("GER ITA", Kind::Other(Code)),
            ("yes", Kind::Other(Code)),
            ("GER_focus_x", Kind::Other(Code)),
            ("a  b", Kind::Other(Aligned)),
            ("Tide  -> event", Kind::Other(Aligned)),
            ("a\tb", Kind::Other(Aligned)),
            ("a   b", Kind::Other(Aligned)),
            ("- a  b", Kind::Other(Aligned)),
            ("Disloyal > Deep State", Kind::Prose),
            ("He says: \"no\" to it", Kind::Prose),
            ("Note: \"x\" and y", Kind::Prose),
            ("3 times.", Kind::Prose),
            ("end.  Next", Kind::Prose),
            ("dog.", Kind::Prose),
            ("GER and ITA", Kind::Prose),
            ("-  The", Kind::Item("-  ")),
            ("12. The", Kind::Item("12. ")),
        ] {
            assert_eq!(super::kind(text), expected, "{text:?}");
        }
    }

    // ---------------------------------------------------------------------
    // Width and prefixes
    // ---------------------------------------------------------------------

    /// A tab advances to the next multiple of 4 columns, in the indent and
    /// in the gap.
    #[test]
    fn tabs_count_to_the_next_stop() {
        // `\t\t# The quick brown fox jumps over` is 8 + 32 columns.
        let src = "\t\t# The quick brown fox jumps over\n";
        assert_unchanged(src, 40);
        assert_reflow(src, 39, "\t\t# The quick brown fox jumps\n\t\t# over\n", 1);
        // Two spaces and a tab reach column 4, not 6.
        let src = "  \t# The quick brown fox jumps over\n";
        assert_unchanged(src, 36);
        assert_reflow(src, 35, "  \t# The quick brown fox jumps\n  \t# over\n", 1);
        // `#\t` takes 4 columns.
        assert_reflow(
            "#\tThe quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "#\tThe quick brown fox jumps\n#\tover the lazy dog\n",
            1,
        );
        // Non-ASCII chars take a column each.
        assert_reflow(
            "# Ça fait très très longtemps que je suis là\n",
            WIDTH,
            "# Ça fait très très longtemps\n# que je suis là\n",
            1,
        );
    }

    /// Any number of `#`s makes the marker, with or without a gap, and the
    /// new lines keep it.
    #[test]
    fn keeps_the_prefix() {
        assert_reflow(
            "## The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "## The quick brown fox jumps\n## over the lazy dog\n",
            1,
        );
        assert_reflow(
            "#The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "#The quick brown fox jumps\n#over the lazy dog\n",
            1,
        );
        assert_reflow(
            "   #   The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "   #   The quick brown fox\n   #   jumps over the lazy dog\n",
            1,
        );
    }

    /// A paragraph whose prefix leaves fewer than [`MIN_TEXT_COLUMNS`]
    /// columns for text is left alone.
    #[test]
    fn skips_deeply_indented_comments() {
        assert_eq!(MIN_TEXT_COLUMNS, 20);
        // `\t\t# ` takes 10 columns, leaving 20 of 30.
        assert_reflow(
            "\t\t# The quick brown fox jumps over\n",
            WIDTH,
            "\t\t# The quick brown fox\n\t\t# jumps over\n",
            1,
        );
        // `\t\t #` and a space take 11, leaving 19.
        assert_unchanged("\t\t # The quick brown fox jumps over\n", WIDTH);
        assert_unchanged(
            "\t\t\t\t\t# The quick brown fox jumps over the lazy dog\n",
            40,
        );
        // An item's hanging indent counts.
        assert_unchanged("\t\t# - The quick brown fox jumps over\n", WIDTH);
    }

    // ---------------------------------------------------------------------
    // Line endings, BOM, and what is not a full-line comment
    // ---------------------------------------------------------------------

    #[test]
    fn keeps_crlf() {
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog\r\n# ok\r\nx = y\r\n",
            WIDTH,
            "# The quick brown fox jumps\r\n# over the lazy dog ok\r\nx = y\r\n",
            1,
        );
        // The last line has no terminator: the file's first is used.
        assert_reflow(
            "x = y\r\n# The quick brown fox jumps over the lazy dog",
            WIDTH,
            "x = y\r\n# The quick brown fox jumps\r\n# over the lazy dog",
            1,
        );
        assert_reflow(
            "# The quick brown fox jumps over the lazy dog",
            WIDTH,
            "# The quick brown fox jumps\n# over the lazy dog",
            1,
        );
    }

    /// A leading BOM is kept, and takes no columns.
    #[test]
    fn keeps_a_bom() {
        assert_reflow(
            "\u{feff}# The quick brown fox jumps over the lazy dog\n",
            WIDTH,
            "\u{feff}# The quick brown fox jumps\n# over the lazy dog\n",
            1,
        );
        assert_unchanged("\u{feff}# The quick brown fox jumps ov\n", WIDTH);
    }

    /// A `#` in a quoted string starts no comment.
    #[test]
    fn leaves_hashes_in_strings() {
        for src in [
            "x = \"# The quick brown fox jumps over the lazy dog\"\n",
            "x = \"a\n# The quick brown fox jumps over the lazy dog\"\n",
            "x = \"a\n  # The quick brown fox jumps over the lazy dog\n\"\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
    }

    /// A comment after code on its line is left as it is, and is not part
    /// of the paragraph on the next line.
    #[test]
    fn leaves_trailing_comments() {
        for src in [
            "x = y # The quick brown fox jumps over the lazy dog\n",
            "a = { # The quick brown fox jumps over the lazy dog\n}\n",
            "a = {\n} # The quick brown fox jumps over the lazy dog\n",
        ] {
            assert_unchanged(src, WIDTH);
        }
        assert_reflow(
            "x = y # The quick brown fox jumps over the lazy dog\n# The quick brown fox jumps over\n",
            WIDTH,
            "x = y # The quick brown fox jumps over the lazy dog\n# The quick brown fox jumps\n# over\n",
            1,
        );
    }

    #[test]
    fn leaves_unparseable_input() {
        for src in [
            "a = {\n# The quick brown fox jumps over the lazy dog\n",
            "# The quick brown fox jumps over the lazy dog\n}\n",
        ] {
            assert_eq!(apply(src, WIDTH), (src.to_owned(), 0));
        }
    }

    /// Every full-line comment is split into its parts, whatever block it
    /// is in; comments after code are not full-line comments.
    #[test]
    fn splits_full_line_comments() {
        let src = "\u{feff}## a  \na = {\n\t#\tb = c\n\tx = y # d\n\t{\n  #- e f\n\t}\n}\n#\n";
        let doc = cst::parse(src).unwrap_or_else(|err| panic!("{err}"));
        let lines: Vec<(&str, &str, &str, &str, Kind<'_>)> = full_line_comments(&doc)
            .iter()
            .map(|line: &Line<'_>| (line.indent, line.marker, line.gap, line.text, line.kind))
            .collect();
        assert_eq!(
            lines,
            [
                ("", "##", " ", "a", Kind::Prose),
                ("\t", "#", "\t", "b = c", Kind::Other(Reason::Code)),
                ("  ", "#", "", "- e f", Kind::Item("- ")),
                ("", "#", "", "", Kind::Other(Reason::Empty)),
            ]
        );
    }

    /// Pseudo-random documents mixing script, comments on their own lines
    /// and after code: the output parses, keeps every token and every word
    /// of every comment in order, fits the lines it rewrites, and is a
    /// fixed point.
    #[test]
    fn random_documents_keep_their_words_and_are_idempotent() {
        let mut generator = Generator(0x2545_f491_4f6c_dd1d);
        let mut reflowed = 0;
        for _ in 0_u32..20_000 {
            let src = generator.document();
            let width = 25 + generator.below(40);
            reflowed += reflow(&src, width).1;
        }
        assert!(
            reflowed > 600,
            "the documents exercise reflowing ({reflowed})"
        );
    }

    // ---------------------------------------------------------------------
    // Corpus
    // ---------------------------------------------------------------------

    /// Every script file under `root`, sorted by path.
    fn corpus_files(root: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = ["common", "events", "history"]
            .iter()
            .flat_map(|sub| walkdir::WalkDir::new(root.join(sub)))
            .filter_map(Result::ok)
            .filter(|entry| {
                !entry.file_type().is_dir()
                    && entry
                        .path()
                        .strip_prefix(root)
                        .ok()
                        .and_then(crate::schema::file_kind)
                        .is_some()
            })
            .map(walkdir::DirEntry::into_path)
            .collect();
        files.sort();
        files
    }

    /// Which of [`REASONS`] `line`, wider than `width`, is left for (or
    /// what prose it is); `held` if its paragraph is left as it is for doubt
    /// about where it ends (`Some(true)`) or as refilling it would break a
    /// word or change how its first line reads (`Some(false)`).
    fn reason(line: &Line<'_>, width: usize, held: Option<bool>) -> usize {
        let one_word = line.words().nth(1).is_none();
        let deep = line.column + MIN_TEXT_COLUMNS > width;
        match (line.kind, held) {
            (Kind::Other(Reason::Code | Reason::Empty), _) => 0,
            (Kind::Other(Reason::Decoration), _) => 1,
            (Kind::Other(Reason::Aligned), _) => 2,
            (Kind::Other(Reason::Block), _) => 3,
            (Kind::Other(Reason::Cell), _) => 4,
            _ if one_word => 5,
            _ if deep => 6,
            (_, Some(true)) => 7,
            (_, Some(false)) => 8,
            (Kind::Prose, None) => 9,
            (Kind::Item(_), None) => 10,
        }
    }

    /// The full-line comments of `src` wider than `width`, counted by
    /// [`reason`].
    fn overflowing(src: &str, width: usize) -> [usize; REASONS.len()] {
        let mut counts = [0; REASONS.len()];
        let Ok(doc) = cst::parse(src) else {
            return counts;
        };
        let mut lines = full_line_comments(&doc);
        blocks(src, &mut lines);
        cells(src, &mut lines);
        items_in_prose(src, &mut lines, width);
        let mut held = vec![None; lines.len()];
        for paragraph in paragraphs(src, &lines, width) {
            let Some(paragraph_lines) = lines.get(paragraph.first..=paragraph.last) else {
                continue;
            };
            let kept = paragraph_lines.iter().any(|line| line.overflows(width))
                && super::reflow(src, paragraph_lines, width).is_none();
            let state = (paragraph.frozen || kept).then_some(paragraph.frozen);
            for index in paragraph.first..=paragraph.last {
                if let Some(held) = held.get_mut(index) {
                    *held = state;
                }
            }
        }
        for (line, held) in lines.iter().zip(held) {
            if line.width > width
                && let Some(count) = counts.get_mut(reason(line, width, held))
            {
                *count += 1;
            }
        }
        counts
    }

    /// The line of `src` holding `offset`, counted from 1.
    fn line_number(src: &str, offset: usize) -> usize {
        src.get(..offset)
            .map_or(0, |before| before.matches('\n').count())
            + 1
    }

    /// Where the line after the one holding `offset` ends, before its
    /// terminator; the end of `src` if there is none.
    fn next_line_end(src: &str, offset: usize) -> usize {
        let end = cst::line_end(src, offset);
        src.get(end..)
            .and_then(|rest| rest.find('\n'))
            .map_or(src.len(), |newline| cst::line_end(src, end + newline + 1))
    }

    /// Reflows one corpus file at `width` and checks the result; `None` for
    /// files that are not UTF-8 or do not parse (which are left alone).
    fn check_corpus_file(root: &Path, path: &Path, width: usize) -> Option<FileReport> {
        let src = std::fs::read_to_string(path).ok()?;
        let doc = cst::parse(&src).ok()?;
        let edits = edits(&doc, width);
        let (out, reflowed) = apply(&src, width);
        let mut report = FileReport {
            left: overflowing(&out, width),
            overflowing: overflowing(&src, width),
            reflowed,
            ..FileReport::default()
        };
        if reflowed != edits.len() {
            report
                .problems
                .push("the count differs from the edits".to_owned());
        }
        if let Err(problem) = check(&src, &out, width) {
            report.problems.push(problem);
        }
        let (again, more) = apply(&out, width);
        if more != 0 || again != out {
            report
                .problems
                .push(format!("not idempotent ({more} more reflowed)"));
        }
        let relative = path.strip_prefix(root).unwrap_or(path).display();
        let mut lines = full_line_comments(&doc);
        blocks(&src, &mut lines);
        cells(&src, &mut lines);
        items_in_prose(&src, &mut lines, width);
        let mut above: Option<Paragraph> = None;
        for paragraph in paragraphs(&src, &lines, width) {
            let (Some(first), Some(last)) = (lines.get(paragraph.first), lines.get(paragraph.last))
            else {
                continue;
            };
            let overflows = lines
                .get(paragraph.first..=paragraph.last)
                .is_some_and(|lines| lines.iter().any(|line| line.overflows(width)));
            if paragraph.frozen && overflows {
                let start = cst::line_start(&src, first.start.saturating_sub(1));
                let end = next_line_end(&src, last.end);
                let from_above = above.map(|above| next_line(&src, &lines, &above, first, width));
                let to_below = lines
                    .get(paragraph.last + 1)
                    .map(|below| next_line(&src, &lines, &paragraph, below, width));
                report.frozen.push(format!(
                    "=== {relative}:{} above {from_above:?} below {to_below:?}\n{}\n",
                    line_number(&src, first.start),
                    src.get(start..end)
                        .unwrap_or_default()
                        .replace('\r', "")
                        .replace('\t', "    ")
                ));
            }
            above = Some(paragraph);
        }
        for edit in &edits {
            let context_start = cst::line_start(&src, edit.span.start.saturating_sub(1));
            let context_end = next_line_end(&src, edit.span.end);
            let mut hunk = vec![format!(
                "--- {relative}:{}",
                line_number(&src, edit.span.start)
            )];
            let before = src.get(context_start..edit.span.start).unwrap_or_default();
            let after = src.get(edit.span.end..context_end).unwrap_or_default();
            for (sign, text) in [
                (' ', before),
                ('-', edit.span.text(&src)),
                ('+', edit.replacement.as_str()),
            ] {
                hunk.extend(text.lines().map(|line| format!("{sign} {line}")));
            }
            hunk.extend(after.lines().skip(1).map(|line| format!("  {line}")));
            report.hunks.push(format!(
                "{}\n",
                hunk.join("\n").replace('\r', "").replace('\t', "    ")
            ));
        }
        Some(report)
    }

    /// Reflows every script file of each `;`-separated root in
    /// `HEARTY_CORPUS` at width 100 (or `HEARTY_REFLOW_WIDTH`) and checks
    /// that the output parses, keeps every token other than comments and
    /// every word of every comment, fits every line it rewrites unless it
    /// holds a single word, and is a fixed point. Prints per-corpus statistics
    /// and sample hunks spread across each corpus; `HEARTY_REFLOW_DUMP=<file>`
    /// writes every hunk there, and `HEARTY_REFLOW_FROZEN=<file>` every
    /// paragraph with a line too wide left as it is for doubt about where it
    /// ends.
    ///
    /// Run with `cargo test --release reflow::tests::corpus -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_reflow_keeps_words_and_is_idempotent() {
        /// Sample hunks to print, shared between the corpora.
        const SAMPLES: usize = 32;
        let max_width = std::env::var("HEARTY_REFLOW_WIDTH")
            .ok()
            .and_then(|width| width.parse().ok())
            .unwrap_or(100);
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let roots: Vec<&str> = roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .collect();
        let mut failures = Vec::new();
        let mut dump = String::new();
        let mut frozen_dump = String::new();
        for root in &roots {
            let root = Path::new(root);
            let reports: Vec<(PathBuf, Option<FileReport>)> = corpus_files(root)
                .into_par_iter()
                .map(|path| {
                    let report = check_corpus_file(root, &path, max_width);
                    (path, report)
                })
                .collect();
            let mut skipped = 0_usize;
            let mut changed = 0_usize;
            let mut reflowed = 0_usize;
            let mut before = [0_usize; REASONS.len()];
            let mut after = [0_usize; REASONS.len()];
            let mut hunks = Vec::new();
            for (path, report) in &reports {
                let Some(report) = report else {
                    skipped += 1;
                    continue;
                };
                changed += usize::from(report.reflowed > 0);
                reflowed += report.reflowed;
                for (total, count) in before.iter_mut().zip(report.overflowing) {
                    *total += count;
                }
                for (total, count) in after.iter_mut().zip(report.left) {
                    *total += count;
                }
                hunks.extend(report.hunks.iter());
                for frozen in &report.frozen {
                    frozen_dump.push_str(frozen);
                }
                failures.extend(
                    report
                        .problems
                        .iter()
                        .map(|problem| format!("{}: {problem}", path.display())),
                );
            }
            let breakdown = |counts: &[usize; REASONS.len()]| {
                REASONS
                    .iter()
                    .zip(counts)
                    .map(|(reason, count)| format!("{reason} {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!(
                "{}\n    files: {} (skipped, not UTF-8 or unparseable: {skipped}) | changed: \
                 {changed} | paragraphs reflowed: {reflowed}\n    full-line comments wider \
                 than {max_width}: before {} ({}); after {} ({})",
                root.display(),
                reports.len(),
                before.iter().sum::<usize>(),
                breakdown(&before),
                after.iter().sum::<usize>(),
                breakdown(&after),
            );
            let samples = SAMPLES / roots.len().max(1);
            let step = (hunks.len() / samples.max(1)).max(1);
            for hunk in hunks.iter().step_by(step).take(samples) {
                print!("{hunk}");
            }
            for hunk in &hunks {
                dump.push_str(hunk);
            }
        }
        for (variable, text) in [
            ("HEARTY_REFLOW_DUMP", dump),
            ("HEARTY_REFLOW_FROZEN", frozen_dump),
        ] {
            if let Ok(path) = std::env::var(variable)
                && let Err(err) = std::fs::write(&path, text)
            {
                println!("could not write {path}: {err}");
            }
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
