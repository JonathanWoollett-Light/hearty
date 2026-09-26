//! Tests the Millennium Dawn benchmark's checkout
//! (`benches/support/checkout.rs`) against a small local repository, so they
//! need git but not the network.

#[path = "../benches/support/checkout.rs"]
mod checkout;
#[path = "../benches/support/git.rs"]
mod git;

use checkout::{MARKER, Source, checkout, git};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SPARSE_DIRS: &[&str] = &["common", "localisation"];

/// A file under `localisation` whose path is long enough that checking it
/// out anywhere goes over Windows' 260 character limit, which Git for
/// Windows only allows with `core.longpaths`.
fn long_path() -> String {
    format!("localisation/{}/{}.yml", "d".repeat(100), "f".repeat(120))
}

/// A bare repository in `root` whose `main` has one commit, served over
/// `file://` so the clone is shallow and blobless as from GitHub.
fn origin(root: &Path) -> (PathBuf, String) {
    let origin = root.join("origin.git");
    git(None, &["init", "-q", "--bare", origin.to_str().unwrap()]).unwrap();
    // Lets the clone leave out the blobs and fetch them when checking out.
    git(Some(&origin), &["config", "uploadpack.allowFilter", "true"]).unwrap();
    let long_path = long_path();
    commit(
        &origin,
        false,
        &[
            ("descriptor.mod", "name=\"test\"\n"),
            ("common/a.txt", "v1\n"),
            ("events/e.txt", "not checked out\n"),
            (&long_path, "l_english:\n"),
        ],
    );
    let path = origin.to_str().unwrap().replace('\\', "/");
    let url = if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    };
    (origin, url)
}

/// Commits `files` to `main` in `origin` (on top of `main` if `parent`),
/// with `git fast-import`, which needs no identity or working tree.
fn commit(origin: &Path, parent: bool, files: &[(&str, &str)]) {
    let mut stream = String::from(
        "commit refs/heads/main\ncommitter hearty <hearty@example.com> 0 +0000\ndata 7\nfixture\n",
    );
    if parent {
        stream.push_str("from refs/heads/main^0\n");
    }
    for (path, contents) in files {
        stream.push_str(&format!(
            "M 100644 inline {path}\ndata {}\n{contents}\n",
            contents.len()
        ));
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(origin)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// `path`'s contents without the line ending, which is CRLF if git is set to
/// convert line endings.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().trim_end().to_owned()
}

fn source(url: &str) -> Source<'_> {
    Source {
        branch: "main",
        sparse_dirs: SPARSE_DIRS,
        url,
    }
}

/// The first checkout clones (including a path too long for Windows'
/// default limit); the next fetches, and undoes what a run changed.
#[test]
fn clones_then_fetches() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (origin, url) = origin(tmp.path());
    let dir = tmp.path().join("Millennium-Dawn");

    let how = checkout(&dir, &source(&url)).unwrap();
    assert!(how.starts_with("cloned in "), "{how}");
    assert!(dir.join(".git").join(MARKER).exists());
    assert!(dir.join("descriptor.mod").exists());
    assert_eq!(read(&dir.join("common/a.txt")), "v1");
    assert!(dir.join(long_path()).exists());
    // Outside the sparse directories.
    assert!(!dir.join("events").exists());
    assert!(!tmp.path().join("Millennium-Dawn.partial").exists());

    // What a run interrupted before its restore could leave.
    std::fs::write(dir.join("common/a.txt"), "changed\n").unwrap();
    std::fs::write(dir.join("common/new.txt"), "new\n").unwrap();
    commit(&origin, true, &[("common/a.txt", "v2\n")]);

    let how = checkout(&dir, &source(&url)).unwrap();
    assert!(how.starts_with("fetched in "), "{how}");
    assert_eq!(read(&dir.join("common/a.txt")), "v2");
    assert!(!dir.join("common/new.txt").exists());
    assert!(dir.join(long_path()).exists());
}

/// A clone that fails after `git clone` (here in the sparse checkout) leaves
/// nothing at the benchmark directory, and the next run clones again.
#[test]
fn recovers_from_a_failed_clone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (_, url) = origin(tmp.path());
    let dir = tmp.path().join("Millennium-Dawn");
    let partial = tmp.path().join("Millennium-Dawn.partial");

    let failing = Source {
        sparse_dirs: &["../outside"],
        ..source(&url)
    };
    let err = checkout(&dir, &failing).unwrap_err().to_string();
    assert!(err.contains("sparse-checkout"), "{err}");
    assert!(!dir.exists());
    // The unfinished clone is left beside it, and cleared by the next run.
    assert!(partial.join(".git").exists());

    let how = checkout(&dir, &source(&url)).unwrap();
    assert!(how.starts_with("cloned in "), "{how}");
    assert!(dir.join(".git").join(MARKER).exists());
    assert!(dir.join(long_path()).exists());
    assert!(!partial.exists());
}

/// An existing empty directory is cloned into.
#[test]
fn clones_into_an_empty_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (_, url) = origin(tmp.path());
    let dir = tmp.path().join("Millennium-Dawn");
    std::fs::create_dir(&dir).unwrap();

    let how = checkout(&dir, &source(&url)).unwrap();
    assert!(how.starts_with("cloned in "), "{how}");
    assert!(dir.join("descriptor.mod").exists());
}

/// A checkout the benchmark did not clone, or a directory that is not a
/// checkout, is left alone.
#[test]
fn refuses_other_directories() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (_, url) = origin(tmp.path());

    let working_copy = tmp.path().join("working-copy");
    git(None, &["init", "-q", working_copy.to_str().unwrap()]).unwrap();
    std::fs::write(working_copy.join("work.txt"), "unsaved\n").unwrap();
    let err = checkout(&working_copy, &source(&url))
        .unwrap_err()
        .to_string();
    assert!(err.contains("refusing to reset it"), "{err}");
    assert!(working_copy.join("work.txt").exists());

    let other = tmp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("file.txt"), "keep\n").unwrap();
    let err = checkout(&other, &source(&url)).unwrap_err().to_string();
    assert!(err.contains("is not a git checkout"), "{err}");
    assert!(other.join("file.txt").exists());
}
