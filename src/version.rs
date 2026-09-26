//! Lint: checks `descriptor.mod` for duplicate keys and for a
//! `supported_version` that does not match the latest HOI4 release.
//!
//! The latest release comes from HOI4's Steam app info, which is cached (see
//! [`cache_path`]) and otherwise asked of steamcmd, downloading steamcmd if
//! there is none. That waits on the network and on another process, so
//! [`spawn`] runs the check on a thread of its own while the rest of the lint
//! works, and its output is buffered until the lint prints it.

use keyvalues_parser::{Value as VdfValue, Vdf};
use miette::{Diagnostic, NamedSource, SourceSpan};
use serde_json::{Map, Value as JsonValue};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;
use std::time::SystemTime;

const DEFAULT_CACHE_DIR: &str = ".hearty-cache";

const HOI4_ID: &str = "394360";

/// Cache file name written inside the cache directory.
const HOI4_CACHE_FILE: &str = "hoi4-version-cache.json";

/// Maximum age of the on-disk cache before steamcmd is re-invoked (86 400 s = 24 h).
const HOI4_CACHE_MAX_AGE_SECS: u64 = 86_400;

/// Maximum bytes to read when downloading steamcmd (32 MiB).
const STEAMCMD_DOWNLOAD_MAX: u64 = 32 * 1_024 * 1_024;

/// descriptor.mod supported_version "{supported_version}" does not match latest HOI4 {latest_version}.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct DescriptorVersionMismatch {
    latest_version: String,
    #[label("unsupported version")]
    span: SourceSpan,
    #[source_code]
    src: NamedSource<Arc<str>>,
    supported_version: String,
}

impl std::error::Error for DescriptorVersionMismatch {}

/// duplicate key "{key}" in descriptor.mod.
#[derive(displaydoc::Display, Debug, Diagnostic)]
#[diagnostic(severity(warning))]
struct DuplicateDescriptorKey {
    key: String,
    #[label("duplicate key")]
    span: SourceSpan,
    #[source_code]
    src: NamedSource<Arc<str>>,
}

impl std::error::Error for DuplicateDescriptorKey {}

/// What the version check prints, held until the lint prints it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Rendered diagnostics, for stderr.
    pub stderr: String,
    /// Progress messages, for stdout.
    pub stdout: String,
}

/// Returns the path to the HOI4 version cache file.
///
/// The directory is read from the `HEARTY_CACHE_DIR` environment variable when set.
/// In GitHub Actions, point `actions/cache` at that path (or at `.hearty-cache`) so
/// the file survives across workflow runs and steamcmd only runs when the cache is stale:
///
/// ```yaml
/// - uses: actions/cache@v4
///   with:
///     path: .hearty-cache
///     key: hoi4-version-cache
/// ```
fn cache_path() -> std::path::PathBuf {
    std::env::var_os("HEARTY_CACHE_DIR")
        .map_or_else(
            || std::path::PathBuf::from(DEFAULT_CACHE_DIR),
            std::path::PathBuf::from,
        )
        .join(HOI4_CACHE_FILE)
}

/// Checks `descriptor` (the text of `descriptor.mod`) against the latest
/// HOI4 version in `hoi4_data`, rendering its diagnostics to `report`.
fn check_descriptor(
    descriptor: &str,
    hoi4_data: &serde_json::Value,
    report: &mut Report,
) -> Option<()> {
    let branches = hoi4_data
        .as_object()?
        .get(HOI4_ID)?
        .as_object()?
        .get("depots")?
        .as_object()?
        .get("branches")?
        .as_object()?;
    let versions = branches
        .keys()
        .filter_map(|k| {
            semver::Version::parse(k).ok().or_else(|| {
                // HOI4 sometimes uses 4-part versions (e.g. "1.17.3.0"); drop the last component.
                let trimmed = k.rsplit_once('.')?.0;
                semver::Version::parse(trimmed).ok()
            })
        })
        .collect::<Vec<_>>();
    let latest_version = versions.into_iter().max()?;

    let src: Arc<str> = Arc::from(descriptor);
    let handler = miette::GraphicalReportHandler::new();

    let tape = jomini::TextTape::from_slice(descriptor.as_bytes()).ok()?;
    let reader = tape.windows1252_reader();
    let mut fields = HashSet::new();
    for (key, _, value) in reader.fields() {
        // Check for duplicate keys.
        let key = key.read_string();
        if fields.contains(&key) && key != "replace_path" {
            let key_str = key.clone();
            let first_end = descriptor.find(&key_str).map_or(0, |p| p + key_str.len());
            let offset = descriptor
                .get(first_end..)
                .and_then(|s| s.find(&key_str))
                .map_or(0, |o| o + first_end);
            let key_len = key_str.len();
            let diag = DuplicateDescriptorKey {
                key: key_str,
                span: (offset, key_len).into(),
                src: NamedSource::new("descriptor.mod", Arc::clone(&src)),
            };
            render(&handler, &diag, report);
            continue;
        }

        // Check supported_version field.
        if key == "supported_version" {
            let req_str = value.read_string().ok()?;
            let req = semver::VersionReq::parse(&req_str).ok()?;
            if !req.matches(&latest_version) {
                let offset = descriptor.find(&req_str).unwrap_or(0);
                let req_len = req_str.len();
                let diag = DescriptorVersionMismatch {
                    supported_version: req_str,
                    latest_version: latest_version.to_string(),
                    span: (offset, req_len).into(),
                    src: NamedSource::new("descriptor.mod", Arc::clone(&src)),
                };
                render(&handler, &diag, report);
            }
        }

        // Track fields for later duplicate key check.
        fields.insert(key);
    }
    Some(())
}

/// Downloads steamcmd from the Steam CDN into the cache directory and returns
/// its path, noting the download in `report`. Returns `None` if the download
/// or extraction fails.
fn download_steamcmd(report: &mut Report) -> Option<std::path::PathBuf> {
    let _span = tracing::info_span!("download steamcmd").entered();
    let dest = steamcmd_cache_path();
    let dir = dest.parent()?;
    std::fs::create_dir_all(dir).ok()?;

    #[cfg(windows)]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip";
    #[cfg(target_os = "macos")]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_osx.tar.gz";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz";

    writeln!(
        report.stdout,
        "steamcmd not found; downloading to {} ...",
        dir.display()
    )
    .ok()?;
    let mut bytes = Vec::new();
    ureq::get(url)
        .call()
        .ok()?
        .into_body()
        .into_with_config()
        .limit(STEAMCMD_DOWNLOAD_MAX)
        .reader()
        .read_to_end(&mut bytes)
        .ok()?;

    #[cfg(windows)]
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .ok()?
        .extract(dir)
        .ok()?;

    #[cfg(not(windows))]
    {
        let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
        tar::Archive::new(gz).unpack(dir).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
    }

    dest.exists().then_some(dest)
}

/// Reads the on-disk cache and returns the HOI4 app data if it is younger than
/// [`HOI4_CACHE_MAX_AGE_SECS`]. Returns `None` if the cache is missing, corrupt,
/// or expired.
fn read_cache() -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(cache_path()).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&content).ok()?;
    let fetched_at = cache.get("fetched_at_secs")?.as_u64()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let data = cache.get("data")?;
    (now.saturating_sub(fetched_at) < HOI4_CACHE_MAX_AGE_SECS).then(|| data.clone())
}

/// Renders `diagnostic` to `report`'s stderr, leaving it out if it fails to
/// render.
fn render(
    handler: &miette::GraphicalReportHandler,
    diagnostic: &dyn Diagnostic,
    report: &mut Report,
) {
    let mut out = String::new();
    if handler.render_report(&mut out, diagnostic).is_ok() {
        report.stderr.push_str(&out);
    }
}

/// Returns the path to a usable steamcmd binary: system PATH first, then the
/// cache directory, downloading if neither is present.
fn resolve_steamcmd(report: &mut Report) -> Option<std::path::PathBuf> {
    if steamcmd_in_path() {
        #[cfg(windows)]
        return Some(std::path::PathBuf::from("steamcmd.exe"));
        #[cfg(not(windows))]
        return Some(std::path::PathBuf::from("steamcmd"));
    }
    let cached = steamcmd_cache_path();
    if cached.exists() {
        return Some(cached);
    }
    download_steamcmd(report)
}

/// Checks `descriptor` (the text of `descriptor.mod`) against the latest
/// HOI4 version: from the cache when it is fresh, else from steamcmd (which
/// refreshes the cache). Without a version to compare with, nothing is
/// reported.
fn run(descriptor: &str) -> Report {
    let _span = tracing::info_span!("version check").entered();
    let mut report = Report::default();
    // A fresh cache answers without steamcmd, so steamcmd is only located (or
    // downloaded) when the cache is missing or stale.
    let hoi4_data_opt = tracing::info_span!("cache read")
        .in_scope(read_cache)
        .or_else(|| {
            let data = tracing::info_span!("steamcmd").in_scope(|| {
                let steamcmd = resolve_steamcmd(&mut report)?;
                let out = std::process::Command::new(&steamcmd)
                    .args(["+login", "anonymous", "+app_info_print", HOI4_ID, "+quit"])
                    .output()
                    .ok()?;
                let text = String::from_utf8_lossy(&out.stdout);
                let start = text.find(&format!("\"{HOI4_ID}\""))?;
                let end = text.rfind("Unloading Steam API")?;
                let values = Vdf::from(keyvalues_parser::parse(text.get(start..end)?).ok()?);
                Some(vdf_to_json(&values))
            })?;
            tracing::info_span!("cache write").in_scope(|| write_cache(&data));
            Some(data)
        });
    if let Some(hoi4_data) = hoi4_data_opt {
        let _: Option<()> = tracing::info_span!("check descriptor")
            .in_scope(|| check_descriptor(descriptor, &hoi4_data, &mut report));
    }
    report
}

/// Starts checking `descriptor` (see [`run`]) on a thread of its own; join
/// it for the [`Report`].
pub fn spawn(descriptor: String) -> std::thread::JoinHandle<Report> {
    std::thread::spawn(move || run(&descriptor))
}

/// Path where a downloaded steamcmd binary is cached alongside the version cache.
fn steamcmd_cache_path() -> std::path::PathBuf {
    let dir = cache_path().parent().map_or_else(
        || std::path::PathBuf::from(DEFAULT_CACHE_DIR),
        std::path::Path::to_path_buf,
    );
    #[cfg(windows)]
    return dir.join("steamcmd.exe");
    #[cfg(not(windows))]
    return dir.join("steamcmd.sh");
}

/// Returns `true` if a `steamcmd` binary is reachable via `PATH`.
fn steamcmd_in_path() -> bool {
    #[cfg(windows)]
    let name = "steamcmd.exe";
    #[cfg(not(windows))]
    let name = "steamcmd";
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).exists()))
}

fn vdf_to_json(vdf: &Vdf) -> JsonValue {
    // Wrap the root key/value into a single-entry object so the top-level
    // "394360" key is preserved.
    let mut root = Map::new();
    root.insert(vdf.key.to_string(), vdf_value_to_json(&vdf.value));
    JsonValue::Object(root)
}

fn vdf_value_to_json(value: &VdfValue) -> JsonValue {
    match value {
        VdfValue::Str(s) => JsonValue::String(s.to_string()),
        VdfValue::Obj(obj) => {
            let mut map = Map::new();
            for (key, values) in obj.iter() {
                let mut converted: Vec<JsonValue> = values.iter().map(vdf_value_to_json).collect();
                // VDF allows duplicate keys at the same level, stored as Vec.
                // Collapse single-element vecs to the bare value; keep arrays for duplicates.
                let entry = if converted.len() == 1 {
                    converted.remove(0)
                } else {
                    JsonValue::Array(converted)
                };
                map.insert(key.to_string(), entry);
            }
            JsonValue::Object(map)
        }
    }
}

/// Writes the HOI4 app data and a `fetched_at_secs` Unix timestamp to the cache
/// file so [`read_cache`] can assess freshness on the next run. Failures are
/// silently ignored — the cache is best-effort.
fn write_cache(data: &serde_json::Value) {
    let path = cache_path();
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = serde_json::json!({
        "fetched_at_secs": now_secs,
        "data": data,
    });
    let Ok(serialized) = serde_json::to_string(&payload) else {
        return;
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }
    std::fs::write(path, serialized).unwrap_or_default();
}
