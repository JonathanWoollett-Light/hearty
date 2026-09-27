//! `--timings` and `--flamegraph`: where a run's time goes.
//!
//! The work is split into [`tracing`] spans (a pass over the files, a file,
//! an action on it, a formatter rule, a parse, a lint's pieces), and
//! [`Recorder`], a tracing layer, measures how long each span is *active*:
//! the time threads spend between entering and exiting it, summed over every
//! enter on every thread. That, not wall time, is the cost of work run in
//! parallel: 24 threads formatting files for one second is 24 seconds of
//! work.
//!
//! Only threads doing a span's work may have it entered, or its active time
//! counts waiting as work. So a parallel section never runs inside its
//! phase's span on the thread that waits for it: each worker enters its own
//! per-item span, created with the phase span as its explicit parent, and the
//! phase span is only entered for the phase's sequential parts.
//!
//! A span's *self* time is its active time minus that of the spans entered
//! inside it on the same thread. Spans on other threads are not subtracted,
//! as the span was not active then. So every instant a thread spends in
//! spans counts once, for its innermost span, and self times sum to the
//! total active time. A node of the [`Profile`] tree is a span name chain
//! from a root, following explicit parents across threads (so every
//! `scripts > file > format > inline > parse` span is one node, whichever
//! thread ran it), and its active time is the sum of the self times at and
//! under it: a flamegraph frame's width.
//!
//! No subscriber is installed unless `--timings` or `--flamegraph` is given,
//! so without them every span callsite is disabled and costs a relaxed
//! atomic load.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{MAIN_SEPARATOR, Path};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

/// Nodes below this share of the total active time, in tenths of a percent,
/// are folded into one `… N more` line under their parent (0.5%).
const HIDE_BELOW_PER_MILLE: u128 = 5;

/// How many of the slowest files [`Profile::timings`] lists per span path.
const SLOWEST_FILES: usize = 5;

/// Source of [`Recorder::id`]s; 0 marks a thread no recorder has seen.
static NEXT_RECORDER: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The spans entered on this thread, innermost last.
    static FRAMES: RefCell<Frames> = const {
        RefCell::new(Frames {
            owner: 0,
            stack: Vec::new(),
        })
    };
}

/// Span names from a root span down to a span.
type SpanPath = Box<[&'static str]>;

/// Per span path, the slowest spans with a `path` field (the files), by
/// active time, slowest first.
type Slowest = BTreeMap<SpanPath, Vec<(Duration, String)>>;

/// A span entered on the current thread and not yet exited.
#[derive(Debug)]
struct Frame {
    /// The entered span.
    id: Id,
    /// Active time of the spans entered and exited while this one was the
    /// innermost entered span on this thread.
    nested: Duration,
    /// When the span was entered.
    start: Instant,
}

/// One thread's entered spans.
#[derive(Debug)]
struct Frames {
    /// [`Recorder::id`] of the recorder the frames belong to; a thread seen
    /// by another recorder starts afresh.
    owner: u64,
    /// Entered spans, innermost last.
    stack: Vec<Frame>,
}

impl Frames {
    /// Pops the frame of `id`, exited at `end`, crediting its active time to
    /// the frame it was nested in. Returns its active and self time, or
    /// `None` if `id` was not entered on this thread.
    fn exit(&mut self, id: &Id, end: Instant) -> Option<(Duration, Duration)> {
        // Spans exit innermost first, so this is the last frame unless
        // guards were dropped out of order.
        let index = self.stack.iter().rposition(|frame| frame.id == *id)?;
        let frame = self.stack.remove(index);
        let active = end.saturating_duration_since(frame.start);
        if let Some(enclosing) = index
            .checked_sub(1)
            .and_then(|below| self.stack.get_mut(below))
        {
            enclosing.nested += active;
        }
        Some((active, active.saturating_sub(frame.nested)))
    }
}

/// Picks the `path` field out of a span's fields.
#[derive(Debug, Default)]
struct PathField(Option<String>);

impl Visit for PathField {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "path" {
            self.0 = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "path" {
            value.clone_into(self.0.get_or_insert_default());
        }
    }
}

/// Timings of every span that ran, aggregated by span path.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// Threads in rayon's pool.
    pool: usize,
    /// The slowest files.
    slowest: Slowest,
    /// Per span path, what its spans took.
    stats: BTreeMap<SpanPath, Stat>,
    /// Threads that entered a span.
    threads: usize,
    /// Wall time of the whole run.
    wall: Duration,
}

impl Profile {
    /// Folded stacks for a flamegraph: `a;b;c <self time in µs>` per span
    /// path. Under a microsecond counts as one, so that every span that ran
    /// has a frame.
    fn folded(&self) -> Vec<String> {
        self.stats
            .iter()
            .map(|(path, stat)| {
                let micros = stat.self_busy.as_micros().max(1);
                format!("{} {micros}", path.join(";"))
            })
            .collect()
    }

    /// The `--timings` report: totals, then the span tree with each node's
    /// active and self time, calls, share of the total active time and, for
    /// root spans (the actions), wall time; then the slowest files.
    fn timings(&self) -> String {
        let active = self.total_active();
        let mut root = Node::default();
        for (path, stat) in &self.stats {
            root.insert(path, *stat);
        }
        root.total();

        let mut rows = vec![Row {
            active: "active".to_owned(),
            calls: "calls".to_owned(),
            name: "span".to_owned(),
            self_busy: "self".to_owned(),
            share: "share".to_owned(),
            wall: "wall".to_owned(),
        }];
        root.rows(0, active, &mut rows);
        let width = |column: fn(&Row) -> &String| {
            rows.iter()
                .map(|row| column(row).chars().count())
                .max()
                .unwrap_or_default()
        };
        let name_width = width(|row| &row.name);
        let active_width = width(|row| &row.active);
        let self_width = width(|row| &row.self_busy);
        let calls_width = width(|row| &row.calls);
        let share_width = width(|row| &row.share);
        let wall_width = width(|row| &row.wall);

        let mut lines = vec![
            format!(
                "Timings: {:.2?} wall, {active:.2?} active on {} threads (rayon pool: {}), parallelism {}",
                self.wall,
                self.threads,
                self.pool,
                ratio(active, self.wall),
            ),
            "  active: time threads spent in a span and the spans under it, summed over threads"
                .to_owned(),
            "  self:   active time not spent in a span nested in it on the same thread".to_owned(),
            "  share:  active time as a share of the total; wall: first to last moment of a top-level part"
                .to_owned(),
            String::new(),
        ];
        lines.extend(rows.iter().map(|row| {
            let Row {
                active,
                calls,
                name,
                self_busy,
                share,
                wall,
            } = row;
            format!(
                "{name:<name_width$}  {active:>active_width$}  {self_busy:>self_width$}  \
                 {calls:>calls_width$}  {share:>share_width$}  {wall:>wall_width$}"
            )
            .trim_end()
            .to_owned()
        }));

        if !self.slowest.is_empty() {
            lines.push(String::new());
            lines.push("Slowest files:".to_owned());
            for (path, files) in &self.slowest {
                lines.push(format!("  {}", path.join(" > ")));
                // `/` separators, so the list reads the same on every platform.
                lines.extend(files.iter().map(|(busy, file)| {
                    format!("    {busy:>10.2?}  {}", file.replace(MAIN_SEPARATOR, "/"))
                }));
            }
        }
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }

    /// Total active time: every span's self time, summed.
    fn total_active(&self) -> Duration {
        self.stats.values().map(|stat| stat.self_busy).sum()
    }

    /// Writes the folded stacks as an SVG flamegraph to `path`.
    fn write_flamegraph(&self, path: &Path, subtitle: String) -> std::io::Result<()> {
        let lines = self.folded();
        let mut options = inferno::flamegraph::Options::default();
        options.title = format!(
            "hearty: {:.2?} wall, {:.2?} active",
            self.wall,
            self.total_active()
        );
        options.subtitle = Some(subtitle);
        // Draw every frame, however narrow: there is one per span path, so
        // few, and a short span can still be found with the search.
        options.min_width = 0.0_f64;
        "µs".clone_into(&mut options.count_name);
        "Span:".clone_into(&mut options.name_type);
        let mut writer = std::io::BufWriter::new(std::fs::File::create(path)?);
        inferno::flamegraph::from_lines(
            &mut options,
            lines.iter().map(String::as_str),
            &mut writer,
        )?;
        writer.flush()
    }
}

/// Installs a [`Recorder`] as the global subscriber, and reports what it
/// recorded when the run is done.
#[derive(Debug)]
pub struct Profiler {
    /// The installed layer.
    recorder: Recorder,
    /// When the run started.
    start: Instant,
}

impl Profiler {
    /// Prints the `--timings` report to stderr if `timings`, and writes the
    /// flamegraph to `flamegraph` if given.
    pub fn finish(self, timings: bool, flamegraph: Option<&Path>) -> std::io::Result<()> {
        let profile = self
            .recorder
            .profile(self.start.elapsed(), rayon::current_num_threads());
        if timings {
            eprint!("\n{}", profile.timings());
        }
        if let Some(path) = flamegraph {
            let args: Vec<String> = std::env::args_os()
                .skip(1)
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            profile.write_flamegraph(path, format!("hearty {}", args.join(" ")))?;
            eprintln!("Wrote flamegraph to {}", path.display());
        }
        Ok(())
    }

    /// Installs a recorder as the global subscriber, timing the run from
    /// `start`. `None` (with a warning) if a subscriber is already set.
    pub fn install(start: Instant) -> Option<Self> {
        let recorder = Recorder::new();
        let subscriber = tracing_subscriber::registry().with(recorder.clone());
        if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
            eprintln!("timings unavailable: {err}");
            return None;
        }
        Some(Self { recorder, start })
    }
}

/// The tracing layer measuring span active time; see the module docs.
#[derive(Debug, Clone)]
pub struct Recorder {
    /// Tells this recorder's [`Frames`] apart from another's on a thread.
    id: u64,
    /// What has been recorded so far.
    shared: Arc<Shared>,
}

impl Recorder {
    /// A recorder that has recorded nothing.
    fn new() -> Self {
        Self {
            id: NEXT_RECORDER.fetch_add(1, Ordering::Relaxed),
            shared: Arc::default(),
        }
    }

    /// What was recorded for the spans closed so far, for a run that took
    /// `wall` on `pool` rayon threads.
    fn profile(&self, wall: Duration, pool: usize) -> Profile {
        Profile {
            pool,
            slowest: lock(&self.shared.slowest).clone(),
            stats: lock(&self.shared.stats).clone(),
            threads: self.shared.threads.load(Ordering::Relaxed),
            wall,
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for Recorder
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let closed = Instant::now();
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(timing) = span.extensions_mut().remove::<SpanTiming>() else {
            return;
        };
        self.shared.record(timing, closed);
    }

    fn on_enter(&self, id: &Id, _ctx: Context<'_, S>) {
        let start = Instant::now();
        FRAMES.with_borrow_mut(|frames| {
            if frames.owner != self.id {
                frames.owner = self.id;
                frames.stack.clear();
                self.shared.threads.fetch_add(1, Ordering::Relaxed);
            }
            frames.stack.push(Frame {
                id: id.clone(),
                nested: Duration::ZERO,
                start,
            });
        });
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let end = Instant::now();
        let Some((active, self_busy)) = FRAMES.with_borrow_mut(|frames| {
            (frames.owner == self.id)
                .then(|| frames.exit(id, end))
                .flatten()
        }) else {
            return;
        };
        if let Some(span) = ctx.span(id)
            && let Some(timing) = span.extensions_mut().get_mut::<SpanTiming>()
        {
            timing.busy += active;
            timing.self_busy += self_busy;
        }
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        // The registry resolves the parent (explicit, or the span entered on
        // this thread), so a worker's span links to its phase's span.
        let path = span.scope().from_root().map(|span| span.name()).collect();
        let mut file = PathField::default();
        attrs.record(&mut file);
        span.extensions_mut().insert(SpanTiming {
            busy: Duration::ZERO,
            created: Instant::now(),
            file: file.0,
            path,
            self_busy: Duration::ZERO,
        });
    }
}

/// One row of [`Profile::timings`]' table, formatted.
#[derive(Debug)]
struct Row {
    /// Active time.
    active: String,
    /// Number of spans.
    calls: String,
    /// The span name, indented by depth.
    name: String,
    /// Self time.
    self_busy: String,
    /// Share of the total active time.
    share: String,
    /// Wall time (root spans only).
    wall: String,
}

/// State shared by clones of a [`Recorder`].
#[derive(Debug, Default)]
struct Shared {
    /// See [`Profile::slowest`].
    slowest: Mutex<Slowest>,
    /// See [`Profile::stats`].
    stats: Mutex<BTreeMap<SpanPath, Stat>>,
    /// See [`Profile::threads`].
    threads: AtomicUsize,
}

impl Shared {
    /// Adds a span closed at `closed` to the totals of its path.
    fn record(&self, timing: SpanTiming, closed: Instant) {
        let SpanTiming {
            busy,
            created,
            file,
            path,
            self_busy,
        } = timing;
        if let Some(file) = file {
            let mut slowest = lock(&self.slowest);
            let files = slowest.entry(path.clone()).or_default();
            let at = files.partition_point(|(other, _)| *other >= busy);
            if at < SLOWEST_FILES {
                files.insert(at, (busy, file));
                files.truncate(SLOWEST_FILES);
            }
        }
        let mut stats = lock(&self.stats);
        let stat = stats.entry(path).or_default();
        stat.calls += 1;
        stat.self_busy += self_busy;
        stat.wall += closed.saturating_duration_since(created);
    }
}

/// What a [`Recorder`] keeps about a span while it is open, in the span's
/// registry extensions.
#[derive(Debug)]
struct SpanTiming {
    /// Active time so far, including spans nested on the same thread.
    busy: Duration,
    /// When the span was created.
    created: Instant,
    /// The span's `path` field, if it has one.
    file: Option<String>,
    /// The span's path.
    path: SpanPath,
    /// Active time so far, not counting spans nested on the same thread.
    self_busy: Duration,
}

/// The spans of one span path, totalled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Stat {
    /// Number of spans (a span entered several times counts once).
    calls: u64,
    /// Total self time.
    self_busy: Duration,
    /// Total time from creating to closing each span.
    wall: Duration,
}

/// A span path in the tree [`Profile::timings`] prints.
#[derive(Debug, Default)]
struct Node {
    /// Self time of this node and every node under it; set by [`Node::total`].
    active: Duration,
    /// The paths one name longer.
    children: BTreeMap<&'static str, Node>,
    /// This path's own totals.
    stat: Stat,
}

impl Node {
    /// Adds `stat` at `path` below this node, creating nodes as needed.
    fn insert(&mut self, path: &[&'static str], stat: Stat) {
        match path.split_first() {
            Some((name, rest)) => self.children.entry(name).or_default().insert(rest, stat),
            None => self.stat = stat,
        }
    }

    /// Appends the rows of this node's children at `depth`, largest first,
    /// then theirs; children below [`HIDE_BELOW_PER_MILLE`] of `total`
    /// (except actions, at depth 0) are folded into one line.
    fn rows(&self, depth: usize, total: Duration, rows: &mut Vec<Row>) {
        let mut children: Vec<(&&str, &Self)> = self.children.iter().collect();
        children.sort_by(|(a_name, a), (b_name, b)| {
            b.active.cmp(&a.active).then_with(|| a_name.cmp(b_name))
        });
        let indent = "  ".repeat(depth);
        let (shown, hidden): (Vec<_>, Vec<_>) = children.into_iter().partition(|(_, child)| {
            depth == 0 || per_mille(child.active, total) >= HIDE_BELOW_PER_MILLE
        });
        for (name, child) in shown {
            rows.push(Row {
                active: format!("{:.2?}", child.active),
                calls: crate::fmt_commas(child.stat.calls),
                name: format!("{indent}{name}"),
                self_busy: format!("{:.2?}", child.stat.self_busy),
                share: percent(child.active, total),
                wall: if depth == 0 {
                    format!("{:.2?}", child.stat.wall)
                } else {
                    String::new()
                },
            });
            child.rows(depth + 1, total, rows);
        }
        if !hidden.is_empty() {
            let active: Duration = hidden.iter().map(|(_, child)| child.active).sum();
            rows.push(Row {
                active: format!("{active:.2?}"),
                calls: String::new(),
                name: format!("{indent}… {} more", hidden.len()),
                self_busy: String::new(),
                share: percent(active, total),
                wall: String::new(),
            });
        }
    }

    /// Sets [`Node::active`] here and below, returning this node's.
    fn total(&mut self) -> Duration {
        let children: Duration = self.children.values_mut().map(Self::total).sum();
        self.active = self.stat.self_busy + children;
        self.active
    }
}

/// Locks `mutex`, carrying on if a panicking thread poisoned it: the totals
/// are only ever added to, so they stay usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `part` as a share of `total` in tenths of a percent (0 if `total` is 0).
fn per_mille(part: Duration, total: Duration) -> u128 {
    part.as_nanos()
        .saturating_mul(1_000)
        .checked_div(total.as_nanos())
        .unwrap_or_default()
}

/// `part` as a percentage of `total` with one decimal, e.g. `12.5%`.
fn percent(part: Duration, total: Duration) -> String {
    let tenths = per_mille(part, total);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `active / wall` with two decimals, e.g. `11.05x`: how many threads were
/// busy on average.
fn ratio(active: Duration, wall: Duration) -> String {
    let hundredths = active
        .as_nanos()
        .saturating_mul(100)
        .checked_div(wall.as_nanos())
        .unwrap_or_default();
    format!("{}.{:02}x", hundredths / 100, hundredths % 100)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail loudly on unexpected input by design"
)]
mod tests {
    use super::{
        Frame, Frames, Profile, Recorder, Shared, Slowest, SpanTiming, Stat, lock, percent, ratio,
    };
    use std::thread::sleep;
    use std::time::{Duration, Instant};
    use tracing::Dispatch;
    use tracing::span::Id;
    use tracing_subscriber::layer::SubscriberExt as _;

    const MS: Duration = Duration::from_millis(1);

    /// Runs `f` with a fresh [`Recorder`] as this thread's subscriber (`f`
    /// gets it to install on the threads it starts), then returns what the
    /// recorder recorded, for a run of 1 s wall on 4 threads.
    fn record<F>(f: F) -> Profile
    where
        F: FnOnce(&Dispatch),
    {
        let recorder = Recorder::new();
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(recorder.clone()));
        tracing::dispatcher::with_default(&dispatch, || f(&dispatch));
        recorder.profile(Duration::from_secs(1), 4)
    }

    /// The totals recorded for the spans at `path`.
    fn stat(profile: &Profile, path: &[&'static str]) -> Stat {
        *profile
            .stats
            .get(path)
            .unwrap_or_else(|| panic!("{path:?} not recorded in {:?}", profile.stats))
    }

    /// A profile of a 50 ms run on 24 threads, 3 of which did work, with the
    /// given `(path, self µs, calls, wall ms)` totals.
    fn profile(stats: &[(&[&'static str], u64, u64, u32)]) -> Profile {
        Profile {
            pool: 24,
            slowest: Slowest::new(),
            stats: stats
                .iter()
                .map(|&(path, self_micros, calls, wall_ms)| {
                    let stat = Stat {
                        calls,
                        self_busy: Duration::from_micros(self_micros),
                        wall: wall_ms * MS,
                    };
                    (path.into(), stat)
                })
                .collect(),
            threads: 3,
            wall: 50 * MS,
        }
    }

    /// A frame's self time leaves out the frames entered and exited inside
    /// it, which are credited to the frame directly enclosing them only.
    #[test]
    fn frames_subtract_nested_time() {
        let base = Instant::now();
        let at = |ms: u32| base + ms * MS;
        let frame = |id: u64, start: u32| Frame {
            id: Id::from_u64(id),
            nested: Duration::ZERO,
            start: at(start),
        };
        let mut frames = Frames {
            owner: 1,
            stack: vec![frame(1, 0), frame(2, 10), frame(3, 15)],
        };
        assert_eq!(
            frames.exit(&Id::from_u64(3), at(25)),
            Some((10 * MS, 10 * MS))
        );
        assert_eq!(
            frames.exit(&Id::from_u64(2), at(40)),
            Some((30 * MS, 20 * MS))
        );
        assert_eq!(
            frames.exit(&Id::from_u64(1), at(50)),
            Some((50 * MS, 20 * MS))
        );
        assert!(frames.stack.is_empty());
        // Exiting a span that is not entered on this thread is ignored.
        assert_eq!(frames.exit(&Id::from_u64(1), at(60)), None);

        // Guards dropped out of order: the outer frame exits first, and the
        // inner one, left with nothing enclosing it, keeps its own time.
        frames.stack = vec![frame(1, 0), frame(2, 10)];
        assert_eq!(
            frames.exit(&Id::from_u64(1), at(20)),
            Some((20 * MS, 20 * MS))
        );
        assert_eq!(
            frames.exit(&Id::from_u64(2), at(30)),
            Some((20 * MS, 20 * MS))
        );
    }

    /// On one thread a span's self time leaves out the spans entered inside
    /// it, and the spans at one path add up.
    #[test]
    fn nested_spans_on_one_thread() {
        let profile = record(|_| {
            let _outer = tracing::info_span!("outer").entered();
            for _ in 0..2_u8 {
                let _inner = tracing::info_span!("inner").entered();
                sleep(25 * MS);
            }
        });
        let outer = stat(&profile, &["outer"]);
        let inner = stat(&profile, &["outer", "inner"]);
        assert_eq!((outer.calls, inner.calls), (1, 2));
        assert!(inner.self_busy >= 50 * MS, "{inner:?}");
        // Without the subtraction `outer` would have 50 ms of self time.
        assert!(outer.self_busy < 25 * MS, "{outer:?}");
        assert!(outer.wall >= outer.self_busy + inner.self_busy, "{outer:?}");
        assert_eq!(profile.total_active(), outer.self_busy + inner.self_busy);
        assert_eq!(profile.threads, 1);
    }

    /// A child span on another thread links to its explicit parent, and is
    /// not subtracted from the parent, which was busy on its own thread at
    /// the same time. A span entered twice counts one call, with the active
    /// time of both enters.
    #[test]
    fn child_on_another_thread() {
        let profile = record(|dispatch| {
            let parent = tracing::info_span!("parent");
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    tracing::dispatcher::with_default(dispatch, || {
                        let _child = tracing::info_span!(parent: &parent, "child").entered();
                        sleep(40 * MS);
                    });
                });
                parent.in_scope(|| sleep(40 * MS));
            });
            parent.in_scope(|| sleep(10 * MS));
        });
        let parent = stat(&profile, &["parent"]);
        let child = stat(&profile, &["parent", "child"]);
        assert_eq!((parent.calls, child.calls), (1, 1));
        assert!(parent.self_busy >= 50 * MS, "{parent:?}");
        assert!(child.self_busy >= 40 * MS, "{child:?}");
        assert_eq!(profile.threads, 2);
        assert_eq!(profile.total_active(), parent.self_busy + child.self_busy);
    }

    /// A phase span left unentered while workers run its items (as the pass
    /// over the script files does) counts none of the wait, while its tree
    /// node counts the items.
    #[test]
    fn waiting_is_not_active() {
        let profile = record(|dispatch| {
            let phase = tracing::info_span!("phase");
            std::thread::scope(|scope| {
                for _ in 0..2_u8 {
                    scope.spawn(|| {
                        tracing::dispatcher::with_default(dispatch, || {
                            let _item = tracing::info_span!(parent: &phase, "item").entered();
                            sleep(30 * MS);
                        });
                    });
                }
            });
        });
        let phase = stat(&profile, &["phase"]);
        let item = stat(&profile, &["phase", "item"]);
        assert_eq!(phase.self_busy, Duration::ZERO);
        assert!(phase.wall >= 30 * MS, "{phase:?}");
        assert_eq!(item.calls, 2);
        assert!(item.self_busy >= 60 * MS, "{item:?}");
        // Only the two workers entered a span.
        assert_eq!(profile.threads, 2);
        let timings = profile.timings();
        let row = timings
            .lines()
            .find(|line| line.starts_with("phase "))
            .expect("phase row");
        assert!(row.contains("0.00ns"), "{timings}");
    }

    /// Wall time runs from creating a span to closing it, active time only
    /// while it is entered.
    #[test]
    fn reentered_span() {
        let profile = record(|_| {
            let span = tracing::info_span!("span");
            for _ in 0..3_u8 {
                span.in_scope(|| sleep(10 * MS));
                sleep(10 * MS);
            }
        });
        let span = stat(&profile, &["span"]);
        assert_eq!(span.calls, 1);
        assert!(span.self_busy >= 30 * MS, "{span:?}");
        assert!(span.wall >= span.self_busy + 30 * MS, "{span:?}");
    }

    /// Spans with a `path` field are listed as the slowest files of their
    /// span path, whether the field is a string or a displayed value.
    #[test]
    fn slowest_files_from_path_fields() {
        let profile = record(|_| {
            tracing::info_span!("file", path = "fast.txt").in_scope(|| {});
            let slow = std::path::Path::new("slow.txt");
            tracing::info_span!("file", path = %slow.display()).in_scope(|| sleep(5 * MS));
        });
        let files: Vec<&str> = profile
            .slowest
            .get(["file"].as_slice())
            .expect("files listed")
            .iter()
            .map(|(_, file)| file.as_str())
            .collect();
        assert_eq!(files, ["slow.txt", "fast.txt"]);
    }

    /// Only the slowest few files are kept, slowest first, and the totals
    /// count every span.
    #[test]
    fn slowest_files_are_capped() {
        let shared = Shared::default();
        let now = Instant::now();
        for ms in [3, 7, 1, 5, 2, 6, 4] {
            shared.record(
                SpanTiming {
                    busy: ms * MS,
                    created: now,
                    file: Some(format!("{ms}.txt")),
                    path: ["format", "file"].into(),
                    self_busy: ms * MS,
                },
                now + 10 * MS,
            );
        }
        let slowest = lock(&shared.slowest);
        let files: Vec<&str> = slowest
            .get(["format", "file"].as_slice())
            .expect("files listed")
            .iter()
            .map(|(_, file)| file.as_str())
            .collect();
        assert_eq!(files, ["7.txt", "6.txt", "5.txt", "4.txt", "3.txt"]);
        let stats = lock(&shared.stats);
        let stat = stats.get(["format", "file"].as_slice()).expect("recorded");
        assert_eq!(stat.calls, 7);
        assert_eq!(stat.self_busy, 28 * MS);
        assert_eq!(stat.wall, 70 * MS);
    }

    /// The tree lists children by active time, the time at and under them;
    /// nodes below 0.5% of the total are folded into one line, except
    /// actions; only actions show wall time.
    #[test]
    fn timings_tree() {
        let profile = profile(&[
            (&["format"], 1_000, 1, 45),
            (&["format", "file"], 9_000, 1_234, 0),
            (&["format", "file", "field order"], 20_000, 1_234, 0),
            (&["format", "file", "inline"], 10_000, 1_234, 0),
            (&["format", "file", "inline", "parse"], 50_000, 4_000, 0),
            (&["format", "file", "read"], 300, 1_234, 0),
            (&["format", "file", "write"], 200, 1_234, 0),
            (&["lint"], 0, 1, 3),
            (&["lint", "missing keys"], 100, 1, 0),
        ]);
        let expected = "\
Timings: 50.00ms wall, 90.60ms active on 3 threads (rayon pool: 24), parallelism 1.81x
  active: time threads spent in a span and the spans under it, summed over threads
  self:   active time not spent in a span nested in it on the same thread
  share:  active time as a share of the total; wall: first to last moment of a top-level part

span               active     self  calls  share     wall
format            90.50ms   1.00ms      1  99.8%  45.00ms
  file            89.50ms   9.00ms  1,234  98.7%
    inline        60.00ms  10.00ms  1,234  66.2%
      parse       50.00ms  50.00ms  4,000  55.1%
    field order   20.00ms  20.00ms  1,234  22.0%
    … 2 more     500.00µs                   0.5%
lint             100.00µs   0.00ns      1   0.1%   3.00ms
  … 1 more       100.00µs                   0.1%
";
        assert_eq!(profile.timings(), expected);
    }

    /// Folded stacks hold each path's self time in microseconds, at least 1.
    #[test]
    fn folded_stacks() {
        let profile = profile(&[
            (&["lint"], 0, 1, 3),
            (&["lint", "missing keys"], 250, 1, 0),
            (&["lint", "walk"], 1_500, 10, 0),
        ]);
        assert_eq!(
            profile.folded(),
            ["lint 1", "lint;missing keys 250", "lint;walk 1500"]
        );
    }

    /// The flamegraph is an SVG titled with the run's wall and active time.
    #[test]
    fn flamegraph_svg() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("flamegraph.svg");
        let profile = profile(&[
            (&["format"], 2_000, 1, 3),
            (&["format", "file"], 8_000, 2, 0),
        ]);
        profile
            .write_flamegraph(&path, "hearty --format".to_owned())
            .expect("flamegraph written");
        let svg = std::fs::read_to_string(&path).expect("flamegraph read");
        assert!(svg.starts_with("<?xml"), "{svg}");
        assert!(
            svg.contains("hearty: 50.00ms wall, 10.00ms active"),
            "{svg}"
        );
        assert!(svg.contains("hearty --format"), "{svg}");
        assert!(svg.contains("file"), "{svg}");
    }

    /// Every span that ran has a frame in the flamegraph, however short:
    /// neither rounded down to nothing nor left out as too narrow to see.
    #[test]
    fn flamegraph_keeps_short_spans() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("flamegraph.svg");
        // Next to 10 s, 5 µs is under inferno's default minimum frame width
        // (0.01% of the total), and under 1 µs rounds down to 0.
        let profile = profile(&[
            (&["format"], 10_000_000, 1, 3),
            (&["format", "keys"], 5, 3, 0),
            (&["format", "lookup"], 0, 3, 0),
        ]);
        profile
            .write_flamegraph(&path, "hearty --format".to_owned())
            .expect("flamegraph written");
        let svg = std::fs::read_to_string(&path).expect("flamegraph read");
        for frame in ["format", "keys", "lookup"] {
            assert!(svg.contains(&format!(">{frame} (")), "no {frame} in {svg}");
        }
    }

    /// Shares and parallelism are printed with fixed decimals, and are 0
    /// rather than a division by zero when nothing was timed.
    #[test]
    fn ratios() {
        assert_eq!(percent(MS, 3 * MS), "33.3%");
        assert_eq!(percent(3 * MS, 3 * MS), "100.0%");
        assert_eq!(percent(MS, Duration::ZERO), "0.0%");
        assert_eq!(ratio(250 * MS, 100 * MS), "2.50x");
        assert_eq!(ratio(MS, Duration::ZERO), "0.00x");
    }
}
