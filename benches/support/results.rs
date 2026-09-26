//! Reading what hearty reported from its output: the problems it found in a
//! mod and its `--timings` breakdown. hearty's summary lines are stable
//! (the integration tests pin them), so the benchmark parses them rather
//! than needing a machine-readable output mode. It is a module of its own
//! so `tests/bench_workshop.rs` can test it.

use serde_json::{Value, json};

/// The problems hearty found in a mod.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Problems {
    /// Warnings about `descriptor.mod` (duplicate keys, an old
    /// `supported_version`).
    pub descriptor_warnings: u64,
    /// Lines `--format` would add.
    pub format_insertions: u64,
    /// Lines `--format` would remove.
    pub format_deletions: u64,
    /// What `--format` would change, by kind (e.g. `blocks joined onto one
    /// line`), in hearty's order.
    pub format_changes: Vec<(String, u64)>,
    /// Files `--format` would change.
    pub files_to_reformat: u64,
    /// Localisation keys used by events, focuses and technologies.
    pub localisation_keys: u64,
    /// Of those, the keys missing a localisation.
    pub missing_localisations: u64,
    /// Redundant fields `--fix` can remove.
    pub redundant_fixable: u64,
    /// Redundant fields (default values and the like).
    pub redundant_fields: u64,
}

impl Problems {
    /// Reads the problems from the output of `hearty --check --lint`.
    pub fn parse(stdout: &str, stderr: &str) -> Self {
        let mut problems = Self::default();
        for line in stdout.lines().filter(|line| !line.starts_with("  ")) {
            if let Some(count) = line.strip_suffix(" would be reformatted:") {
                problems.files_to_reformat = leading_number(count).unwrap_or(0);
            } else if line.contains(" changed, ") || line.ends_with(" changed") {
                // `3 files changed, 25 insertions(+), 33 deletions(-)`; git
                // leaves out a count of zero.
                for part in line.split(", ").skip(1) {
                    if part.ends_with("(+)") {
                        problems.format_insertions = leading_number(part).unwrap_or(0);
                    } else if part.ends_with("(-)") {
                        problems.format_deletions = leading_number(part).unwrap_or(0);
                    }
                }
            } else if let Some(changes) = line.strip_prefix("Changes: ") {
                problems.format_changes = changes
                    .split(", ")
                    .filter_map(|change| {
                        let (count, kind) = change.split_once(' ')?;
                        Some((kind.to_owned(), number(count)?))
                    })
                    .collect();
            } else if let Some(rest) = line.strip_prefix("Found ") {
                if let Some(counts) = rest.strip_suffix(" missing localisations.") {
                    if let Some((missing, keys)) = counts.split_once('/') {
                        problems.missing_localisations = number(missing).unwrap_or(0);
                        problems.localisation_keys = number(keys).unwrap_or(0);
                    }
                } else if let Some((count, fixable)) = rest.split_once(" redundant fields (") {
                    problems.redundant_fields = number(count).unwrap_or(0);
                    problems.redundant_fixable = leading_number(fixable).unwrap_or(0);
                }
            }
        }
        problems.descriptor_warnings = stderr
            .lines()
            .filter(|line| {
                line.contains("in descriptor.mod.") || line.contains("does not match latest HOI4")
            })
            .count() as u64;
        problems
    }

    /// The problems as JSON.
    pub fn to_json(&self) -> Value {
        json!({
            "descriptor_warnings": self.descriptor_warnings,
            "files_to_reformat": self.files_to_reformat,
            "format_changes": self
                .format_changes
                .iter()
                .map(|(kind, count)| json!({ "kind": kind, "count": count }))
                .collect::<Vec<_>>(),
            "format_deletions": self.format_deletions,
            "format_insertions": self.format_insertions,
            "localisation_keys": self.localisation_keys,
            "missing_localisations": self.missing_localisations,
            "redundant_fields": self.redundant_fields,
            "redundant_fixable": self.redundant_fixable,
        })
    }
}

/// One row of a `--timings` report's tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimingRow {
    /// Calls (spans closed), unless the row folds several small spans.
    pub calls: Option<u64>,
    /// Active seconds: time threads spent in the span and the spans under
    /// it.
    pub active: f64,
    /// The span names from the top-level part down to this row; a row
    /// folding small spans is named like `… 2 more`.
    pub path: Vec<String>,
    /// Active seconds not spent in a nested span, unless the row folds
    /// several small spans.
    pub self_time: Option<f64>,
    /// Wall seconds, for top-level parts.
    pub wall: Option<f64>,
}

/// The headline figures and the tree of a `--timings` report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timings {
    /// Active time summed over threads, in seconds.
    pub active: f64,
    /// Active time divided by wall time.
    pub parallelism: f64,
    /// The report's tree, in its order (parents before their children).
    pub rows: Vec<TimingRow>,
    /// Threads that did work.
    pub threads: u64,
    /// Wall time, in seconds.
    pub wall: f64,
}

impl Timings {
    /// Reads a `--timings` report, e.g.
    ///
    /// ```text
    /// Timings: 229.41ms wall, 4.32s active on 26 threads (rayon pool: 24), parallelism 18.85x
    /// span                    active      self  calls  share      wall
    /// scripts                  4.14s    0.00ns      1  95.7%  206.22ms
    ///   file                   4.14s   36.20ms  6,076  95.7%
    ///     … 2 more            21.74ms                    0.5%
    /// ```
    ///
    /// Returns `None` if it has no `Timings:` header or a row doesn't read.
    pub fn parse(report: &str) -> Option<Self> {
        let mut lines = report.lines();
        let header = lines.find_map(|line| line.strip_prefix("Timings: "))?;
        let (wall, rest) = header.split_once(" wall, ")?;
        let (active, rest) = rest.split_once(" active on ")?;
        let (threads, rest) = rest.split_once(" threads")?;
        let parallelism = rest
            .rsplit_once("parallelism ")?
            .1
            .trim_end_matches('x')
            .parse()
            .ok()?;
        let table = lines
            .skip_while(|line| !line.starts_with("span "))
            .skip(1)
            .take_while(|line| !line.trim().is_empty());
        let mut rows: Vec<TimingRow> = Vec::new();
        let mut stack: Vec<String> = Vec::new();
        for line in table {
            let depth = (line.len() - line.trim_start().len()) / 2;
            // Names may hold spaces, so read the columns from the right of
            // the share column: `active self calls share [wall]`, or
            // `active share` for a row folding small spans.
            let columns: Vec<&str> = line.split_whitespace().collect();
            let share = columns.iter().rposition(|column| column.ends_with('%'))?;
            let wall = columns.get(share + 1).and_then(|wall| parse_duration(wall));
            let folded = columns.first().is_some_and(|first| *first == "…");
            let (name_end, self_time, calls) = if folded {
                (share.checked_sub(1)?, None, None)
            } else {
                let name_end = share.checked_sub(3)?;
                (
                    name_end,
                    Some(parse_duration(columns.get(name_end + 1)?)?),
                    Some(number(columns.get(name_end + 2)?)?),
                )
            };
            let name = columns.get(..name_end)?.join(" ");
            stack.truncate(depth);
            stack.push(name);
            rows.push(TimingRow {
                active: parse_duration(columns.get(name_end)?)?,
                calls,
                path: stack.clone(),
                self_time,
                wall,
            });
        }
        Some(Self {
            active: parse_duration(active)?,
            parallelism,
            rows,
            threads: threads.parse().ok()?,
            wall: parse_duration(wall)?,
        })
    }

    /// The timings as JSON.
    pub fn to_json(&self) -> Value {
        json!({
            "active_secs": self.active,
            "parallelism": self.parallelism,
            "rows": self
                .rows
                .iter()
                .map(|row| json!({
                    "active_secs": row.active,
                    "calls": row.calls,
                    "path": row.path,
                    "self_secs": row.self_time,
                    "wall_secs": row.wall,
                }))
                .collect::<Vec<_>>(),
            "threads": self.threads,
            "wall_secs": self.wall,
        })
    }
}

/// Seconds in a `Duration`'s debug form, e.g. `4.14s`, `115.94ms`,
/// `21.20µs` or `0.00ns`.
pub fn parse_duration(text: &str) -> Option<f64> {
    let units = [
        ("ns", 1e-9),
        ("µs", 1e-6),
        ("us", 1e-6),
        ("ms", 1e-3),
        ("s", 1.0),
    ];
    units.iter().find_map(|(unit, scale)| {
        let value: f64 = text.strip_suffix(unit)?.parse().ok()?;
        Some(value * scale)
    })
}

/// A number printed with thousands separators, e.g. `4,303`.
fn number(text: &str) -> Option<u64> {
    text.trim().replace(',', "").parse().ok()
}

/// The number a text starts with, e.g. `25` in `25 insertions(+)`.
fn leading_number(text: &str) -> Option<u64> {
    number(text.trim().split(' ').next()?)
}
