//! Tests the workshop benchmark's support code
//! (`benches/support/workshop.rs` and `benches/support/results.rs`): reading
//! hearty's output, finding Steam libraries, and syncing a mod into its git
//! working copy. They need git but not Steam or the network.

#[path = "../benches/support/git.rs"]
mod git;
#[path = "../benches/support/results.rs"]
mod results;
#[path = "../benches/support/workshop.rs"]
#[expect(
    dead_code,
    reason = "finding mods in Steam depends on the machine; only the parts that don't are tested here"
)]
mod workshop;

use results::{Problems, TimingRow, Timings, parse_duration};
use std::path::{Path, PathBuf};
use workshop::{Found, MODS, Synced, library_folders, parse_library_folders, sync, workshop_dir};

/// The summary lines of `hearty --check --lint` on Old World Blues (the
/// per-file lines are indented and ignored).
const CHECK_LINT_STDOUT: &str = "\
4,303 files would be reformatted:
  common/abilities/x.txt  +1  -2  (1 spacing fix)
4,303 files changed, 234,097 insertions(+), 415,025 deletions(-)
Changes: 94,619 blocks joined onto one line, 3,190 events moved, 25,897 blocks with reordered fields, 2,616 focuses moved, 134 blank-line fixes, 12,126 spacing fixes

Found 595/39,571 missing localisations.

Found 3,210 redundant fields (3,149 fixable with --fix).
Finished in 229.00ms.
";

/// The `--timings` report of that run.
const TIMINGS: &str = "\
Timings: 229.41ms wall, 4.32s active on 26 threads (rayon pool: 24), parallelism 18.85x
  active: time threads spent in a span and the spans under it, summed over threads
  self:   active time not spent in a span nested in it on the same thread
  share:  active time as a share of the total; wall: first to last moment of a top-level part

span                    active      self  calls  share      wall
scripts                  4.14s    0.00ns      1  95.7%  206.22ms
  file                   4.14s   36.20ms  6,076  95.7%
    check                3.54s   51.28ms  6,076  81.7%
      line stats         2.76s     2.76s  4,303  63.9%
      … 2 more         21.74ms                    0.5%
    read              512.46ms  512.46ms  6,076  11.8%
localisation load     115.94ms    0.00ns      1   2.6%  206.22ms
  file                115.94ms    3.44ms  1,053   2.6%
version check           8.28ms   80.10µs      1   0.1%    8.29ms

Slowest files:
  scripts > file
      132.95ms  common/national_focus/New California Republic (NCR) Focus.txt
";

#[test]
fn problems_are_read_from_the_summary() {
    let stderr = "  ! duplicate key \"name\" in descriptor.mod.\n  ! descriptor.mod supported_version \"1.12.*\" does not match latest HOI4 1.17.3.\n";
    let problems = Problems::parse(CHECK_LINT_STDOUT, stderr);
    assert_eq!(problems.files_to_reformat, 4_303);
    assert_eq!(problems.format_insertions, 234_097);
    assert_eq!(problems.format_deletions, 415_025);
    assert_eq!(problems.missing_localisations, 595);
    assert_eq!(problems.localisation_keys, 39_571);
    assert_eq!(problems.redundant_fields, 3_210);
    assert_eq!(problems.redundant_fixable, 3_149);
    assert_eq!(problems.descriptor_warnings, 2);
    assert_eq!(problems.format_changes.len(), 6);
    assert_eq!(
        problems.format_changes.first(),
        Some(&("blocks joined onto one line".to_owned(), 94_619))
    );
    assert_eq!(
        problems.format_changes.last(),
        Some(&("spacing fixes".to_owned(), 12_126))
    );
}

#[test]
fn problems_of_a_clean_mod_are_zero() {
    let stdout = "Formatting check passed: no files would change.\n\nFound 0/12 missing localisations.\n\nFound 0 redundant fields (0 fixable with --fix).\n";
    let problems = Problems::parse(stdout, "");
    assert_eq!(
        problems,
        Problems {
            localisation_keys: 12,
            ..Problems::default()
        }
    );
    // git leaves out a zero count: here there are no insertions.
    let problems = Problems::parse("1 file changed, 5 deletions(-)\n", "");
    assert_eq!(
        (problems.format_insertions, problems.format_deletions),
        (0, 5)
    );
}

#[test]
fn timings_are_read_as_a_tree() {
    let timings = Timings::parse(TIMINGS).unwrap();
    assert!((timings.wall - 0.229_41).abs() < 1e-9);
    assert!((timings.active - 4.32).abs() < 1e-9);
    assert_eq!(timings.threads, 26);
    assert!((timings.parallelism - 18.85).abs() < 1e-9);
    let paths: Vec<String> = timings
        .rows
        .iter()
        .map(|row| row.path.join(" > "))
        .collect();
    assert_eq!(
        paths,
        [
            "scripts",
            "scripts > file",
            "scripts > file > check",
            "scripts > file > check > line stats",
            "scripts > file > check > … 2 more",
            "scripts > file > read",
            "localisation load",
            "localisation load > file",
            "version check",
        ]
    );
    let row = |path: &str| {
        timings
            .rows
            .iter()
            .find(|row| row.path.join(" > ") == path)
            .unwrap()
    };
    let scripts: &TimingRow = row("scripts");
    assert!((scripts.active - 4.14).abs() < 1e-9);
    assert_eq!(scripts.calls, Some(1));
    assert!((scripts.wall.unwrap() - 0.206_22).abs() < 1e-9);
    let file = row("scripts > file");
    assert_eq!(file.calls, Some(6_076));
    assert_eq!(file.wall, None);
    let folded = row("scripts > file > check > … 2 more");
    assert!((folded.active - 0.021_74).abs() < 1e-9);
    assert_eq!((folded.self_time, folded.calls), (None, None));
    assert!((row("version check").self_time.unwrap() - 80.10e-6).abs() < 1e-12);
    assert_eq!(Timings::parse("no report"), None);
}

/// `scripts/plot_benchmarks.py` reads these keys.
#[test]
fn results_are_written_as_the_chart_script_reads_them() {
    let problems = Problems::parse(CHECK_LINT_STDOUT, "").to_json();
    for key in [
        "descriptor_warnings",
        "files_to_reformat",
        "format_deletions",
        "format_insertions",
        "localisation_keys",
        "missing_localisations",
        "redundant_fields",
        "redundant_fixable",
    ] {
        assert!(problems[key].is_u64(), "{key} in {problems}");
    }
    assert_eq!(
        problems["format_changes"][0]["kind"],
        "blocks joined onto one line"
    );
    assert_eq!(problems["format_changes"][0]["count"], 94_619);

    let timings = Timings::parse(TIMINGS).unwrap().to_json();
    assert_eq!(timings["threads"], 26);
    assert!(timings["wall_secs"].is_f64() && timings["active_secs"].is_f64());
    let rows = timings["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 9);
    assert_eq!(
        rows[3]["path"],
        serde_json::json!(["scripts", "file", "check", "line stats"])
    );
    assert!(rows[3]["self_secs"].is_f64());
    assert!(rows[4]["self_secs"].is_null());
    assert!(rows[0]["wall_secs"].is_f64());
}

#[test]
fn durations_in_every_unit() {
    let close = |text: &str, secs: f64| (parse_duration(text).unwrap() - secs).abs() < 1e-12;
    assert!(close("4.14s", 4.14));
    assert!(close("115.94ms", 0.115_94));
    assert!(close("21.20µs", 21.2e-6));
    assert!(close("0.00ns", 0.0));
    assert_eq!(parse_duration("fast"), None);
}

#[test]
fn library_folders_are_read_from_the_vdf() {
    let vdf = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"apps"
		{
			"394360"		"4815162342"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
    assert_eq!(
        parse_library_folders(vdf),
        [
            PathBuf::from(r"C:\Program Files (x86)\Steam"),
            PathBuf::from(r"D:\SteamLibrary"),
        ]
    );

    let root = tempfile::TempDir::new().unwrap();
    let steamapps = root.path().join("steamapps");
    std::fs::create_dir_all(&steamapps).unwrap();
    std::fs::write(steamapps.join("libraryfolders.vdf"), vdf).unwrap();
    let libraries = library_folders(root.path());
    assert_eq!(libraries.first().map(PathBuf::as_path), Some(root.path()));
    assert_eq!(libraries.len(), 3);
}

#[test]
fn mods_have_distinct_ids_and_slugs() {
    for (index, workshop_mod) in MODS.iter().enumerate() {
        assert!(workshop_mod.id.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(
            workshop_mod
                .slug
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert!(!workshop_mod.name.is_empty());
        assert_eq!(
            workshop_mod.env_var(),
            format!("HEARTY_BENCH_MOD_{}", workshop_mod.id)
        );
        for other in MODS.iter().skip(index + 1) {
            assert_ne!(workshop_mod.id, other.id);
            assert_ne!(workshop_mod.slug, other.slug);
        }
    }
    let dir = workshop_dir(Path::new("lib"), "123");
    assert_eq!(
        dir,
        Path::new("lib/steamapps/workshop/content/394360/123")
            .components()
            .collect::<PathBuf>()
    );
    let found = Found::Library(dir.clone());
    assert_eq!(found.path(), dir);
    assert!(found.describe().ends_with("(Steam Workshop)"));
}

/// Writes `files` (relative path, bytes) under `root`.
fn write_files(root: &Path, files: &[(&str, &[u8])]) {
    for (relative, bytes) in files {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

/// Every file under `root` but `.git`, relative, with its bytes.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn sync_copies_what_hearty_reads_byte_for_byte() {
    let source = tempfile::TempDir::new().unwrap();
    write_files(
        source.path(),
        &[
            ("descriptor.mod", b"name=\"Test\"\r\n"),
            ("common/ideas/crlf.txt", b"ideas = {\r\n}\r\n"),
            ("common/ideas/lf.txt", b"ideas = {\n}\n"),
            // A mod's own attributes must not change the bytes git stores.
            ("common/.gitattributes", b"* text=auto eol=lf\n"),
            ("common/.gitignore", b"*.txt\n"),
            ("events/e.txt", b"country_event = { id = e.1 }\n"),
            (
                "localisation/english/x_l_english.yml",
                b"\xEF\xBB\xBFl_english:\r\n x: \"X\"\r\n",
            ),
            ("gfx/interface/big.dds", b"not copied"),
            ("thumbnail.png", b"not copied"),
        ],
    );
    let work = tempfile::TempDir::new().unwrap();
    let dest = work.path().join("copy");

    let synced = sync(source.path(), &dest).unwrap();
    assert_eq!(
        synced,
        Synced {
            copied: 7,
            deleted: 0,
            created: true
        }
    );
    let mut expected = snapshot(source.path());
    expected.retain(|(path, _)| !path.starts_with("gfx") && path != Path::new("thumbnail.png"));
    assert_eq!(snapshot(&dest), expected);

    // A run changes a file; a reset (what the benchmark does) and a new sync
    // both restore it byte for byte.
    std::fs::write(dest.join("common/ideas/crlf.txt"), b"changed\n").unwrap();
    git::git(Some(&dest), &["reset", "--hard", "-q", "HEAD"]).unwrap();
    assert_eq!(snapshot(&dest), expected);
    std::fs::write(dest.join("common/ideas/crlf.txt"), b"changed\n").unwrap();
    let synced = sync(source.path(), &dest).unwrap();
    assert_eq!(
        (synced.copied, synced.deleted, synced.created),
        (0, 0, false)
    );
    assert_eq!(snapshot(&dest), expected);

    // The mod is updated: a file changes, one is removed and one is added.
    write_files(
        source.path(),
        &[
            ("events/e.txt", b"country_event = { id = e.2 }\n"),
            ("history/countries/X.txt", b"capital = 1\n"),
        ],
    );
    std::fs::remove_file(source.path().join("common/ideas/lf.txt")).unwrap();
    let synced = sync(source.path(), &dest).unwrap();
    assert_eq!(
        (synced.copied, synced.deleted, synced.created),
        (2, 1, false)
    );
    let mut expected = snapshot(source.path());
    expected.retain(|(path, _)| !path.starts_with("gfx") && path != Path::new("thumbnail.png"));
    assert_eq!(snapshot(&dest), expected);
    let status = git::git(Some(&dest), &["status", "--porcelain"]).unwrap();
    assert_eq!(status.trim(), "", "the sync left changes uncommitted");
}

#[test]
fn sync_refuses_a_repository_it_did_not_create() {
    let source = tempfile::TempDir::new().unwrap();
    write_files(source.path(), &[("descriptor.mod", b"name=\"Test\"\n")]);
    let dest = tempfile::TempDir::new().unwrap();
    git::git(Some(dest.path()), &["init", "-q"]).unwrap();
    write_files(dest.path(), &[("mine.txt", b"keep me")]);
    let err = sync(source.path(), dest.path()).unwrap_err().to_string();
    assert!(err.contains("refusing"), "{err}");
    assert_eq!(
        std::fs::read(dest.path().join("mine.txt")).unwrap(),
        b"keep me"
    );
}
