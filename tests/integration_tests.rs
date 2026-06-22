use std::process::Stdio;

use base64::prelude::*;

#[cfg(windows)]
const HASH: &str = "2R58mJqNBlknrwYl+kzYumJ+506ktsAHfEw5r1HBR2I=";
#[cfg(not(windows))]
const HASH: &str = "89VtIYe+jFVkdV5yLopzN7daVYgpTpnYGLJToBAS/Uw=";

const BINARY: &str = env!("CARGO_BIN_EXE_hearty");

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

    let output = std::process::Command::new(BINARY)
        .args([to.to_str().unwrap(), "--format"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    println!("executed");

    assert!(output.status.success()); // Assert that the tool finished successfully.

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
        let contents = std::fs::read_to_string(entry.path()).unwrap();
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
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let to = tmp_dir.path().join("test_mod");
    copy_dir::copy_dir("tests/test_mod", &to).unwrap();

    let run = || {
        let output = std::process::Command::new(BINARY)
            .args([to.to_str().unwrap(), "--format"])
            .output()
            .unwrap();
        assert!(output.status.success());
        dasher::hash_directory(to.clone()).unwrap()
    };

    let first = run();
    let second = run();
    assert_eq!(
        BASE64_STANDARD.encode(&first),
        BASE64_STANDARD.encode(&second),
        "second --format changed the files; formatting is not idempotent"
    );
}
