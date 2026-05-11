use std::process::Stdio;

use base64::prelude::*;

#[cfg(windows)]
const HASH: &str = "+1gLtSwyXfBbj/CeOV+4Yv0WiSt3oOHF5Sshq8n5Bj4=";
#[cfg(not(windows))]
const HASH: &str = "zqztMp2ck2kBydZ8iL7EumZQ+QYJ9CGTOpT0zNO6usc=";

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
