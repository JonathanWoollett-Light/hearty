//! Running git for the benchmarks.

use std::path::Path;
use std::process::Command;

/// Everything that can fail here is reported and ends the benchmark.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Runs git (in `dir`, if given), returning its stdout; its stderr is in the
/// error if it fails.
pub fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    // Git for Windows cannot create a file whose full path is over 260
    // characters unless `core.longpaths` is set, and Millennium Dawn has
    // 106 character paths, so a deep benchmark directory would fail.
    command.args(["-c", "core.longpaths=true"]);
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    // Fail rather than wait for a password nobody will type.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command.args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines.get(lines.len().saturating_sub(20)..).unwrap_or(&[]);
        return Err(format!(
            "git {} failed ({}):\n{}",
            args.join(" "),
            output.status,
            tail.join("\n")
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
