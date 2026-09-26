//! Formatter rule: sorts the top-level fields of definition blocks (focuses,
//! decisions, events, ideas, ...) into the canonical order given by
//! [`BlockKind::field_order`](crate::schema::BlockKind::field_order).
//!
//! Only listed fields move, and only among the positions ("slots") listed
//! fields already occupy: unlisted fields and bare elements keep their slots,
//! and repeated fields keep their relative order. A field moves as a whole
//! line unit (see [`cst::line_units`]), together with its attached leading
//! comments and its trailing same-line comment; blank lines and dangling
//! comments between fields stay where they are. The output is therefore a
//! permutation of whole lines of the input, so line endings, indentation and
//! a BOM are all preserved.
//!
//! A block whose entries share a line with each other or with its braces
//! cannot be split into line units, and is left untouched (definition blocks
//! nested inside it are still sorted).

use crate::cst::{self, Block, Document, Edit, Span, Unit, Value};
use crate::schema::{self, FileKind};
use std::collections::BTreeMap;

/// Safety net for the fixpoint loop in [`apply_doc`]. Every pass sorts at
/// least the innermost out-of-order block, sorting a block never unsorts
/// another, and definition blocks nest at most three deep (`focus_tree >
/// focus > offset`, `<character> > instance > <role>`), so real input settles
/// within four passes.
const MAX_PASSES: usize = 16;

/// What sorting the fields of one definition block takes.
#[derive(Debug)]
enum Plan {
    /// The listed fields already appear in canonical order.
    InOrder,
    /// The listed fields are out of order; this edit sorts them.
    Reorder(Edit),
    /// The listed fields are out of order, but the entries cannot be moved
    /// as whole lines, so the block is left as it is.
    Unsplittable,
}

/// Reorders the listed fields of every definition block in `src`, returning
/// the new text and the number of blocks whose fields moved. Returns `src`
/// unchanged (and 0) if it does not parse.
#[cfg(test)]
#[must_use]
pub fn apply(src: &str, file: FileKind) -> (String, usize) {
    let Ok(doc) = cst::parse(src) else {
        return (src.to_owned(), 0);
    };
    apply_doc(&doc, file).map_or_else(|| (src.to_owned(), 0), |(text, moved, _)| (text, moved))
}

/// Reorders the listed fields of every definition block of the parsed `doc`.
/// Returns `None` if no field moves; else the new text, the number of blocks
/// whose fields moved, and the new text's parse when the last pass made one
/// (it does unless [`MAX_PASSES`] ran out).
pub fn apply_doc(doc: &Document<'_>, file: FileKind) -> Option<(String, usize, Option<Block>)> {
    // A definition block nested in another one (an event's options, a
    // character's roles) lies inside a unit its parent may move, so their
    // edits can overlap. Each pass applies the innermost of the overlapping
    // edits, then re-parses; outer blocks are sorted on a later pass.
    let edits = innermost(pass_edits(doc, file));
    if edits.is_empty() {
        return None;
    }
    let mut moved = edits.len();
    let mut text = cst::apply_edits(doc.src, edits)?;
    for _ in 1..MAX_PASSES {
        let Ok(next) = cst::parse(&text) else {
            // A pass permutes whole lines within a block, which re-lex to
            // the same tokens. Should that ever not hold, keep the file as
            // it was rather than break it.
            return None;
        };
        let edits = innermost(pass_edits(&next, file));
        if edits.is_empty() {
            let root = next.root;
            return Some((text, moved, Some(root)));
        }
        let count = edits.len();
        let Some(applied) = cst::apply_edits(&text, edits) else {
            let root = next.root;
            return Some((text, moved, Some(root)));
        };
        moved += count;
        text = applied;
    }
    Some((text, moved, None))
}

/// The edits of a maximal set of pairwise non-overlapping `edits`,
/// preferring the innermost: smallest first, each accepted unless it overlaps
/// one already accepted.
fn innermost(mut edits: Vec<Edit>) -> Vec<Edit> {
    edits.sort_by_key(|edit| edit.span.end.saturating_sub(edit.span.start));
    // Accepted edits never overlap one another, so a new edit overlaps one of
    // them iff it overlaps the accepted edit that starts last before its end.
    let mut accepted: BTreeMap<usize, Edit> = BTreeMap::new();
    for edit in edits {
        let overlaps = accepted
            .range(..edit.span.end)
            .next_back()
            .is_some_and(|(_, previous)| previous.span.end > edit.span.start);
        if !overlaps {
            accepted.insert(edit.span.start, edit);
        }
    }
    accepted.into_values().collect()
}

/// The edit sorting each out-of-order definition block of `doc`.
fn pass_edits(doc: &Document<'_>, file: FileKind) -> Vec<Edit> {
    let mut edits = Vec::new();
    cst::visit_blocks(doc, &mut |path, entry, block| {
        // A tagged block (`rgb { .. }`) is a value, never a definition.
        if !matches!(entry.value, Value::Block(_)) {
            return;
        }
        if let Some(kind) = schema::block_kind(file, path)
            && let Plan::Reorder(edit) = plan(doc.src, block, kind.field_order())
        {
            edits.push(edit);
        }
    });
    edits
}

/// How to sort the fields of `block` into `order`.
fn plan(src: &str, block: &Block, order: &[&str]) -> Plan {
    let ranks: Vec<Option<usize>> = block
        .entries
        .iter()
        .map(|entry| {
            let key = entry.key_str(src)?;
            order.iter().position(|field| *field == key)
        })
        .collect();
    if ranks.iter().flatten().is_sorted() {
        return Plan::InOrder;
    }
    reorder(src, block, &ranks).map_or(Plan::Unsplittable, Plan::Reorder)
}

/// The edit that stably sorts the ranked entries of `block` (`ranks` holds
/// each entry's index in the field order, `None` for unlisted fields and bare
/// elements) into the slots ranked entries occupy. `None` if the block cannot
/// be split into line units that each end with a line terminator.
fn reorder(src: &str, block: &Block, ranks: &[Option<usize>]) -> Option<Edit> {
    // The entries of a single-line block share its brace lines, so
    // `line_units` would refuse it too, but only after scanning back to the
    // start of the line. In a minified file thousands of blocks share one
    // line, and those scans would cost blocks x line length; this check costs
    // the block's length.
    if block.is_single_line(src) {
        return None;
    }
    let units = cst::line_units(src, block)?;
    // A unit moves together with its line terminator; one without (the last
    // line of a file) could only move by inventing one.
    let terminated = units.iter().all(|unit| {
        src.get(unit.start..unit.end)
            .is_some_and(|text| text.ends_with('\n'))
    });
    if !terminated {
        return None;
    }

    let rank = |unit: &Unit| ranks.get(unit.entry).copied().flatten();
    // `sort_by_key` is stable, so repeated fields keep their relative order.
    let mut ranked: Vec<&Unit> = units.iter().filter(|unit| rank(unit).is_some()).collect();
    ranked.sort_by_key(|unit| rank(unit));
    let mut sorted = ranked.into_iter();
    // `placed[slot]` is the unit that ends up in `units[slot]`'s place. There
    // are exactly as many sorted units as ranked slots, so the fallback to
    // the slot's own unit is never taken.
    let placed: Vec<&Unit> = units
        .iter()
        .map(|slot| {
            if rank(slot).is_some() {
                sorted.next().unwrap_or(slot)
            } else {
                slot
            }
        })
        .collect();

    // Rewrite only the slots from the first to the last that changes, which
    // keeps a parent's edit clear of child blocks that do not move.
    let changes = |(unit, slot): (&&Unit, &Unit)| unit.entry != slot.entry;
    let first = placed.iter().zip(&units).position(changes)?;
    let last = placed.iter().zip(&units).rposition(changes)?;
    let slots = units.get(first..=last)?;
    let start = slots.first()?.start;
    let mut replacement = String::with_capacity(slots.last()?.end.saturating_sub(start));
    let mut cursor = start;
    for (slot, unit) in slots.iter().zip(placed.get(first..=last)?) {
        // The gap before the slot (blank lines, dangling comments) stays.
        replacement.push_str(src.get(cursor..slot.start)?);
        replacement.push_str(src.get(unit.start..unit.end)?);
        cursor = slot.end;
    }
    Some(Edit {
        replacement,
        span: Span { end: cursor, start },
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{Plan, apply, innermost, plan};
    use crate::cst::{self, Block, Edit, Span, Value, line_end, line_start};
    use crate::schema::{self, BlockKind, FileKind};
    use rayon::prelude::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::iter;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Out-of-order blocks left untouched to show per corpus.
    const SKIP_EXAMPLES: usize = 12;

    /// What checking one corpus file found.
    #[derive(Debug, Default)]
    struct FileReport {
        changed: bool,
        not_utf8: bool,
        /// Blocks reordered, by kind.
        reordered: BTreeMap<String, usize>,
        /// Definition blocks `line_units` cannot split, by kind:
        /// (all of them, those whose fields are out of order).
        skipped: BTreeMap<String, (usize, usize)>,
        /// Out-of-order definition blocks left untouched: (reason,
        /// `file:line: text`).
        skipped_examples: Vec<(&'static str, String)>,
        unparsable: bool,
        violations: Vec<String>,
    }

    /// The texts compared by [`compare_block`], and the kind of file they are.
    struct Sides<'src> {
        after: &'src str,
        before: &'src str,
        file: FileKind,
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// `lines`, each terminated by `\n`.
    fn text(lines: &[&str]) -> String {
        lines.iter().flat_map(|line| [*line, "\n"]).collect()
    }

    /// The lines of `text` with their terminators, sorted.
    fn sorted_lines(text: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
        lines.sort_unstable();
        lines
    }

    /// Runs [`apply`] and checks what every output must satisfy: applying it
    /// again changes nothing, and it is a permutation of the input's lines
    /// (terminators included).
    fn run(src: &str, file: FileKind) -> (String, usize) {
        let (out, count) = apply(src, file);
        assert_eq!(
            apply(&out, file),
            (out.clone(), 0),
            "not idempotent for {src:?}"
        );
        assert_eq!(
            sorted_lines(&out),
            sorted_lines(src),
            "lines changed for {src:?}"
        );
        (out, count)
    }

    /// Asserts that `src` comes out of [`run`] unchanged.
    fn assert_untouched(src: &str, file: FileKind) {
        assert_eq!(run(src, file), (src.to_owned(), 0));
    }

    // ---------------------------------------------------------------------
    // Reordering
    // ---------------------------------------------------------------------

    #[test]
    fn decision_icon_moves_before_complete_effect() {
        let src = text(&[
            "MLT_decisions = {",
            "\tMLT_decision = {",
            "\t\tcomplete_effect = {",
            "\t\t\tadd_political_power = 50",
            "\t\t}",
            "\t\ticon = generic_decision",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "MLT_decisions = {",
            "\tMLT_decision = {",
            "\t\ticon = generic_decision",
            "\t\tcomplete_effect = {",
            "\t\t\tadd_political_power = 50",
            "\t\t}",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));
    }

    #[test]
    fn repeated_fields_keep_their_relative_order() {
        let src = text(&[
            "focus_tree = {",
            "\tfocus = {",
            "\t\tprerequisite = { focus = a }",
            "\t\tid = c",
            "\t\tprerequisite = { focus = b }",
            "\t\tcost = 10",
            "\t\tx = 1",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "focus_tree = {",
            "\tfocus = {",
            "\t\tid = c",
            "\t\tprerequisite = { focus = a }",
            "\t\tprerequisite = { focus = b }",
            "\t\tx = 1",
            "\t\tcost = 10",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::NationalFocus), (expected, 1));

        let src = text(&[
            "country_event = {",
            "\toption = { name = e.1.a }",
            "\toption = { name = e.1.b }",
            "\tid = e.1",
            "}",
        ]);
        let expected = text(&[
            "country_event = {",
            "\tid = e.1",
            "\toption = { name = e.1.a }",
            "\toption = { name = e.1.b }",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Events), (expected, 1));
    }

    #[test]
    fn unlisted_fields_and_bare_elements_keep_their_slots() {
        // Listed: complete_effect, "icon" (matched unquoted), ai_will_do.
        let src = text(&[
            "cat = {",
            "\tdec = {",
            "\t\tcomplete_effect = { }",
            "\t\tcustom_field = yes",
            "\t\tsome_flag",
            "\t\t\"icon\" = generic",
            "\t\tai_will_do = { factor = 1 }",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "cat = {",
            "\tdec = {",
            "\t\t\"icon\" = generic",
            "\t\tcustom_field = yes",
            "\t\tsome_flag",
            "\t\tai_will_do = { factor = 1 }",
            "\t\tcomplete_effect = { }",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));
    }

    #[test]
    fn attached_comments_travel_with_their_field() {
        let src = text(&[
            "cat = {",
            "\tdec = {",
            "\t\t# Grants the bonus.",
            "\t\t# Second line.",
            "\t\tcomplete_effect = {",
            "\t\t\tadd_political_power = 50 # inner",
            "\t\t}",
            "\t\t# The icon.",
            "\t\ticon = generic",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "cat = {",
            "\tdec = {",
            "\t\t# The icon.",
            "\t\ticon = generic",
            "\t\t# Grants the bonus.",
            "\t\t# Second line.",
            "\t\tcomplete_effect = {",
            "\t\t\tadd_political_power = 50 # inner",
            "\t\t}",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));
    }

    #[test]
    fn dangling_comments_and_blank_lines_stay_in_place() {
        let src = text(&[
            "cat = {",
            "\tdec = { # on the brace line",
            "",
            "\t\tcomplete_effect = { }",
            "",
            "\t\t# Dangling: a blank line separates it from `icon`.",
            "",
            "\t\ticon = generic",
            "\t\tallowed = { tag = MLT }",
            "\t\t# Dangling before the closing brace.",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "cat = {",
            "\tdec = { # on the brace line",
            "",
            "\t\ticon = generic",
            "",
            "\t\t# Dangling: a blank line separates it from `icon`.",
            "",
            "\t\tallowed = { tag = MLT }",
            "\t\tcomplete_effect = { }",
            "\t\t# Dangling before the closing brace.",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));
    }

    #[test]
    fn trailing_comments_travel_with_their_field() {
        let src = text(&[
            "cat = {",
            "\tdec = {",
            "\t\tcomplete_effect = { } # does nothing yet",
            "\t\ticon = generic # placeholder",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "cat = {",
            "\tdec = {",
            "\t\ticon = generic # placeholder",
            "\t\tcomplete_effect = { } # does nothing yet",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));
    }

    #[test]
    fn crlf_mixed_line_endings_and_bom_are_preserved() {
        let lines = [
            "cat = {",
            "\tdec = {",
            "\t\tcomplete_effect = {",
            "\t\t\tadd_political_power = 50",
            "\t\t}",
            "\t\ticon = generic",
            "\t}",
            "}",
        ];
        let src = format!("\u{feff}{}", text(&lines).replace('\n', "\r\n"));
        let expected = format!(
            "\u{feff}{}",
            text(&[
                "cat = {",
                "\tdec = {",
                "\t\ticon = generic",
                "\t\tcomplete_effect = {",
                "\t\t\tadd_political_power = 50",
                "\t\t}",
                "\t}",
                "}",
            ])
            .replace('\n', "\r\n")
        );
        assert_eq!(run(&src, FileKind::Decisions), (expected, 1));

        // Every line keeps its own terminator when lines endings are mixed.
        let src = "cat = {\n\tdec = {\r\n\t\tcomplete_effect = { }\n\t\ticon = generic\r\n\t}\n}";
        let expected =
            "cat = {\n\tdec = {\r\n\t\ticon = generic\r\n\t\tcomplete_effect = { }\n\t}\n}";
        assert_eq!(run(src, FileKind::Decisions), (expected.to_owned(), 1));
    }

    #[test]
    fn nested_definition_blocks_are_all_sorted() {
        // The event moves `option`, `desc` and the fields around them, so its
        // edit overlaps theirs and lands on a later pass.
        let src = text(&[
            "country_event = {",
            "\toption = {",
            "\t\tai_chance = { base = 1 }",
            "\t\tname = e.1.a",
            "\t}",
            "\ttitle = e.1.t",
            "\tid = e.1",
            "\tdesc = {",
            "\t\ttrigger = { tag = GER }",
            "\t\ttext = e.1.d",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "country_event = {",
            "\tid = e.1",
            "\ttitle = e.1.t",
            "\tdesc = {",
            "\t\ttext = e.1.d",
            "\t\ttrigger = { tag = GER }",
            "\t}",
            "\toption = {",
            "\t\tname = e.1.a",
            "\t\tai_chance = { base = 1 }",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Events), (expected, 3));
    }

    #[test]
    fn characters_are_sorted_three_levels_deep() {
        let src = text(&[
            "characters = {",
            "\tGER_a = {",
            "\t\tinstance = {",
            "\t\t\tadvisor = {",
            "\t\t\t\tidea_token = GER_a_token",
            "\t\t\t\tslot = political_advisor",
            "\t\t\t}",
            "\t\t\tallowed = { tag = GER }",
            "\t\t}",
            "\t\tname = GER_a",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "characters = {",
            "\tGER_a = {",
            "\t\tname = GER_a",
            "\t\tinstance = {",
            "\t\t\tallowed = { tag = GER }",
            "\t\t\tadvisor = {",
            "\t\t\t\tslot = political_advisor",
            "\t\t\t\tidea_token = GER_a_token",
            "\t\t\t}",
            "\t\t}",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Characters), (expected, 3));
    }

    #[test]
    fn count_is_the_number_of_blocks_reordered() {
        let src = text(&[
            "cat = {",
            "\ta = {",
            "\t\tcomplete_effect = { }",
            "\t\ticon = x",
            "\t}",
            "\tb = {",
            "\t\ticon = x",
            "\t\tcomplete_effect = { }",
            "\t}",
            "\tc = {",
            "\t\tai_will_do = { }",
            "\t\tvisible = { }",
            "\t}",
            "}",
        ]);
        let (out, count) = run(&src, FileKind::Decisions);
        assert_eq!(count, 2);
        assert_eq!(
            out,
            text(&[
                "cat = {",
                "\ta = {",
                "\t\ticon = x",
                "\t\tcomplete_effect = { }",
                "\t}",
                "\tb = {",
                "\t\ticon = x",
                "\t\tcomplete_effect = { }",
                "\t}",
                "\tc = {",
                "\t\tvisible = { }",
                "\t\tai_will_do = { }",
                "\t}",
                "}",
            ])
        );
    }

    // ---------------------------------------------------------------------
    // Left alone
    // ---------------------------------------------------------------------

    #[test]
    fn blocks_whose_entries_share_lines_are_untouched() {
        for src in [
            "cat = {\n\tdec = {\n\t\tcomplete_effect = { } icon = generic\n\t\tallowed = { }\n\t}\n}\n",
            "cat = {\n\tdec = { complete_effect = { } icon = generic }\n}\n",
            "cat = {\n\tdec = { complete_effect = { }\n\t\ticon = generic\n\t}\n}\n",
            "cat = {\n\tdec = {\n\t\tcomplete_effect = { }\n\t\ticon = generic }\n}\n",
        ] {
            assert_untouched(src, FileKind::Decisions);
        }
        // An unsplittable event still has its option sorted.
        let src = text(&[
            "country_event = {",
            "\toption = {",
            "\t\tai_chance = { base = 1 }",
            "\t\tname = e.1.a",
            "\t} id = e.1",
            "}",
        ]);
        let expected = text(&[
            "country_event = {",
            "\toption = {",
            "\t\tname = e.1.a",
            "\t\tai_chance = { base = 1 }",
            "\t} id = e.1",
            "}",
        ]);
        assert_eq!(run(&src, FileKind::Events), (expected, 1));
    }

    #[test]
    fn non_definition_files_and_blocks_are_untouched() {
        let decision = text(&[
            "cat = {",
            "\tdec = {",
            "\t\tcomplete_effect = { }",
            "\t\ticon = generic",
            "\t}",
            "}",
        ]);
        assert_untouched(&decision, FileKind::Other);
        assert_untouched(&decision, FileKind::Ideas);
        // A category of a decisions file is not a definition, nor is a block
        // inside a decision, nor an event fired from inside another event,
        // even when their keys happen to be listed fields out of order.
        assert_untouched(
            &text(&[
                "cat = {",
                "\tcomplete_effect = { }",
                "\ticon = generic",
                "}",
            ]),
            FileKind::Decisions,
        );
        assert_untouched(
            &text(&[
                "cat = {",
                "\tdec = {",
                "\t\tcomplete_effect = {",
                "\t\t\tcost = 1",
                "\t\t\tname = x",
                "\t\t}",
                "\t}",
                "}",
            ]),
            FileKind::Decisions,
        );
        assert_untouched(
            &text(&[
                "add_namespace = e",
                "country_event = {",
                "\tid = e.1",
                "\timmediate = {",
                "\t\tcountry_event = {",
                "\t\t\ttrigger = { always = yes }",
                "\t\t\tid = e.2",
                "\t\t}",
                "\t}",
                "}",
            ]),
            FileKind::Events,
        );
    }

    #[test]
    fn tagged_blocks_are_not_definitions() {
        assert_untouched(
            &text(&[
                "cat = {",
                "\tdec = tagged {",
                "\t\tcomplete_effect = { }",
                "\t\ticon = generic",
                "\t}",
                "}",
            ]),
            FileKind::Decisions,
        );
    }

    #[test]
    fn sorted_input_is_untouched() {
        assert_untouched(
            &text(&[
                "cat = {",
                "\tdec = {",
                "\t\ticon = generic",
                "\t\tcustom_field = yes",
                "\t\tcomplete_effect = { }",
                "\t}",
                "}",
            ]),
            FileKind::Decisions,
        );
        assert_untouched("", FileKind::Decisions);
    }

    #[test]
    fn unparsable_input_is_returned_unchanged() {
        for src in [
            "cat = {\n\tdec = {\n\t\tcomplete_effect = { }\n\t\ticon = generic\n\t}\n",
            "cat = {\n\tdec = {\n\t\tcomplete_effect = { }\n\t\ticon = \n\t}\n}\n",
            "}\ncat = {\n\tdec = {\n\t\tcomplete_effect = { }\n\t\ticon = generic\n\t}\n}\n",
        ] {
            assert_eq!(apply(src, FileKind::Decisions), (src.to_owned(), 0));
        }
    }

    #[test]
    fn innermost_prefers_the_smallest_disjoint_edits() {
        let edit = |start: usize, end: usize| Edit {
            replacement: String::new(),
            span: Span { end, start },
        };
        let spans: Vec<(usize, usize)> = innermost(vec![
            edit(0, 10),
            edit(9, 12),
            edit(2, 4),
            edit(4, 8),
            edit(3, 6),
        ])
        .iter()
        .map(|edit| (edit.span.start, edit.span.end))
        .collect();
        // (2, 4) and (4, 8) touch without overlapping; (3, 6) overlaps
        // (2, 4); (0, 10) contains them.
        assert_eq!(spans, [(2, 4), (4, 8), (9, 12)]);
    }

    // ---------------------------------------------------------------------
    // Performance
    // ---------------------------------------------------------------------

    /// The fastest of three runs of [`apply`] on `src`, which must come out
    /// unchanged.
    fn fastest_untouched(src: &str, file: FileKind) -> Duration {
        iter::repeat_with(|| {
            let started = Instant::now();
            let result = apply(src, file);
            let elapsed = started.elapsed();
            assert!(result == (src.to_owned(), 0), "a single-line block moved");
            elapsed
        })
        .take(3)
        .min()
        .unwrap_or_default()
    }

    #[test]
    fn out_of_order_single_line_blocks_on_one_line_take_linear_time() {
        // Thousands of single-line definition blocks on one physical line, as
        // in a minified or generated file. None can be split into line units,
        // so the out-of-order ones must be turned down as cheaply as the
        // in-order ones are: scanning each back to the start of the shared
        // line would cost blocks x line length.
        const BLOCKS: usize = 8_000;
        let decisions = |body: &str| {
            let blocks = format!(" decision = {{ {body} }}").repeat(BLOCKS);
            format!("cat = {{{blocks} }}\n")
        };
        let paths = |body: &str| {
            let blocks = format!(" path = {{ {body} }}").repeat(BLOCKS);
            format!("technologies = {{ tech = {{{blocks} }} }}\n")
        };
        for (file, in_order, out_of_order) in [
            (
                FileKind::Decisions,
                decisions("icon = x complete_effect = { }"),
                decisions("complete_effect = { } icon = x"),
            ),
            (
                FileKind::Technologies,
                paths("leads_to_tech = x research_cost_coeff = 1"),
                paths("research_cost_coeff = 1 leads_to_tech = x"),
            ),
        ] {
            let in_order = fastest_untouched(&in_order, file);
            let out_of_order = fastest_untouched(&out_of_order, file);
            // Generous bounds: with the quadratic scan, the out-of-order case
            // was 70 (release) to 540 (debug) times slower at this size.
            assert!(
                out_of_order <= in_order * 4 + Duration::from_millis(50),
                "{file:?}: out of order {out_of_order:?}, in order {in_order:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Corpus
    // ---------------------------------------------------------------------

    /// Every `*.txt` under `common/`, `events/` and `history/` of `root` that
    /// [`schema::file_kind`] classifies, sorted by path.
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

    /// Every comment of the file, sorted.
    fn all_comments<'src>(src: &'src str, root: &Block) -> Vec<&'src str> {
        let mut texts = Vec::new();
        let mut stack = vec![root];
        while let Some(block) = stack.pop() {
            texts.extend(block.comments.iter().map(|span| span.text(src)));
            stack.extend(
                block
                    .entries
                    .iter()
                    .filter_map(|entry| entry.value.as_block()),
            );
        }
        texts.sort_unstable();
        texts
    }

    /// Checks that `after` is `before` with only the listed fields of a
    /// definition block permuted: entries are matched by key and occurrence
    /// (so same-key entries must keep their relative order), every unlisted
    /// entry and every entry of a non-definition block keeps its index,
    /// scalar entries and the text before each nested `{` are unchanged,
    /// each block keeps its comments, and nested blocks pass the same check.
    fn compare_block<'src>(
        sides: &Sides<'src>,
        (before, after): (&Block, &Block),
        kind: Option<BlockKind>,
        path: &mut Vec<&'src str>,
        violations: &mut Vec<String>,
    ) {
        let at = path.join(" > ");
        if before.entries.len() != after.entries.len() {
            violations.push(format!(
                "[{at}] {} entries became {}",
                before.entries.len(),
                after.entries.len()
            ));
            return;
        }
        let comments = |src, block: &Block| {
            let mut texts: Vec<&str> = block.comments.iter().map(|span| span.text(src)).collect();
            texts.sort_unstable();
            texts
        };
        if comments(sides.before, before) != comments(sides.after, after) {
            violations.push(format!("[{at}] the block's comments changed"));
        }
        let mut by_key: BTreeMap<Option<&str>, VecDeque<usize>> = BTreeMap::new();
        for (index, entry) in before.entries.iter().enumerate() {
            by_key
                .entry(entry.key_str(sides.before))
                .or_default()
                .push_back(index);
        }
        for (index, new) in after.entries.iter().enumerate() {
            let key = new.key_str(sides.after);
            let Some(old_index) = by_key.get_mut(&key).and_then(VecDeque::pop_front) else {
                violations.push(format!("[{at}] entry {key:?} appeared at {index}"));
                return;
            };
            let old = before.entries.get(old_index).expect("index from enumerate");
            let listed = kind
                .zip(key)
                .is_some_and(|(kind, key)| kind.field_order().contains(&key));
            if !listed && old_index != index {
                violations.push(format!(
                    "[{at}] unlisted entry {key:?} moved from {old_index} to {index}"
                ));
            }
            match (&old.value, &new.value) {
                (Value::Scalar(_), Value::Scalar(_)) => {
                    if old.span.text(sides.before) != new.span.text(sides.after) {
                        violations.push(format!("[{at}] entry {key:?} #{index} changed"));
                    }
                }
                (Value::Block(old_block), Value::Block(new_block))
                | (
                    Value::Tagged {
                        block: old_block, ..
                    },
                    Value::Tagged {
                        block: new_block, ..
                    },
                ) => {
                    let head = |src: &'src str, start: usize, block: &Block| {
                        src.get(start..block.open.unwrap_or(start))
                    };
                    if head(sides.before, old.span.start, old_block)
                        != head(sides.after, new.span.start, new_block)
                    {
                        violations.push(format!("[{at}] head of entry {key:?} changed"));
                    }
                    path.push(old.key_str(sides.before).unwrap_or_default());
                    let child = if matches!(old.value, Value::Block(_)) {
                        schema::block_kind(sides.file, path)
                    } else {
                        None
                    };
                    compare_block(sides, (old_block, new_block), child, path, violations);
                    path.pop();
                }
                (Value::Block(_) | Value::Scalar(_) | Value::Tagged { .. }, _) => {
                    violations.push(format!("[{at}] entry {key:?} changed its kind of value"));
                }
            }
        }
    }

    /// Why `line_units` cannot split `block`, or why [`plan`] still refuses.
    fn skip_reason(src: &str, block: &Block) -> &'static str {
        if cst::line_units(src, block).is_some() {
            return "a unit lacks a line terminator";
        }
        if block.is_single_line(src) {
            return "single-line block";
        }
        let (Some(open), Some(close)) = (block.open, block.close) else {
            return "root block";
        };
        if block
            .entries
            .first()
            .is_some_and(|entry| line_start(src, entry.span.start) <= open)
        {
            return "an entry on the `{` line";
        }
        if block
            .entries
            .last()
            .is_some_and(|entry| close < line_end(src, entry.span.end))
        {
            return "an entry on the `}` line";
        }
        let shared = block.entries.windows(2).any(|pair| match pair {
            [first, second] => line_start(src, second.span.start) <= first.span.end,
            _ => false,
        });
        if shared {
            "entries share a line"
        } else {
            "a trailing comment behind a lone CR"
        }
    }

    /// `line: text` of the line holding `offset`.
    fn line_of(src: &str, offset: usize) -> String {
        let line = src.get(..offset).unwrap_or_default().matches('\n').count() + 1;
        let text: String = src
            .get(line_start(src, offset)..line_end(src, offset))
            .unwrap_or_default()
            .trim()
            .chars()
            .take(90)
            .collect();
        format!("{line}: {text}")
    }

    fn check_corpus_file(root: &Path, path: &Path, file: FileKind) -> FileReport {
        let mut report = FileReport::default();
        let name = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let Ok(src) = std::fs::read_to_string(path) else {
            report.not_utf8 = true;
            return report;
        };
        let (out, count) = apply(&src, file);
        let Ok(before) = cst::parse(&src) else {
            report.unparsable = true;
            if out != src || count != 0 {
                report
                    .violations
                    .push(format!("{name}: unparsable input changed"));
            }
            return report;
        };
        report.changed = out != src;

        // (a) The output parses.
        let after = match cst::parse(&out) {
            Ok(after) => after,
            Err(err) => {
                report
                    .violations
                    .push(format!("{name}: output does not parse: {err}"));
                return report;
            }
        };
        // (b) Idempotent.
        let (again, again_count) = apply(&out, file);
        if again != out || again_count != 0 {
            report.violations.push(format!(
                "{name}: not idempotent ({again_count} blocks moved on a second run)"
            ));
        }
        // (c) Semantics preserved: a permutation of whole lines, the same
        // comments, and the same entries with only listed fields moved.
        if sorted_lines(&src) != sorted_lines(&out) {
            report
                .violations
                .push(format!("{name}: not a permutation of the input's lines"));
        }
        if all_comments(&src, &before.root) != all_comments(&out, &after.root) {
            report
                .violations
                .push(format!("{name}: the file's comments changed"));
        }
        let mut violations = Vec::new();
        compare_block(
            &Sides {
                after: &out,
                before: &src,
                file,
            },
            (&before.root, &after.root),
            None,
            &mut Vec::new(),
            &mut violations,
        );
        report.violations.extend(
            violations
                .into_iter()
                .map(|violation| format!("{name}: {violation}")),
        );

        // (d) Statistics, and the count against the blocks needing a sort.
        let mut expected = 0_usize;
        cst::visit_blocks(&before, &mut |path, entry, block| {
            if !matches!(entry.value, Value::Block(_)) {
                return;
            }
            let Some(kind) = schema::block_kind(file, path) else {
                return;
            };
            let kind_name = format!("{kind:?}");
            let planned = plan(&src, block, kind.field_order());
            let left_out_of_order = matches!(planned, Plan::Unsplittable);
            if matches!(planned, Plan::Reorder(_)) {
                expected += 1;
                *report.reordered.entry(kind_name.clone()).or_default() += 1;
            }
            if cst::line_units(&src, block).is_none() {
                let skipped = report.skipped.entry(kind_name).or_default();
                skipped.0 += 1;
                if left_out_of_order {
                    skipped.1 += 1;
                }
            }
            if left_out_of_order {
                report.skipped_examples.push((
                    skip_reason(&src, block),
                    format!("{name}:{}", line_of(&src, entry.span.start)),
                ));
            }
        });
        if expected != count {
            report.violations.push(format!(
                "{name}: {count} blocks reported moved, {expected} were out of order"
            ));
        }
        report
    }

    /// Runs [`apply`] over every `*.txt` under `common/`, `events/` and
    /// `history/` of each `;`-separated root in `HEARTY_CORPUS` and checks
    /// that (a) the output parses, (b) the rule is idempotent, and (c) it
    /// preserves semantics: the output is a permutation of the input's lines
    /// with the same comments, and block by block holds the same entries with
    /// only listed fields of definition blocks moved (see [`compare_block`]).
    /// Also checks that the reported count is the number of out-of-order
    /// splittable definition blocks, and prints per-corpus statistics:
    /// files changed, blocks reordered by kind, and definition blocks that
    /// `line_units` cannot split, with examples of out-of-order ones.
    ///
    /// Run with `cargo test --release corpus_field_order -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_field_order() {
        let Ok(roots) = std::env::var("HEARTY_CORPUS") else {
            println!("HEARTY_CORPUS is not set; nothing to check");
            return;
        };
        let mut violations: Vec<String> = Vec::new();
        for root in roots
            .split(';')
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            let root = Path::new(root);
            let reports: Vec<FileReport> = corpus_files(root)
                .into_par_iter()
                .map(|(path, file)| check_corpus_file(root, &path, file))
                .collect();

            let mut reordered: BTreeMap<String, usize> = BTreeMap::new();
            let mut skipped: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();
            let mut examples: Vec<String> = Vec::new();
            for report in &reports {
                for (kind, count) in &report.reordered {
                    *reordered.entry(kind.clone()).or_default() += count;
                }
                for (kind, (all, out_of_order)) in &report.skipped {
                    let total = skipped.entry(kind.clone()).or_default();
                    total.0 += all;
                    total.1 += out_of_order;
                }
                for (reason, example) in &report.skipped_examples {
                    *reasons.entry(reason).or_default() += 1;
                    examples.push(format!("{example} [{reason}]"));
                }
                violations.extend(report.violations.iter().cloned());
            }
            let count = |predicate: fn(&FileReport) -> bool| {
                reports.iter().filter(|report| predicate(report)).count()
            };
            let join = |map: &BTreeMap<String, usize>| {
                map.iter()
                    .map(|(kind, count)| format!("{kind} {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!(
                "\n== {}\nfiles: {} | changed: {} | not UTF-8: {} | unparsable: {} | violations: {}",
                root.display(),
                reports.len(),
                count(|report| report.changed),
                count(|report| report.not_utf8),
                count(|report| report.unparsable),
                reports
                    .iter()
                    .map(|report| report.violations.len())
                    .sum::<usize>(),
            );
            println!(
                "blocks reordered: {} ({})",
                reordered.values().sum::<usize>(),
                join(&reordered)
            );
            println!(
                "definition blocks line_units cannot split: {} (out of order: {})",
                skipped.values().map(|(all, _)| all).sum::<usize>(),
                skipped
                    .values()
                    .map(|(_, out_of_order)| out_of_order)
                    .sum::<usize>(),
            );
            for (kind, (all, out_of_order)) in &skipped {
                println!("    {kind}: {all} (out of order: {out_of_order})");
            }
            println!("out-of-order blocks left untouched, by reason:");
            for (reason, count) in &reasons {
                println!("    {reason}: {count}");
            }
            for example in examples.iter().take(SKIP_EXAMPLES) {
                println!("    {example}");
            }
        }
        if !violations.is_empty() {
            println!("\nVIOLATIONS: {}", violations.len());
            for violation in violations.iter().take(50) {
                println!("    {violation}");
            }
        }
        assert!(violations.is_empty(), "see the violations above");
    }
}
