use std::process::Stdio;

use base64::prelude::*;

#[cfg(windows)]
const HASH: &str = "ec8eGvVgKclmyILc1MdjJB8MiLq8nkf318atWR28rIU=";
#[cfg(not(windows))]
const HASH: &str = "H817Ltn1rkS8ql9d2ZJF1f0xeBpF3HTDtq+aKdjSCXU=";

const BINARY: &str = env!("CARGO_BIN_EXE_hearty");

#[test]
fn test_fmt() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let tmp_dir_path = tmp_dir.keep();
    let to = tmp_dir_path.join("test_mod");
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

    // Check that the result from formatting or fixing matches a specific target.
    let hash = dasher::hash_directory(to.clone()).unwrap();
    assert_eq!(BASE64_STANDARD.encode(&hash), HASH);
}
