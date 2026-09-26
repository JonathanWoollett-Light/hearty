//! Lossless concrete syntax tree (CST) for Clausewitz / HOI4 script.
//!
//! Formatter rules (field reordering, inlining short blocks, spacing
//! normalisation) and auto-fixing lints need byte-exact positions and the
//! comments of a file, neither of which jomini's `TextTape` exposes. This CST
//! stores [`Span`]s into the original `&str` — nothing is copied — and every
//! transform works by computing [`Edit`]s against the source, so bytes a
//! transform does not touch are always preserved.
//!
//! Every byte of a successfully parsed source is either inside a token span
//! (scalar, operator, brace or comment) or is whitespace / a leading BOM.
//!
//! Lexing: whitespace is anything `char::is_whitespace`; `#` starts a comment
//! running to the end of the line; a `"` at the start of a token opens a
//! quoted scalar (a backslash escapes the next char; it may span lines) but is
//! literal inside a bare scalar; the operators are `= == != < <= > >= ?=`; a
//! bare scalar runs until whitespace, a brace, `=`, `<`, `>`, `#` or the start
//! of `!=` / `?=`, so `[?var]`, `$PARAM$` and `FROM.owner` are single tokens;
//! and `@[ ... ]` inline math extends to its matching `]`.
use std::fmt;

/// The UTF-8 byte order mark. Only a leading BOM is trivia; anywhere else it
/// is an ordinary scalar character (as it is for jomini and the game).
const BOM: char = '\u{feff}';

/// The lexer's class of every byte; see [`ByteClass`].
const BYTE_CLASSES: [ByteClass; 256] = {
    let mut table = [ByteClass::Bare; 256];
    // The ASCII chars `char::is_whitespace` accepts: `\t`, `\n`, vertical
    // tab, form feed, `\r` and space.
    table[0x09] = ByteClass::Space;
    table[0x0a] = ByteClass::Space;
    table[0x0b] = ByteClass::Space;
    table[0x0c] = ByteClass::Space;
    table[0x0d] = ByteClass::Space;
    table[0x20] = ByteClass::Space;
    // `#`, `<`, `=`, `>`, `{` and `}`.
    table[0x23] = ByteClass::Delimiter;
    table[0x3c] = ByteClass::Delimiter;
    table[0x3d] = ByteClass::Delimiter;
    table[0x3e] = ByteClass::Delimiter;
    table[0x7b] = ByteClass::Delimiter;
    table[0x7d] = ByteClass::Delimiter;
    // `!` and `?`.
    table[0x21] = ByteClass::MaybeOperator;
    table[0x3f] = ByteClass::MaybeOperator;
    // The lead bytes of the non-ASCII chars `char::is_whitespace` accepts:
    // U+0085 and U+00A0 (0xC2), U+1680 (0xE1), U+2000..=U+200A, U+2028,
    // U+2029, U+202F and U+205F (0xE2), and U+3000 (0xE3).
    table[0xc2] = ByteClass::MaybeSpace;
    table[0xe1] = ByteClass::MaybeSpace;
    table[0xe2] = ByteClass::MaybeSpace;
    table[0xe3] = ByteClass::MaybeSpace;
    table
};

/// Deepest block nesting [`parse`] accepts; deeper input is a [`ParseError`].
///
/// Parsing is iterative, but the tree's derived `Clone`/`Debug`/`PartialEq`/
/// `Drop` impls recurse once per level; in an unoptimised build `Clone` and
/// `{:?}` take about 2 KiB of stack per level and `{:#?}` about 3 KiB. 512
/// levels overflowed a 1 MiB stack (the Windows main-thread default), an
/// abort that cannot be caught. At 128 levels the costliest (`{:#?}`, ~390
/// KiB) fits in half of such a stack, leaving the rest to recursive callers
/// such as formatter and lint passes.
/// Real scripts nest far less: the deepest `common/`, `events/` or `history/`
/// file in vanilla HOI4 and three large mods nests 16 levels.
const MAX_DEPTH: usize = 128;

/// A `{ ... }` block, or the implicit root block of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    /// Byte offset of `}`; `None` for the root.
    pub close: Option<usize>,
    /// Spans of every `#` comment lexically inside this block but NOT inside
    /// a nested block, in source order. A comment span runs from `#` up to
    /// (not including) the line terminator (`\r\n` or `\n`).
    pub comments: Vec<Span>,
    pub entries: Vec<Entry>,
    /// Byte offset of `{`; `None` for the implicit root block.
    pub open: Option<usize>,
}

impl Block {
    /// Whether this block, or any block nested inside it, holds a comment.
    #[must_use]
    pub fn has_comments_recursive(&self) -> bool {
        let mut stack = vec![self];
        while let Some(block) = stack.pop() {
            if !block.comments.is_empty() {
                return true;
            }
            stack.extend(
                block
                    .entries
                    .iter()
                    .filter_map(|entry| entry.value.as_block()),
            );
        }
        false
    }

    /// Whether the braces sit on one line (no `\n` between `{` and `}`).
    /// Always `false` for the root.
    #[must_use]
    pub fn is_single_line(&self, src: &str) -> bool {
        self.span()
            .and_then(|span| src.get(span.start..span.end))
            .is_some_and(|text| !text.contains('\n'))
    }

    /// `{` ..= `}`; `None` for the root.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        Some(Span {
            end: self.close? + 1,
            start: self.open?,
        })
    }
}

/// How the lexer treats a byte when skipping whitespace or scanning a bare
/// scalar. Every byte of a multi-byte char other than the lead bytes in
/// `MaybeSpace` is `Bare`, so byte-wise scans only stop on char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Continues a bare scalar and is not whitespace.
    Bare,
    /// `{ } = < > #`: ends a bare scalar.
    Delimiter,
    /// `!` / `?`: ends a bare scalar only when `=` follows.
    MaybeOperator,
    /// The lead byte of a multi-byte char that may be whitespace; the char
    /// must be decoded to tell.
    MaybeSpace,
    /// ASCII whitespace.
    Space,
}

/// A parsed file: the source it borrows from plus its implicit root block.
#[derive(Debug, Clone)]
pub struct Document<'src> {
    pub root: Block,
    pub src: &'src str,
}

/// A replacement of `span` in the source by `replacement` (an empty span is
/// an insertion, an empty replacement a deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub replacement: String,
    pub span: Span,
}

/// One entry of a block: `key op value`, `key { ... }`, or a bare element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `None` for bare array elements (`{ a b c }`, `{ { 1 2 } { 3 4 } }`).
    pub key: Option<Scalar>,
    /// `None` for bare elements, and for the operator-less form `key { ... }`
    /// (which is kept operator-less; an `=` is never invented).
    pub op: Option<Op>,
    /// From the first byte of the key (or of the value for bare elements) to
    /// the end of the value (after the closing `}` for blocks). Excludes
    /// comments before or after the entry.
    pub span: Span,
    pub value: Value,
}

impl Entry {
    /// The key with any quotes stripped; `None` for bare elements.
    #[must_use]
    pub fn key_str<'src>(&self, src: &'src str) -> Option<&'src str> {
        self.key.map(|key| key.unquoted(src))
    }
}

/// A comparison/assignment operator token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    pub kind: OpKind,
    pub span: Span,
}

/// The operators of Clausewitz script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Assignment or equality, `=`.
    Eq,
    /// Strict equality, `==`.
    EqEq,
    /// Exists-assignment, `?=`.
    Exists,
    /// Greater than or equal, `>=`.
    Ge,
    /// Greater than, `>`.
    Gt,
    /// Less than or equal, `<=`.
    Le,
    /// Less than, `<`.
    Lt,
    /// Inequality, `!=`.
    NotEq,
}

impl OpKind {
    /// The operator's source text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::EqEq => "==",
            Self::Exists => "?=",
            Self::Ge => ">=",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Lt => "<",
            Self::NotEq => "!=",
        }
    }
}

/// A syntax error, located at byte `offset` of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl ParseError {
    fn new(offset: usize, message: &str) -> Self {
        Self {
            message: message.to_owned(),
            offset,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// A bare or quoted scalar token. For quoted scalars the span INCLUDES the
/// quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scalar {
    pub quoted: bool,
    pub span: Span,
}

impl Scalar {
    /// The raw source text, including quotes.
    #[must_use]
    pub fn text<'src>(&self, src: &'src str) -> &'src str {
        self.span.text(src)
    }

    /// The text with surrounding quotes stripped. Escapes are left as-is.
    #[must_use]
    pub fn unquoted<'src>(&self, src: &'src str) -> &'src str {
        let text = self.text(src);
        if self.quoted {
            text.get(1..text.len().saturating_sub(1))
                .unwrap_or_default()
        } else {
            text
        }
    }
}

/// Half-open byte range `[start, end)` into the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub end: usize,
    pub start: usize,
}

impl Span {
    /// The source text covered by this span (empty if it is out of range or
    /// not on char boundaries).
    #[must_use]
    pub fn text(self, src: &str) -> &str {
        src.get(self.start..self.end).unwrap_or_default()
    }
}

/// One reorderable/removable "line unit" of a multi-line block; see
/// [`line_units`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    /// Just past the line terminator ending the entry's last line (after any
    /// trailing same-line comment); `src.len()` if the file ends without one.
    pub end: usize,
    /// Index into `block.entries`.
    pub entry: usize,
    /// Start of the entry's own line (== `start` when there are no attached
    /// comments).
    pub entry_line_start: usize,
    /// Start of the line of the first ATTACHED leading comment line, else the
    /// start of the entry's own line (includes indentation).
    pub start: usize,
}

/// A value: a scalar, a block, or a tagged block such as `rgb { 1 2 3 }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Block(Block),
    Scalar(Scalar),
    /// `rgb { 1 2 3 }`, `hsv { .. }`, `hsv360 { .. }` — any bare scalar in
    /// VALUE position immediately followed (after optional whitespace and
    /// comments) by `{`.
    Tagged {
        block: Block,
        tag: Scalar,
    },
}

impl Value {
    /// The block of a `Block` or `Tagged` value.
    #[must_use]
    pub const fn as_block(&self) -> Option<&Block> {
        match self {
            Self::Block(block) | Self::Tagged { block, .. } => Some(block),
            Self::Scalar(_) => None,
        }
    }

    /// The value's extent: the scalar (incl. quotes), `{` ..= `}`, or the tag
    /// through the closing `}`.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Self::Scalar(scalar) => scalar.span,
            Self::Block(block) => block_span(block),
            Self::Tagged { block, tag } => Span {
                end: block_span(block).end,
                start: tag.span.start,
            },
        }
    }
}

/// A lexer + stack-based parser; the parse never recurses.
///
/// The entries and comments of every open block live on two shared stacks,
/// innermost block last, and move into exactly sized `Vec`s when their block
/// closes. An entry goes onto the stack as soon as its first token is read
/// and is completed in place as the rest arrives (see [`State`]).
///
/// Performance notes, measured on the corpora `corpus_round_trip` reads (see
/// `parse_throughput`): the shared stacks with exactly sized `Vec`s beat
/// growing one `Vec` per open block by 13-24%. Forcing the lexer inline into
/// [`Parser::run`], so tokens stay in registers, gains 1-6%. Completing
/// entries in place from values the parser already holds, rather than
/// reading back what it just stored or assigning over values with drop glue
/// (whose drop call makes the compiler spill the new value), gains 0-4%.
struct Parser<'src> {
    /// Comments of the open blocks; see [`Frame::comments`].
    comments: Vec<Span>,
    /// Entries of the open blocks; see [`Frame::entries`].
    entries: Vec<Entry>,
    /// The open non-root blocks, innermost last.
    frames: Vec<Frame>,
    lexer: Lexer<'src>,
}

impl Parser<'_> {
    /// Closes the innermost open block at `close`.
    fn close(&mut self, close: usize) -> Result<(), SyntaxError> {
        let Some(frame) = self.frames.pop() else {
            return Err(SyntaxError::new(close, "unmatched `}`"));
        };
        // `split_off` moves the block's part of each stack into an exactly
        // sized `Vec` with one copy. `min` keeps it in range; the stacks
        // never shrink below a frame's marks while it is open.
        let mut comments = self
            .comments
            .split_off(frame.comments.min(self.comments.len()));
        let mut entries = self
            .entries
            .split_off(frame.entries.min(self.entries.len()));
        // That leaves the block's own entry, pushed by `open`, on top.
        if let Some(entry) = self.entries.last_mut()
            && let Value::Block(block) | Value::Tagged { block, .. } = &mut entry.value
        {
            entry.span.end = close + 1;
            block.close = Some(close);
            // Swapped in rather than assigned: dropping the block's empty
            // `Vec`s first would make the compiler spill the new ones.
            std::mem::swap(&mut block.comments, &mut comments);
            std::mem::swap(&mut block.entries, &mut entries);
        }
        Ok(())
    }

    /// The root block, at the end of the input.
    fn finish(self, state: State) -> Result<Block, SyntaxError> {
        if state == State::Operator {
            return Err(SyntaxError::new(
                self.lexer.bytes.len(),
                "expected a value after operator, found end of file",
            ));
        }
        if let Some(frame) = self.frames.last() {
            // Report the innermost unclosed `{`.
            return Err(SyntaxError::new(frame.open, "unclosed `{` at end of file"));
        }
        // What is left on the stacks is the root's. Their capacity may exceed
        // it (up to twice the most entries open at once); shrinking them
        // cost 1-4% of the parse.
        Ok(Block {
            close: None,
            comments: self.comments,
            entries: self.entries,
            open: None,
        })
    }

    /// Opens a block at `open`: a bare block if an entry starts here,
    /// otherwise the value of the entry in progress on top of the stack
    /// (`key { .. }`, `key op { .. }` or `key op tag { .. }`, by `state`).
    /// `close` sets the entry's end.
    fn open(&mut self, open: usize, state: State) -> Result<(), SyntaxError> {
        if self.frames.len() >= MAX_DEPTH {
            return Err(SyntaxError::new(open, "blocks nested too deeply"));
        }
        let block = || Block {
            close: None,
            comments: Vec::new(),
            entries: Vec::new(),
            open: Some(open),
        };
        if state == State::Start {
            self.entries.push(Entry {
                key: None,
                op: None,
                span: Span {
                    end: open + 1,
                    start: open,
                },
                value: Value::Block(block()),
            });
        } else if let Some(entry) = self.entries.last_mut()
            // The entry in progress still has a scalar value (the key, or
            // the value that becomes the tag); matching on it lets the
            // compiler skip the drop glue of the old value.
            && let Value::Scalar(_) = entry.value
        {
            match state {
                State::Scalar(key) => {
                    entry.key = Some(key);
                    entry.value = Value::Block(block());
                }
                State::Operator | State::Start => entry.value = Value::Block(block()),
                State::Value(tag) => {
                    entry.value = Value::Tagged {
                        block: block(),
                        tag,
                    };
                }
            }
        }
        self.frames.push(Frame {
            comments: self.comments.len(),
            entries: self.entries.len(),
            open,
        });
        Ok(())
    }

    /// Parses the whole input, one token at a time. Comments go onto
    /// `comments` as the lexer passes them, which gives each to the block
    /// innermost at that point (a `{` only opens its block after them).
    fn run(mut self) -> Result<Block, SyntaxError> {
        let mut state = State::Start;
        loop {
            let Some(token) = self.lexer.next_token(&mut self.comments)? else {
                return self.finish(state);
            };
            state = match (state, token) {
                (State::Scalar(key), Token::Op(op)) => {
                    if let Some(entry) = self.entries.last_mut() {
                        entry.key = Some(key);
                        entry.op = Some(op);
                    }
                    State::Operator
                }
                (State::Operator, Token::Scalar(value)) => {
                    if let Some(entry) = self.entries.last_mut() {
                        entry.span.end = value.span.end;
                        // The value is still the key; see `State::Operator`.
                        if let Value::Scalar(scalar) = &mut entry.value {
                            *scalar = value;
                        }
                    }
                    if value.quoted {
                        State::Start
                    } else {
                        State::Value(value)
                    }
                }
                (State::Operator, Token::Close(close)) => {
                    return Err(SyntaxError::new(
                        close,
                        "expected a value after operator, found `}`",
                    ));
                }
                (State::Operator, Token::Op(other)) => {
                    return Err(SyntaxError::new(
                        other.span.start,
                        "expected a value after operator, found another operator",
                    ));
                }
                (State::Start | State::Value(_), Token::Op(op)) => {
                    return Err(SyntaxError::new(op.span.start, "operator without a key"));
                }
                (_, Token::Open(open)) => {
                    self.open(open, state)?;
                    State::Start
                }
                // Otherwise the entry in progress, if any, is complete.
                (_, Token::Close(close)) => {
                    self.close(close)?;
                    State::Start
                }
                (_, Token::Scalar(scalar)) => {
                    self.entries.push(Entry {
                        key: None,
                        op: None,
                        span: scalar.span,
                        value: Value::Scalar(scalar),
                    });
                    State::Scalar(scalar)
                }
            };
        }
    }
}

/// An open non-root block.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Length of [`Parser::comments`] when the block opened: its comments
    /// are the ones pushed since, less those of blocks nested inside it,
    /// which take theirs when they close.
    comments: usize,
    /// Length of [`Parser::entries`] when the block opened, its own entry
    /// included; see `comments`.
    entries: usize,
    /// Byte offset of `{`.
    open: usize,
}

/// Byte-oriented tokenizer. All delimiters are ASCII, so every offset it
/// produces for a delimiter lies on a char boundary; non-ASCII chars are only
/// decoded when their lead byte may start whitespace.
struct Lexer<'src> {
    bytes: &'src [u8],
    pos: usize,
    src: &'src str,
}

impl<'src> Lexer<'src> {
    /// End (exclusive) of the bare scalar that continues at `pos`: it runs
    /// until whitespace, a brace, `=`, `<`, `>`, `#`, or a `!` / `?` followed
    /// by `=`.
    ///
    /// A `"` only opens a quoted scalar at the start of a token (see
    /// `next_token`); inside a bare scalar it is an ordinary char. This
    /// matches jomini and the game: vanilla `FIN_names_divisions.txt` has
    /// lines like `13 = { Lahden suojeluskuntapiiri" } #Lahti"`, which would
    /// otherwise pair stray quotes across lines and swallow the structure.
    fn bare_end(&self, mut pos: usize) -> usize {
        loop {
            // Most bytes are `Bare`: skip them with one compare each before
            // telling the others apart.
            let rest = self.bytes.get(pos..).unwrap_or_default();
            pos += rest
                .iter()
                .position(|&byte| byte_class(byte) != ByteClass::Bare)
                .unwrap_or(rest.len());
            let Some(&byte) = self.bytes.get(pos) else {
                return pos;
            };
            let ends = match byte_class(byte) {
                ByteClass::Bare => false,
                ByteClass::Delimiter | ByteClass::Space => true,
                ByteClass::MaybeOperator => self.bytes.get(pos + 1) == Some(&b'='),
                ByteClass::MaybeSpace => self.unicode_space_len(pos).is_some(),
            };
            if ends {
                return pos;
            }
            pos += 1;
        }
    }

    /// End (exclusive) of the `@[ ... ]` inline-math expression at `start`:
    /// just past the `]` matching the opening `[`.
    fn inline_math_end(&self, start: usize) -> Result<usize, SyntaxError> {
        let mut depth: usize = 0;
        let rest = self.bytes.get(start + 1..).unwrap_or_default();
        for (index, byte) in rest.iter().enumerate() {
            match byte {
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(start + 1 + index + 1);
                    }
                }
                _ => {}
            }
        }
        Err(SyntaxError::new(start, "unterminated `@[` inline math"))
    }

    fn new(src: &'src str) -> Self {
        Self {
            bytes: src.as_bytes(),
            pos: bom_len(src),
            src,
        }
    }

    /// The next significant token, or `None` at the end of the input.
    /// Comments are pushed onto `comments`.
    ///
    /// Always inlined into [`Parser::run`], its one caller, so no token
    /// makes a round trip through memory; for the same reason the helpers
    /// it calls return offsets rather than tokens.
    #[expect(
        clippy::inline_always,
        reason = "the compiler does not always inline it; measured 1-12% of the parse"
    )]
    #[inline(always)]
    fn next_token(&mut self, comments: &mut Vec<Span>) -> Result<Option<Token>, SyntaxError> {
        loop {
            let start = self.skip_whitespace(self.pos);
            let Some(&byte) = self.bytes.get(start) else {
                self.pos = start;
                return Ok(None);
            };
            let next = self.bytes.get(start + 1).copied();
            let (kind, len) = match (byte, next) {
                (b'#', _) => {
                    let end = line_end(self.src, start);
                    comments.push(Span { end, start });
                    self.pos = end;
                    continue;
                }
                (b'{', _) => {
                    self.pos = start + 1;
                    return Ok(Some(Token::Open(start)));
                }
                (b'}', _) => {
                    self.pos = start + 1;
                    return Ok(Some(Token::Close(start)));
                }
                (b'"', _) => {
                    let Some(end) = self.quoted_end(start) else {
                        return Err(SyntaxError::new(start, "unterminated quoted string"));
                    };
                    self.pos = end;
                    return Ok(Some(Token::Scalar(Scalar {
                        quoted: true,
                        span: Span { end, start },
                    })));
                }
                (b'=', Some(b'=')) => (OpKind::EqEq, 2),
                (b'=', _) => (OpKind::Eq, 1),
                (b'<', Some(b'=')) => (OpKind::Le, 2),
                (b'<', _) => (OpKind::Lt, 1),
                (b'>', Some(b'=')) => (OpKind::Ge, 2),
                (b'>', _) => (OpKind::Gt, 1),
                (b'!', Some(b'=')) => (OpKind::NotEq, 2),
                (b'?', Some(b'=')) => (OpKind::Exists, 2),
                (b'[', Some(b'[')) => {
                    return Err(SyntaxError::new(
                        start,
                        "unsupported `[[` conditional parameter block",
                    ));
                }
                _ => {
                    let from = if (byte, next) == (b'@', Some(b'[')) {
                        self.inline_math_end(start)?
                    } else {
                        start
                    };
                    let end = self.bare_end(from);
                    if end == start {
                        // Unreachable given the arms above; kept so a future
                        // change there cannot cause an infinite loop.
                        return Err(SyntaxError::new(start, "unexpected character"));
                    }
                    self.pos = end;
                    return Ok(Some(Token::Scalar(Scalar {
                        quoted: false,
                        span: Span { end, start },
                    })));
                }
            };
            self.pos = start + len;
            return Ok(Some(Token::Op(Op {
                kind,
                span: Span {
                    end: start + len,
                    start,
                },
            })));
        }
    }

    /// End (exclusive) of the quoted scalar whose opening `"` is at `start`,
    /// or `None` if it is unterminated. A backslash escapes the next byte;
    /// the string may span lines.
    fn quoted_end(&self, start: usize) -> Option<usize> {
        let mut pos = start + 1;
        loop {
            let rest = self.bytes.get(pos..)?;
            let offset = rest.iter().position(|&b| b == b'"' || b == b'\\')?;
            let at = pos + offset;
            if rest.get(offset) == Some(&b'"') {
                return Some(at + 1);
            }
            // A backslash: skip it and the byte it escapes. Skipping a single
            // byte of a multi-byte char is harmless because only the ASCII
            // bytes `"` and `\` are ever compared.
            pos = at + 2;
        }
    }

    /// The first offset at or after `pos` that is not whitespace.
    fn skip_whitespace(&self, mut pos: usize) -> usize {
        loop {
            let rest = self.bytes.get(pos..).unwrap_or_default();
            pos += rest
                .iter()
                .position(|&byte| byte_class(byte) != ByteClass::Space)
                .unwrap_or(rest.len());
            match self.bytes.get(pos) {
                Some(&byte) if byte_class(byte) == ByteClass::MaybeSpace => {
                    match self.unicode_space_len(pos) {
                        Some(len) => pos += len,
                        None => return pos,
                    }
                }
                _ => return pos,
            }
        }
    }

    /// Length of the char at `pos` if it is whitespace.
    fn unicode_space_len(&self, pos: usize) -> Option<usize> {
        let ch = self.src.get(pos..)?.chars().next()?;
        ch.is_whitespace().then(|| ch.len_utf8())
    }
}

/// How far the parser is into the entry on top of its stack. The scalars
/// ride along so completing the entry never reads back what was just
/// stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// `key op`, with no value yet; the entry's value is still the key.
    Operator,
    /// A bare scalar: a bare element, or the key of the entry if an
    /// operator or `{` follows.
    Scalar(Scalar),
    /// Between entries: the next token starts one.
    Start,
    /// `key op value` with an unquoted `value`, which is the tag of a tagged
    /// block if `{` follows.
    Value(Scalar),
}

/// A [`ParseError`] before its message is copied into a `String`: cheap to
/// pass through the lexer's and parser's `Result`s.
#[derive(Debug, Clone, Copy)]
struct SyntaxError {
    message: &'static str,
    offset: usize,
}

impl SyntaxError {
    const fn new(offset: usize, message: &'static str) -> Self {
        Self { message, offset }
    }
}

/// A significant (non-comment) token.
#[derive(Debug, Clone, Copy)]
enum Token {
    /// `}` at this offset.
    Close(usize),
    Op(Op),
    /// `{` at this offset.
    Open(usize),
    Scalar(Scalar),
}

/// Applies non-overlapping `edits` (in any input order) to `src`.
///
/// Edits may touch (`a.end == b.start`). Zero-width inserts at the same offset
/// are applied in input order, and an insert at the start offset of a
/// replacement is applied before it. Returns `None` if any two edits overlap,
/// or if an edit's span is inverted, out of range, or not on char boundaries.
#[must_use]
pub fn apply_edits(src: &str, mut edits: Vec<Edit>) -> Option<String> {
    // Stable sort: equal (start, end) keys — i.e. zero-width inserts at one
    // offset — keep their input order.
    edits.sort_by_key(|edit| (edit.span.start, edit.span.end));
    let added: usize = edits.iter().map(|edit| edit.replacement.len()).sum();
    let mut out = String::with_capacity(src.len() + added);
    let mut cursor = 0;
    for edit in &edits {
        if edit.span.start < cursor || edit.span.end < edit.span.start {
            return None;
        }
        out.push_str(src.get(cursor..edit.span.start)?);
        // Validates that the replaced range is in bounds and on boundaries.
        src.get(edit.span.start..edit.span.end)?;
        out.push_str(&edit.replacement);
        cursor = edit.span.end;
    }
    out.push_str(src.get(cursor..)?);
    Some(out)
}

/// `{` ..= `}` of a non-root block (an empty span at 0 for the root, which
/// never occurs as a value).
fn block_span(block: &Block) -> Span {
    block.span().unwrap_or(Span { end: 0, start: 0 })
}

/// Byte length of a leading BOM (0 if there is none).
fn bom_len(src: &str) -> usize {
    if src.starts_with(BOM) {
        BOM.len_utf8()
    } else {
        0
    }
}

/// The lexer's class of `byte`.
fn byte_class(byte: u8) -> ByteClass {
    // Always in range: the table has an entry for every `u8`.
    BYTE_CLASSES
        .get(usize::from(byte))
        .copied()
        .unwrap_or(ByteClass::Bare)
}

/// Whether a comment of `block` starts at `offset`.
fn is_comment_start(block: &Block, offset: usize) -> bool {
    block
        .comments
        .binary_search_by_key(&offset, |span| span.start)
        .is_ok()
}

/// Where the line holding `pos` ends: the offset of the next `\n` at or after
/// `pos`, or `src.len()` on the last line. When the line ends in `\r\n` the
/// offset of the `\r` is returned instead, so the result is always where the
/// line terminator starts.
///
/// Both bytes of a terminator belong to the line they end (as for
/// [`line_start`]), so `line_start(src, p)..line_end(src, p)` is the text of
/// `p`'s line without its terminator for every `p`. When `pos` is the `\n` of
/// a `\r\n` the result is therefore `pos - 1`. A lone `\r` is whitespace, not
/// a line terminator.
#[must_use]
pub fn line_end(src: &str, pos: usize) -> usize {
    let bytes = src.as_bytes();
    let pos = pos.min(bytes.len());
    let Some(newline) = bytes
        .get(pos..)
        .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
        .map(|offset| pos + offset)
    else {
        return bytes.len();
    };
    newline
        .checked_sub(1)
        .filter(|&before| bytes.get(before) == Some(&b'\r'))
        .unwrap_or(newline)
}

/// Offset just after the previous `\n` before `pos` (0 on the first line). A
/// `\n` at `pos` belongs to the line it ends, so it is not "previous".
#[must_use]
pub fn line_start(src: &str, pos: usize) -> usize {
    let bytes = src.as_bytes();
    bytes
        .get(..pos.min(bytes.len()))
        .and_then(|before| before.iter().rposition(|&b| b == b'\n'))
        .map_or(0, |newline| newline + 1)
}

/// Splits a block whose entries each occupy their own line(s) into line
/// units, one per entry, in entry order and never overlapping.
///
/// Returns `None` unless EVERY entry of `block` occupies its own line(s):
/// only whitespace precedes the entry on its first line (so the `{` line and
/// the `}` line hold no entries), and only whitespace plus at most one `#`
/// comment follows the entry's end on its last line.
///
/// Lines end at `\n` or `\r\n` only; a lone `\r` is whitespace. Editors draw a
/// lone `\r` as a line break, though, so a comment behind one (`}\r# header`)
/// is on the entry's line here but looks like a header of the next line.
/// Rather than move or reflow such a comment against what the reader sees,
/// `line_units` returns `None` when a lone `\r` separates an entry from its
/// trailing comment. Lone `\r`s anywhere else are ordinary whitespace.
///
/// A unit also owns its attached leading comments: comment-only lines (first
/// non-whitespace char is the `#` of one of `block`'s comments) immediately
/// above the entry's line with no blank line in between. Blank lines and
/// non-attached (dangling) comment lines belong to no unit. A leading BOM
/// belongs to no unit either: on the first line of a file with a BOM, `start`
/// and `entry_line_start` point just past it.
#[must_use]
pub fn line_units(src: &str, block: &Block) -> Option<Vec<Unit>> {
    let bytes = src.as_bytes();
    let bom = bom_len(src);
    let mut units = Vec::with_capacity(block.entries.len());
    // Attached comment lines may not start before this: the end of the
    // previous unit, or (for the first entry) just past the `{`.
    let mut floor = block.open.map_or(bom, |open| open + 1);
    for (index, entry) in block.entries.iter().enumerate() {
        let entry_line_start = line_start(src, entry.span.start).max(bom);
        if entry_line_start < floor
            || !src
                .get(entry_line_start..entry.span.start)?
                .chars()
                .all(char::is_whitespace)
        {
            return None;
        }

        let last_line_end = line_end(src, entry.span.end);
        let tail = src.get(entry.span.end..last_line_end)?;
        let comment = tail.trim_start();
        if !comment.is_empty()
            && (!is_comment_start(block, last_line_end - comment.len())
                || tail.get(..tail.len() - comment.len())?.contains('\r'))
        {
            return None;
        }
        let end = match bytes.get(last_line_end) {
            Some(b'\r') => last_line_end + 2,
            Some(b'\n') => last_line_end + 1,
            _ => last_line_end,
        };

        let mut start = entry_line_start;
        while start > floor {
            let newline = start - 1;
            let previous_start = line_start(src, newline).max(bom);
            if previous_start < floor {
                break;
            }
            let line = src.get(previous_start..newline)?;
            let text = line.trim_start();
            if !text.starts_with('#')
                || !is_comment_start(block, previous_start + line.len() - text.len())
            {
                break;
            }
            start = previous_start;
        }

        units.push(Unit {
            end,
            entry: index,
            entry_line_start,
            start,
        });
        floor = end;
    }
    Some(units)
}

/// Parses Clausewitz script into a lossless [`Document`].
///
/// # Errors
///
/// Returns a [`ParseError`] (never panics) for: an unmatched `}` (including
/// at the root), end of file inside a block, an operator with no key at the
/// start of an entry, an operator followed by `}` / end of file / another
/// operator, an unterminated quoted string, a `[[` conditional parameter
/// block, an unterminated `@[` inline-math expression, or blocks nested more
/// than 128 deep ([`MAX_DEPTH`]; input nested exactly 128 deep parses).
pub fn parse(src: &str) -> Result<Document<'_>, ParseError> {
    let _span = tracing::info_span!("parse").entered();
    let parser = Parser {
        comments: Vec::new(),
        entries: Vec::new(),
        frames: Vec::new(),
        lexer: Lexer::new(src),
    };
    let root = parser
        .run()
        .map_err(|err| ParseError::new(err.offset, err.message))?;
    Ok(Document { root, src })
}

/// Pre-order visit of every entry whose value is a block or tagged block,
/// passing the key path from the root to (and including) this entry's key.
/// Bare (keyless) entries contribute `""` to the path.
pub fn visit_blocks<'doc>(
    doc: &'doc Document<'_>,
    f: &mut dyn FnMut(&[&'doc str], &'doc Entry, &'doc Block),
) {
    let mut path: Vec<&'doc str> = Vec::new();
    let mut stack = vec![doc.root.entries.iter()];
    while let Some(iter) = stack.last_mut() {
        let Some(entry) = iter.next() else {
            stack.pop();
            path.pop();
            continue;
        };
        if let Some(block) = entry.value.as_block() {
            path.push(entry.key_str(doc.src).unwrap_or_default());
            f(&path, entry, block);
            stack.push(block.entries.iter());
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    /// The parser as it was before its rewrite for speed, kept verbatim
    /// (only `parse` differs: it returns the root block) so the tests below
    /// can hold the rewrite to it: the same tree, or the same error at the
    /// same offset, for every input.
    mod reference {
        use crate::cst::{
            Block, Entry, MAX_DEPTH, Op, OpKind, ParseError, Scalar, Span, Value, bom_len, line_end,
        };

        /// A lexer + stack-based parser. `current` is the innermost open block;
        /// `parents` holds each enclosing block together with how `current` attaches
        /// to it once closed, so the parse never recurses.
        struct Parser<'src> {
            current: Block,
            lexer: Lexer<'src>,
            parents: Vec<(Block, Pending)>,
            /// One significant token of lookahead.
            peeked: Option<Token>,
        }

        impl Parser<'_> {
            /// Parses the value of `key op`.
            fn after_op(&mut self, key: Scalar, op: Op) -> Result<(), ParseError> {
                let pending = |tag| Pending {
                    key: Some(key),
                    op: Some(op),
                    start: key.span.start,
                    tag,
                };
                match self.next()? {
                    Token::Scalar(value) => {
                        if !value.quoted
                            && let Token::Open(open) = self.peek()?
                        {
                            self.peeked = None;
                            return self.open(open, pending(Some(value)));
                        }
                        self.current.entries.push(Entry {
                            key: Some(key),
                            op: Some(op),
                            span: Span {
                                end: value.span.end,
                                start: key.span.start,
                            },
                            value: Value::Scalar(value),
                        });
                        Ok(())
                    }
                    Token::Open(open) => self.open(open, pending(None)),
                    Token::Close(close) => Err(ParseError::new(
                        close,
                        "expected a value after operator, found `}`",
                    )),
                    Token::Eof(eof) => Err(ParseError::new(
                        eof,
                        "expected a value after operator, found end of file",
                    )),
                    Token::Op(other) => Err(ParseError::new(
                        other.span.start,
                        "expected a value after operator, found another operator",
                    )),
                }
            }

            /// Handles an entry that started with scalar `key`: `key op value`,
            /// `key op tag { .. }`, `key { .. }`, or a bare scalar element.
            fn after_scalar(&mut self, key: Scalar) -> Result<(), ParseError> {
                match self.peek()? {
                    Token::Op(op) => {
                        self.peeked = None;
                        self.after_op(key, op)
                    }
                    Token::Open(open) => {
                        self.peeked = None;
                        self.open(
                            open,
                            Pending {
                                key: Some(key),
                                op: None,
                                start: key.span.start,
                                tag: None,
                            },
                        )
                    }
                    Token::Close(_) | Token::Eof(_) | Token::Scalar(_) => {
                        self.current.entries.push(Entry {
                            key: None,
                            op: None,
                            span: key.span,
                            value: Value::Scalar(key),
                        });
                        Ok(())
                    }
                }
            }

            /// Closes `current` at `close` and appends it to its parent as an entry.
            fn close(&mut self, close: usize) -> Result<(), ParseError> {
                let Some((parent, pending)) = self.parents.pop() else {
                    return Err(ParseError::new(close, "unmatched `}`"));
                };
                let mut block = std::mem::replace(&mut self.current, parent);
                block.close = Some(close);
                let value = match pending.tag {
                    Some(tag) => Value::Tagged { block, tag },
                    None => Value::Block(block),
                };
                self.current.entries.push(Entry {
                    key: pending.key,
                    op: pending.op,
                    span: Span {
                        end: close + 1,
                        start: pending.start,
                    },
                    value,
                });
                Ok(())
            }

            /// The next significant token; comments on the way are recorded in the
            /// innermost open block.
            fn next(&mut self) -> Result<Token, ParseError> {
                match self.peeked.take() {
                    Some(token) => Ok(token),
                    None => self.lexer.next_token(&mut self.current.comments),
                }
            }

            /// Opens a nested block at `open`; `pending` says how it attaches to the
            /// block that is current now.
            fn open(&mut self, open: usize, pending: Pending) -> Result<(), ParseError> {
                if self.parents.len() >= MAX_DEPTH {
                    return Err(ParseError::new(open, "blocks nested too deeply"));
                }
                let child = Block {
                    open: Some(open),
                    ..Block::default()
                };
                let parent = std::mem::replace(&mut self.current, child);
                self.parents.push((parent, pending));
                Ok(())
            }

            /// Looks at the next significant token without consuming it. Comments
            /// skipped while peeking belong to the current block whatever the token
            /// turns out to be (a `{` only opens a new block after them).
            fn peek(&mut self) -> Result<Token, ParseError> {
                let token = self.next()?;
                self.peeked = Some(token);
                Ok(token)
            }

            fn run(mut self) -> Result<Block, ParseError> {
                loop {
                    match self.next()? {
                        Token::Eof(eof) => {
                            if self.parents.is_empty() {
                                return Ok(self.current);
                            }
                            // Report the innermost unclosed `{`.
                            let open = self.current.open.unwrap_or(eof);
                            return Err(ParseError::new(open, "unclosed `{` at end of file"));
                        }
                        Token::Close(close) => self.close(close)?,
                        Token::Open(open) => self.open(
                            open,
                            Pending {
                                key: None,
                                op: None,
                                start: open,
                                tag: None,
                            },
                        )?,
                        Token::Op(op) => {
                            return Err(ParseError::new(op.span.start, "operator without a key"));
                        }
                        Token::Scalar(key) => self.after_scalar(key)?,
                    }
                }
            }
        }

        /// How a block that is still open will become an entry of its parent.
        #[derive(Debug, Clone, Copy)]
        struct Pending {
            key: Option<Scalar>,
            op: Option<Op>,
            /// `Entry::span.start` of the entry being built.
            start: usize,
            tag: Option<Scalar>,
        }

        /// Byte-oriented tokenizer. All delimiters are ASCII, so every offset it
        /// produces for a delimiter lies on a char boundary; non-ASCII chars are only
        /// decoded to test them for whitespace.
        struct Lexer<'src> {
            bytes: &'src [u8],
            pos: usize,
            src: &'src str,
        }

        impl<'src> Lexer<'src> {
            /// Lexes the bare scalar starting at `start`.
            fn bare(&mut self, start: usize) -> Result<Token, ParseError> {
                let mut pos = start;
                match self.bytes.get(start..start + 2) {
                    Some(b"[[") => {
                        return Err(ParseError::new(
                            start,
                            "unsupported `[[` conditional parameter block",
                        ));
                    }
                    Some(b"@[") => pos = self.inline_math_end(start)?,
                    _ => {}
                }
                while let Some(len) = self.bare_len(pos) {
                    pos += len;
                }
                if pos == start {
                    // Unreachable given `next_token`'s dispatch; kept so a future
                    // change there cannot cause an infinite loop.
                    return Err(ParseError::new(start, "unexpected character"));
                }
                self.pos = pos;
                Ok(Token::Scalar(Scalar {
                    quoted: false,
                    span: Span { end: pos, start },
                }))
            }

            /// Length of the char at `pos` if it may continue a bare scalar.
            ///
            /// A `"` only opens a quoted scalar at the start of a token (see
            /// `next_token`); inside a bare scalar it is an ordinary char. This
            /// matches jomini and the game: vanilla `FIN_names_divisions.txt` has
            /// lines like `13 = { Lahden suojeluskuntapiiri" } #Lahti"`, which would
            /// otherwise pair stray quotes across lines and swallow the structure.
            fn bare_len(&self, pos: usize) -> Option<usize> {
                match *self.bytes.get(pos)? {
                    b'{' | b'}' | b'=' | b'<' | b'>' | b'#' => None,
                    b'!' | b'?' if self.bytes.get(pos + 1) == Some(&b'=') => None,
                    byte if byte.is_ascii() => (!char::from(byte).is_whitespace()).then_some(1),
                    _ => {
                        let ch = self.src.get(pos..)?.chars().next()?;
                        (!ch.is_whitespace()).then(|| ch.len_utf8())
                    }
                }
            }

            /// End (exclusive) of the `@[ ... ]` inline-math expression at `start`:
            /// just past the `]` matching the opening `[`.
            fn inline_math_end(&self, start: usize) -> Result<usize, ParseError> {
                let mut depth: usize = 0;
                let rest = self.bytes.get(start + 1..).unwrap_or_default();
                for (index, byte) in rest.iter().enumerate() {
                    match byte {
                        b'[' => depth += 1,
                        b']' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                return Ok(start + 1 + index + 1);
                            }
                        }
                        _ => {}
                    }
                }
                Err(ParseError::new(start, "unterminated `@[` inline math"))
            }

            fn new(src: &'src str) -> Self {
                Self {
                    bytes: src.as_bytes(),
                    pos: bom_len(src),
                    src,
                }
            }

            /// The next significant token. Comments are pushed onto `comments`.
            fn next_token(&mut self, comments: &mut Vec<Span>) -> Result<Token, ParseError> {
                loop {
                    while let Some(len) = self.whitespace_len(self.pos) {
                        self.pos += len;
                    }
                    let start = self.pos;
                    let Some(&byte) = self.bytes.get(start) else {
                        return Ok(Token::Eof(start));
                    };
                    let next = self.bytes.get(start + 1).copied();
                    let (kind, len) = match (byte, next) {
                        (b'#', _) => {
                            let end = line_end(self.src, start);
                            comments.push(Span { end, start });
                            self.pos = end;
                            continue;
                        }
                        (b'{', _) => {
                            self.pos = start + 1;
                            return Ok(Token::Open(start));
                        }
                        (b'}', _) => {
                            self.pos = start + 1;
                            return Ok(Token::Close(start));
                        }
                        (b'"', _) => return self.quoted(start),
                        (b'=', Some(b'=')) => (OpKind::EqEq, 2),
                        (b'=', _) => (OpKind::Eq, 1),
                        (b'<', Some(b'=')) => (OpKind::Le, 2),
                        (b'<', _) => (OpKind::Lt, 1),
                        (b'>', Some(b'=')) => (OpKind::Ge, 2),
                        (b'>', _) => (OpKind::Gt, 1),
                        (b'!', Some(b'=')) => (OpKind::NotEq, 2),
                        (b'?', Some(b'=')) => (OpKind::Exists, 2),
                        _ => return self.bare(start),
                    };
                    self.pos = start + len;
                    return Ok(Token::Op(Op {
                        kind,
                        span: Span {
                            end: start + len,
                            start,
                        },
                    }));
                }
            }

            /// Lexes the quoted scalar whose opening `"` is at `start`. A backslash
            /// escapes the next byte; the string may span lines.
            fn quoted(&mut self, start: usize) -> Result<Token, ParseError> {
                let mut pos = start + 1;
                loop {
                    let rest = self.bytes.get(pos..).unwrap_or_default();
                    let Some(offset) = rest.iter().position(|&b| b == b'"' || b == b'\\') else {
                        return Err(ParseError::new(start, "unterminated quoted string"));
                    };
                    let at = pos + offset;
                    if rest.get(offset) == Some(&b'"') {
                        self.pos = at + 1;
                        return Ok(Token::Scalar(Scalar {
                            quoted: true,
                            span: Span { end: at + 1, start },
                        }));
                    }
                    // A backslash: skip it and the byte it escapes. Skipping a single
                    // byte of a multi-byte char is harmless because only the ASCII
                    // bytes `"` and `\` are ever compared.
                    pos = at + 2;
                }
            }

            /// Length of the whitespace char at `pos`, if it is one.
            fn whitespace_len(&self, pos: usize) -> Option<usize> {
                let byte = *self.bytes.get(pos)?;
                if byte.is_ascii() {
                    char::from(byte).is_whitespace().then_some(1)
                } else {
                    let ch = self.src.get(pos..)?.chars().next()?;
                    ch.is_whitespace().then(|| ch.len_utf8())
                }
            }
        }

        /// A significant (non-comment) token.
        #[derive(Debug, Clone, Copy)]
        enum Token {
            /// `}` at this offset.
            Close(usize),
            /// End of input at this offset.
            Eof(usize),
            Op(Op),
            /// `{` at this offset.
            Open(usize),
            Scalar(Scalar),
        }

        pub fn parse(src: &str) -> Result<Block, ParseError> {
            let parser = Parser {
                current: Block::default(),
                lexer: Lexer::new(src),
                parents: Vec::new(),
                peeked: None,
            };
            parser.run()
        }
    }

    use super::{
        BOM, Block, ByteClass, Document, Edit, MAX_DEPTH, OpKind, ParseError, Scalar, Span, Unit,
        Value, apply_edits, byte_class, line_end, line_start, line_units, parse, visit_blocks,
    };
    use rayon::prelude::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// jomini ends bare scalars at `[` and `]` (and rejects a `[` that
    /// starts a key), so it splits scalars the game reads as one token:
    /// scripted-localisation values (`picture = [GetHitlerHandshakeEventPicture]`,
    /// `FLAG_DAYS = [?flag:days]`) and `meta_effect` templates
    /// (`set_leader_[TAG] = yes`). Quoting exactly those scalars hands
    /// jomini the same tokens without changing the CST's structure, so the
    /// rest of the file can still be compared.
    const BRACKETED: &str = "jomini splits bare scalars at `[`/`]`; clean once they are quoted";

    /// Pieces the fuzzing inputs are made of: every token kind and operator,
    /// the error triggers (`[[`, `@[`, stray quotes and braces), escapes,
    /// line terminators, and ASCII and non-ASCII whitespace and non-whitespace
    /// (including chars whose lead byte makes the lexer decode them: U+00B0,
    /// U+3001, and U+E000 behind a lead byte it never decodes).
    const FUZZ_PIECES: [&str; 44] = [
        "{", "}", "=", "==", "!=", "?=", "<", "<=", ">", ">=", "!", "?", "a", "rgb", "1", "\"",
        "\\", "#", "# c", "\n", "\r\n", "\r", " ", "\t", "\u{b}", "\u{c}", "@[", "[", "]", "[[",
        ";", "é", "\u{a0}", "\u{85}", "\u{1680}", "\u{2009}", "\u{2028}", "\u{3000}", "\u{b0}",
        "\u{3001}", "\u{e000}", "\u{feff}", "$P$", "x.y",
    ];

    /// The first structural difference between two sibling lists.
    #[derive(Debug)]
    struct Difference {
        category: &'static str,
        detail: String,
        offset: Option<usize>,
        path: String,
    }

    #[derive(Debug)]
    enum Outcome {
        Clean,
        /// The CST parsed, jomini did not.
        JominiFailed(String),
        /// Every difference is explained by a documented jomini quirk.
        KnownDivergence(&'static str),
        Lossless(String),
        /// An unexplained difference, with its `line: text` location.
        Mismatch(Difference, String),
        NotUtf8,
        ParseFailed {
            jomini_ok: bool,
            location: String,
            message: String,
        },
    }

    /// Structural view of an entry, comparable between the CST and jomini.
    #[derive(Debug)]
    struct Shape {
        key: Option<String>,
        /// Byte offset of the entry (CST side only; ignored when comparing).
        offset: Option<usize>,
        /// `None` for bare elements; `=` for the operator-less `key { }`.
        op: Option<&'static str>,
        value: ShapeValue,
    }

    #[derive(Debug)]
    enum ShapeValue {
        Block(Vec<Shape>),
        Scalar(String),
        Tagged(String, Vec<Shape>),
    }

    impl ShapeValue {
        const fn kind(&self) -> &'static str {
            match self {
                Self::Block(_) => "block",
                Self::Scalar(_) => "scalar",
                Self::Tagged(..) => "tagged block",
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum TokenKind {
        Close,
        Comment,
        Op(OpKind),
        Open,
        Scalar(Scalar),
    }

    /// A small xorshift generator: deterministic, so failures reproduce.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13_u32;
            self.0 ^= self.0 >> 7_u32;
            self.0 ^= self.0 << 17_u32;
            usize::try_from(self.0 % u64::try_from(bound.max(1)).expect("small")).expect("fits")
        }

        /// A char boundary of `text`, uniformly among its bytes' positions.
        fn boundary(&mut self, text: &str) -> usize {
            text.floor_char_boundary(self.below(text.len() + 1))
        }

        fn piece(&mut self) -> &'static str {
            FUZZ_PIECES
                .get(self.below(FUZZ_PIECES.len()))
                .expect("in range")
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// Parses `src`, panicking on error or on any invariant violation.
    fn parse_ok(src: &str) -> Document<'_> {
        let doc = parse(src).unwrap_or_else(|err| panic!("{src:?}: {err}"));
        if let Err(violation) = check_invariants(&doc) {
            panic!("{src:?}: {violation}");
        }
        doc
    }

    fn parse_err(src: &str) -> ParseError {
        match parse(src) {
            Ok(doc) => panic!("{src:?} parsed unexpectedly: {:?}", doc.root),
            Err(err) => err,
        }
    }

    /// Compact rendering of the tree: `key op value`, `key{..}` for the
    /// operator-less form, `{..}` for bare blocks and `tag!{..}` for tagged
    /// values. Scalars are rendered raw (quotes included).
    fn dump(doc: &Document<'_>) -> String {
        let mut out = String::new();
        dump_block(doc.src, &doc.root, &mut out);
        out
    }

    fn dump_block(src: &str, block: &Block, out: &mut String) {
        for (index, entry) in block.entries.iter().enumerate() {
            if index > 0 {
                out.push(' ');
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
                    dump_block(src, inner, out);
                    out.push('}');
                }
                Value::Tagged { block: inner, tag } => {
                    out.push_str(tag.text(src));
                    out.push_str("!{");
                    dump_block(src, inner, out);
                    out.push('}');
                }
            }
        }
    }

    fn only_block<'doc>(doc: &'doc Document<'_>) -> &'doc Block {
        let [entry] = doc.root.entries.as_slice() else {
            panic!("expected exactly one root entry: {:?}", doc.root);
        };
        entry.value.as_block().expect("root entry is a block")
    }

    fn comment_texts<'src>(src: &'src str, block: &Block) -> Vec<&'src str> {
        block.comments.iter().map(|span| span.text(src)).collect()
    }

    fn unit_texts<'src>(src: &'src str, units: &[Unit]) -> Vec<&'src str> {
        units
            .iter()
            .map(|unit| src.get(unit.start..unit.end).expect("unit on boundaries"))
            .collect()
    }

    fn collect_tokens(block: &Block, out: &mut Vec<(Span, TokenKind)>) {
        if let Some(open) = block.open {
            out.push((
                Span {
                    end: open + 1,
                    start: open,
                },
                TokenKind::Open,
            ));
        }
        if let Some(close) = block.close {
            out.push((
                Span {
                    end: close + 1,
                    start: close,
                },
                TokenKind::Close,
            ));
        }
        out.extend(
            block
                .comments
                .iter()
                .map(|&span| (span, TokenKind::Comment)),
        );
        for entry in &block.entries {
            if let Some(key) = entry.key {
                out.push((key.span, TokenKind::Scalar(key)));
            }
            if let Some(op) = entry.op {
                out.push((op.span, TokenKind::Op(op.kind)));
            }
            match &entry.value {
                Value::Scalar(scalar) => out.push((scalar.span, TokenKind::Scalar(*scalar))),
                Value::Block(inner) => collect_tokens(inner, out),
                Value::Tagged { block: inner, tag } => {
                    out.push((tag.span, TokenKind::Scalar(*tag)));
                    collect_tokens(inner, out);
                }
            }
        }
    }

    /// Invariant 1 (losslessness): every byte is inside exactly one token
    /// span or is whitespace / the leading BOM, each token's text is what
    /// its kind says, and gaps + tokens rebuild the source exactly.
    fn check_lossless(doc: &Document<'_>) -> Result<(), String> {
        let src = doc.src;
        let mut tokens = Vec::new();
        collect_tokens(&doc.root, &mut tokens);
        tokens.sort_by_key(|(span, _)| (span.start, span.end));
        let mut rebuilt = String::with_capacity(src.len());
        let mut cursor = 0;
        for (span, kind) in &tokens {
            if span.start < cursor {
                return Err(format!(
                    "{kind:?} at {span:?} overlaps the previous token (ends at {cursor})"
                ));
            }
            let gap = src
                .get(cursor..span.start)
                .ok_or_else(|| format!("gap {cursor}..{} not on char boundaries", span.start))?;
            check_gap(gap, cursor)?;
            let text = src
                .get(span.start..span.end)
                .ok_or_else(|| format!("{kind:?} at {span:?} not on char boundaries"))?;
            check_token_text(src, *span, *kind, text)?;
            rebuilt.push_str(gap);
            rebuilt.push_str(text);
            cursor = span.end;
        }
        let tail = src
            .get(cursor..)
            .ok_or_else(|| format!("tail at {cursor} not on a char boundary"))?;
        check_gap(tail, cursor)?;
        rebuilt.push_str(tail);
        if rebuilt != src {
            return Err("rebuilt source differs from the original".to_owned());
        }
        Ok(())
    }

    fn check_gap(gap: &str, at: usize) -> Result<(), String> {
        let trivia = gap
            .char_indices()
            .all(|(index, ch)| ch.is_whitespace() || (ch == BOM && at == 0 && index == 0));
        if trivia {
            Ok(())
        } else {
            Err(format!(
                "bytes {gap:?} at {at} are not covered by any token"
            ))
        }
    }

    fn check_token_text(src: &str, span: Span, kind: TokenKind, text: &str) -> Result<(), String> {
        let ok = match kind {
            TokenKind::Open => text == "{",
            TokenKind::Close => text == "}",
            TokenKind::Op(op) => text == op.as_str(),
            TokenKind::Comment => {
                let after = src.get(span.end..).unwrap_or_default();
                text.starts_with('#')
                    && !text.contains('\n')
                    && (after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n"))
            }
            TokenKind::Scalar(scalar) if scalar.quoted => {
                text.len() >= 2 && text.starts_with('"') && text.ends_with('"')
            }
            TokenKind::Scalar(_) => {
                let math = text.starts_with("@[");
                !text.is_empty()
                    && !text.starts_with('"')
                    && text
                        .chars()
                        .all(|ch| math || !(ch.is_whitespace() || "{}=<>#".contains(ch)))
            }
        };
        if ok {
            Ok(())
        } else {
            Err(format!("{kind:?} at {span:?} has unexpected text {text:?}"))
        }
    }

    /// Invariant 2: spans nest properly and entries contain their parts.
    fn check_nesting(src: &str, block: &Block, low: usize, high: usize) -> Result<(), String> {
        let mut previous_end = low;
        for entry in &block.entries {
            let span = entry.span;
            if span.start < previous_end || span.end > high || span.start >= span.end {
                return Err(format!("entry {span:?} escapes {previous_end}..{high}"));
            }
            if !src.is_char_boundary(span.start) || !src.is_char_boundary(span.end) {
                return Err(format!("entry {span:?} not on char boundaries"));
            }
            let value = entry.value.span();
            let first = entry.key.map_or(value.start, |key| key.span.start);
            if span.start != first || span.end != value.end {
                return Err(format!("entry {span:?} does not match its key/value"));
            }
            if entry.key.is_none() && entry.op.is_some() {
                return Err(format!("entry {span:?} has an operator but no key"));
            }
            if let (Some(key), Some(op)) = (entry.key, entry.op)
                && (op.span.start < key.span.end || op.span.end > value.start)
            {
                return Err(format!("operator {:?} not between key and value", op.span));
            }
            if let Some(key) = entry.key
                && key.span.end > value.start
            {
                return Err(format!("key {:?} overlaps value {value:?}", key.span));
            }
            if let Value::Tagged { block: inner, tag } = &entry.value
                && inner.open.is_none_or(|open| tag.span.end > open)
            {
                return Err(format!("tag {:?} not before its block", tag.span));
            }
            if let Some(inner) = entry.value.as_block() {
                let (Some(open), Some(close)) = (inner.open, inner.close) else {
                    return Err(format!("nested block of {span:?} lacks braces"));
                };
                if close + 1 != value.end || open >= close {
                    return Err(format!(
                        "nested block {open}..{close} misplaced in {span:?}"
                    ));
                }
                check_nesting(src, inner, open + 1, close)?;
            }
            previous_end = span.end;
        }
        let mut previous_comment = low;
        for comment in &block.comments {
            if comment.start < previous_comment || comment.end > high {
                return Err(format!("comment {comment:?} out of order or bounds"));
            }
            let nested = block
                .entries
                .iter()
                .filter_map(|entry| entry.value.as_block()?.span())
                .any(|inner| comment.start >= inner.start && comment.start < inner.end);
            if nested {
                return Err(format!("comment {comment:?} belongs to a nested block"));
            }
            previous_comment = comment.end;
        }
        Ok(())
    }

    fn check_invariants(doc: &Document<'_>) -> Result<(), String> {
        check_lossless(doc)?;
        if doc.root.open.is_some() || doc.root.close.is_some() {
            return Err("root has braces".to_owned());
        }
        check_nesting(doc.src, &doc.root, 0, doc.src.len())
    }

    // ---------------------------------------------------------------------
    // Lexical rules
    // ---------------------------------------------------------------------

    #[test]
    fn empty_and_trivia_only_files_parse_to_an_empty_root() {
        for src in [
            "",
            " ",
            "\n\n",
            "\r\n",
            "\t \r\n  \n",
            "# only a comment",
            "# a\n# b\n",
            "\u{feff}",
            "\u{feff}# c\r\n",
            "\u{a0}\u{2003}\u{b}\u{c}\n",
        ] {
            let doc = parse_ok(src);
            assert!(doc.root.entries.is_empty(), "{src:?}");
            assert_eq!(doc.root.span(), None);
        }
        let src = "# a\r\n  # b\n#c";
        assert_eq!(
            comment_texts(src, &parse_ok(src).root),
            ["# a", "# b", "#c"]
        );
    }

    #[test]
    fn simple_assignment_spans() {
        let src = "key = value\n";
        let doc = parse_ok(src);
        let [entry] = doc.root.entries.as_slice() else {
            panic!("one entry expected");
        };
        assert_eq!(entry.key_str(src), Some("key"));
        assert_eq!(
            entry.key.map(|key| key.span),
            Some(Span { end: 3, start: 0 })
        );
        let op = entry.op.expect("has an operator");
        assert_eq!((op.kind, op.span), (OpKind::Eq, Span { end: 5, start: 4 }));
        assert_eq!(entry.value.span(), Span { end: 11, start: 6 });
        assert_eq!(entry.span, Span { end: 11, start: 0 });
    }

    #[test]
    fn every_operator_is_lexed_with_longest_match() {
        for (text, kind) in [
            ("=", OpKind::Eq),
            ("==", OpKind::EqEq),
            ("!=", OpKind::NotEq),
            ("<", OpKind::Lt),
            ("<=", OpKind::Le),
            (">", OpKind::Gt),
            (">=", OpKind::Ge),
            ("?=", OpKind::Exists),
        ] {
            assert_eq!(kind.as_str(), text);
            for src in [
                format!("a {text} 1"),
                format!("a{text}1"),
                format!("{{ a{text}1 }}"),
            ] {
                let doc = parse_ok(&src);
                let entry = doc
                    .root
                    .entries
                    .first()
                    .and_then(|entry| {
                        entry
                            .value
                            .as_block()
                            .map_or(Some(entry), |b| b.entries.first())
                    })
                    .expect("an entry");
                assert_eq!(entry.op.map(|op| op.kind), Some(kind), "{src}");
                assert_eq!(entry.key_str(&src), Some("a"), "{src}");
            }
        }
    }

    #[test]
    fn bare_scalars_are_single_tokens() {
        for scalar in [
            "-1",
            "0.05",
            "1939.1.1",
            "@CONST",
            "var:my_var",
            "FROM.FROM.owner",
            "$PARAM$",
            "[?global.x]",
            "[This.GetName]",
            "GER_focus_1",
            "yes",
            "ROOT",
            "event_target:foo",
            "relative_position_id",
            "a!b",
            "a?b",
            "!x",
            "?x",
            "é_ü",
            "a;b",
            "a\"b",
            "@[ base * 2 ]",
            "@[a*[b+1]]",
            "@[x]tail",
        ] {
            for src in [
                format!("k = {scalar}"),
                format!("{scalar} = v"),
                format!("{{ {scalar} }}"),
                format!("k={scalar}\n"),
            ] {
                let doc = parse_ok(&src);
                let mut texts = Vec::new();
                let mut tokens = Vec::new();
                collect_tokens(&doc.root, &mut tokens);
                for (span, kind) in tokens {
                    if let TokenKind::Scalar(found) = kind {
                        assert!(!found.quoted);
                        texts.push(span.text(&src));
                    }
                }
                assert!(texts.contains(&scalar), "{src:?} lexed as {texts:?}");
            }
        }
    }

    #[test]
    fn operators_split_bare_scalars_but_lone_bang_and_question_do_not() {
        assert_eq!(
            dump(&parse_ok("a!=b c?=d e<=f g>h i==j")),
            "a!=b c?=d e<=f g>h i==j"
        );
        assert_eq!(dump(&parse_ok("a!b ?c")), "a!b ?c");
        assert_eq!(dump(&parse_ok("k = x! k2 = y?")), "k=x! k2=y?");
    }

    #[test]
    fn inline_math_is_one_scalar_even_with_whitespace() {
        let src = "k = @[ base * 2 ] j = @[a*[b + 1]] m = 3";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "k=@[ base * 2 ] j=@[a*[b + 1]] m=3");
        let err = parse_err("k = @[ 1 + 2");
        assert_eq!(
            (err.offset, err.message.as_str()),
            (4, "unterminated `@[` inline math")
        );
    }

    #[test]
    fn quoted_scalars() {
        let src = "a = \"x # not a comment\" b = \"esc \\\" quote\" c = \"multi\nline\"\n\"key\" = \"\" d = \"é\"";
        let doc = parse_ok(src);
        assert!(doc.root.comments.is_empty());
        let values: Vec<(&str, &str, &str)> = doc
            .root
            .entries
            .iter()
            .map(|entry| {
                let Value::Scalar(value) = &entry.value else {
                    panic!("scalar value expected");
                };
                assert!(value.quoted);
                (
                    entry.key_str(src).unwrap_or_default(),
                    value.text(src),
                    value.unquoted(src),
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("a", "\"x # not a comment\"", "x # not a comment"),
                ("b", "\"esc \\\" quote\"", "esc \\\" quote"),
                ("c", "\"multi\nline\"", "multi\nline"),
                ("key", "\"\"", ""),
                ("d", "\"é\"", "é"),
            ]
        );
        let key = doc
            .root
            .entries
            .get(3)
            .and_then(|entry| entry.key)
            .expect("quoted key");
        assert!(key.quoted);
        assert_eq!(key.text(src), "\"key\"");
        // A quote opens a quoted scalar only at the start of a token: inside
        // a bare scalar it is literal, and a quoted scalar ends at its
        // closing quote even when a bare scalar follows immediately.
        assert_eq!(dump(&parse_ok("{ a\"b\"c }")), "{a\"b\"c}");
        assert_eq!(dump(&parse_ok("{ \"a\"b\"c\" }")), "{\"a\" b\"c\"}");
        let src = "13 = { Lahden suojeluskuntapiiri\" } #Lahti\"\n14 = { x }";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "13={Lahden suojeluskuntapiiri\"} 14={x}");
        assert_eq!(comment_texts(src, &doc.root), ["#Lahti\""]);
    }

    #[test]
    fn unicode_and_control_whitespace_separate_tokens() {
        assert_eq!(dump(&parse_ok("a\u{a0}=\u{2003}b\u{3000}c")), "a=b c");
        assert_eq!(dump(&parse_ok("a\u{b}=\u{c}b")), "a=b");
    }

    #[test]
    fn bom_is_trivia_only_at_the_start() {
        let src = "\u{feff}a = b\n";
        let doc = parse_ok(src);
        assert_eq!(
            doc.root.entries.first().map(|entry| entry.span.start),
            Some(3)
        );
        let src = "a = \u{feff}b";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "a=\u{feff}b");
    }

    // ---------------------------------------------------------------------
    // Grammar
    // ---------------------------------------------------------------------

    #[test]
    fn tagged_values() {
        let src = "color = rgb { 1 2 3 }";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "color=rgb!{1 2 3}");
        let entry = doc.root.entries.first().expect("entry");
        assert_eq!(entry.value.span(), Span { end: 21, start: 8 });
        assert_eq!(entry.span, Span { end: 21, start: 0 });
        assert!(matches!(entry.value, Value::Tagged { .. }));
        assert_eq!(
            entry.value.as_block().map(|block| block.entries.len()),
            Some(3)
        );

        assert_eq!(
            dump(&parse_ok("c = hsv{ 0.1 0.2 0.3 }")),
            "c=hsv!{0.1 0.2 0.3}"
        );
        assert_eq!(dump(&parse_ok("c = hsv360 {}")), "c=hsv360!{}");
        let src = "c = hsv360 # comment\n { 1 2 3 }";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "c=hsv360!{1 2 3}");
        assert_eq!(comment_texts(src, &doc.root), ["# comment"]);
        // A quoted scalar is never a tag: the `{` starts a bare block.
        assert_eq!(
            dump(&parse_ok("c = \"rgb\" { 1 2 3 }")),
            "c=\"rgb\" {1 2 3}"
        );
        // Nor is a bare element.
        assert_eq!(dump(&parse_ok("{ a b { 1 } }")), "{a b{1}}");
    }

    #[test]
    fn operator_less_keyed_blocks_stay_operator_less() {
        let src = "key { a = b } \"quoted\" { } outer { inner { x } }";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "key{a=b} \"quoted\"{} outer{inner{x}}");
        for entry in &doc.root.entries {
            assert!(entry.key.is_some());
            assert!(entry.op.is_none());
        }
        assert_eq!(
            doc.root.entries.get(1).and_then(|entry| entry.key_str(src)),
            Some("quoted")
        );
    }

    #[test]
    fn bare_arrays_and_mixed_blocks() {
        assert_eq!(dump(&parse_ok("{ a b c }")), "{a b c}");
        assert_eq!(
            dump(&parse_ok("k = { { 1 2 } { 3 4 } }")),
            "k={{1 2} {3 4}}"
        );
        assert_eq!(dump(&parse_ok("k = { a b = c d }")), "k={a b=c d}");
        assert_eq!(dump(&parse_ok("k = { 1 \"two\" 3 }")), "k={1 \"two\" 3}");
        assert_eq!(dump(&parse_ok("{} {}")), "{} {}");
        let doc = parse_ok("k = { a b }");
        for entry in &only_block(&doc).entries {
            assert!(entry.key.is_none() && entry.op.is_none());
            assert_eq!(entry.span, entry.value.span());
        }
    }

    #[test]
    fn nested_blocks_and_spans() {
        let src = "a = {\n\tb = {\n\t\tc = d\n\t}\n}\n";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "a={b={c=d}}");
        let outer = only_block(&doc);
        assert_eq!((outer.open, outer.close), (Some(4), Some(24)));
        assert_eq!(outer.span(), Some(Span { end: 25, start: 4 }));
        assert!(!outer.is_single_line(src));
        let b = outer.entries.first().expect("b");
        assert_eq!(b.span.text(src), "b = {\n\t\tc = d\n\t}");
        let inner = b.value.as_block().expect("inner block");
        assert_eq!(
            inner.entries.first().map(|c| c.span.text(src)),
            Some("c = d")
        );
        assert!(
            parse_ok("a = { b = { c = d } }")
                .root
                .entries
                .iter()
                .all(|entry| {
                    entry
                        .value
                        .as_block()
                        .is_some_and(|block| block.is_single_line("a = { b = { c = d } }"))
                })
        );
        assert!(!parse_ok(src).root.is_single_line(src));
    }

    #[test]
    fn comments_at_every_position() {
        let src = "# before first\n\
                   a # between key and op\n\
                   = # between op and value\n\
                   b # trailing\n\
                   c = { # after open\n\
                   \td = e # trailing inner\n\
                   \t# before close\n\
                   } # after close\n\
                   f = rgb # between tag and brace\n\
                   { 1 # in tagged\n\
                   }\n\
                   # end of file";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "a=b c={d=e} f=rgb!{1}");
        assert_eq!(
            comment_texts(src, &doc.root),
            [
                "# before first",
                "# between key and op",
                "# between op and value",
                "# trailing",
                "# after close",
                "# between tag and brace",
                "# end of file",
            ]
        );
        let entries = &doc.root.entries;
        let c = entries
            .get(1)
            .and_then(|entry| entry.value.as_block())
            .expect("c");
        assert_eq!(
            comment_texts(src, c),
            ["# after open", "# trailing inner", "# before close"]
        );
        let f = entries
            .get(2)
            .and_then(|entry| entry.value.as_block())
            .expect("f");
        assert_eq!(comment_texts(src, f), ["# in tagged"]);
        // The entry spans the comments inside it but not those around it.
        let a = entries.first().expect("a");
        assert!(a.span.text(src).starts_with("a # between"));
        assert!(a.span.text(src).ends_with("value\nb"));
        assert!(doc.root.has_comments_recursive());
        assert!(
            !parse_ok("a = { b = { c = d } }")
                .root
                .has_comments_recursive()
        );
        let nested = "a = { b = { c = d # deep\n } }";
        let doc = parse_ok(nested);
        assert!(doc.root.comments.is_empty());
        assert!(doc.root.has_comments_recursive());
        // `#` directly after a scalar ends it.
        assert_eq!(dump(&parse_ok("a = b#c\nd = e")), "a=b d=e");
    }

    #[test]
    fn crlf_and_lf_files() {
        let src = "a = b # c\r\nd = {\r\n\te = f # g\r\n}\r\n";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "a=b d={e=f}");
        assert_eq!(comment_texts(src, &doc.root), ["# c"]);
        let d = doc
            .root
            .entries
            .get(1)
            .and_then(|entry| entry.value.as_block())
            .expect("d");
        assert_eq!(comment_texts(src, d), ["# g"]);
        let lf = src.replace("\r\n", "\n");
        assert_eq!(dump(&parse_ok(&lf)), "a=b d={e=f}");
        // A lone `\r` is whitespace between tokens but not a line terminator.
        let src = "a = b\rc = d # x\ry";
        let doc = parse_ok(src);
        assert_eq!(dump(&doc), "a=b c=d");
        assert_eq!(comment_texts(src, &doc.root), ["# x\ry"]);
    }

    #[test]
    fn files_without_trailing_newline() {
        assert_eq!(dump(&parse_ok("a = b")), "a=b");
        assert_eq!(dump(&parse_ok("a = { b }")), "a={b}");
        let src = "a = b # c";
        assert_eq!(comment_texts(src, &parse_ok(src).root), ["# c"]);
        assert_eq!(dump(&parse_ok("x")), "x");
        assert_eq!(dump(&parse_ok("\"q\"")), "\"q\"");
    }

    #[test]
    fn errors_are_reported_with_offsets() {
        for (src, offset, message) in [
            ("}", 0, "unmatched `}`"),
            ("a = b }", 6, "unmatched `}`"),
            ("a = { b } }", 10, "unmatched `}`"),
            ("a = {", 4, "unclosed `{` at end of file"),
            ("a = { b = {\n c = d }", 4, "unclosed `{` at end of file"),
            ("= b", 0, "operator without a key"),
            ("a = b = c", 6, "operator without a key"),
            ("{ } = b", 4, "operator without a key"),
            ("{ >= 1 }", 2, "operator without a key"),
            ("a = }", 4, "expected a value after operator, found `}`"),
            ("{ a = }", 6, "expected a value after operator, found `}`"),
            (
                "a =",
                3,
                "expected a value after operator, found end of file",
            ),
            (
                "a = # c\n",
                8,
                "expected a value after operator, found end of file",
            ),
            (
                "a = = b",
                4,
                "expected a value after operator, found another operator",
            ),
            (
                "a => b",
                3,
                "expected a value after operator, found another operator",
            ),
            ("a = \"b", 4, "unterminated quoted string"),
            ("a = \"b\\\"", 4, "unterminated quoted string"),
            ("a = \"b\\", 4, "unterminated quoted string"),
            (
                "[[!PARAM] a = b ]",
                0,
                "unsupported `[[` conditional parameter block",
            ),
            (
                "a = { [[PARAM] b = c ] }",
                6,
                "unsupported `[[` conditional parameter block",
            ),
            ("a = @[ 1 + [2 ]", 4, "unterminated `@[` inline math"),
        ] {
            let err = parse_err(src);
            assert_eq!(
                (err.offset, err.message.as_str()),
                (offset, message),
                "{src:?}"
            );
        }
        let err = parse_err("a = }");
        assert_eq!(
            err.to_string(),
            "expected a value after operator, found `}` at byte 4"
        );
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert!(boxed.to_string().contains("byte 4"));
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let src = format!("a = {}", "{".repeat(100_000));
        let err = parse_err(&src);
        assert_eq!(
            (err.offset, err.message.as_str()),
            (4 + MAX_DEPTH, "blocks nested too deeply")
        );
        let deepest = format!("{}{}", "{".repeat(MAX_DEPTH), "}".repeat(MAX_DEPTH));
        parse_ok(&deepest);
        let too_deep = format!("{}{}", "{".repeat(MAX_DEPTH + 1), "}".repeat(MAX_DEPTH + 1));
        assert_eq!(parse_err(&too_deep).message, "blocks nested too deeply");
    }

    /// `parse`'s `# Errors` section states the nesting limit; it must be the
    /// one enforced, so input nested up to it parses and one deeper fails.
    #[test]
    fn documented_nesting_limit_is_the_enforced_one() {
        let source = include_str!("cst.rs");
        let docs: Vec<&str> = source
            .split_once("\npub fn parse(")
            .map(|(before, _)| before)
            .expect("`parse` is defined")
            .lines()
            .rev()
            .map_while(|line| line.trim().strip_prefix("///").map(str::trim))
            .collect();
        let docs = docs.into_iter().rev().collect::<Vec<_>>().join(" ");
        let documented: usize = docs
            .split_once("nested more than ")
            .map(|(_, after)| {
                after
                    .chars()
                    .skip_while(|ch| !ch.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse().ok())
            .expect("`parse` documents its nesting limit");
        assert_eq!(documented, MAX_DEPTH, "documented vs enforced limit");
        let at_limit = format!("{}{}", "{".repeat(documented), "}".repeat(documented));
        parse_ok(&at_limit);
        let over = documented + 1;
        let err = parse_err(&format!("{}{}", "{".repeat(over), "}".repeat(over)));
        assert_eq!(
            (err.offset, err.message.as_str()),
            (documented, "blocks nested too deeply")
        );
    }

    /// The derived `Clone`/`Debug`/`PartialEq`/`Drop` impls recurse once per
    /// nesting level, so [`MAX_DEPTH`] must keep every tree `parse` accepts
    /// safe on a 1 MiB stack (the Windows main-thread default) with room to
    /// spare for recursive callers: all of them run here on half that. A
    /// stack overflow cannot be caught; it aborts the test binary with
    /// "thread 'cst-deepest-tree' has overflowed its stack".
    #[test]
    fn deepest_accepted_tree_is_safe_to_clone_debug_compare_and_drop_on_a_small_stack() {
        const STACK: usize = 512 * 1024;
        // A keyed block, and a tagged one (`Value::Tagged` adds a level of
        // `Debug` nesting, making it the costliest shape per level).
        for open in ["a = {", "a = rgb {"] {
            let src = format!("{}{}", open.repeat(MAX_DEPTH), "}".repeat(MAX_DEPTH));
            let root = parse(&src)
                .unwrap_or_else(|err| panic!("{open:?} x {MAX_DEPTH}: {err}"))
                .root;
            std::thread::Builder::new()
                .name("cst-deepest-tree".to_owned())
                .stack_size(STACK)
                .spawn(move || {
                    let copy = root.clone();
                    assert!(copy == root, "{open:?}: clone differs");
                    let compact = format!("{root:?}");
                    let pretty = format!("{root:#?}");
                    assert!(compact.len() < pretty.len(), "{open:?}");
                    drop(copy);
                    drop(root);
                })
                .expect("spawn a thread with a small stack")
                .join()
                .unwrap_or_else(|_| panic!("{open:?} x {MAX_DEPTH}: thread panicked"));
        }
    }

    /// Exhaustive short inputs plus pseudo-random longer ones over a hostile
    /// alphabet: parsing never panics, errors point inside the input, and
    /// every successful parse satisfies the invariants.
    #[test]
    fn hostile_inputs_never_panic_and_stay_lossless() {
        const ALPHABET: [&str; 18] = [
            "{", "}", "=", "a", "\"", "#", "\n", "@[", "]", "!", "?", "<", " ", "\r\n", "[[", "\\",
            "\u{a0}", "é",
        ];
        fn check(src: &str) {
            match parse(src) {
                Ok(doc) => {
                    if let Err(violation) = check_invariants(&doc) {
                        panic!("{src:?}: {violation}");
                    }
                }
                Err(err) => assert!(
                    err.offset <= src.len() && src.is_char_boundary(err.offset),
                    "{src:?}: {err}"
                ),
            }
        }
        let mut frontier = vec![String::new()];
        for _ in 0_u32..4 {
            let mut next = Vec::with_capacity(frontier.len() * ALPHABET.len());
            for prefix in &frontier {
                for piece in ALPHABET {
                    let src = format!("{prefix}{piece}");
                    check(&src);
                    next.push(src);
                }
            }
            frontier = next;
        }
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut random = || {
            state ^= state << 13_u32;
            state ^= state >> 7_u32;
            state ^= state << 17_u32;
            usize::try_from(state % 1_000_000).expect("small")
        };
        for _ in 0_u32..20_000 {
            let len = 5 + random() % 40;
            let src: String = std::iter::repeat_with(|| {
                *ALPHABET.get(random() % ALPHABET.len()).expect("in range")
            })
            .take(len)
            .collect();
            check(&src);
        }
    }

    // ---------------------------------------------------------------------
    // Helpers on the tree
    // ---------------------------------------------------------------------

    #[test]
    fn block_value_entry_and_scalar_helpers() {
        let src = "\"k\" = { a = 1 } s = \"q\" t = rgb { 1 }";
        let doc = parse_ok(src);
        let [block_entry, scalar_entry, tagged_entry] = doc.root.entries.as_slice() else {
            panic!("three entries");
        };
        assert_eq!(block_entry.key_str(src), Some("k"));
        assert_eq!(block_entry.key.map(|key| key.text(src)), Some("\"k\""));
        assert!(
            block_entry
                .value
                .as_block()
                .is_some_and(|block| block.is_single_line(src))
        );
        assert_eq!(block_entry.value.span().text(src), "{ a = 1 }");
        assert_eq!(scalar_entry.value.as_block(), None);
        assert_eq!(scalar_entry.value.span().text(src), "\"q\"");
        assert_eq!(tagged_entry.value.span().text(src), "rgb { 1 }");
        assert_eq!(doc.root.span(), None);
        assert!(!doc.root.is_single_line(src));
        let Value::Scalar(scalar) = &scalar_entry.value else {
            panic!("scalar");
        };
        assert_eq!((scalar.text(src), scalar.unquoted(src)), ("\"q\"", "q"));
        assert_eq!(Span { end: 99, start: 0 }.text(src), "");
    }

    #[test]
    fn line_start_and_line_end() {
        let src = "ab\r\ncd\nef";
        for (pos, start, end) in [
            (0, 0, 2),
            (1, 0, 2),
            (2, 0, 2),
            (3, 0, 2),
            (4, 4, 6),
            (6, 4, 6),
            (7, 7, 9),
            (9, 7, 9),
            (100, 7, 9),
        ] {
            assert_eq!(line_start(src, pos), start, "line_start({pos})");
            assert_eq!(line_end(src, pos), end, "line_end({pos})");
        }
        assert_eq!((line_start("", 0), line_end("", 0)), (0, 0));
        assert_eq!(line_end("a\r", 0), 2);
    }

    /// Both bytes of a `\r\n` belong to the line they terminate, so
    /// `line_start(p)..line_end(p)` is that line's content for either.
    #[test]
    fn line_end_on_the_newline_of_a_crlf_returns_the_cr() {
        let src = "ab\r\ncd";
        for pos in 0..=3 {
            assert_eq!(line_end(src, pos), 2, "line_end({pos})");
            assert_eq!(
                src.get(line_start(src, pos)..line_end(src, pos)),
                Some("ab"),
                "line of {pos}"
            );
        }
        // The line above a line start `s`, taken as a caller would.
        let s = line_start(src, 5);
        assert_eq!(
            src.get(line_start(src, s - 1)..line_end(src, s - 1)),
            Some("ab")
        );
        // A unit's final `\n` (`unit.end - 1`) on a CRLF file.
        let block_src = "a = {\r\n\tb = c\r\n}\r\n";
        let doc = parse_ok(block_src);
        let unit = *line_units(block_src, only_block(&doc))
            .expect("units")
            .first()
            .expect("one unit");
        let last = unit.end - 1;
        assert_eq!(
            block_src.get(line_start(block_src, last)..line_end(block_src, last)),
            Some("\tb = c")
        );
        // Terminators at the very start of the input.
        for (text, pos, end) in [
            ("\r\n", 0, 0),
            ("\r\n", 1, 0),
            ("\n", 0, 0),
            ("\r\nx", 1, 0),
        ] {
            assert_eq!(line_end(text, pos), end, "line_end({text:?}, {pos})");
        }
        // A `\n` after a lone `\r` that is itself after a `\n` is still CRLF.
        assert_eq!(line_end("a\n\r\n", 3), 2);
    }

    // ---------------------------------------------------------------------
    // line_units
    // ---------------------------------------------------------------------

    #[test]
    fn line_units_attach_leading_comments_but_not_dangling_ones() {
        let src = "focus = {\n\
                   \tid = a\n\
                   \t# attached to icon\n\
                   \t# second attached line\n\
                   \ticon = b # trailing\n\
                   \n\
                   \t# dangling\n\
                   \n\
                   \tcost = 1\n\
                   \t# attached after blank\n\
                   \tavailable = {\n\
                   \t\thas_war = yes\n\
                   \t} # trailing after block\n\
                   \t# dangling before close\n\
                   }\n";
        let doc = parse_ok(src);
        let block = only_block(&doc);
        let units = line_units(src, block).expect("one entry per line");
        assert_eq!(
            unit_texts(src, &units),
            [
                "\tid = a\n",
                "\t# attached to icon\n\t# second attached line\n\ticon = b # trailing\n",
                "\tcost = 1\n",
                "\t# attached after blank\n\tavailable = {\n\t\thas_war = yes\n\t} # trailing after block\n",
            ]
        );
        for (index, unit) in units.iter().enumerate() {
            assert_eq!(unit.entry, index);
            let entry = block.entries.get(index).expect("entry");
            assert_eq!(unit.entry_line_start, line_start(src, entry.span.start));
            assert!(
                unit.start <= unit.entry_line_start && unit.entry_line_start <= entry.span.start
            );
        }
        let icon = units.get(1).expect("icon");
        assert_eq!(
            src.get(icon.entry_line_start..icon.end),
            Some("\ticon = b # trailing\n")
        );
        assert!(units.windows(2).all(|pair| match pair {
            [first, second] => first.end <= second.start,
            _ => false,
        }));
    }

    #[test]
    fn line_units_require_one_entry_per_line() {
        for (src, expected) in [
            ("a = { b = c d = e\n}", None),
            ("a = {\n\tb = c d = e\n}", None),
            ("a = {\n\tb = c }", None),
            ("a = { b = c }", None),
            ("a = {\n\tb = c\n}", Some(1)),
            ("a = {\n}", Some(0)),
            ("a = {}", Some(0)),
            ("a = {\n\tb = { c\n\t}\n}", Some(1)),
            ("a = {\n\tb = {\n\t\tc } d = e\n}", None),
        ] {
            let doc = parse_ok(src);
            let found = line_units(src, only_block(&doc)).map(|units| units.len());
            assert_eq!(found, expected, "{src:?}");
        }
        // Nested blocks are judged on their own entries only.
        let src = "a = {\n\tb = { c d }\n}";
        let doc = parse_ok(src);
        let outer = only_block(&doc);
        assert!(line_units(src, outer).is_some());
        let inner = outer
            .entries
            .first()
            .and_then(|entry| entry.value.as_block())
            .expect("b");
        assert!(line_units(src, inner).is_none());
    }

    #[test]
    fn line_units_comment_edge_cases() {
        // A comment on the `{` line is not attached; one below it is.
        let src = "a = { # on open line\n\tb = c\n}";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, only_block(&doc)).expect("units")),
            ["\tb = c\n"]
        );
        let src = "a = {\n\t# about b\n\tb = c\n}";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, only_block(&doc)).expect("units")),
            ["\t# about b\n\tb = c\n"]
        );
        // A `#` line inside a multi-line string is not a comment.
        let src = "a = {\n\tx = \"foo\n# bar\"\n\tb = c\n}";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, only_block(&doc)).expect("units")),
            ["\tx = \"foo\n# bar\"\n", "\tb = c\n"]
        );
        // Comments inside an entry (between key and value) stay inside it.
        let src = "a = {\n\tb # k\n\t= d\n\t# about e\n\te = f\n}";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, only_block(&doc)).expect("units")),
            ["\tb # k\n\t= d\n", "\t# about e\n\te = f\n"]
        );
        // Root block, BOM, attached header comment, no trailing newline.
        let src = "\u{feff}# header\na = b\n\nc = d";
        let doc = parse_ok(src);
        let units = line_units(src, &doc.root).expect("units");
        assert_eq!(unit_texts(src, &units), ["# header\na = b\n", "c = d"]);
        assert_eq!(units.first().map(|unit| unit.start), Some(3));
        assert_eq!(units.last().map(|unit| unit.end), Some(src.len()));
        // Root entries sharing a line.
        let src = "a = b c = d\n";
        assert!(line_units(src, &parse_ok(src).root).is_none());
    }

    #[test]
    fn line_units_crlf() {
        let src = "a = {\r\n\t# c\r\n\tb = c # t\r\n\r\n\td = e\r\n}\r\n";
        let doc = parse_ok(src);
        let units = line_units(src, only_block(&doc)).expect("units");
        assert_eq!(
            unit_texts(src, &units),
            ["\t# c\r\n\tb = c # t\r\n", "\td = e\r\n"]
        );
    }

    /// A lone `\r` is whitespace, not a line terminator, so a comment behind
    /// one is on its entry's line by the `\n` line model; editors draw the
    /// `\r` as a line break and show that comment as a header of the next
    /// line instead. Which line it belongs to is ambiguous, so the block is
    /// not split into units (vanilla `FR_SCRIPTING_FULL_AUTOMATED.txt` has
    /// `}` CR CR `#### Ideologies ####`).
    #[test]
    fn line_units_refuse_a_comment_behind_a_lone_cr() {
        for src in [
            "a = {\n}\r\r# Section header\nb = 2\n",
            "a = 1\r# about b\nb = 2\n",
            "a = 1 \r # about b\r\nb = 2\r\n",
        ] {
            let doc = parse_ok(src);
            assert_eq!(line_units(src, &doc.root), None, "{src:?}");
        }
        let src = "x = {\n\ta = 1\r\t# about b\n\tb = 2\n}\n";
        let doc = parse_ok(src);
        assert_eq!(line_units(src, only_block(&doc)), None);

        // Lone CRs elsewhere stay harmless: at the end of an entry's line,
        // in indentation, on blank lines, inside a comment, and at the end
        // of the file.
        let src = "x = {\r\n\ta = 1\r\r\n\r\r\n\r\t# c\r\n\tb = 2 # t\ry\r\n}\r\n\r";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, only_block(&doc)).expect("units")),
            ["\ta = 1\r\r\n", "\r\t# c\r\n\tb = 2 # t\ry\r\n"]
        );
        let src = "a = 1\r\n\r# c\r\nb = 2\r";
        let doc = parse_ok(src);
        assert_eq!(
            unit_texts(src, &line_units(src, &doc.root).expect("units")),
            ["a = 1\r\n", "\r# c\r\nb = 2\r"]
        );
    }

    // ---------------------------------------------------------------------
    // apply_edits
    // ---------------------------------------------------------------------

    fn edit(start: usize, end: usize, replacement: &str) -> Edit {
        Edit {
            replacement: replacement.to_owned(),
            span: Span { end, start },
        }
    }

    #[test]
    fn apply_edits_sorts_and_rejects_overlaps() {
        let src = "abcdef";
        assert_eq!(apply_edits(src, vec![]).as_deref(), Some(src));
        assert_eq!(
            apply_edits(src, vec![edit(4, 6, "XY"), edit(0, 1, "Z")]).as_deref(),
            Some("ZbcdXY")
        );
        assert_eq!(
            apply_edits(src, vec![edit(2, 4, "2"), edit(0, 2, "1")]).as_deref(),
            Some("12ef")
        );
        assert_eq!(apply_edits(src, vec![edit(0, 3, ""), edit(2, 4, "")]), None);
        assert_eq!(
            apply_edits(src, vec![edit(0, 4, ""), edit(2, 2, "x")]),
            None
        );
        assert_eq!(apply_edits(src, vec![edit(1, 5, ""), edit(1, 5, "")]), None);
        assert_eq!(
            apply_edits(src, vec![edit(3, 3, "1"), edit(3, 3, "2")]).as_deref(),
            Some("abc12def")
        );
        assert_eq!(
            apply_edits(src, vec![edit(3, 3, "2"), edit(3, 3, "1")]).as_deref(),
            Some("abc21def")
        );
        assert_eq!(
            apply_edits(src, vec![edit(2, 4, "R"), edit(2, 2, "i")]).as_deref(),
            Some("abiRef")
        );
        assert_eq!(
            apply_edits(src, vec![edit(2, 4, "R"), edit(4, 4, "i")]).as_deref(),
            Some("abRief")
        );
        assert_eq!(
            apply_edits(src, vec![edit(6, 6, "!")]).as_deref(),
            Some("abcdef!")
        );
        assert_eq!(apply_edits(src, vec![edit(5, 9, "")]), None);
        assert_eq!(apply_edits(src, vec![edit(4, 2, "")]), None);
        assert_eq!(apply_edits("é", vec![edit(1, 2, "")]), None);
    }

    #[test]
    fn deleting_a_line_unit_removes_the_entry_and_its_comments() {
        let src = "d = {\n\ta = 1\n\t# why\n\tfire_only_once = no # pointless\n\tb = 2\n}\n";
        let doc = parse_ok(src);
        let block = only_block(&doc);
        let units = line_units(src, block).expect("units");
        let unit = units.get(1).expect("second unit");
        let edited = apply_edits(
            src,
            vec![Edit {
                replacement: String::new(),
                span: Span {
                    end: unit.end,
                    start: unit.start,
                },
            }],
        )
        .expect("edits apply");
        assert_eq!(edited, "d = {\n\ta = 1\n\tb = 2\n}\n");
        // Swapping two units reorders entries with their comments.
        let (Some(first), Some(second)) = (units.first(), units.get(1)) else {
            panic!("two units");
        };
        let text = |unit: &Unit| src.get(unit.start..unit.end).unwrap_or_default().to_owned();
        let swapped = apply_edits(
            src,
            vec![
                Edit {
                    replacement: text(second),
                    span: Span {
                        end: first.end,
                        start: first.start,
                    },
                },
                Edit {
                    replacement: text(first),
                    span: Span {
                        end: second.end,
                        start: second.start,
                    },
                },
            ],
        )
        .expect("edits apply");
        assert_eq!(
            swapped,
            "d = {\n\t# why\n\tfire_only_once = no # pointless\n\ta = 1\n\tb = 2\n}\n"
        );
    }

    // ---------------------------------------------------------------------
    // visit_blocks
    // ---------------------------------------------------------------------

    #[test]
    fn visit_blocks_reports_key_paths_in_pre_order() {
        let src = "a = { b = { c = 1 } d = rgb { 1 2 3 } { e = { } } }\nf { }\ng = h\n\"q\" = { }";
        let doc = parse_ok(src);
        let mut seen: Vec<(String, Option<String>, usize)> = Vec::new();
        visit_blocks(&doc, &mut |path, entry, block| {
            seen.push((
                path.join("/"),
                entry.key_str(src).map(str::to_owned),
                block.entries.len(),
            ));
        });
        let expected: Vec<(String, Option<String>, usize)> = [
            ("a", Some("a"), 3),
            ("a/b", Some("b"), 1),
            ("a/d", Some("d"), 3),
            ("a/", None, 1),
            ("a//e", Some("e"), 0),
            ("f", Some("f"), 0),
            ("q", Some("q"), 0),
        ]
        .into_iter()
        .map(|(path, key, len)| (path.to_owned(), key.map(str::to_owned), len))
        .collect();
        assert_eq!(seen, expected);
    }

    // ---------------------------------------------------------------------
    // Corpus: losslessness and structural fidelity against jomini
    // ---------------------------------------------------------------------

    fn cst_shape(src: &str, block: &Block) -> Vec<Shape> {
        block
            .entries
            .iter()
            .map(|entry| Shape {
                key: entry.key_str(src).map(str::to_owned),
                offset: Some(entry.span.start),
                op: entry
                    .key
                    .map(|_| entry.op.map_or("=", |op| op.kind.as_str())),
                value: match &entry.value {
                    Value::Scalar(scalar) => ShapeValue::Scalar(scalar.unquoted(src).to_owned()),
                    Value::Block(inner) => ShapeValue::Block(cst_shape(src, inner)),
                    Value::Tagged { block: inner, tag } => {
                        ShapeValue::Tagged(tag.text(src).to_owned(), cst_shape(src, inner))
                    }
                },
            })
            .collect()
    }

    fn jomini_text(scalar: jomini::Scalar<'_>) -> String {
        String::from_utf8_lossy(scalar.as_bytes()).into_owned()
    }

    /// Converts the tape tokens `start..end` of one container. An object
    /// holds `key [operator] value` triples, an array holds bare values, and
    /// after a `MixedContainer` marker the rest holds values where one that
    /// is followed by an operator is the key of a `key op value` entry.
    fn jomini_shape(
        tokens: &[jomini::TextToken<'_>],
        start: usize,
        end: usize,
        object: bool,
    ) -> Result<Vec<Shape>, String> {
        use jomini::TextToken;
        let mut out = Vec::new();
        let mut index = start;
        let mut mixed = false;
        while index < end {
            let token = tokens.get(index).ok_or("tape index out of range")?;
            if matches!(token, TextToken::MixedContainer) {
                mixed = true;
                index += 1;
                continue;
            }
            if mixed {
                let (value, next) = jomini_value(tokens, index)?;
                if let Some(TextToken::Operator(op)) = tokens.get(next).filter(|_| next < end) {
                    let ShapeValue::Scalar(key) = value else {
                        return Err("mixed-mode key is not a scalar".to_owned());
                    };
                    let (value, after) = jomini_value(tokens, next + 1)?;
                    out.push(Shape {
                        key: Some(key),
                        offset: None,
                        op: Some(op.symbol()),
                        value,
                    });
                    index = after;
                } else {
                    out.push(Shape {
                        key: None,
                        offset: None,
                        op: None,
                        value,
                    });
                    index = next;
                }
            } else if object {
                let (TextToken::Unquoted(key) | TextToken::Quoted(key)) = token else {
                    return Err(format!("object key is {token:?}"));
                };
                let key = jomini_text(*key);
                index += 1;
                let mut op = "=";
                if let Some(TextToken::Operator(operator)) = tokens.get(index) {
                    op = operator.symbol();
                    index += 1;
                }
                let (value, next) = jomini_value(tokens, index)?;
                out.push(Shape {
                    key: Some(key),
                    offset: None,
                    op: Some(op),
                    value,
                });
                index = next;
            } else {
                let (value, next) = jomini_value(tokens, index)?;
                out.push(Shape {
                    key: None,
                    offset: None,
                    op: None,
                    value,
                });
                index = next;
            }
        }
        Ok(out)
    }

    fn jomini_value(
        tokens: &[jomini::TextToken<'_>],
        index: usize,
    ) -> Result<(ShapeValue, usize), String> {
        use jomini::TextToken;
        match tokens.get(index) {
            Some(TextToken::Unquoted(scalar) | TextToken::Quoted(scalar)) => {
                Ok((ShapeValue::Scalar(jomini_text(*scalar)), index + 1))
            }
            Some(TextToken::Header(tag)) => {
                let (inner, next) = jomini_value(tokens, index + 1)?;
                let ShapeValue::Block(entries) = inner else {
                    return Err("header not followed by a container".to_owned());
                };
                Ok((ShapeValue::Tagged(jomini_text(*tag), entries), next))
            }
            Some(TextToken::Array { end, .. }) => Ok((
                ShapeValue::Block(jomini_shape(tokens, index + 1, *end, false)?),
                end + 1,
            )),
            Some(TextToken::Object { end, .. }) => Ok((
                ShapeValue::Block(jomini_shape(tokens, index + 1, *end, true)?),
                end + 1,
            )),
            other => Err(format!("unexpected value token {other:?}")),
        }
    }

    fn label(shape: &Shape, index: usize) -> String {
        shape.key.clone().unwrap_or_else(|| format!("[{index}]"))
    }

    fn first_difference(
        cst: &[Shape],
        jomini: &[Shape],
        path: &mut Vec<String>,
    ) -> Option<Difference> {
        let len = cst.len().max(jomini.len());
        for index in 0..len {
            let difference = |category, detail: String, offset| Difference {
                category,
                detail,
                offset,
                path: path.join(" > "),
            };
            let (ours, theirs) = match (cst.get(index), jomini.get(index)) {
                (Some(ours), Some(theirs)) => (ours, theirs),
                (Some(ours), None) => {
                    return Some(difference(
                        "extra CST entry",
                        format!("CST has {:?} {}", ours.key, ours.value.kind()),
                        ours.offset,
                    ));
                }
                (None, Some(theirs)) => {
                    return Some(difference(
                        "missing CST entry",
                        format!("jomini has {:?} {}", theirs.key, theirs.value.kind()),
                        cst.last().and_then(|last| last.offset),
                    ));
                }
                (None, None) => return None,
            };
            if ours.key != theirs.key {
                return Some(difference(
                    "key",
                    format!("CST {:?} vs jomini {:?}", ours.key, theirs.key),
                    ours.offset,
                ));
            }
            if ours.op != theirs.op {
                return Some(difference(
                    "operator",
                    format!("CST {:?} vs jomini {:?}", ours.op, theirs.op),
                    ours.offset,
                ));
            }
            let nested = match (&ours.value, &theirs.value) {
                (ShapeValue::Scalar(a), ShapeValue::Scalar(b)) => {
                    if a != b {
                        return Some(difference(
                            "scalar text",
                            format!("CST {a:?} vs jomini {b:?}"),
                            ours.offset,
                        ));
                    }
                    None
                }
                (ShapeValue::Block(a), ShapeValue::Block(b)) => Some((a, b)),
                (ShapeValue::Tagged(tag_a, a), ShapeValue::Tagged(tag_b, b)) => {
                    if tag_a != tag_b {
                        return Some(difference(
                            "tag",
                            format!("CST {tag_a:?} vs jomini {tag_b:?}"),
                            ours.offset,
                        ));
                    }
                    Some((a, b))
                }
                (a, b) => {
                    return Some(difference(
                        "value kind",
                        format!("CST {} vs jomini {}", a.kind(), b.kind()),
                        ours.offset,
                    ));
                }
            };
            if let Some((a, b)) = nested {
                path.push(label(ours, index));
                if let Some(found) = first_difference(a, b, path) {
                    return Some(found);
                }
                path.pop();
            }
        }
        None
    }

    /// The first structural difference between the CST of `src` and
    /// jomini's parse of it; `Err` if jomini cannot parse `src`.
    fn compare_with_jomini(src: &str, doc: &Document<'_>) -> Result<Option<Difference>, String> {
        let tape = jomini::TextTape::from_slice(src.as_bytes()).map_err(|err| err.to_string())?;
        let tokens = tape.tokens();
        let theirs = jomini_shape(tokens, 0, tokens.len(), true)
            .map_err(|err| format!("unreadable tape: {err}"))?;
        Ok(first_difference(
            &cst_shape(src, &doc.root),
            &theirs,
            &mut Vec::new(),
        ))
    }

    /// `src` with every bare scalar containing `[` or `]` wrapped in quotes
    /// (see [`BRACKETED`]); `None` if there are none.
    fn quote_bracketed_scalars(doc: &Document<'_>) -> Option<String> {
        let mut tokens = Vec::new();
        collect_tokens(&doc.root, &mut tokens);
        let quote = |at: usize| Edit {
            replacement: "\"".to_owned(),
            span: Span { end: at, start: at },
        };
        let edits: Vec<Edit> = tokens
            .iter()
            .filter(|(span, kind)| {
                let text = span.text(doc.src);
                matches!(kind, TokenKind::Scalar(scalar) if !scalar.quoted)
                    && text.contains(['[', ']'])
                    && !text.contains('"')
            })
            .flat_map(|(span, _)| [quote(span.start), quote(span.end)])
            .collect();
        if edits.is_empty() {
            return None;
        }
        apply_edits(doc.src, edits)
    }

    /// `line: text` of the line holding `offset`.
    fn line_of(src: &str, offset: Option<usize>) -> String {
        let Some(offset) = offset else {
            return "?".to_owned();
        };
        let line = src.get(..offset).unwrap_or_default().matches('\n').count() + 1;
        let text: String = src
            .get(line_start(src, offset)..line_end(src, offset))
            .unwrap_or_default()
            .trim()
            .chars()
            .take(100)
            .collect();
        format!("{line}: {text}")
    }

    fn check_corpus_file(path: &Path) -> Outcome {
        let Ok(bytes) = std::fs::read(path) else {
            return Outcome::NotUtf8;
        };
        let Ok(src) = String::from_utf8(bytes) else {
            return Outcome::NotUtf8;
        };
        let doc = match parse(&src) {
            Ok(doc) => doc,
            Err(err) => {
                return Outcome::ParseFailed {
                    jomini_ok: jomini::TextTape::from_slice(src.as_bytes()).is_ok(),
                    location: line_of(&src, Some(err.offset)),
                    message: err.message,
                };
            }
        };
        if let Err(violation) = check_invariants(&doc) {
            return Outcome::Lossless(violation);
        }
        let first = compare_with_jomini(&src, &doc);
        if matches!(first, Ok(None)) {
            return Outcome::Clean;
        }
        if let Some(variant) = quote_bracketed_scalars(&doc) {
            let variant_doc = match parse(&variant) {
                Ok(variant_doc) => variant_doc,
                Err(err) => {
                    let at = line_of(&variant, Some(err.offset));
                    let difference = Difference {
                        category: "CST rejects the bracket-quoted variant",
                        detail: err.to_string(),
                        offset: Some(err.offset),
                        path: String::new(),
                    };
                    return Outcome::Mismatch(difference, at);
                }
            };
            let ours = cst_shape(&src, &doc.root);
            let quoted = cst_shape(&variant, &variant_doc.root);
            if let Some(difference) = first_difference(&ours, &quoted, &mut Vec::new()) {
                let at = line_of(&src, difference.offset);
                let difference = Difference {
                    category: "quoting bracketed scalars changed the CST",
                    ..difference
                };
                return Outcome::Mismatch(difference, at);
            }
            return match compare_with_jomini(&variant, &variant_doc) {
                Ok(None) => Outcome::KnownDivergence(BRACKETED),
                Ok(Some(difference)) => {
                    let at = line_of(&variant, difference.offset);
                    Outcome::Mismatch(difference, at)
                }
                Err(err) => {
                    Outcome::JominiFailed(format!("{err} (after quoting bracketed scalars)"))
                }
            };
        }
        match first {
            Ok(None) => Outcome::Clean,
            Ok(Some(difference)) => {
                let at = line_of(&src, difference.offset);
                Outcome::Mismatch(difference, at)
            }
            Err(err) => Outcome::JominiFailed(err),
        }
    }

    fn corpus_files(roots: &str) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            for sub in ["common", "events", "history"] {
                for entry in walkdir::WalkDir::new(Path::new(root).join(sub))
                    .into_iter()
                    .filter_map(Result::ok)
                {
                    let is_txt = entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"));
                    if !entry.file_type().is_dir() && is_txt {
                        files.push(entry.into_path());
                    }
                }
            }
        }
        files.sort();
        files
    }

    /// Parses every `*.txt` under `common/`, `events/` and `history/` of each
    /// `;`-separated root in `HEARTY_CORPUS`, checks the losslessness and
    /// nesting invariants, and compares the tree level by level (keys,
    /// operators, value kinds, unquoted scalar text) with jomini's tape.
    ///
    /// Run with `cargo test --release corpus_round_trip -- --ignored
    /// --nocapture`. Fails on any losslessness violation or unexplained
    /// structural mismatch. Parse failures are only reported: on the four
    /// reference corpora (vanilla, Millennium Dawn, The Fire Rises, Rising
    /// Tide) every one is a genuinely malformed file — one `{` never closed
    /// (jomini silently closes it at end of file), a stray `}` at the root
    /// (jomini skips it), or a key with an empty value (`fire_only_once =`
    /// followed by the next line's `key = value`).
    ///
    /// Intentional divergences from jomini:
    /// - jomini ends bare scalars at `[`/`]`, splitting `[?var]`,
    ///   `[ROOT.GetName]` and `set_leader_[TAG]`, which the game reads as one
    ///   token; handled by re-comparing with those scalars quoted
    ///   ([`BRACKETED`]).
    /// - jomini cannot parse a root that is a bare list
    ///   (`graphicalculturetype.txt`, `synchronized_dynamic_tokens/*.txt`);
    ///   the CST parses it as bare elements. Reported as "jomini failed".
    /// - The CST rejects what jomini repairs or garbles: an unclosed `{` at
    ///   end of file, an unmatched `}` at the root, an operator with no key
    ///   (jomini turns `=` into a key), `=` after a bare block (jomini skips
    ///   it), and `key = "quoted" { .. }` (jomini errors; the CST reads a
    ///   scalar followed by a bare block).
    /// - Not seen in the corpora: jomini treats `;` as whitespace and only
    ///   ASCII whitespace as whitespace (the CST uses `char::is_whitespace`
    ///   and keeps `;` in scalars), ends `@[` inline math at the first `]`
    ///   (the CST counts bracket depth), drops some empty `{}` blocks, and
    ///   does not form tagged or operator-less keyed blocks inside mixed
    ///   `{ a b = c }` containers.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_round_trip() {
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let files = corpus_files(&roots);
        let outcomes: Vec<(PathBuf, Outcome)> = files
            .into_par_iter()
            .map(|path| {
                let outcome = check_corpus_file(&path);
                (path, outcome)
            })
            .collect();

        let mut clean = 0_usize;
        let mut not_utf8 = Vec::new();
        let mut lossless = Vec::new();
        let mut jomini_failed = Vec::new();
        let mut known: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        let mut failures: BTreeMap<(String, bool), Vec<String>> = BTreeMap::new();
        let mut mismatches: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        for (path, outcome) in &outcomes {
            let file = path.display();
            match outcome {
                Outcome::Clean => clean += 1,
                Outcome::NotUtf8 => not_utf8.push(file.to_string()),
                Outcome::Lossless(violation) => lossless.push(format!("{file}: {violation}")),
                Outcome::JominiFailed(err) => jomini_failed.push(format!("{file}: {err}")),
                Outcome::KnownDivergence(reason) => {
                    known.entry(reason).or_default().push(file.to_string());
                }
                Outcome::ParseFailed {
                    jomini_ok,
                    location,
                    message,
                } => failures
                    .entry((message.clone(), *jomini_ok))
                    .or_default()
                    .push(format!("{file}:{location}")),
                Outcome::Mismatch(difference, at) => mismatches
                    .entry(difference.category)
                    .or_default()
                    .push(format!(
                        "{file}:{at} [{}] {}",
                        difference.path, difference.detail
                    )),
            }
        }

        let mut report = vec![format!(
            "files: {} | clean: {clean} | known jomini divergences: {} | not UTF-8 (skipped): {} \
             | parse failures: {} | CST ok but jomini failed: {} | unexplained mismatches: {} \
             | LOSSLESS VIOLATIONS: {}",
            outcomes.len(),
            known.values().map(Vec::len).sum::<usize>(),
            not_utf8.len(),
            failures.values().map(Vec::len).sum::<usize>(),
            jomini_failed.len(),
            mismatches.values().map(Vec::len).sum::<usize>(),
            lossless.len(),
        )];
        let mut section = |title: String, examples: &[String], limit: usize| {
            report.push(format!("\n{title}: {}", examples.len()));
            report.extend(
                examples
                    .iter()
                    .take(limit)
                    .map(|example| format!("    {example}")),
            );
        };
        section("LOSSLESS VIOLATIONS".to_owned(), &lossless, 30);
        section("not UTF-8".to_owned(), &not_utf8, 5);
        for (reason, files) in &known {
            section(format!("known divergence: {reason}"), files, 3);
        }
        section("CST ok, jomini failed".to_owned(), &jomini_failed, 40);
        for ((message, jomini_ok), examples) in &failures {
            let jomini = if *jomini_ok {
                "jomini accepts"
            } else {
                "jomini also rejects"
            };
            section(
                format!("parse failure `{message}` ({jomini})"),
                examples,
                30,
            );
        }
        for (category, examples) in &mismatches {
            section(
                format!("UNEXPLAINED structural mismatch `{category}`"),
                examples,
                30,
            );
        }
        println!("{}", report.join("\n"));
        assert!(
            lossless.is_empty(),
            "losslessness violated; see the report above"
        );
        assert!(
            mismatches.is_empty(),
            "unexplained structural mismatches with jomini; see the report above"
        );
    }

    // ---------------------------------------------------------------------
    // Differential: the parser before it was rewritten for speed
    // ---------------------------------------------------------------------

    /// `None` if [`parse`] and [`reference::parse`] agree on `src`: the same
    /// tree (every span, comment and variant) or the same error. Otherwise
    /// where their `Debug` renderings first differ.
    fn reference_mismatch(src: &str) -> Option<String> {
        let ours = parse(src).map(|doc| doc.root);
        let theirs = reference::parse(src);
        if ours == theirs {
            return None;
        }
        let (ours, theirs) = (format!("{ours:?}"), format!("{theirs:?}"));
        let at = ours
            .bytes()
            .zip(theirs.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| ours.len().min(theirs.len()));
        let window = |text: &str| {
            let from = text.floor_char_boundary(at.saturating_sub(120));
            let to = text.ceil_char_boundary((at + 120).min(text.len()));
            text.get(from..to).unwrap_or_default().to_owned()
        };
        let input: String = src.chars().take(200).collect();
        Some(format!(
            "input {input:?} ({} bytes)\n  new:       ...{}...\n  reference: ...{}...",
            src.len(),
            window(&ours),
            window(&theirs)
        ))
    }

    /// `src` with 1 to 4 random edits: a piece inserted, a range deleted or
    /// duplicated, or the text cut short.
    fn mutate(src: &str, rng: &mut Rng) -> String {
        let mut text = src.to_owned();
        for _ in 0..=rng.below(4) {
            let at = rng.boundary(&text);
            let end = text.floor_char_boundary(at + rng.below(40)).max(at);
            match rng.below(4) {
                0 => text.insert_str(at, rng.piece()),
                1 => text.replace_range(at..end, ""),
                2 => {
                    let copy = text.get(at..end).unwrap_or_default().to_owned();
                    text.insert_str(end, &copy);
                }
                _ => text.truncate(at),
            }
        }
        text
    }

    /// Every input of up to three [`FUZZ_PIECES`], random longer ones, and
    /// random mutations of realistic snippets: the rewritten parser returns
    /// exactly what the reference parser returns, errors included.
    #[test]
    fn matches_the_reference_parser_on_fuzzed_inputs() {
        let mut failures = Vec::new();
        let mut check = |src: &str| {
            if failures.len() < 10
                && let Some(mismatch) = reference_mismatch(src)
            {
                failures.push(mismatch);
            }
        };
        let mut frontier = vec![String::new()];
        for _ in 0_u32..3 {
            let mut next = Vec::with_capacity(frontier.len() * FUZZ_PIECES.len());
            for prefix in &frontier {
                for piece in FUZZ_PIECES {
                    let src = format!("{prefix}{piece}");
                    check(&src);
                    next.push(src);
                }
            }
            frontier = next;
        }
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0_u32..20_000 {
            let len = 4 + rng.below(60);
            let src: String = std::iter::repeat_with(|| rng.piece()).take(len).collect();
            check(&src);
        }
        let seeds = [
            "focus = {\n\tid = GER_focus\n\ticon = GFX_x # icon\n\tcost = 10\n\
             \tprerequisite = { focus = A focus = B }\n\tavailable = { has_war = yes }\n}\n",
            "country_event = {\r\n\tid = x.1\r\n\ttrigger = {\r\n\t\tOR = { tag = GER tag = ENG }\
             \r\n\t\thas_country_flag = \"quoted \\\" flag\"\r\n\t}\r\n\tcolor = rgb { 1 2 3 }\r\n}\r\n",
            "\u{feff}# header\nk = @[ base * [2 + x] ]\nv = [?global.var] w >= 3 z != 4 q ?= 5\n\
             set_leader_[TAG] = yes\nlist = { a b \"c\" { 1 2 } }\nhsv360 # note\n{ 3 4 }\n",
            "a = { b = { c = { d = { e = f } } } } g = h\u{a0}i = j # last",
        ];
        for seed in seeds {
            check(seed);
            for _ in 0_u32..5_000 {
                check(&mutate(seed, &mut rng));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    /// The fixture mod's script files parse exactly as the reference parser
    /// parses them.
    #[test]
    fn matches_the_reference_parser_on_the_fixture_mod() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test_mod");
        let mut checked = 0_usize;
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let is_txt = entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"));
            let Ok(src) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if is_txt {
                if let Some(mismatch) = reference_mismatch(&src) {
                    panic!("{}: {mismatch}", entry.path().display());
                }
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} fixture files found");
    }

    /// Every byte's class agrees with `char::is_whitespace` and the lexing
    /// rules in the module docs, and every non-ASCII whitespace char starts
    /// with a byte the lexer decodes.
    #[test]
    fn byte_classes_match_the_lexing_rules() {
        for byte in 0..=u8::MAX {
            let expected = match byte {
                b'{' | b'}' | b'=' | b'<' | b'>' | b'#' => ByteClass::Delimiter,
                b'!' | b'?' => ByteClass::MaybeOperator,
                0..=0x7f if char::from(byte).is_whitespace() => ByteClass::Space,
                0..=0x7f => ByteClass::Bare,
                _ => byte_class(byte),
            };
            assert_eq!(byte_class(byte), expected, "byte {byte:#04x}");
            if byte >= 0x80 {
                assert!(
                    matches!(expected, ByteClass::Bare | ByteClass::MaybeSpace),
                    "byte {byte:#04x}"
                );
            }
        }
        let mut lead_bytes = Vec::new();
        for ch in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            let mut buffer = [0; 4];
            let lead = ch.encode_utf8(&mut buffer).as_bytes().first().copied();
            let lead = lead.expect("a char encodes to at least one byte");
            if !ch.is_ascii() && ch.is_whitespace() {
                assert_eq!(byte_class(lead), ByteClass::MaybeSpace, "{ch:?}");
                lead_bytes.push(lead);
            }
        }
        lead_bytes.sort_unstable();
        lead_bytes.dedup();
        let decoded: Vec<u8> = (0x80..=u8::MAX)
            .filter(|&byte| byte_class(byte) == ByteClass::MaybeSpace)
            .collect();
        assert_eq!(
            decoded, lead_bytes,
            "only whitespace lead bytes are decoded"
        );
    }

    /// Parses every corpus file (see [`corpus_round_trip`]) with both
    /// parsers, as is and in random mutations that exercise the error
    /// paths, and fails on any difference in the tree or the error.
    ///
    /// Run with `cargo test --release corpus_matches_the_reference_parser --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_matches_the_reference_parser() {
        const MUTATIONS: usize = 8;
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let files = corpus_files(&roots);
        let outcomes: Vec<(usize, usize, usize, Vec<String>)> = files
            .par_iter()
            .enumerate()
            .filter_map(|(index, path)| {
                let src = std::fs::read_to_string(path).ok()?;
                let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::try_from(index).ok()?);
                let (mut parsed, mut rejected, mut mismatches) = (0, 0, Vec::new());
                let variants = std::iter::once(src.clone())
                    .chain(std::iter::repeat_with(|| mutate(&src, &mut rng)).take(MUTATIONS));
                for variant in variants {
                    if reference::parse(&variant).is_ok() {
                        parsed += 1;
                    } else {
                        rejected += 1;
                    }
                    if let Some(mismatch) = reference_mismatch(&variant) {
                        mismatches.push(format!("{}: {mismatch}", path.display()));
                    }
                }
                Some((1, parsed, rejected, mismatches))
            })
            .collect();
        let files = outcomes.iter().map(|outcome| outcome.0).sum::<usize>();
        let parsed = outcomes.iter().map(|outcome| outcome.1).sum::<usize>();
        let rejected = outcomes.iter().map(|outcome| outcome.2).sum::<usize>();
        let mismatches: Vec<&String> = outcomes.iter().flat_map(|outcome| &outcome.3).collect();
        println!(
            "UTF-8 files: {files} | inputs (files + {MUTATIONS} mutations each): {} | \
             parsed: {parsed} | rejected: {rejected} | MISMATCHES: {}",
            parsed + rejected,
            mismatches.len()
        );
        for mismatch in mismatches.iter().take(10) {
            println!("{mismatch}\n");
        }
        assert!(
            mismatches.is_empty(),
            "the rewrite differs from the reference parser"
        );
    }

    /// Single-threaded parse throughput, in MB/s, of the rewrite and the
    /// reference parser on each `;`-separated root of `HEARTY_CORPUS` (the
    /// files `corpus_round_trip` reads, loaded up front). Each figure is the
    /// median of `HEARTY_BENCH_RUNS` (default 11) timed passes over every
    /// file, parsers alternating, the trees dropped inside the timing.
    ///
    /// Run with `cargo test --release parse_throughput -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a benchmark; needs HEARTY_CORPUS"]
    fn parse_throughput() {
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to measure");
            return;
        };
        let runs: usize = std::env::var("HEARTY_BENCH_RUNS")
            .ok()
            .and_then(|runs| runs.parse().ok())
            .unwrap_or(11);
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            let sources: Vec<String> = corpus_files(root)
                .iter()
                .filter_map(|path| std::fs::read_to_string(path).ok())
                .collect();
            let bytes = sources.iter().map(String::len).sum::<usize>();
            let time = |parse: &dyn Fn(&str) -> bool| {
                let start = std::time::Instant::now();
                for src in &sources {
                    std::hint::black_box(parse(std::hint::black_box(src)));
                }
                start.elapsed().as_secs_f64()
            };
            let new = |src: &str| parse(src).is_ok();
            let old = |src: &str| reference::parse(src).is_ok();
            let (mut new_times, mut old_times) = (Vec::new(), Vec::new());
            time(&new);
            time(&old);
            for _ in 0..runs {
                old_times.push(time(&old));
                new_times.push(time(&new));
            }
            let median = |times: &mut Vec<f64>| {
                times.sort_by(f64::total_cmp);
                times.get(times.len() / 2).copied().unwrap_or(f64::NAN)
            };
            #[expect(clippy::cast_precision_loss, reason = "megabytes, for display")]
            let megabytes = bytes as f64 / 1e6_f64;
            let (new_time, old_time) = (median(&mut new_times), median(&mut old_times));
            println!(
                "{root}: {} files, {megabytes:.1} MB | reference {:.0} MB/s | new {:.0} MB/s | x{:.2}",
                sources.len(),
                megabytes / old_time,
                megabytes / new_time,
                old_time / new_time
            );
        }
    }
}
