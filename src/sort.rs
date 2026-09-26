//! Formatter rule: sorts the focuses of each focus tree and the events of an
//! event file.
//!
//! - Focuses (the `focus` blocks of a top-level `focus_tree`) are sorted so
//!   each follows the focus it is positioned relative to
//!   (`relative_position_id`) and its prerequisites; otherwise they keep their
//!   order.
//! - Events (top-level `<event_type> = { .. }` blocks) are sorted so each
//!   group of events connected by the events they fire stays together:
//!   groups come in natural order of their earliest event id (`e.2` before
//!   `e.10`), and events within a group in that order too, after the events
//!   that fire them.
//!
//! Both use [`priority_toposort`], which is stable, so sorting sorted blocks
//! changes nothing.
//!
//! Blocks are found in the CST, and only a block with an `id` entry of its
//! own moves. It moves as a whole line unit (see [`cst::line_units`]): its
//! own lines, the comment lines attached above it and a comment after it on
//! its last line. Anything else after it on its last line (another entry,
//! or the `}` closing the focus tree) is not part of it and stays where it
//! is, starting a line of its own (every moved block ends its line). What
//! lies between two moving blocks stays where it is: a gap of blank lines
//! becomes exactly one blank line, and any other gap (another entry, a
//! comment separated by a blank line) is kept as it is.
//!
//! A block that would move must start its own line. If one does not (it
//! follows a tree's `{`, another entry or another block's `}` on its line),
//! its focus tree is left as it is, though the file's other trees are still
//! sorted; in an event file, the file is left as it is. A whole file is
//! also left as it is when it does not parse, when two blocks that would
//! move share an id (in one focus tree, or in an event file), or when the
//! order the blocks of a focus tree or an event file must follow has a
//! cycle.

use crate::cst::{self, Block, Document, Edit, Span, Value};
use crate::schema::EVENT_TYPES;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// The UTF-8 byte order mark; a leading one belongs to no line unit.
const BOM: char = '\u{feff}';

/// A focus found in a focus tree: (entry index, id, the focus it is
/// positioned relative to, its prerequisites).
type FoundFocus<'src> = (usize, &'src str, Option<&'src str>, Vec<&'src str>);

/// A focus tree whose focuses cannot be ordered, which leaves its file
/// unsorted (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unsortable;

/// Lexicographically-ordered piece of a natural-sort key. Variant order
/// (`Number < Text`) is load-bearing: ASCII digits sort before letters, and
/// the derived `Ord` compares variants by declaration position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NaturalPart {
    Number(u64),
    Text(String),
}

/// Byte length of a leading BOM (0 if there is none).
fn bom_len(src: &str) -> usize {
    if src.starts_with(BOM) {
        BOM.len_utf8()
    } else {
        0
    }
}

/// Sorts the events of an event file (see the module docs). Returns the new
/// text and how many events changed position, or `None` if the text stays
/// as it is.
pub fn events(doc: &Document<'_>) -> Option<(String, usize)> {
    let src = doc.src;
    let root = &doc.root;
    // (entry index, id, ids of the events it fires), in file order.
    let mut found: Vec<(usize, &str, Vec<&str>)> = Vec::new();
    for (index, entry) in root.entries.iter().enumerate() {
        let (Some(key), Value::Block(body)) = (entry.key_str(src), &entry.value) else {
            continue;
        };
        if !EVENT_TYPES.contains(&key) {
            continue;
        }
        let Some(id) = own_id(src, body) else {
            continue;
        };
        let mut fired = Vec::new();
        for field in &body.entries {
            match field.key_str(src) {
                Some("id") => {}
                Some(key) if EVENT_TYPES.contains(&key) => {
                    fired_event(src, &field.value, &mut fired);
                }
                Some(_) | None => {
                    if let Value::Block(inner) = &field.value {
                        fired_events(src, inner, &mut fired);
                    }
                }
            }
        }
        found.push((index, id, fired));
    }
    if found.is_empty() {
        return None;
    }

    // Nodes in natural order of their ids: `priority_toposort` breaks ties
    // by node, so each connected group comes out in the order of its
    // naturally-earliest event, and its events in natural order where the
    // events they fire allow. The sort is stable, so ids with equal keys
    // (`e.01`, `e.1`) keep their file order.
    let mut by_name: Vec<usize> = (0..found.len()).collect();
    by_name.sort_by_cached_key(|&event| found.get(event).map(|(_, id, _)| natural_key(id)));
    let mut node_of: HashMap<&str, usize> = HashMap::with_capacity(found.len());
    for (node, &event) in by_name.iter().enumerate() {
        let (_, id, _) = found.get(event)?;
        if node_of.insert(id, node).is_some() {
            // Two events share an id: which one another fires is ambiguous.
            return None;
        }
    }
    let mut edges = Vec::new();
    for (_, id, fired) in &found {
        let from = *node_of.get(id)?;
        edges.extend(
            fired
                .iter()
                .filter_map(|target| Some((from, *node_of.get(target)?))),
        );
    }
    let sorted = priority_toposort(found.len(), &edges)?;

    let indices: Vec<usize> = found.iter().map(|(index, ..)| *index).collect();
    let blocks = own_line_units(src, root, &indices)?;
    // `sorted` holds nodes; map them back to the blocks, in file order.
    let order: Vec<usize> = sorted
        .iter()
        .map(|&node| by_name.get(node).copied())
        .collect::<Option<_>>()?;
    let (edit, moved) = rearrange(src, &blocks, &order, newline(src))?;
    Some((cst::apply_edits(src, vec![edit])?, moved))
}

/// Records the event `value` fires: the `id` entries of `<event_type> = {
/// id = .. }`, or the scalar of the shorthand `<event_type> = id`.
fn fired_event<'src>(src: &'src str, value: &Value, out: &mut Vec<&'src str>) {
    match value {
        Value::Block(block) => out.extend(block.entries.iter().filter_map(|entry| {
            match (entry.key_str(src), &entry.value) {
                (Some("id"), Value::Scalar(id)) => Some(id.unquoted(src)),
                _ => None,
            }
        })),
        Value::Scalar(id) => out.push(id.unquoted(src)),
        Value::Tagged { .. } => {}
    }
}

/// Records every event fired anywhere in `block` (see [`fired_event`]).
fn fired_events<'src>(src: &'src str, block: &Block, out: &mut Vec<&'src str>) {
    for entry in &block.entries {
        if entry
            .key_str(src)
            .is_some_and(|key| EVENT_TYPES.contains(&key))
        {
            fired_event(src, &entry.value, out);
        } else if let Value::Block(inner) = &entry.value {
            fired_events(src, inner, out);
        }
    }
}

/// Sorts the focuses of one focus `tree`: the edit and how many focuses
/// move, or `None` if nothing changes or a focus that would move does not
/// start its own line. `Unsortable` if two focuses share an id or their
/// order has a cycle.
fn focus_tree(src: &str, tree: &Block, newline: &str) -> Result<Option<(Edit, usize)>, Unsortable> {
    let mut found: Vec<FoundFocus<'_>> = Vec::new();
    for (index, entry) in tree.entries.iter().enumerate() {
        let (Some("focus"), Value::Block(focus)) = (entry.key_str(src), &entry.value) else {
            continue;
        };
        let Some(id) = own_id(src, focus) else {
            continue;
        };
        let mut relative = None;
        let mut prerequisites = Vec::new();
        for field in &focus.entries {
            match (field.key_str(src), &field.value) {
                (Some("relative_position_id"), Value::Scalar(other)) => {
                    relative = Some(other.unquoted(src));
                }
                (Some("prerequisite"), Value::Block(block)) => {
                    prerequisites.extend(block.entries.iter().filter_map(|required| {
                        match (required.key_str(src), &required.value) {
                            (Some("focus"), Value::Scalar(other)) => Some(other.unquoted(src)),
                            _ => None,
                        }
                    }));
                }
                _ => {}
            }
        }
        found.push((index, id, relative, prerequisites));
    }
    if found.is_empty() {
        return Ok(None);
    }

    // Nodes in file order, so focuses only move where they must.
    let mut node_of: HashMap<&str, usize> = HashMap::with_capacity(found.len());
    for (node, (_, id, ..)) in found.iter().enumerate() {
        if node_of.insert(id, node).is_some() {
            return Err(Unsortable);
        }
    }
    let mut edges = Vec::new();
    for (node, (_, _, relative, prerequisites)) in found.iter().enumerate() {
        edges.extend(
            relative
                .iter()
                .chain(prerequisites)
                .filter_map(|before| Some((*node_of.get(before)?, node))),
        );
    }
    let order = priority_toposort(found.len(), &edges).ok_or(Unsortable)?;

    let indices: Vec<usize> = found.iter().map(|(index, ..)| *index).collect();
    // A tree whose focuses share lines is left as it is, but does not stop
    // the file's other trees from being sorted.
    Ok(own_line_units(src, tree, &indices)
        .and_then(|blocks| rearrange(src, &blocks, &order, newline)))
}

/// Sorts the focuses of every focus tree of a national focus file (see the
/// module docs). Returns the new text and how many focuses changed position,
/// or `None` if the text stays as it is. A tree whose focuses cannot be
/// ordered leaves the whole file as it is.
pub fn focuses(doc: &Document<'_>) -> Option<(String, usize)> {
    let src = doc.src;
    let newline = newline(src);
    let mut edits = Vec::new();
    let mut moved = 0;
    for entry in &doc.root.entries {
        if let (Some("focus_tree"), Value::Block(tree)) = (entry.key_str(src), &entry.value)
            && let Some((edit, tree_moved)) = focus_tree(src, tree, newline).ok()?
        {
            edits.push(edit);
            moved += tree_moved;
        }
    }
    if edits.is_empty() {
        return None;
    }
    Some((cst::apply_edits(src, edits)?, moved))
}

/// Whether a comment of `block` starts at `offset`.
fn is_comment_start(block: &Block, offset: usize) -> bool {
    block
        .comments
        .binary_search_by_key(&offset, |span| span.start)
        .is_ok()
}

/// Splits a string into a key that orders `germany.2` before `germany.10`.
fn natural_key(s: &str) -> Vec<NaturalPart> {
    let mut parts: Vec<NaturalPart> = Vec::new();
    let mut rest = s;
    while let Some(first) = rest.chars().next() {
        let digits = first.is_ascii_digit();
        let end = rest
            .find(|ch: char| ch.is_ascii_digit() != digits)
            .unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        parts.push(if digits {
            NaturalPart::Number(run.bytes().fold(0_u64, |number, digit| {
                number
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit - b'0'))
            }))
        } else {
            NaturalPart::Text(run.to_owned())
        });
        rest = tail;
    }
    parts
}

/// The line terminator a moved block is written with: `\r\n` if the file
/// has one anywhere, else `\n`.
fn newline(src: &str) -> &'static str {
    if src.contains("\r\n") { "\r\n" } else { "\n" }
}

/// The id of a focus or event `block`: the scalar of its first own `id`
/// entry.
fn own_id<'src>(src: &'src str, block: &Block) -> Option<&'src str> {
    block
        .entries
        .iter()
        .find_map(|entry| match (entry.key_str(src), &entry.value) {
            (Some("id"), Value::Scalar(id)) => Some(id.unquoted(src)),
            _ => None,
        })
}

/// The line units (see [`cst::line_units`]) of the entries of `block` at
/// `indices` (ascending), or `None` unless each of them starts its own line:
/// only whitespace before it on its first line.
///
/// A unit takes the rest of its entry's last line when that holds only
/// whitespace and at most a comment. When anything else follows the entry
/// there (another entry, or the `}` closing `block`), the unit ends with the
/// entry, and the rest of the line belongs to what follows the unit. The
/// other entries of the block may share lines; they belong to no unit, nor
/// do the comment lines above them.
fn own_line_units(src: &str, block: &Block, indices: &[usize]) -> Option<Vec<Span>> {
    let bytes = src.as_bytes();
    let bom = bom_len(src);
    let mut units = Vec::with_capacity(indices.len());
    // Attached comment lines may not start before this: the end of the
    // previous unit, or (for the first) just past the `{`.
    let mut floor = block.open.map_or(bom, |open| open + 1);
    for &index in indices {
        let entry = block.entries.get(index)?;
        let entry_line_start = cst::line_start(src, entry.span.start).max(bom);
        if entry_line_start < floor
            || !src
                .get(entry_line_start..entry.span.start)?
                .chars()
                .all(char::is_whitespace)
        {
            return None;
        }

        let last_line_end = cst::line_end(src, entry.span.end);
        let tail = src.get(entry.span.end..last_line_end)?;
        let rest = tail.trim_start();
        // As in `line_units`: an editor shows a lone `\r` after the entry as
        // a line break, so what follows it is not treated as on the entry's
        // line, nor as on a line of its own.
        if !rest.is_empty() && tail.get(..tail.len() - rest.len())?.contains('\r') {
            return None;
        }
        let end = if rest.is_empty() || is_comment_start(block, last_line_end - rest.len()) {
            match bytes.get(last_line_end) {
                Some(b'\r') => last_line_end + 2,
                Some(b'\n') => last_line_end + 1,
                _ => last_line_end,
            }
        } else {
            entry.span.end
        };

        let mut start = entry_line_start;
        while start > floor {
            let newline = start - 1;
            let previous_start = cst::line_start(src, newline).max(bom);
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
        units.push(Span { end, start });
        floor = end;
    }
    Some(units)
}

/// Linearises the graph on nodes `0..nodes` with `edges` (`(a, b)`: `a`
/// comes before `b`), or `None` if it has a cycle.
///
/// Weakly connected components are emitted one at a time, in order of their
/// lowest node. Within each, a stable topological sort is used: among the
/// nodes whose predecessors have all been emitted, the lowest is emitted
/// next. Callers number nodes in the order to keep where edges allow (file
/// order for focuses, natural order for events), and because the sort is
/// stable it is a fixed point: sorting a sorted sequence reproduces it, so
/// formatting is idempotent.
fn priority_toposort(nodes: usize, edges: &[(usize, usize)]) -> Option<Vec<usize>> {
    // Union-find for the weakly connected components. Unions keep the lower
    // root, so a component's root is its lowest node.
    let mut parent: Vec<usize> = (0..nodes).collect();
    let find = |parent: &mut Vec<usize>, node: usize| -> Option<usize> {
        let mut root = node;
        while let Some(&up) = parent.get(root)
            && up != root
        {
            root = up;
        }
        let mut current = node;
        while current != root {
            current = std::mem::replace(parent.get_mut(current)?, root);
        }
        Some(root)
    };
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); nodes];
    let mut in_degree = vec![0_usize; nodes];
    for &(from, to) in edges {
        successors.get_mut(from)?.push(to);
        *in_degree.get_mut(to)? += 1;
        let (a, b) = (find(&mut parent, from)?, find(&mut parent, to)?);
        *parent.get_mut(a.max(b))? = a.min(b);
    }
    let mut components: Vec<Vec<usize>> = vec![Vec::new(); nodes];
    for node in 0..nodes {
        let root = find(&mut parent, node)?;
        components.get_mut(root)?.push(node);
    }

    let mut sorted = Vec::with_capacity(nodes);
    // Indexed by root, which is each component's lowest node.
    for component in &components {
        let mut ready: BinaryHeap<Reverse<usize>> = component
            .iter()
            .filter(|&&node| in_degree.get(node) == Some(&0))
            .map(|&node| Reverse(node))
            .collect();
        while let Some(Reverse(node)) = ready.pop() {
            sorted.push(node);
            for &next in successors.get(node)? {
                let degree = in_degree.get_mut(next)?;
                *degree -= 1;
                if *degree == 0 {
                    ready.push(Reverse(next));
                }
            }
        }
    }
    (sorted.len() == nodes).then_some(sorted)
}

/// The edit putting block `order[pos]` of `blocks` (the blocks' line units,
/// in file order) at position `pos`, and how many blocks change position.
/// Every block is written with exactly one `newline` after it, and each gap
/// between two positions holding only whitespace becomes one blank line;
/// other gaps are kept as they are, as is everything before the first block
/// and after the last. `None` if that changes nothing.
fn rearrange(src: &str, blocks: &[Span], order: &[usize], newline: &str) -> Option<(Edit, usize)> {
    let (first, last) = (blocks.first()?, blocks.last()?);
    let moved = order
        .iter()
        .enumerate()
        .filter(|&(pos, &block)| pos != block)
        .count();
    // The gap after position `pos` as it is, and as it is written.
    let gap = |pos: usize| {
        let gap = src.get(blocks.get(pos)?.end..blocks.get(pos + 1)?.start)?;
        Some((gap, if gap.trim().is_empty() { newline } else { gap }))
    };
    let unchanged = moved == 0
        && blocks.iter().all(|block| {
            block
                .text(src)
                .strip_suffix(newline)
                .is_some_and(|body| !body.ends_with(['\r', '\n']))
        })
        && (1..blocks.len()).all(|next| gap(next - 1).is_some_and(|(gap, written)| gap == written));
    if unchanged {
        return None;
    }

    let mut replacement = String::with_capacity(last.end - first.start + 64);
    for (pos, &index) in order.iter().enumerate() {
        // Exactly one terminator after every block. Normalizing here is what
        // makes the rewrite idempotent: a file's last block may have none,
        // and without this a block moved out of last place would only gain
        // its blank-line separator on the next run.
        replacement.push_str(blocks.get(index)?.text(src).trim_end_matches(['\r', '\n']));
        replacement.push_str(newline);
        if pos + 1 < order.len() {
            replacement.push_str(gap(pos)?.1);
        }
    }
    Some((
        Edit {
            replacement,
            span: Span {
                end: last.end,
                start: first.start,
            },
        },
        moved,
    ))
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::expect_used,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{NaturalPart, events, focuses, natural_key, priority_toposort};
    use crate::cst::{self, Document, Value};

    /// `lines`, each terminated by `\n`.
    fn text(lines: &[&str]) -> String {
        lines.iter().flat_map(|line| [*line, "\n"]).collect()
    }

    /// Runs `sort` on `src`, checking that sorting its output changes
    /// nothing, and returns the output (`src` itself if unchanged) and how
    /// many blocks moved.
    fn run(src: &str, sort: fn(&Document<'_>) -> Option<(String, usize)>) -> (String, usize) {
        let doc = cst::parse(src).unwrap_or_else(|err| panic!("{src:?} does not parse: {err}"));
        let Some((out, moved)) = sort(&doc) else {
            return (src.to_owned(), 0);
        };
        assert_ne!(out, src, "a change that changes nothing");
        let again = cst::parse(&out).expect("the output parses");
        assert_eq!(sort(&again), None, "not idempotent: {src:?} -> {out:?}");
        (out, moved)
    }

    fn event(id: &str, body: &[&str]) -> Vec<String> {
        let mut lines = vec!["country_event = {".to_owned(), format!("\tid = {id}")];
        lines.extend(body.iter().map(|line| format!("\t{line}")));
        lines.push("}".to_owned());
        lines
    }

    fn file(blocks: &[Vec<String>], separator: &str) -> String {
        let blocks: Vec<String> = blocks.iter().map(|block| block.join("\n")).collect();
        let mut out = blocks.join(separator);
        out.push('\n');
        out
    }

    #[test]
    fn natural_keys_order_numbers_by_value() {
        let mut ids = vec!["e.10", "e.2", "e.1", "d.3", "e.1a", "e"];
        ids.sort_by_key(|id| natural_key(id));
        assert_eq!(ids, ["d.3", "e", "e.1", "e.1a", "e.2", "e.10"]);
        assert_eq!(
            natural_key("ab12c"),
            [
                NaturalPart::Text("ab".to_owned()),
                NaturalPart::Number(12),
                NaturalPart::Text("c".to_owned())
            ]
        );
        assert_eq!(natural_key(""), []);
        // Overlong numbers saturate instead of overflowing.
        assert_eq!(
            natural_key("99999999999999999999999"),
            [NaturalPart::Number(u64::MAX)]
        );
        // Non-ASCII text stays whole.
        assert_eq!(
            natural_key("é1"),
            [NaturalPart::Text("é".to_owned()), NaturalPart::Number(1)]
        );
    }

    #[test]
    fn toposort_is_stable_and_groups_components() {
        // No edges: the input order.
        assert_eq!(priority_toposort(3, &[]), Some(vec![0, 1, 2]));
        // 2 must come before 0; 1 stands alone.
        assert_eq!(priority_toposort(3, &[(2, 0)]), Some(vec![2, 0, 1]));
        // Components come out whole, in order of their lowest node: {0, 3}
        // before {1}, {2}.
        assert_eq!(priority_toposort(4, &[(0, 3)]), Some(vec![0, 3, 1, 2]));
        // Among ready nodes, the lowest goes first.
        assert_eq!(
            priority_toposort(4, &[(3, 1), (3, 2), (0, 2)]),
            Some(vec![0, 3, 1, 2])
        );
        // Duplicate edges are harmless.
        assert_eq!(priority_toposort(2, &[(1, 0), (1, 0)]), Some(vec![1, 0]));
        // Cycles, including a self-loop, cannot be sorted.
        assert_eq!(priority_toposort(2, &[(0, 1), (1, 0)]), None);
        assert_eq!(priority_toposort(2, &[(1, 1)]), None);
        assert_eq!(priority_toposort(0, &[]), Some(vec![]));
        // Sorting a sorted order again changes nothing.
        let edges = [(4, 0), (3, 1), (0, 2)];
        let sorted = priority_toposort(5, &edges).expect("acyclic");
        let position = |node: usize| sorted.iter().position(|&n| n == node).expect("node");
        let renumbered: Vec<(usize, usize)> = edges
            .iter()
            .map(|&(a, b)| (position(a), position(b)))
            .collect();
        assert_eq!(
            priority_toposort(5, &renumbered),
            Some((0..5).collect::<Vec<_>>())
        );
    }

    #[test]
    fn events_follow_natural_order_and_chains() {
        let src = file(
            &[
                event("e.10", &[]),
                event("e.2", &["option = { country_event = e.3 }"]),
                event("e.3", &[]),
                event(
                    "e.1",
                    &["immediate = { country_event = { id = e.10 days = 1 } }"],
                ),
            ],
            "\n\n",
        );
        let expected = file(
            &[
                event(
                    "e.1",
                    &["immediate = { country_event = { id = e.10 days = 1 } }"],
                ),
                event("e.10", &[]),
                event("e.2", &["option = { country_event = e.3 }"]),
                event("e.3", &[]),
            ],
            "\n\n",
        );
        assert_eq!(run(&src, events), (expected, 4));
    }

    #[test]
    fn events_keep_structural_gaps_and_normalise_blank_ones() {
        let src = [
            "add_namespace = e",
            "country_event = {\n\tid = e.2\n}",
            "",
            "",
            "",
            "# A section comment, separated by a blank line.",
            "",
            "country_event = {\n\tid = e.1\n}",
            "country_event = {\n\tid = e.3\n}",
            "# trailing",
        ]
        .join("\n");
        let expected = [
            "add_namespace = e",
            "country_event = {\n\tid = e.1\n}",
            "",
            "",
            "",
            "# A section comment, separated by a blank line.",
            "",
            "country_event = {\n\tid = e.2\n}",
            "",
            "country_event = {\n\tid = e.3\n}",
            "# trailing",
        ]
        .join("\n");
        assert_eq!(run(&src, events), (expected, 2));
        // Nothing to move, but a missing blank line is added.
        let src = "country_event = {\n\tid = e.1\n}\ncountry_event = {\n\tid = e.2\n}\n";
        assert_eq!(
            run(src, events),
            (
                "country_event = {\n\tid = e.1\n}\n\ncountry_event = {\n\tid = e.2\n}\n".to_owned(),
                0
            )
        );
        // Already sorted and spaced: nothing to do.
        let src = "country_event = {\n\tid = e.1\n}\n\ncountry_event = {\n\tid = e.2\n}\n";
        assert_eq!(run(src, events), (src.to_owned(), 0));
    }

    #[test]
    fn comments_travel_with_their_block() {
        let src = text(&[
            "# About e.2.",
            "# More about e.2.",
            "country_event = { # e.2's brace",
            "\tid = e.2",
            "} # after e.2",
            "",
            "# About e.1.",
            "country_event = {",
            "\tid = e.1",
            "}#after e.1",
        ]);
        let expected = text(&[
            "# About e.1.",
            "country_event = {",
            "\tid = e.1",
            "}#after e.1",
            "",
            "# About e.2.",
            "# More about e.2.",
            "country_event = { # e.2's brace",
            "\tid = e.2",
            "} # after e.2",
        ]);
        assert_eq!(run(&src, events), (expected, 2));
    }

    /// Vanilla `NSB_Baltic.txt` has an event ending `}#comment` followed by
    /// another event: the comment belongs to the event it follows, so it
    /// moves with it and does not become the next event's leading comment
    /// (which gained a blank line on every second run).
    #[test]
    fn a_comment_after_a_closing_brace_stays_on_its_line() {
        let src = text(&[
            "country_event = {",
            "\tid = b.3",
            "}#About b.2",
            "country_event = {",
            "\tid = b.2",
            "}",
            "#About b.1",
            "country_event = {",
            "\tid = b.1",
            "}",
        ]);
        let expected = text(&[
            "#About b.1",
            "country_event = {",
            "\tid = b.1",
            "}",
            "",
            "country_event = {",
            "\tid = b.2",
            "}",
            "",
            "country_event = {",
            "\tid = b.3",
            "}#About b.2",
        ]);
        assert_eq!(run(&src, events), (expected, 2));
    }

    #[test]
    fn events_sharing_a_line_are_left_alone() {
        for src in [
            // Once a panic: two events on one line.
            "country_event = {\n\tid = e.2\n} country_event = {\n\tid = e.1\n}\n",
            "country_event = { id = e.2 } country_event = { id = e.1 }\n",
            // An entry before an event on its first line.
            "x = 1 country_event = {\n\tid = e.2\n}\ncountry_event = {\n\tid = e.1\n}\n",
        ] {
            assert_eq!(run(src, events), (src.to_owned(), 0), "{src:?}");
        }
        // Other entries may share lines with each other.
        let src =
            "a = 1 b = 2\ncountry_event = {\n\tid = e.2\n}\n\ncountry_event = {\n\tid = e.1\n}\n";
        assert_eq!(
            run(src, events).0,
            "a = 1 b = 2\ncountry_event = {\n\tid = e.1\n}\n\ncountry_event = {\n\tid = e.2\n}\n"
        );
    }

    /// An entry after an event's `}` on its line is not part of the event: it
    /// stays where it is, on a line of its own.
    #[test]
    fn an_entry_after_a_closing_brace_stays_put() {
        let src = "country_event = {\n\tid = e.2\n} x = 1\n\
            country_event = {\n\tid = e.1\n} add_namespace = z\n";
        let expected = "country_event = {\n\tid = e.1\n}\n x = 1\n\
            country_event = {\n\tid = e.2\n}\n add_namespace = z\n";
        assert_eq!(run(src, events), (expected.to_owned(), 2));
    }

    #[test]
    fn unsortable_events_are_left_alone() {
        for src in [
            // A cycle, and an event firing itself.
            file(
                &[
                    event("c.2", &["country_event = c.1"]),
                    event("c.1", &["option = { country_event = { id = c.2 } }"]),
                ],
                "\n\n",
            ),
            file(
                &[event("s.2", &["country_event = s.2"]), event("s.1", &[])],
                "\n\n",
            ),
            // Two events with one id.
            file(
                &[event("d.2", &[]), event("d.1", &[]), event("d.1", &[])],
                "\n\n",
            ),
            // No events with ids.
            "add_namespace = n\ncountry_event = n.1\ncountry_event = {\n\ttitle = t\n}\n"
                .to_owned(),
            String::new(),
        ] {
            assert_eq!(run(&src, events), (src.clone(), 0), "{src:?}");
        }
    }

    #[test]
    fn only_top_level_event_blocks_with_ids_move() {
        // The id-less event and the nested event stay put; `news_event` and
        // quoted ids count.
        let src = text(&[
            "country_event = {",
            "\ttitle = no_id",
            "}",
            "news_event = {",
            "\tid = \"n.2\"",
            "\timmediate = { country_event = { id = n.9 } }",
            "}",
            "country_event = {",
            "\tid = n.1",
            "}",
        ]);
        let expected = text(&[
            "country_event = {",
            "\ttitle = no_id",
            "}",
            "country_event = {",
            "\tid = n.1",
            "}",
            "",
            "news_event = {",
            "\tid = \"n.2\"",
            "\timmediate = { country_event = { id = n.9 } }",
            "}",
        ]);
        assert_eq!(run(&src, events), (expected, 2));
    }

    #[test]
    fn line_endings_bom_and_a_missing_final_newline() {
        let src = "\u{feff}country_event = {\r\n\tid = e.2\r\n}\r\n\r\ncountry_event = {\r\n\tid = e.1\r\n}";
        let expected = "\u{feff}country_event = {\r\n\tid = e.1\r\n}\r\n\r\ncountry_event = {\r\n\tid = e.2\r\n}\r\n";
        assert_eq!(run(src, events), (expected.to_owned(), 2));
        // A file with any CRLF gets CRLF after moved blocks.
        let src = "country_event = {\n\tid = e.2\n}\n\ncountry_event = {\r\n\tid = e.1\r\n}\r\n";
        let expected =
            "country_event = {\r\n\tid = e.1\r\n}\r\n\r\ncountry_event = {\n\tid = e.2\n}\r\n";
        assert_eq!(run(src, events), (expected.to_owned(), 2));
    }

    fn tree(focuses: &[&[&str]]) -> String {
        let mut lines = vec!["focus_tree = {", "\tid = tree"];
        for focus in focuses {
            lines.push("\tfocus = {");
            lines.extend(focus.iter().copied());
            lines.push("\t}");
        }
        lines.push("}");
        text(&lines)
    }

    #[test]
    fn focuses_follow_prerequisites_and_positions() {
        let src = tree(&[
            &["\t\tid = c", "\t\tprerequisite = { focus = b }"],
            &["\t\tid = b", "\t\trelative_position_id = a"],
            &["\t\tid = a"],
            &["\t\tid = d"],
        ]);
        let expected = tree(&[
            &["\t\tid = a"],
            &["\t\tid = b", "\t\trelative_position_id = a"],
            &["\t\tid = c", "\t\tprerequisite = { focus = b }"],
            &["\t\tid = d"],
        ]);
        let (out, moved) = run(&src, focuses);
        // Adjacent focuses get a blank line between them; `b` stays second.
        assert_eq!(out, expected.replace("\t}\n\tfocus", "\t}\n\n\tfocus"));
        assert_eq!(moved, 2);
    }

    #[test]
    fn focus_trees_are_sorted_separately() {
        let src = [
            tree(&[
                &["\t\tid = b", "\t\tprerequisite = { focus = a }"],
                &["\t\tid = a"],
            ]),
            tree(&[
                &["\t\tid = b", "\t\tprerequisite = { focus = a }"],
                &["\t\tid = a"],
            ]),
        ]
        .join("\n");
        let sorted = tree(&[
            &["\t\tid = a"],
            &["\t\tid = b", "\t\tprerequisite = { focus = a }"],
        ])
        .replace("\t}\n\tfocus", "\t}\n\n\tfocus");
        assert_eq!(run(&src, focuses), ([sorted.clone(), sorted].join("\n"), 4));
    }

    /// A tree whose focuses cannot be ordered leaves the whole file as it is.
    #[test]
    fn unorderable_focus_trees_leave_the_file_alone() {
        let sortable = tree(&[
            &["\t\tid = b", "\t\tprerequisite = { focus = a }"],
            &["\t\tid = a"],
        ]);
        for other in [
            // A cycle.
            tree(&[
                &["\t\tid = x", "\t\tprerequisite = { focus = y }"],
                &["\t\tid = y", "\t\tprerequisite = { focus = x }"],
            ]),
            // Two focuses with one id.
            tree(&[&["\t\tid = x"], &["\t\tid = x"]]),
        ] {
            for src in [
                format!("{sortable}\n{other}"),
                format!("{other}\n{sortable}"),
            ] {
                assert_eq!(run(&src, focuses), (src.clone(), 0), "{src}");
            }
        }
    }

    /// A tree with a focus to move that does not start its own line is left
    /// as it is (it once had focuses moved out of it), but the file's other
    /// trees are still sorted.
    #[test]
    fn focus_trees_sharing_lines_are_left_alone() {
        let sortable = tree(&[
            &["\t\tid = b", "\t\tprerequisite = { focus = a }"],
            &["\t\tid = a"],
        ]);
        let (sorted, moved) = run(&sortable, focuses);
        assert_eq!(moved, 2);
        for other in [
            // A focus on the tree's `{` line.
            "focus_tree = { focus = {\n\t\tid = x\n\t\tprerequisite = { focus = y }\n\t}\n\
                \tfocus = {\n\t\tid = y\n\t}\n}\n",
            // Two focuses on one line.
            "focus_tree = {\n\tfocus = { id = x prerequisite = { focus = y } } focus = { id = y }\n}\n",
        ] {
            assert_eq!(run(other, focuses), (other.to_owned(), 0), "{other}");
            for (src, expected) in [
                (format!("{sortable}\n{other}"), format!("{sorted}\n{other}")),
                (format!("{other}\n{sortable}"), format!("{other}\n{sorted}")),
            ] {
                assert_eq!(run(&src, focuses), (expected, 2), "{src}");
            }
        }
    }

    /// A tree's `}` on the line of its last focus stays where it is, on a
    /// line of its own, when the focuses are sorted.
    #[test]
    fn a_tree_closing_on_its_last_focus_line_is_sorted() {
        let src = "focus_tree = {\n\tfocus = {\n\t\tid = b\n\t\tprerequisite = { focus = a }\n\t}\n\
            \tfocus = {\n\t\tid = a\n\t} }\n";
        let expected = "focus_tree = {\n\tfocus = {\n\t\tid = a\n\t}\n\n\
            \tfocus = {\n\t\tid = b\n\t\tprerequisite = { focus = a }\n\t}\n }\n";
        assert_eq!(run(src, focuses), (expected.to_owned(), 2));
        // At the end of a file without a final newline.
        let src = src.replace("\t} }\n", "\t}}");
        let expected = expected.replace("\t}\n }\n", "\t}\n}");
        assert_eq!(run(&src, focuses), (expected, 2));
    }

    #[test]
    fn focuses_without_ids_and_other_entries_stay_put() {
        let src = text(&[
            "focus_tree = odd",
            "shared_focus = {",
            "\tid = s",
            "}",
            "focus_tree = {",
            "\tfocus = odd_scalar_focus",
            "\tfocus = {",
            "\t\tx = 3",
            "\t}",
            "\tfocus = {",
            "\t\tid = b",
            "\t\tprerequisite = { focus = a }",
            "\t}",
            "\tcontinuous_focus_position = { x = 1 y = 2 }",
            "\t# About a.",
            "\tfocus = {",
            "\t\tid = a",
            "\t}",
            "}",
        ]);
        let expected = text(&[
            "focus_tree = odd",
            "shared_focus = {",
            "\tid = s",
            "}",
            "focus_tree = {",
            "\tfocus = odd_scalar_focus",
            "\tfocus = {",
            "\t\tx = 3",
            "\t}",
            "\t# About a.",
            "\tfocus = {",
            "\t\tid = a",
            "\t}",
            "\tcontinuous_focus_position = { x = 1 y = 2 }",
            "\tfocus = {",
            "\t\tid = b",
            "\t\tprerequisite = { focus = a }",
            "\t}",
            "}",
        ]);
        assert_eq!(run(&src, focuses), (expected, 2));
    }

    /// Every token's text (scalars, operators, braces, comments) of `doc`,
    /// sorted: what sorting blocks must keep.
    fn tokens<'src>(doc: &Document<'src>) -> Vec<&'src str> {
        let mut texts = Vec::new();
        let mut stack = vec![&doc.root];
        while let Some(block) = stack.pop() {
            let brace = |at: Option<usize>| doc.src.get(at?..=at?);
            texts.extend(brace(block.open));
            texts.extend(brace(block.close));
            texts.extend(block.comments.iter().map(|span| span.text(doc.src)));
            for entry in &block.entries {
                texts.extend(entry.key.map(|key| key.text(doc.src)));
                texts.extend(entry.op.map(|op| op.span.text(doc.src)));
                match &entry.value {
                    Value::Scalar(scalar) => texts.push(scalar.text(doc.src)),
                    Value::Block(inner) => stack.push(inner),
                    Value::Tagged { block: inner, tag } => {
                        texts.push(tag.text(doc.src));
                        stack.push(inner);
                    }
                }
            }
        }
        texts.sort_unstable();
        texts
    }

    /// Sorts the events and focuses of every events and focus file of each
    /// `;`-separated root in `HEARTY_CORPUS`, and checks that the output
    /// parses, holds the same tokens and comments, and is sorted already.
    ///
    /// Run with `cargo test --release corpus_sort -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs HEARTY_CORPUS=<root>;<root>;... pointing at HOI4 / mod directories"]
    fn corpus_sort_is_lossless_and_idempotent() {
        use crate::schema::FileKind;
        use rayon::prelude::*;
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
            let files = crate::files::scripts(std::path::Path::new(root), &tracing::Span::none());
            let results: Vec<(bool, Vec<String>)> = files
                .par_iter()
                .filter_map(|file| {
                    let sort: fn(&Document<'_>) -> Option<(String, usize)> = match file.kind {
                        FileKind::Events => events,
                        FileKind::NationalFocus => focuses,
                        FileKind::Characters
                        | FileKind::DecisionCategories
                        | FileKind::Decisions
                        | FileKind::Ideas
                        | FileKind::Other
                        | FileKind::Technologies => return None,
                    };
                    let src = std::fs::read_to_string(&file.path).ok()?;
                    let doc = cst::parse(&src).ok()?;
                    let name = file.relative.display();
                    let Some((out, _)) = sort(&doc) else {
                        return Some((false, Vec::new()));
                    };
                    let mut problems = Vec::new();
                    match cst::parse(&out) {
                        Ok(after) => {
                            if tokens(&doc) != tokens(&after) {
                                problems.push(format!("{name}: tokens changed"));
                            }
                            if let Some((again, _)) = sort(&after) {
                                let line = out
                                    .lines()
                                    .zip(again.lines())
                                    .position(|(a, b)| a != b)
                                    .map_or(0, |line| line + 1);
                                problems.push(format!("{name}: not idempotent (line {line})"));
                            }
                        }
                        Err(err) => problems.push(format!("{name}: output does not parse: {err}")),
                    }
                    Some((true, problems))
                })
                .collect();
            println!(
                "== {root}: {} files, {} sorted",
                results.len(),
                results.iter().filter(|(sorted, _)| *sorted).count()
            );
            failures.extend(results.into_iter().flat_map(|(_, problems)| problems));
        }
        for failure in failures.iter().take(30) {
            println!("FAILURE {failure}");
        }
        assert!(failures.is_empty(), "{} failures", failures.len());
    }
}
