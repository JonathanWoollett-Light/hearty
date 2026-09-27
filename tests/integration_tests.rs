//! Runs the `hearty` binary on copies of `tests/test_mod`, a small mod with a
//! file of every kind hearty handles (the comments in its files say what each
//! one is for), and on small mods the tests build.
//!
//! `--lint` checks `descriptor.mod` against HOI4's newest version, which
//! hearty reads from a cache and, when that is missing or stale, asks
//! steamcmd for, downloading steamcmd if it has none. No test may reach the
//! network: every lint run here has a fresh cache (see [`lint`]) or a fake
//! steamcmd where hearty looks for one (see [`lint_with_fake_steamcmd`]).
//! As a second line of defence, [`hearty`] sends hearty's HTTP requests to a
//! proxy on a closed local port, so a download started by mistake fails
//! instead of leaving the machine.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::prelude::*;
use serde_json::{Value, json};

#[cfg(windows)]
const HASH: &str = "SDPQEdSrbAqnTERExVNhfB4hBcOpVbaC2gMgBrDmfwE=";
#[cfg(not(windows))]
const HASH: &str = "13uqw3XrAxxoOWM/OGVkKf/b11xpJQWEj6oahOgx3RI=";

const BINARY: &str = env!("CARGO_BIN_EXE_hearty");

/// The HOI4 version cache inside `HEARTY_CACHE_DIR` (see `cache_path` in
/// `main.rs`).
const CACHE_FILE: &str = "hoi4-version-cache.json";

/// Branches of the cache [`seeded_cache`] writes: the newest version is
/// 1.17.3, and the last two are not versions at all.
const CACHED_BRANCHES: &[&str] = &["1.16.1", "1.17.3.0", "1.9.x", "public"];

/// A proxy URL on a port nothing listens on (see the module docs).
const DEAD_PROXY: &str = "http://127.0.0.1:9";

/// Output of the fake steamcmd: `app_info_print 394360` cut down to what
/// hearty reads, between the log lines steamcmd prints around it. The newest
/// branch is 1.18.0.0, newer than any in [`CACHED_BRANCHES`], so a test can
/// tell which source hearty used; `394361` is repeated because VDF allows
/// duplicate keys.
const FAKE_APP_INFO: &str = r#"Redirecting stderr to 'logs\stderr.txt'
Loading Steam API...OK
Connecting anonymously to Steam Public...OK
AppID : 394360, change number : 1/0, last change : Thu Jan  1 00:00:00 2026
"394360"
{
	"common"
	{
		"name"		"Hearts of Iron IV"
	}
	"depots"
	{
		"branches"
		{
			"public"
			{
				"buildid"		"3"
			}
			"1.16.1"
			{
				"buildid"		"1"
			}
			"1.18.0.0"
			{
				"buildid"		"2"
			}
		}
		"394361"
		{
			"oslist"		"windows"
		}
		"394361"
		{
			"oslist"		"linux"
		}
	}
}
Unloading Steam API...OK
"#;

/// Tells a copy of this binary run as steamcmd what to print; see
/// [`steamcmd_stub_login_anonymous`].
const FAKE_STEAMCMD_ENV: &str = "HEARTY_TEST_FAKE_STEAMCMD";

/// A file the fake steamcmd appends its arguments to, a line per run, so a
/// test can tell that hearty ran it: hearty ignores a steamcmd that fails to
/// start, which otherwise looks just like one whose output it ignored.
const FAKE_STEAMCMD_LOG_ENV: &str = "HEARTY_TEST_FAKE_STEAMCMD_LOG";

/// Where hearty looks for steamcmd: this name on `PATH` (`steamcmd_in_path`
/// in `main.rs`), then this name in the cache directory
/// (`steamcmd_cache_path`).
#[cfg(windows)]
const STEAMCMD_ON_PATH: &str = "steamcmd.exe";
#[cfg(not(windows))]
const STEAMCMD_ON_PATH: &str = "steamcmd";
#[cfg(windows)]
const STEAMCMD_IN_CACHE: &str = "steamcmd.exe";
#[cfg(not(windows))]
const STEAMCMD_IN_CACHE: &str = "steamcmd.sh";

/// Files of the test mod that no command changes: files that do not parse,
/// are not UTF-8, are already formatted (the sorters leave blocks sharing a
/// line alone), or are not script files.
const UNCHANGED_FILES: &[&str] = &[
    "common/national_focus/readme.md",
    "common/national_focus/shared_line.txt",
    "events/latin1.txt",
    "events/namespace_only.txt",
    "events/shared_line.txt",
    "events/unclosed.txt",
    "history/states/1-Stray.txt",
    "history/states/2-Formatted.txt",
    "interface/hearty_l_english.gfx",
    "localisation/english/hearty_l_english.yml",
    "localisation/english/hearty_notes_l_english.txt",
];

/// How a run of the binary exited, and what it printed.
struct Run {
    status: ExitStatus,
    stderr: String,
    stdout: String,
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

/// The fake steamcmd, not a real test. [`install_fake_steamcmd`] puts this
/// test binary where hearty looks for steamcmd. hearty runs it as
/// `steamcmd +login anonymous +app_info_print 394360 +quit`, the test harness
/// takes those arguments as test-name filters, and `anonymous` picks out this
/// test alone. It logs its arguments to [`FAKE_STEAMCMD_LOG_ENV`], then
/// prints [`FAKE_APP_INFO`] (or, in `garbage` mode, text with no app info)
/// straight to stdout, which the harness does not capture. In a normal test
/// run [`FAKE_STEAMCMD_ENV`] is unset and it does nothing.
#[test]
fn steamcmd_stub_login_anonymous() {
    let Ok(mode) = std::env::var(FAKE_STEAMCMD_ENV) else {
        return;
    };
    let log = std::env::var_os(FAKE_STEAMCMD_LOG_ENV).unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    writeln!(log, "{}", args.join(" ")).unwrap();
    let output = match mode.as_str() {
        "app_info" => FAKE_APP_INFO,
        _ => "Loading Steam API...OK\nConnecting anonymously to Steam Public...FAILED\n",
    };
    let mut stdout = std::io::stdout();
    stdout.write_all(output.as_bytes()).unwrap();
    stdout.flush().unwrap();
}

/// A `hearty` command on `path` with `flags`, sending HTTP(S) requests to
/// [`DEAD_PROXY`].
fn hearty(path: &Path, flags: &[&str]) -> Command {
    let mut command = hearty_with(&[]);
    command.arg(path).args(flags);
    command
}

/// A `hearty` command with `args` (and no path unless they hold one),
/// sending HTTP(S) requests to [`DEAD_PROXY`].
fn hearty_with(args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        // Leave out the base game's localisation, which would be read from
        // the HOI4 installed on the machine, unless a test names a game.
        .env("HEARTY_GAME_DIR", "");
    for var in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
        command.env(var, DEAD_PROXY);
    }
    command
}

fn output(command: &mut Command) -> Run {
    let output = command.output().unwrap();
    Run {
        status: output.status,
        stderr: String::from_utf8(output.stderr).unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
    }
}

/// Runs the binary on `path` with `flags`, which must not lint (see
/// [`lint`]).
fn run(path: &Path, flags: &[&str]) -> Run {
    assert!(
        !flags.contains(&"--lint")
            && ["--check", "--fix", "--format"]
                .iter()
                .any(|action| flags.contains(action)),
        "linting needs a version cache; use `lint`"
    );
    output(&mut hearty(path, flags))
}

/// A `hearty` command on `path` with `flags` (none lints: it is the default
/// action) and `cache` as `HEARTY_CACHE_DIR`, after checking that it holds a
/// fresh cache, so the version check never needs steamcmd. `PATH` is `cache`
/// too, which holds no steamcmd either.
fn lint_command(path: &Path, flags: &[&str], cache: &Path) -> Command {
    assert_fresh_cache(cache);
    assert!(
        !cache.join(STEAMCMD_ON_PATH).exists(),
        "{} holds a steamcmd",
        cache.display()
    );
    let mut command = hearty(path, flags);
    command.env("HEARTY_CACHE_DIR", cache).env("PATH", cache);
    command
}

/// Runs [`lint_command`].
fn lint(path: &Path, flags: &[&str], cache: &Path) -> Run {
    output(&mut lint_command(path, flags, cache))
}

/// Runs `hearty --lint` on `path` with `cache` as `HEARTY_CACHE_DIR` and
/// `bin` as `PATH`, after checking that a fake steamcmd (see
/// [`install_fake_steamcmd`]) is in one of them, so hearty runs it rather
/// than downloading the real one. `mode` says what the fake prints. Panics
/// unless hearty ran the fake exactly once, as steamcmd is run.
fn lint_with_fake_steamcmd(path: &Path, cache: &Path, bin: &Path, mode: &str) -> Run {
    assert!(
        bin.join(STEAMCMD_ON_PATH).is_file() || cache.join(STEAMCMD_IN_CACHE).is_file(),
        "no fake steamcmd where hearty looks: it would download the real one"
    );
    let log_dir = tempfile::TempDir::new().unwrap();
    let log = log_dir.path().join("steamcmd-runs.txt");
    let result = output(
        hearty(path, &["--lint"])
            .env("HEARTY_CACHE_DIR", cache)
            .env("PATH", bin)
            .env(FAKE_STEAMCMD_ENV, mode)
            .env(FAKE_STEAMCMD_LOG_ENV, &log),
    );
    let runs = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        runs.lines().collect::<Vec<_>>(),
        ["+login anonymous +app_info_print 394360 +quit"],
        "hearty did not run the fake steamcmd once\n{result}"
    );
    result
}

/// Puts this test binary at `dir/name` to stand in for steamcmd; see
/// [`steamcmd_stub_login_anonymous`].
///
/// On unix it is a symlink, not a copy. A hearty process that another test
/// forks while the copy is still open for writing holds that open file until
/// it execs, and running the copy meanwhile fails with `ETXTBSY` ("Text file
/// busy"), which hearty takes as steamcmd failing. Windows processes inherit
/// no such handle, and symlinks there need developer mode, so there it is
/// copied.
fn install_fake_steamcmd(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let exe = std::env::current_exe().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(exe, dir.join(name)).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(exe, dir.join(name)).unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// HOI4's app info as hearty caches it, listing `branches`.
fn app_info(branches: &[&str]) -> Value {
    let branches: serde_json::Map<String, Value> = branches
        .iter()
        .map(|branch| ((*branch).to_owned(), json!({})))
        .collect();
    json!({ "394360": { "depots": { "branches": branches } } })
}

/// Writes a version cache holding `data`, fetched at `fetched_at_secs`, to
/// `dir`.
fn write_cache(dir: &Path, fetched_at_secs: u64, data: &Value) {
    std::fs::create_dir_all(dir).unwrap();
    let cache = json!({ "fetched_at_secs": fetched_at_secs, "data": data });
    std::fs::write(dir.join(CACHE_FILE), cache.to_string()).unwrap();
}

/// The version cache in `dir`.
fn read_cache(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(CACHE_FILE)).unwrap()).unwrap()
}

/// Panics unless `dir` holds a version cache fetched in the last hour (hearty
/// trusts one for a day).
fn assert_fresh_cache(dir: &Path) {
    let fetched_at = read_cache(dir)["fetched_at_secs"].as_u64().unwrap();
    assert!(
        now_secs().abs_diff(fetched_at) < 3_600,
        "the version cache in {} is not fresh: hearty would run steamcmd",
        dir.display()
    );
}

/// A temp `HEARTY_CACHE_DIR` with a fresh cache listing [`CACHED_BRANCHES`].
fn seeded_cache() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    write_cache(dir.path(), now_secs(), &app_info(CACHED_BRANCHES));
    dir
}

/// Copies the test mod into a fresh temp dir, returning the dir guard and the
/// copied mod path.
fn copy_test_mod() -> (tempfile::TempDir, PathBuf) {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let to = tmp_dir.path().join("test_mod");
    copy_dir::copy_dir("tests/test_mod", &to).unwrap();
    (tmp_dir, to)
}

/// Reads every file under `dir` once. On Windows the first read of a file
/// after it is written waits for the antivirus to scan it; a timed run of
/// a freshly copied mod can spend most of its time there, folding the parts
/// a test looks for into `… N more` and leaving them out of the flamegraph.
fn read_all(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_all(&path);
        } else {
            std::fs::read(&path).unwrap();
        }
    }
}

/// A mod holding just a `descriptor.mod` with `descriptor`.
fn descriptor_only_mod(descriptor: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let to = tmp_dir.path().join("mod");
    std::fs::create_dir(&to).unwrap();
    std::fs::write(to.join("descriptor.mod"), descriptor).unwrap();
    (tmp_dir, to)
}

/// A file of the mod at `root`, with line endings normalised to `\n`.
fn read(root: &Path, file: &str) -> String {
    std::fs::read_to_string(root.join(file))
        .unwrap()
        .replace("\r\n", "\n")
}

/// A file of the mod at `root`, byte for byte.
fn read_bytes(root: &Path, file: &str) -> Vec<u8> {
    std::fs::read(root.join(file)).unwrap()
}

/// Sets or clears the read-only flag of `path`.
fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn test_fmt() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let to = tmp_dir.path().join("test_mod");
    println!("using \"{}\"", to.display());

    if to.exists() {
        std::fs::remove_dir_all(&to).unwrap();
    }
    copy_dir::copy_dir("tests/test_mod", &to).unwrap();
    println!("copied");

    let result = run(&to, &["--format"]);
    println!("executed");

    assert!(result.status.success(), "{result}"); // Assert that the tool finished successfully.

    // Print the formatted files so they can be inspected with `--nocapture`
    // before the temp dir is dropped at end of scope.
    let formatted_dirs = [to.join("events"), to.join("common").join("national_focus")];
    for entry in walkdir::WalkDir::new(&to) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }
        let in_formatted_dir = entry
            .path()
            .parent()
            .is_some_and(|p| formatted_dirs.iter().any(|d| d == p));
        if !in_formatted_dir {
            continue;
        }
        let relative = entry.path().strip_prefix(&to).unwrap_or(entry.path());
        let contents = String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).into_owned();
        println!("--- {} ---\n{contents}", relative.display());
    }

    // Check that the result from formatting or fixing matches a specific target.
    let hash = dasher::hash_directory(to.clone()).unwrap();
    assert_eq!(BASE64_STANDARD.encode(&hash), HASH);
}

/// Formatting must be idempotent: a second `--format` over already-formatted
/// files must produce byte-identical output (the directory hash is unchanged).
/// Guards against the sort/ separator oscillation that made the formatter churn
/// the file on every run.
#[test]
fn test_fmt_idempotent() {
    let (_tmp, to) = copy_test_mod();

    let format = || {
        let result = run(&to, &["--format"]);
        assert!(result.status.success(), "{result}");
        (dasher::hash_directory(to.clone()).unwrap(), result.stdout)
    };

    let (first, _) = format();
    let (second, stdout) = format();
    assert_eq!(
        BASE64_STANDARD.encode(&first),
        BASE64_STANDARD.encode(&second),
        "second --format changed the files; formatting is not idempotent"
    );
    assert!(
        stdout.contains("Formatting: all files already formatted."),
        "{stdout}"
    );
}

/// `--format` reorders decision fields, joins short blocks, normalises spacing
/// and reports the lines it changed.
#[test]
fn test_fmt_rules_and_report() {
    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--format"]);
    assert!(result.status.success(), "{result}");
    let stdout = &result.stdout;

    let decisions = read(&to, "common/decisions/malta.txt");
    // Fields follow the canonical decision order (`icon` before `complete_effect`).
    let icon = decisions.find("icon = generic_construction").unwrap();
    let effect = decisions
        .find("complete_effect = { add_political_power = 50 }")
        .unwrap();
    assert!(icon < effect, "{decisions}");
    // Short single-entry blocks are joined; multi-entry blocks are not.
    assert!(
        decisions.contains("\t\tallowed = { original_tag = MLT }\n"),
        "{decisions}"
    );
    assert!(
        decisions.contains("\t\tavailable = { has_war = no }\n"),
        "{decisions}"
    );
    assert!(
        decisions.contains("\t\tcomplete_effect = {\n\t\t\tadd_stability = 0.05\n"),
        "{decisions}"
    );
    // Operators get single spaces.
    assert!(decisions.contains("\t\tcost = 25\n"), "{decisions}");
    // Formatting never removes redundant fields; that's `--fix`.
    assert!(decisions.contains("fire_only_once = no"), "{decisions}");

    let focuses = read(&to, "common/national_focus/bulgaria.txt");
    assert!(focuses.contains("x = -1"), "{focuses}");

    assert!(stdout.contains("Formatted 20 files:"), "{stdout}");
    assert!(stdout.contains("common/decisions/malta.txt"), "{stdout}");
    assert!(
        stdout.contains("20 files changed, 190 insertions(+), 220 deletions(-)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Changes: 20 blocks joined onto one line, 3 comments reflowed, 13 events moved, 39 \
             blocks with reordered fields, 9 focuses moved, 1 blank-line fix, 10 spacing fixes"
        ),
        "{stdout}"
    );
}

/// `--format` rewraps comments of prose wider than `--max-width` to fit,
/// and leaves alone commented-out script (a block, a trigger, a localisation
/// entry, a list of names), decorations, a paragraph that may run on into the
/// line below it, comments after code, a word too wide for any line and `#`
/// in a string, however wide.
#[test]
fn test_fmt_reflows_comments() {
    const FILE: &str = "common/scripted_effects/hearty_comments.txt";
    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--format"]);
    assert!(result.status.success(), "{result}");
    assert!(
        result.stdout.contains(
            "common/scripted_effects/hearty_comments.txt         +7   -5  (3 comments reflowed)"
        ),
        "{result}"
    );
    let original = read(Path::new("tests/test_mod"), FILE);
    let comments = read(&to, FILE);
    // A paragraph wrapped at 115 columns is refilled at 100.
    assert!(
        comments.starts_with(
            "# Comments of prose wider than --max-width (100 columns unless set) are rewrapped to \
             fit. This\n# paragraph is wrapped at 115 columns, so --format refills it at 100; the \
             comments below show what\n# it leaves as they are.\n#\n"
        ),
        "{comments}"
    );
    // A note is wrapped on its own, and the next is not joined to it.
    assert!(
        comments.contains(
            "\t# A note of its own is wider than the limit, so it is wrapped by itself; the note \
             below is not\n\t# joined to it.\n\t# TODO: a note starting with a label stays on its \
             own line.\n"
        ),
        "{comments}"
    );
    // An item's words hang under its text.
    assert!(
        comments.contains(
            "\t# - The first item is wider than the limit, so its words carry over onto lines \
             that hang under\n\t#   the text after its bullet, and on into its own continuation \
             line.\n\t# - The second item fits.\n"
        ),
        "{comments}"
    );
    // The other lines wider than 100 columns are as they were: commented-out
    // script, decorations, a paragraph that may run on, a comment after
    // code, a link and a string.
    let wide: Vec<&str> = original
        .lines()
        .filter(|line| line.len() > 100 && !comments.contains(&format!("{line}\n")))
        .collect();
    assert_eq!(
        wide,
        [
            "# Comments of prose wider than --max-width (100 columns unless set) are rewrapped to \
             fit. This paragraph is",
            "# wrapped at 115 columns, so --format refills it at 100; the comments below show what \
             it leaves as they are.",
            "\t# A note of its own is wider than the limit, so it is wrapped by itself; the note \
             below is not joined to it.",
            "\t# - The first item is wider than the limit, so its words carry over onto lines that \
             hang under the text",
        ],
        "only the prose changed\n{comments}"
    );
    assert_eq!(
        original.lines().filter(|line| line.len() > 100).count(),
        wide.len() + 11,
        "eleven other lines are wider than 100 columns"
    );

    // Formatted, the file passes the check; unformatted, it fails it.
    let result = run(&to, &["--check"]);
    assert!(result.status.success(), "{result}");
    std::fs::copy(Path::new("tests/test_mod").join(FILE), to.join(FILE)).unwrap();
    let result = run(&to, &["--check"]);
    assert_eq!(result.status.code(), Some(1), "{result}");
    assert!(
        result.stdout.contains(
            "1 file would be reformatted:\n  common/scripted_effects/hearty_comments.txt  +7  -5  \
             (3 comments reflowed)\n"
        ),
        "{result}"
    );
}

/// `--format` handles every kind of script file, keeps each file's line
/// endings and BOM, and leaves alone what it cannot or need not change.
#[test]
fn test_fmt_every_file_kind() {
    let (_tmp, to) = copy_test_mod();
    let original = |file: &str| read_bytes(Path::new("tests/test_mod"), file);
    let result = run(&to, &["--format"]);
    assert!(result.status.success(), "{result}");
    let stdout = &result.stdout;

    // Per-file breakdowns name each kind of change.
    for line in [
        "events/chains.txt                                  +37  -37  (5 events moved, 3 blocks \
         with reordered fields)",
        "events/spacing.txt                                  +1   -2  (1 blank-line fix)",
        "common/countries/Hearty.txt                         +2   -2  (2 spacing fixes)",
        "common/national_focus/odd_shapes.txt                +6   -5  (2 focuses moved)",
        "events/odd_shapes.txt                               +8   -8  (2 events moved)",
        "common/scripted_effects/hearty_comments.txt         +7   -5  (3 comments reflowed)",
    ] {
        assert!(stdout.contains(line), "{line}\n{stdout}");
    }
    for file in UNCHANGED_FILES.iter().chain(&["events/cycle.txt"]) {
        assert!(!stdout.contains(file), "{file} was reformatted\n{stdout}");
        assert_eq!(read_bytes(&to, file), original(file), "{file} changed");
    }
    // Among those: events sharing a line (`} country_event = {`), a focus
    // on its tree's `{` line, and out-of-order events in a file that does
    // not parse are left as they are, however out of order.
    assert!(
        read(&to, "events/shared_line.txt").find("id = shared_line.2")
            < read(&to, "events/shared_line.txt").find("id = shared_line.1")
    );
    assert!(
        read(&to, "events/unclosed.txt").find("id = unclosed.2")
            < read(&to, "events/unclosed.txt").find("id = unclosed.1")
    );

    // Characters: the character's own fields, those of its roles (also
    // inside `instance`) are sorted.
    let characters = read(&to, "common/characters/hearty_characters.txt");
    assert!(
        characters.contains(
            "\tHRT_leader = {\n\t\tallowed = { original_tag = HRT }\n\t\tname = HRT_leader_name\n"
        ),
        "{characters}"
    );
    assert!(
        characters.contains("\t\t\tfield_marshal = {\n\t\t\t\ttraits = { }\n\t\t\t\tskill = 3\n"),
        "{characters}"
    );
    // Decision categories: sorted, and the BOM stays.
    let categories = read(&to, "common/decisions/categories/hearty_categories.txt");
    assert!(
        categories.starts_with("\u{feff}HRT_category = {\n\ticon = generic_political_actions\n"),
        "{categories}"
    );
    // Ideas: the file is CRLF on every platform, and stays so.
    let ideas = String::from_utf8(read_bytes(&to, "common/ideas/hearty_ideas.txt")).unwrap();
    assert!(
        ideas.contains(
            "\t\t\tcancel = { always = no }\r\n\t\t\tmodifier = { stability_factor = 0.05 }\r\n"
        ),
        "{ideas}"
    );
    assert!(!ideas.replace("\r\n", "").contains('\n'), "{ideas}");
    // Technologies: `path` and `folder` are sorted too; `@` constants stay.
    let tech = read(&to, "common/technologies/hearty_tech.txt");
    assert!(
        tech.contains(
            "\t\tpath = {\n\t\t\tleads_to_tech = HRT_tech_b\n\t\t\tresearch_cost_coeff = 1\n"
        ),
        "{tech}"
    );
    assert!(
        tech.contains(
            "\t\tfolder = {\n\t\t\tname = infantry_folder\n\t\t\tposition = { x = 0 y = 0 }\n"
        ),
        "{tech}"
    );
    assert!(tech.contains("\t@hearty_year = 1936\n"), "{tech}");
    // Events: chains stay together (chains.2 and .3 follow chains.1, which
    // fires them), in natural order (chains.10 last); the file stays LF.
    let chains = String::from_utf8(read_bytes(&to, "events/chains.txt")).unwrap();
    assert!(!chains.contains('\r'), "{chains}");
    let positions: Vec<usize> = [
        "chains.1",
        "chains.2",
        "chains.3",
        "chains.4",
        "chains.5",
        "chains.10",
    ]
    .iter()
    .map(|id| chains.find(&format!("\tid = {id}\n")).unwrap())
    .collect();
    assert!(positions.is_sorted(), "{chains}");
    assert!(
        chains.contains("\tdesc = {\n\t\ttext = chains.3.d_war\n\t\ttrigger = { has_war = yes }\n"),
        "{chains}"
    );
    // A leading comment travels with its event.
    assert!(
        chains.contains("# Fired by chains.1.\ncountry_event = {\n\tid = chains.3\n"),
        "{chains}"
    );
    // So does a comment after its closing brace, which stays on that line
    // (and so does not become the next event's leading comment, which
    // made formatting add a blank line on every other run).
    let comments = read(&to, "events/comments.txt");
    assert!(
        comments.ends_with("\tid = comments.3\n\thidden = yes\n}# Ends comments.3.\n"),
        "{comments}"
    );
    assert!(
        comments.find("id = comments.1") < comments.find("id = comments.2"),
        "{comments}"
    );
    // What is not an event stays where it is; the events are sorted.
    let odd = read(&to, "events/odd_shapes.txt");
    assert!(
        odd.contains(
            "add_namespace = odd\ncountry_event = odd.3\n\ncountry_event = {\n\tid = odd.1\n"
        ),
        "{odd}"
    );
    assert!(odd.find("id = odd.1") < odd.find("id = odd.2"), "{odd}");
    // A cycle of events cannot be sorted and is left as it is.
    let cycle = read(&to, "events/cycle.txt");
    assert!(
        cycle.find("id = cycle.2") < cycle.find("id = cycle.1"),
        "{cycle}"
    );
    // Only direct children of `events/` are event files: elsewhere an
    // option is an ordinary block, and a short one is joined.
    let nested = read(&to, "events/nested/deep.txt");
    assert!(
        nested.contains("\toption = { name = nested.1.a }\n"),
        "{nested}"
    );
    // Focuses: sorted so prerequisites come first; the comment moves with
    // its focus, and the focus tree's own fields are sorted.
    let tree = read(&to, "common/national_focus/hearty_tree.txt");
    assert!(
        tree.contains("\t# The root of the tree.\n\tfocus = {\n\t\tid = HRT_first\n"),
        "{tree}"
    );
    assert!(
        tree.find("id = HRT_first") < tree.find("id = HRT_second"),
        "{tree}"
    );
    assert!(
        tree.contains("focus_tree = {\n\tid = \"hearty_focus_tree\"\n\tcountry = {\n"),
        "{tree}"
    );
    // A tree with a prerequisite cycle is not sorted, nor are the other
    // trees of its file; fields are still sorted.
    let cycle = read(&to, "common/national_focus/cycle.txt");
    assert!(
        cycle.find("id = HRT_acyclic_b") < cycle.find("id = HRT_acyclic_a"),
        "{cycle}"
    );
    // A focus without an id stays first; the others are sorted after it.
    let odd = read(&to, "common/national_focus/odd_shapes.txt");
    assert!(odd.find("x = 3") < odd.find("id = HRT_odd_a"), "{odd}");
    assert!(
        odd.find("id = HRT_odd_a") < odd.find("id = HRT_odd_b"),
        "{odd}"
    );
    // Shared and joint focuses are focuses too.
    let shared = read(&to, "common/national_focus/shared.txt");
    assert!(
        shared.contains("\toffset = {\n\t\tx = 1\n\t\ty = 1\n\t\ttrigger = { tag = HRT }\n"),
        "{shared}"
    );
    // Other files: spacing and joining only.
    let history = read(&to, "history/countries/HRT - Hearty.txt");
    assert!(
        history.starts_with("capital = 1\noob = \"HRT_1936\"\n"),
        "{history}"
    );
    assert!(
        history.contains("add_ideas = { HRT_idea HRT_advisor_idea }\n"),
        "{history}"
    );
    let countries = read(&to, "common/countries/Hearty.txt");
    assert!(
        countries.contains("color = rgb { 120 40 40 }\ncolor_ui = rgb { 150 60 60 }\n"),
        "{countries}"
    );
}

/// `--max-width` limits the lines joining may create, and the width comments
/// are rewrapped to.
#[test]
fn test_fmt_max_width() {
    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--format", "--max-width", "38"]);
    assert!(result.status.success(), "{result}");
    let decisions = read(&to, "common/decisions/malta.txt");
    // 8 columns of tabs + 28 characters fits in 38 columns...
    assert!(
        decisions.contains("\t\tavailable = { has_war = no }\n"),
        "{decisions}"
    );
    // ...but 8 + 32 does not.
    assert!(
        decisions.contains("\t\tallowed = {\n\t\t\toriginal_tag = MLT\n\t\t}\n"),
        "{decisions}"
    );
    // A narrower limit joins fewer blocks than the default of 100, and
    // rewraps more comments.
    assert!(
        result
            .stdout
            .contains("Changes: 5 blocks joined onto one line, 20 comments reflowed"),
        "{result}"
    );
    let decisions = read(&to, "common/decisions/hearty_decisions.txt");
    assert!(
        decisions.contains(
            "\t\t# A field set twice is never\n\t\t# reported: removing one could\n\t\t# change \
             which one the game\n\t\t# uses.\n"
        ),
        "{decisions}"
    );
}

/// `--check` reports drift with line counts, exits non-zero and leaves files
/// untouched; after `--format` it passes.
#[test]
fn test_check() {
    let (_tmp, to) = copy_test_mod();
    let before = dasher::hash_directory(to.clone()).unwrap();
    let result = run(&to, &["--check"]);
    assert!(!result.status.success(), "{result}");
    assert!(
        result.stdout.contains("20 files would be reformatted:"),
        "{result}"
    );
    assert!(result.stdout.contains("events/germany.txt"), "{result}");
    assert!(
        result
            .stderr
            .contains("formatting check failed: one or more files would be reformatted"),
        "{result}"
    );
    let after = dasher::hash_directory(to.clone()).unwrap();
    assert_eq!(before, after, "--check modified files");

    let result = run(&to, &["--format"]);
    assert!(result.status.success(), "{result}");
    let result = run(&to, &["--check"]);
    assert!(result.status.success(), "{result}");
    assert!(
        result.stdout.contains("Formatting check passed"),
        "{result}"
    );
}

/// `--fix` removes redundant fields (and only those) and reports what it removed.
#[test]
fn test_fix() {
    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--fix"]);
    assert!(result.status.success(), "{result}");
    let stdout = &result.stdout;

    let decisions = read(&to, "common/decisions/malta.txt");
    assert!(!decisions.contains("fire_only_once = no"), "{decisions}");
    assert!(!decisions.contains("visible = { }"), "{decisions}");
    assert!(decisions.contains("fire_only_once = yes"), "{decisions}");
    assert!(decisions.contains("# Fortify the island."), "{decisions}");

    assert!(stdout.contains("Fixed 8 files:"), "{stdout}");
    assert!(
        stdout.contains(
            "common/decisions/malta.txt                         +0   -2  (2 redundant fields \
             removed)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("8 files changed, 1 insertion(+), 60 deletions(-)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Changes: 56 redundant fields removed"),
        "{stdout}"
    );

    // A second run has nothing left to fix.
    let result = run(&to, &["--fix"]);
    assert!(result.status.success(), "{result}");
    assert!(result.stdout.contains("Nothing to fix."), "{result}");
}

/// Every redundant-field rule is found and fixed; lookalikes that are not
/// redundant, and redundant fields whose removal would delete a comment,
/// stay.
#[test]
fn test_fix_every_rule() {
    // Redundant fields on lines of their own, by file: each rule at least once.
    const REDUNDANT: &[(&str, &[&str])] = &[
        (
            "common/characters/hearty_characters.txt",
            &[
                "\t\tcan_be_captured = yes\n",
                "\t\tavailable = { }\n",
                "\t\t\tallowed = { }\n",
                "\t\t\tvisible = { always = yes }\n",
                "\t\t\t\tvisible = { }\n",
            ],
        ),
        (
            "common/decisions/categories/hearty_categories.txt",
            &["\tvisible_when_empty = no\n", "\tavailable = { }\n"],
        ),
        (
            "common/decisions/hearty_decisions.txt",
            &[
                "\t\tselectable_mission = no\n",
                "\t\tis_good = No\n",
                "\t\tallowed = { always = yes }\n",
                "\t\tcancel_if_not_visible = no\n",
                "\t\tvisible = {\n\t\t\talways = yes\n\t\t}\n",
            ],
        ),
        ("common/decisions/malta.txt", &["\t\tfire_only_once = no\n"]),
        (
            "common/ideas/hearty_ideas.txt",
            &[
                "\t\t\tpicture = HRT_idea\n",
                "\t\t\tallowed_civil_war = { always = no }\n",
                "\t\t\tcancel = { always = no }\n",
            ],
        ),
        (
            "common/national_focus/hearty_tree.txt",
            &[
                "\tcontinuous_focus_position = { x = 50 y = 1000 }\n",
                "\tdefault = no\n",
                "\t\tavailable = { }\n",
                "\t\tcancel_if_invalid = yes\n",
                "\t\tcontinue_if_invalid = no\n",
                "\t\tavailable_if_capitulated = no\n",
                "\t\tallow_branch = { }\n",
                "\t\toffset = { x = 0 y = 0.0 }\n",
                "\t\tmutually_exclusive = { }\n",
                "\t\tai_will_do = { factor = 1 }\n",
                "\t\tallow_branch = { always = yes }\n",
                "\t\tai_will_do = {\n\t\t\tbase = 1\n\t\t}\n",
                "\t\tai_will_do = { }\n",
            ],
        ),
        (
            "common/national_focus/shared.txt",
            &["\toffset = { x = 0 y = 0 }\n"],
        ),
        (
            "events/chains.txt",
            &[
                "\t\tai_chance = { factor = 1 }\n",
                "\t\ttrigger = { always = yes }\n",
                "\tmajor = no\n",
                "\thidden = no\n",
                "\tis_triggered_only = no\n",
                "\t\tai_chance = { base = 1 }\n",
                "\tfire_only_once = no\n",
                "\ttrigger = { }\n",
                "\t\tai_chance = { }\n",
            ],
        ),
    ];
    // Text that `--fix` must keep: not redundant, or not removable.
    const KEPT: &[(&str, &[&str])] = &[
        (
            "common/characters/commented.txt",
            &[
                "can_be_captured = yes # Written out on purpose.\n",
                "visible = {\n\t\t\t\t# Always visible.\n\t\t\t}\n",
            ],
        ),
        (
            "common/decisions/hearty_decisions.txt",
            &[
                "\t\tfire_only_once = no\n\t\tfire_only_once = no\n",
                "\t\tvisible = { always = no }\n",
            ],
        ),
        (
            "common/decisions/categories/hearty_categories.txt",
            &["\tvisible = {\n\t\thas_war = yes\n\t}\n"],
        ),
        (
            "common/ideas/hearty_ideas.txt",
            &[
                "\t\t\tpicture = HRT_named_idea\n",
                "\t\t\tpicture = GFX_idea_generic\n",
                "\t\t\tcancel = { always = yes }\n",
            ],
        ),
        (
            "common/national_focus/hearty_tree.txt",
            &["\t\tcompletion_reward = { }\n"],
        ),
        ("events/chains.txt", &["\tis_triggered_only = yes\n"]),
        ("events/unclosed.txt", &["\tfire_only_once = no\n"]),
        ("events/latin1.txt", &["\tfire_only_once = no\n"]),
    ];
    let each = |table: &'static [(&'static str, &'static [&'static str])]| {
        table
            .iter()
            .flat_map(|&(file, fields)| fields.iter().map(move |&field| (file, field)))
    };
    let original = Path::new("tests/test_mod");
    let text = |root: &Path, file: &str| {
        String::from_utf8_lossy(&read_bytes(root, file)).replace("\r\n", "\n")
    };
    for (file, field) in each(REDUNDANT).chain(each(KEPT)) {
        assert!(
            text(original, file).contains(field),
            "{file} lacks {field:?}"
        );
    }

    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--fix"]);
    assert!(result.status.success(), "{result}");
    for (file, field) in each(REDUNDANT) {
        assert!(
            !text(&to, file).contains(field),
            "{file} still has {field:?}"
        );
    }
    for (file, field) in each(KEPT) {
        assert!(text(&to, file).contains(field), "{file} lost {field:?}");
    }
    // Fields sharing a line with others go with their whitespace.
    assert!(
        text(&to, "common/decisions/categories/hearty_categories.txt")
            .contains("HRT_quick_category = { icon = generic }\n"),
    );
    // A file whose only findings keep their comments is not rewritten.
    assert_eq!(
        read_bytes(&to, "common/characters/commented.txt"),
        read_bytes(original, "common/characters/commented.txt")
    );
    for file in UNCHANGED_FILES {
        assert_eq!(
            read_bytes(&to, file),
            read_bytes(original, file),
            "{file} changed"
        );
    }
    assert!(
        result.stdout.contains(
            "2 redundant field(s) left in place because removing them would delete a comment; \
             run --lint to see them."
        ),
        "{result}"
    );

    // Those two are all a second run finds.
    let result = run(&to, &["--fix"]);
    assert!(result.status.success(), "{result}");
    assert!(
        result.stdout.starts_with(
            "Nothing to fix.\n2 redundant field(s) left in place because removing them would \
             delete a comment"
        ),
        "{result}"
    );
}

/// Fixing then formatting reaches a fixed point: a further `--fix --format`
/// changes nothing.
#[test]
fn test_fix_then_format_idempotent() {
    let (_tmp, to) = copy_test_mod();
    let result = run(&to, &["--fix", "--format"]);
    assert!(result.status.success(), "{result}");
    let first = dasher::hash_directory(to.clone()).unwrap();
    let result = run(&to, &["--fix", "--format"]);
    assert!(result.status.success(), "{result}");
    let second = dasher::hash_directory(to.clone()).unwrap();
    assert_eq!(
        BASE64_STANDARD.encode(&first),
        BASE64_STANDARD.encode(&second),
        "a second --fix --format changed the files"
    );
}

/// A file that cannot be written is reported and not listed as formatted,
/// and fails the run once the other files are formatted. What follows sees
/// the file as it is on disk: `--check` finds it unformatted.
#[test]
fn test_fmt_unwritable_file() {
    let (_tmp, to) = copy_test_mod();
    let file = to.join("common/decisions/malta.txt");
    let before = std::fs::read(&file).unwrap();
    set_readonly(&file, true);
    // A privileged user (root) can write a read-only file: nothing to test.
    if std::fs::OpenOptions::new().append(true).open(&file).is_ok() {
        set_readonly(&file, false);
        return;
    }
    let result = run(&to, &["--fix", "--format", "--check"]);
    set_readonly(&file, false);
    assert_eq!(result.status.code(), Some(1), "{result}");
    assert_eq!(
        result.stderr.matches("failed to write ").count(),
        2,
        "{result}"
    );
    assert!(result.stderr.contains("malta.txt: "), "{result}");
    assert!(
        result
            .stderr
            .contains("Error: \"failed to write 1 file(s)\""),
        "{result}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // Neither fixed nor formatted, but counted after the files that were;
    // the check sees it unchanged.
    assert!(result.stdout.contains("Fixed 7 files:"), "{result}");
    assert!(result.stdout.contains("Formatted 19 files:"), "{result}");
    let (written, checked) = result
        .stdout
        .split_once("1 file would be reformatted:")
        .unwrap();
    assert!(!written.contains("malta.txt"), "{result}");
    assert!(checked.contains("malta.txt"), "{result}");
    assert!(
        result
            .stdout
            .contains("\nFixing: 1 file could not be written.\n2 redundant field(s) left"),
        "{result}"
    );
    assert!(
        result
            .stdout
            .contains("\nFormatting: 1 file could not be written.\n1 file would be reformatted:"),
        "{result}"
    );
    assert!(
        result.stdout.contains(
            "1 file would be reformatted:\n  common/decisions/malta.txt  +6  -12  (3 blocks joined \
             onto one line, 2 blocks with reordered fields, 1 spacing fix)\n"
        ),
        "{result}"
    );
    let germany = read(&to, "events/germany.txt");
    assert!(
        germany.find("id = germany.1\n") < germany.find("id = germany.2\n"),
        "{germany}"
    );
}

/// When no file an action changed could be written, its summary says so,
/// not that there was nothing to do.
#[test]
fn test_fmt_only_file_unwritable() {
    let (_tmp, to) = descriptor_only_mod("name = \"unwritable\"\n");
    let file = to.join("common/decisions/d.txt");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    // A redundant field to fix, and fields to reorder.
    let text = "cat = {\n\tdec = {\n\t\tfire_only_once = no\n\t\tcomplete_effect = { add_political_power = 1 }\n\t\ticon = x\n\t}\n}\n";
    std::fs::write(&file, text).unwrap();
    set_readonly(&file, true);
    // A privileged user (root) can write a read-only file: nothing to test.
    if std::fs::OpenOptions::new().append(true).open(&file).is_ok() {
        set_readonly(&file, false);
        return;
    }
    let fixed = run(&to, &["--fix"]);
    let formatted = run(&to, &["--format"]);
    let both = run(&to, &["--fix", "--format"]);
    set_readonly(&file, false);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    for (result, stdout) in [
        (&fixed, "Fixing: 1 file could not be written.\n"),
        (&formatted, "Formatting: 1 file could not be written.\n"),
        (
            &both,
            "Fixing: 1 file could not be written.\nFormatting: 1 file could not be written.\n",
        ),
    ] {
        assert_eq!(result.status.code(), Some(1), "{result}");
        assert!(result.stdout.starts_with(stdout), "{result}");
        assert!(result.stdout.contains("\nFinished in "), "{result}");
        assert!(result.stderr.contains("d.txt: "), "{result}");
        assert!(
            result
                .stderr
                .contains("Error: \"failed to write 1 file(s)\""),
            "{result}"
        );
    }
}

/// Linting is the default action. It reports keys missing from the English
/// localisation (the default language), problems in `descriptor.mod` and
/// redundant fields, and exits non-zero. Without `HEARTY_CACHE_DIR` the
/// version cache is read from `.hearty-cache` in the working directory.
#[test]
fn test_lint_default() {
    let (_tmp, to) = copy_test_mod();
    let cwd = tempfile::TempDir::new().unwrap();
    let cache = cwd.path().join(".hearty-cache");
    write_cache(&cache, now_secs(), &app_info(CACHED_BRANCHES));
    assert_fresh_cache(&cache);
    let empty = tempfile::TempDir::new().unwrap();
    let result = output(
        hearty(&to, &[])
            .current_dir(cwd.path())
            .env_remove("HEARTY_CACHE_DIR")
            .env("PATH", empty.path()),
    );
    let Run { stderr, stdout, .. } = &result;

    assert!(!result.status.success(), "{result}");
    assert!(stderr.contains("lint found 65 problem(s)"), "{result}");
    assert!(!stdout.contains("Format"), "{result}");
    assert!(
        stdout.contains("\nFound 7/76 missing localisations.\n"),
        "{result}"
    );
    assert!(
        stdout.contains("\nFound 58 redundant fields (56 fixable with --fix).\n"),
        "{result}"
    );

    // descriptor.mod: the version check against the cache's newest branch,
    // and duplicate keys (`replace_path` may repeat).
    assert!(
        stderr.contains(
            "descriptor.mod supported_version \"1.16.*\" does not match latest HOI4 1.17.3."
        ),
        "{result}"
    );
    assert!(
        stderr.contains("duplicate key \"name\" in descriptor.mod."),
        "{result}"
    );
    assert!(!stderr.contains("\"replace_path\""), "{result}");

    // Missing keys from events, focuses and (not here) technologies; fewer
    // than 10, so all are shown.
    for key in [
        "HRT_odd_a",
        "HRT_odd_b",
        "cycle.1.a",
        "cycle.2.a",
        "spacing.4.a",
        "unclosed.1.a",
        "unclosed.1.t",
    ] {
        assert!(
            stderr.contains(&format!("\"{key}\" not localised in: english.")),
            "{key}\n{result}"
        );
    }
    assert!(stderr.contains("missing localisation"), "{result}");
    // Only `*_l_<language>.yml` files under `localisation/` are
    // localisation: `interface/hearty_l_english.gfx` and
    // `localisation/english/hearty_notes_l_english.txt` look like entries
    // for HRT_odd_a and HRT_odd_b, which are still missing (above).
    // Only `*.txt` files are script files: `national_focus/readme.md`
    // holds a focus, whose id is not reported.
    assert!(!stderr.contains("HRT_readme"), "{result}");
    // A missing key points where it is defined, not where it first
    // appears (HRT_odd_a first appears as HRT_odd_b's prerequisite). Its
    // file is named relative to the mod's folder, as a redundant field's is.
    let sep = std::path::MAIN_SEPARATOR;
    assert!(
        stderr.contains(&format!(
            "[common{sep}national_focus{sep}odd_shapes.txt:21:8]"
        )),
        "{result}"
    );
    // A key used in several files is reported in the first of them:
    // cycle.1.a is an option name in `events/cycle.txt` and a title in
    // `events/shared_line.txt`.
    assert!(
        stderr.contains(&format!("[events{sep}cycle.txt:23:10]")),
        "{result}"
    );
    assert!(!stderr.contains("shared_line.txt"), "{result}");

    // Redundant fields: the first 10 are shown, those that --fix leaves
    // (both in the alphabetically first file) with their own help.
    assert_eq!(stderr.matches("has no effect").count(), 10, "{result}");
    assert!(
        stderr.contains(
            "`can_be_captured = yes` has no effect: `can_be_captured` defaults to `yes`."
        ),
        "{result}"
    );
    assert!(
        stderr.contains("`visible = { # Always visible. }` has no effect"),
        "{result}"
    );
    assert_eq!(
        stderr
            .matches("help: remove it (not auto-fixed because a comment would be lost)")
            .count(),
        2,
        "{result}"
    );
    assert!(
        stderr.contains("help: remove it, or run `hearty --fix`"),
        "{result}"
    );
    assert!(stderr.contains("\u{22ee} (48 more not shown)"), "{result}");
    assert_eq!(stderr.matches("more not shown").count(), 1, "{result}");
}

/// A file or directory name that is not UTF-8 (an unpaired surrogate on
/// Windows, a lone byte 0xff elsewhere), ending in `suffix`.
fn non_utf8_name(suffix: &str) -> std::ffi::OsString {
    #[cfg(windows)]
    let mut name = {
        use std::os::windows::ffi::OsStringExt as _;
        std::ffi::OsString::from_wide(&[0xd800, u16::from(b'x')])
    };
    #[cfg(unix)]
    let mut name = {
        use std::os::unix::ffi::OsStringExt as _;
        std::ffi::OsString::from_vec(vec![0xff, b'x'])
    };
    name.push(suffix);
    name
}

/// Linting works from inside the mod with the default path `.` (as the
/// README's GitHub Action runs it) and with `..` from a subdirectory, and
/// skips files and directories whose names are not UTF-8 or that cannot be
/// read, rather than failing.
#[test]
fn test_lint_default_path_and_odd_names() {
    let (_tmp, to) = copy_test_mod();
    let cache = seeded_cache();
    assert_fresh_cache(cache.path());
    // A file system may refuse names that are not UTF-8 (macOS does); then
    // there is nothing to skip.
    std::fs::create_dir_all(to.join("gfx")).unwrap();
    let odd_files = [
        to.join("localisation/english")
            .join(non_utf8_name("_l_english.yml")),
        to.join("common/national_focus").join(non_utf8_name(".txt")),
        to.join("gfx").join(non_utf8_name("_l_english.dds")),
    ];
    for file in &odd_files {
        let _ignored: std::io::Result<()> = std::fs::write(
            file,
            "l_english:
 HRT_odd_a:0 \"A\"
",
        );
    }
    let odd_dir = to.join("localisation").join(non_utf8_name(""));
    if std::fs::create_dir(&odd_dir).is_ok() {
        std::fs::write(
            odd_dir.join("odd_l_english.yml"),
            "l_english:
",
        )
        .unwrap();
    }
    // A directory nobody may read (unless root, who reads it anyway).
    #[cfg(unix)]
    let locked = {
        use std::os::unix::fs::PermissionsExt as _;
        let locked = to.join("localisation/locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(
            locked.join("locked_l_english.yml"),
            "l_english:
",
        )
        .unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        locked
    };

    for (dir, args) in [
        (to.clone(), &["--lint"][..]),
        (to.clone(), &[][..]),
        (to.join("events"), &["..", "--lint"][..]),
    ] {
        let result = output(
            hearty_with(args)
                .current_dir(&dir)
                .env("HEARTY_CACHE_DIR", cache.path())
                .env("PATH", cache.path()),
        );
        assert_eq!(
            result.status.code(),
            Some(1),
            "{args:?}
{result}"
        );
        assert!(
            result.stdout.contains(
                "
Found 7/76 missing localisations.
"
            ),
            "{args:?}
{result}"
        );
        assert!(
            result.stderr.contains("lint found 65 problem(s)"),
            "{args:?}
{result}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// `--all` checks every language: past 10 missing keys the rest are counted
/// but not shown. Only direct children of `events/` are event files, and
/// `desc` blocks carry no key of their own. The report does not depend on how
/// many threads share the work.
#[test]
fn test_lint_all_languages() {
    let (_tmp, to) = copy_test_mod();
    let cache = seeded_cache();
    let result = lint(&to, &["--lint", "--all"], cache.path());
    assert!(!result.status.success(), "{result}");
    assert!(
        result
            .stdout
            .contains("\nFound 76/76 missing localisations.\n"),
        "{result}"
    );
    assert!(
        result.stderr.contains(
            "\"HRT_odd_b\" not localised in: braz_por, simp_chinese, english, french, german, \
             japanese, korean, polish, russian, spanish."
        ),
        "{result}"
    );
    assert!(
        result
            .stderr
            .contains("\"HRT_odd_a\" not localised in: english."),
        "{result}"
    );
    assert!(
        result.stderr.contains("\u{22ee} (66 more not shown)"),
        "{result}"
    );
    assert!(
        result.stderr.contains("lint found 134 problem(s)"),
        "{result}"
    );
    for key in ["nested.1", "chains.3.d_war", "latin.1"] {
        assert!(!result.stderr.contains(key), "{key}\n{result}");
    }

    // The report does not depend on the order files are processed in.
    let single =
        output(lint_command(&to, &["--lint", "--all"], cache.path()).env("RAYON_NUM_THREADS", "1"));
    assert_eq!(
        (&single.stdout, &single.stderr),
        (&result.stdout, &result.stderr)
    );
}

/// `--lang` may be repeated; languages are listed in a fixed order.
#[test]
fn test_lint_languages() {
    let (_tmp, to) = copy_test_mod();
    let cache = seeded_cache();
    let result = lint(&to, &["--lang", "german", "--lang", "french"], cache.path());
    assert!(!result.status.success(), "{result}");
    assert!(
        result
            .stdout
            .contains("\nFound 75/76 missing localisations.\n"),
        "{result}"
    );
    assert!(
        result
            .stderr
            .contains("\"HRT_odd_b\" not localised in: french, german."),
        "{result}"
    );
    assert!(
        result
            .stderr
            .contains("\"HRT_first\" not localised in: french."),
        "{result}"
    );
    assert!(!result.stderr.contains("\"HRT_odd_a\""), "{result}");

    // `--all` and `--lang` contradict each other.
    let result = lint(&to, &["--all", "--lang", "english"], cache.path());
    assert_eq!(result.status.code(), Some(2), "{result}");
    assert!(result.stderr.contains("cannot be used with"), "{result}");
}

/// A mod with nothing to report passes, whatever `descriptor.mod`'s
/// `supported_version` or the cached versions hold, as long as they do not
/// say the mod is out of date.
#[test]
fn test_lint_descriptor_versions() {
    let cache = seeded_cache();
    for descriptor in [
        "name=\"Up to date\"\nsupported_version=\"1.17.*\"\n",
        "supported_version=\"not a version\"\nname=\"A\"\nname=\"B\"\n",
        "supported_version={ 1.16 }\n",
        "name=\"No version\"\n",
    ] {
        let (_tmp, to) = descriptor_only_mod(descriptor);
        let result = lint(&to, &["--lint"], cache.path());
        assert!(result.status.success(), "{descriptor}\n{result}");
        assert!(
            result
                .stdout
                .contains("\nFound 0/0 missing localisations.\n"),
            "{result}"
        );
        assert!(
            result
                .stdout
                .contains("\nFound 0 redundant fields (0 fixable with --fix).\n"),
            "{result}"
        );
        assert!(result.stdout.contains("Finished in "), "{result}");
        assert!(!result.stderr.contains("does not match"), "{result}");
    }

    // The version check needs a version among the cached branches.
    let (_tmp, to) = descriptor_only_mod("supported_version=\"1.16.*\"\n");
    for data in [
        app_info(&["public", "1.9.x"]),
        json!({ "394360": { "depots": {} } }),
        json!({}),
    ] {
        let cache = tempfile::TempDir::new().unwrap();
        write_cache(cache.path(), now_secs(), &data);
        let result = lint(&to, &["--lint"], cache.path());
        assert!(result.status.success(), "{data}\n{result}");
        assert!(!result.stderr.contains("does not match"), "{result}");
    }
}

/// Linting a directory without a `descriptor.mod` fails.
#[test]
fn test_lint_not_a_mod() {
    let (_tmp, to) = copy_test_mod();
    std::fs::remove_file(to.join("descriptor.mod")).unwrap();
    let cache = seeded_cache();
    let result = lint(&to, &["--lint"], cache.path());
    assert_eq!(result.status.code(), Some(1), "{result}");
    assert!(
        result
            .stderr
            .contains("is not a mod directory (no descriptor.mod found)"),
        "{result}"
    );
    assert!(!result.stdout.contains("Found"), "{result}");
}

/// Every action at once runs in the order fix, format, check, lint, and the
/// lint then sees the fixed and formatted files.
#[test]
fn test_all_actions() {
    let (_tmp, to) = copy_test_mod();
    let cache = seeded_cache();
    let result = lint(
        &to,
        &["--lint", "--check", "--format", "--fix"],
        cache.path(),
    );
    assert!(!result.status.success(), "{result}");
    let position = |text: &str| {
        result
            .stdout
            .find(text)
            .unwrap_or_else(|| panic!("{text}\n{result}"))
    };
    let order = [
        position("Fixed 8 files:"),
        position("Formatted 20 files:"),
        position("Formatting check passed: no files would change."),
        position("Found 7/76 missing localisations."),
        position("Found 2 redundant fields (0 fixable with --fix)."),
    ];
    assert!(order.is_sorted(), "{result}");
    assert!(
        result.stderr.contains("lint found 9 problem(s)"),
        "{result}"
    );
}

/// With no cache, hearty runs steamcmd from `PATH`, reads HOI4's versions
/// from its output and caches them (creating the cache directory); the next
/// run reads the cache instead.
#[test]
fn test_lint_steamcmd_on_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("new").join("cache");
    let bin = tmp.path().join("bin");
    install_fake_steamcmd(&bin, STEAMCMD_ON_PATH);
    let (_mod_tmp, to) = descriptor_only_mod("supported_version=\"1.17.*\"\n");

    let result = lint_with_fake_steamcmd(&to, &cache, &bin, "app_info");
    assert!(result.status.success(), "{result}");
    assert!(
        result.stderr.contains(
            "descriptor.mod supported_version \"1.17.*\" does not match latest HOI4 1.18.0."
        ),
        "{result}"
    );

    // The app info is cached as JSON, duplicate keys as arrays.
    assert_fresh_cache(&cache);
    let data = &read_cache(&cache)["data"]["394360"];
    assert_eq!(data["common"]["name"], "Hearts of Iron IV");
    assert_eq!(
        data["depots"]["branches"]["1.18.0.0"]["buildid"], "2",
        "{data}"
    );
    assert_eq!(
        data["depots"]["394361"],
        json!([{ "oslist": "windows" }, { "oslist": "linux" }])
    );

    // Now the cache answers: no steamcmd is needed.
    let result = lint(&to, &["--lint"], &cache);
    assert!(result.stderr.contains("latest HOI4 1.18.0."), "{result}");
}

/// A corrupt cache is ignored: hearty runs the steamcmd it keeps in the cache
/// directory and replaces the cache.
#[test]
fn test_lint_corrupt_cache() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    install_fake_steamcmd(&cache, STEAMCMD_IN_CACHE);
    std::fs::write(cache.join(CACHE_FILE), "{ not json").unwrap();
    let (_mod_tmp, to) = descriptor_only_mod("supported_version=\"1.17.*\"\n");

    let result = lint_with_fake_steamcmd(&to, &cache, &bin, "app_info");
    assert!(result.status.success(), "{result}");
    assert!(
        result.stderr.contains("does not match latest HOI4 1.18.0."),
        "{result}"
    );
    assert_fresh_cache(&cache);
}

/// A stale cache is not used, so hearty runs steamcmd (which
/// [`lint_with_fake_steamcmd`] checks), and output steamcmd did not get the
/// app info into is ignored: linting goes on without the version check, and
/// the cache is left as it was.
#[test]
fn test_lint_stale_cache_and_steamcmd_failure() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let bin = tmp.path().join("bin");
    install_fake_steamcmd(&bin, STEAMCMD_ON_PATH);
    let two_days_ago = now_secs() - 2 * 86_400;
    write_cache(&cache, two_days_ago, &app_info(CACHED_BRANCHES));
    let stale = std::fs::read(cache.join(CACHE_FILE)).unwrap();
    let (_mod_tmp, to) = descriptor_only_mod("supported_version=\"1.16.*\"\n");

    let result = lint_with_fake_steamcmd(&to, &cache, &bin, "garbage");
    assert!(result.status.success(), "{result}");
    assert!(!result.stderr.contains("does not match"), "{result}");
    assert!(
        result.stdout.contains("Found 0/0 missing localisations."),
        "{result}"
    );
    assert_eq!(std::fs::read(cache.join(CACHE_FILE)).unwrap(), stale);
}

/// `--timings` prints where the time went to stderr and `--flamegraph`
/// writes it as an SVG; neither changes what formatting writes.
#[test]
fn test_timings_and_flamegraph() {
    let (tmp, to) = copy_test_mod();
    read_all(&to);
    let svg = tmp.path().join("flamegraph.svg");
    let result = output(&mut hearty(
        &to,
        &[
            "--format",
            "--timings",
            "--flamegraph",
            svg.to_str().unwrap(),
        ],
    ));
    assert!(result.status.success(), "{result}");
    // The report goes to stderr, leaving stdout as it was.
    assert!(result.stdout.contains("Formatted "), "{result}");
    assert!(!result.stdout.contains("Timings:"), "{result}");
    assert!(result.stderr.contains("Timings: "), "{result}");
    assert!(result.stderr.contains(" threads (rayon pool: "), "{result}");
    assert!(result.stderr.contains(", parallelism "), "{result}");
    let table = result
        .stderr
        .lines()
        .skip_while(|line| !line.starts_with("span "))
        .collect::<Vec<_>>();
    assert!(
        table
            .first()
            .is_some_and(|header| header.contains("active")),
        "{result}"
    );
    // The pass over the script files is a root, its files are under it,
    // the actions under them, and the rules under those.
    for row in ["scripts ", "  file ", "    format ", "      inline "] {
        assert!(
            table.iter().any(|line| line.starts_with(row)),
            "no {row:?} row in {result}"
        );
    }
    assert!(
        result.stderr.contains("Slowest files:\n  scripts > file\n"),
        "{result}"
    );
    assert!(result.stderr.contains("Wrote flamegraph to "), "{result}");

    let svg = std::fs::read_to_string(&svg).unwrap();
    assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"), "{svg}");
    for frame in [
        "format",
        "file",
        "sort focuses",
        "field order",
        "inline",
        "reflow comments",
        "parse",
    ] {
        assert!(
            svg.contains(&format!(">{frame} (")),
            "no {frame} frame in {svg}"
        );
    }

    // The files are formatted exactly as without the flags.
    let hash = dasher::hash_directory(to).unwrap();
    assert_eq!(BASE64_STANDARD.encode(&hash), HASH);
}

/// Every action and the lint's steps show up in the flamegraph. The lint
/// runs offline: a fresh version cache answers without steamcmd.
#[test]
fn test_flamegraph_of_every_action() {
    let (tmp, to) = copy_test_mod();
    read_all(&to);
    let cache = seeded_cache();
    let svg = tmp.path().join("flamegraph.svg");
    let result = output(&mut lint_command(
        &to,
        &[
            "--fix",
            "--format",
            "--check",
            "--lint",
            "--timings",
            "--flamegraph",
            svg.to_str().unwrap(),
        ],
        cache.path(),
    ));
    // Lint findings exit with 1; the timings are still reported.
    assert_eq!(result.status.code(), Some(1), "{result}");
    let table = result
        .stderr
        .lines()
        .skip_while(|line| !line.starts_with("span "))
        .collect::<Vec<_>>();
    // The script pass, the localisation (loaded alongside it), the version
    // check (on a thread of its own) and printing the report are roots;
    // each action is a step of every file.
    for root in [
        "scripts",
        "localisation load",
        "version check",
        "report",
        "find files",
    ] {
        assert!(
            table
                .iter()
                .any(|line| line.starts_with(&format!("{root} "))),
            "no {root} row in {result}"
        );
    }

    let svg = std::fs::read_to_string(&svg).unwrap();
    for frame in [
        "fix",
        "format",
        "check",
        "lint",
        "read",
        "write",
        "line stats",
        "redundant find",
        "redundant fix",
        "sort events",
        "keys",
        "version check",
        "cache read",
        "check descriptor",
        "localisation load",
        "lookup",
        "walk",
        "missing keys",
        "redundant fields",
    ] {
        assert!(
            svg.contains(&format!(">{frame} (")),
            "no {frame} frame in {svg}"
        );
    }
    // The cache was fresh, so steamcmd never ran.
    assert!(!svg.contains(">steamcmd ("), "{svg}");
}

/// A flamegraph that cannot be written fails the run, after the actions ran.
#[test]
fn test_flamegraph_write_error() {
    let (tmp, to) = copy_test_mod();
    let svg = tmp.path().join("missing-dir").join("flamegraph.svg");
    let svg = svg.to_str().unwrap();
    let result = output(&mut hearty(&to, &["--check", "--flamegraph", svg]));
    assert!(!result.status.success(), "{result}");
    assert!(result.stdout.contains(" would be reformatted:"), "{result}");
    // `--check`'s drift takes precedence over the flamegraph error.
    assert!(
        result.stderr.contains("formatting check failed"),
        "{result}"
    );

    let formatted = run(&to, &["--format"]);
    assert!(formatted.status.success(), "{formatted}");
    let result = output(&mut hearty(&to, &["--check", "--flamegraph", svg]));
    assert!(!result.status.success(), "{result}");
    assert!(
        result.stderr.contains("failed to write the flamegraph"),
        "{result}"
    );
}

/// The base game's localisation counts as the mod's: a key only the game
/// defines isn't missing, unless the mod's `replace_path` replaces the
/// folder defining it (but not the folders under that one).
#[test]
fn test_lint_base_game_localisation() {
    let (_tmp, to) = copy_test_mod();
    let cache = seeded_cache();
    let game = Path::new("tests").join("fake_game");
    let lint_with_game = |extra: &[&str], env: Option<&Path>| {
        let mut flags = vec!["--lint"];
        flags.extend(extra);
        let mut command = lint_command(&to, &flags, cache.path());
        if let Some(env) = env {
            command.env("HEARTY_GAME_DIR", env);
        }
        output(&mut command)
    };

    // Without the game, the keys it defines are missing.
    let result = lint_with_game(&[], None);
    assert!(
        result
            .stdout
            .contains("The base game's localisation was not read"),
        "{result}"
    );
    assert!(
        result.stdout.contains("Found 7/76 missing localisations."),
        "{result}"
    );
    assert!(
        result.stderr.contains("\"cycle.1.a\" not localised"),
        "{result}"
    );

    // With it (from the variable), they aren't. Only English is checked, so
    // only its two files are read.
    let result = lint_with_game(&[], Some(&game));
    assert!(
        result
            .stdout
            .contains("Read 2 localisation files of the base game from "),
        "{result}"
    );
    assert!(
        result.stdout.contains("Found 4/76 missing localisations."),
        "{result}"
    );
    assert!(!result.stderr.contains("\"cycle.1.a\""), "{result}");
    assert!(!result.stderr.contains("\"spacing.4.a\""), "{result}");

    // --game-dir does the same, and German reads the German file too.
    let result = lint_with_game(
        &["--game-dir", game.to_str().unwrap(), "--lang", "german"],
        None,
    );
    assert!(
        result
            .stdout
            .contains("Read 1 localisation files of the base game from "),
        "{result}"
    );
    assert!(
        !result
            .stderr
            .contains("\"unclosed.1.t\" not localised in: german"),
        "{result}"
    );

    // A mod replacing `localisation/english` hides the game's files there,
    // but not those in its subfolders.
    let descriptor = to.join("descriptor.mod");
    let mut text = std::fs::read_to_string(&descriptor).unwrap();
    text.push_str("\nreplace_path=\"localisation/english\"\n");
    std::fs::write(&descriptor, text).unwrap();
    let result = lint_with_game(&[], Some(&game));
    assert!(
        result
            .stdout
            .contains("Read 1 localisation files of the base game from "),
        "{result}"
    );
    assert!(
        result.stdout.contains("Found 6/76 missing localisations."),
        "{result}"
    );
    assert!(
        result.stderr.contains("\"cycle.1.a\" not localised"),
        "{result}"
    );
    assert!(!result.stderr.contains("\"spacing.4.a\""), "{result}");
}
