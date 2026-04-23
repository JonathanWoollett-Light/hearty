use base64::prelude::*;

#[cfg(windows)]
const HASH: &str = "KWOmhI5EBHFkWKS2hdDvAL6nMlg514witWoRklBWkcM=";
#[cfg(not(windows))]
const HASH: &str = "MziVnUf8RlQgj28JXAWZqR7F7uSBs5cOtEVHh2mALtI=";

const BINARY: &str = env!("CARGO_BIN_EXE_hearty");

#[test]
fn test_mod() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let tmp_dir_path = tmp_dir.keep();
    let to = tmp_dir_path.join("test_mod");
    println!("using {to:?}");

    if to.exists() {
        std::fs::remove_dir_all(&to).unwrap();
    }
    copy_dir::copy_dir("tests/test_mod", &to).unwrap();
    println!("copied");

    let output = std::process::Command::new(BINARY)
        .args([to.to_str().unwrap()])
        .status()
        .unwrap();
    println!("executed");

    assert!(!output.success()); // Assert that the tool communicated a failure.

    // Check that the result from formatting or fixing matches a specific target.
    let hash = dasher::hash_directory(to.clone()).unwrap();
    assert_eq!(BASE64_STANDARD.encode(&hash), HASH);
}
