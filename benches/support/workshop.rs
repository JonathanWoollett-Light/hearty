//! Finding HOI4 Steam Workshop mods for the workshop benchmark, and keeping
//! a git working copy of the files hearty reads so the runs that change
//! files can be undone. The mod's own folder (inside Steam) is only read.
//! It is a module of its own so `tests/bench_workshop.rs` can test it.

use rayon::prelude::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git::{Result, git};

/// Hearts of Iron IV's Steam app id.
pub const HOI4_APP_ID: &str = "394360";

/// The directories of a mod that hearty reads, copied besides
/// `descriptor.mod`.
pub const MOD_DIRS: &[&str] = &["common", "events", "history", "localisation"];

/// Marks a working copy as made by this benchmark; a git repository
/// without it is never reset or synced into.
const MARKER: &str = "hearty-bench";

/// A Steam Workshop mod to benchmark.
#[derive(Debug, Clone, Copy)]
pub struct WorkshopMod {
    /// Its Workshop item id.
    pub id: &'static str,
    /// Its name.
    pub name: &'static str,
    /// A file-name-safe short name, used for its working copy and outputs.
    pub slug: &'static str,
}

impl WorkshopMod {
    /// The environment variable that can point at a copy of the mod.
    pub fn env_var(&self) -> String {
        format!("HEARTY_BENCH_MOD_{}", self.id)
    }
}

/// The mods the workshop benchmark runs on.
pub const MODS: &[WorkshopMod] = &[
    WorkshopMod {
        id: "820260968",
        name: "The Road to 56",
        slug: "road-to-56",
    },
    WorkshopMod {
        id: "1521695605",
        name: "Kaiserreich",
        slug: "kaiserreich",
    },
    WorkshopMod {
        id: "2265420196",
        name: "Old World Blues",
        slug: "old-world-blues",
    },
    WorkshopMod {
        id: "3350890356",
        name: "The Fire Rises",
        slug: "the-fire-rises",
    },
    WorkshopMod {
        id: "2777392649",
        name: "Millennium Dawn",
        slug: "millennium-dawn",
    },
];

/// Where a mod was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The path in its `HEARTY_BENCH_MOD_<id>` environment variable.
    Env(PathBuf),
    /// A Steam library's Workshop folder (the mod is subscribed to).
    Library(PathBuf),
    /// Downloaded with steamcmd.
    Steamcmd(PathBuf),
}

impl Found {
    /// The mod's folder.
    pub fn path(&self) -> &Path {
        match self {
            Self::Env(path) | Self::Library(path) | Self::Steamcmd(path) => path,
        }
    }

    /// How the mod was found, for the report.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(path) => format!("{} (environment)", path.display()),
            Self::Library(path) => format!("{} (Steam Workshop)", path.display()),
            Self::Steamcmd(path) => format!("{} (steamcmd)", path.display()),
        }
    }
}

/// What [`sync`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Synced {
    /// Files copied because they were new or differed.
    pub copied: usize,
    /// Files deleted because the mod no longer has them.
    pub deleted: usize,
    /// Whether the working copy was created by this call.
    pub created: bool,
}

/// Finds `workshop_mod`: its `HEARTY_BENCH_MOD_<id>` environment variable,
/// then the Workshop folder of every Steam library, then, if
/// `HEARTY_BENCH_STEAM_USER` names a Steam account that owns HOI4, a
/// steamcmd download into `steamcmd_dir`. HOI4 is not free, so steamcmd's
/// anonymous login cannot download its Workshop items. Returns `None` if
/// none of these has the mod.
pub fn locate(workshop_mod: &WorkshopMod, steamcmd_dir: &Path) -> Result<Option<Found>> {
    if let Some(path) = std::env::var_os(workshop_mod.env_var()) {
        let path = PathBuf::from(path);
        if !path.join("descriptor.mod").is_file() {
            return Err(format!(
                "{} is set to {}, which has no descriptor.mod",
                workshop_mod.env_var(),
                path.display()
            )
            .into());
        }
        return Ok(Some(Found::Env(path)));
    }
    let libraries: Vec<PathBuf> = steam_roots()
        .iter()
        .flat_map(|root| library_folders(root))
        .collect();
    if let Some(path) = libraries
        .iter()
        .map(|library| workshop_dir(library, workshop_mod.id))
        .find(|dir| dir.join("descriptor.mod").is_file())
    {
        return Ok(Some(Found::Library(path)));
    }
    if let Some(user) = std::env::var_os("HEARTY_BENCH_STEAM_USER") {
        let user = user
            .to_str()
            .ok_or("HEARTY_BENCH_STEAM_USER is not UTF-8")?;
        return download(workshop_mod, steamcmd_dir, user).map(|path| Some(Found::Steamcmd(path)));
    }
    Ok(None)
}

/// The Workshop folder of item `id` in the Steam library at `library`.
pub fn workshop_dir(library: &Path, id: &str) -> PathBuf {
    library
        .join("steamapps")
        .join("workshop")
        .join("content")
        .join(HOI4_APP_ID)
        .join(id)
}

/// Candidate Steam installation folders: `HEARTY_BENCH_STEAM_DIR`, the one
/// the Windows registry names, and the usual default locations, keeping
/// those that exist.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("HEARTY_BENCH_STEAM_DIR") {
        roots.push(PathBuf::from(dir));
    }
    if cfg!(windows) {
        // The defaults come first so that, when the registry names the same
        // folder in its own spelling, the report shows the familiar one.
        roots.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        roots.push(PathBuf::from(r"C:\Program Files\Steam"));
        roots.extend(registry_steam_path());
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".local/share/Steam"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
        roots.push(home.join("Library/Application Support/Steam"));
    }
    let mut seen = BTreeSet::new();
    roots
        .into_iter()
        .filter(|root| root.is_dir())
        .filter(|root| seen.insert(root.canonicalize().unwrap_or_else(|_| root.clone())))
        .collect()
}

/// Steam's installation folder from the Windows registry, if `reg` can read
/// it.
fn registry_steam_path() -> Option<PathBuf> {
    let output = Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        let (_, path) = line.split_once("REG_SZ")?;
        let path = path.trim();
        // The registry stores it with forward slashes, e.g.
        // `c:/program files (x86)/steam`.
        (!path.is_empty()).then(|| PathBuf::from(path.replace('/', "\\")))
    })
}

/// The Steam library folders of the installation at `root`: `root` itself
/// and every library listed in its `steamapps/libraryfolders.vdf`.
pub fn library_folders(root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![root.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        libraries.extend(parse_library_folders(&vdf));
    }
    libraries
}

/// The `"path"` values of a `libraryfolders.vdf`, unescaping `\\`.
pub fn parse_library_folders(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut quoted = line.split('"').skip(1).step_by(2);
            let key = quoted.next()?;
            let value = quoted.next()?;
            key.eq_ignore_ascii_case("path")
                .then(|| PathBuf::from(value.replace(r"\\", r"\")))
        })
        .collect()
}

/// Downloads (or updates) `workshop_mod` with steamcmd, logged in as `user`,
/// into `dir`, returning the mod's folder. steamcmd only fetches what
/// changed since its last download. It must already hold the account's
/// credentials (log in once interactively with `steamcmd +login <user>`);
/// the benchmark never asks for a password.
fn download(workshop_mod: &WorkshopMod, dir: &Path, user: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let steamcmd = std::env::var_os("HEARTY_BENCH_STEAMCMD").unwrap_or_else(|| "steamcmd".into());
    println!(
        "downloading {} ({}) with steamcmd as {user} ...",
        workshop_mod.name, workshop_mod.id
    );
    let output = Command::new(&steamcmd)
        .arg("+force_install_dir")
        .arg(dir)
        .args([
            "+login",
            user,
            "+workshop_download_item",
            HOI4_APP_ID,
            workshop_mod.id,
            "+quit",
        ])
        .output()
        .map_err(|err| format!("could not run {}: {err}", Path::new(&steamcmd).display()))?;
    let path = workshop_dir(dir, workshop_mod.id);
    if path.join("descriptor.mod").is_file() {
        return Ok(path);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let tail = lines.get(lines.len().saturating_sub(10)..).unwrap_or(&[]);
    Err(format!(
        "steamcmd did not download {} ({}):\n{}",
        workshop_mod.name,
        workshop_mod.id,
        tail.join("\n")
    )
    .into())
}

/// Makes the git working copy at `dest` hold exactly the `descriptor.mod`
/// and [`MOD_DIRS`] of the mod at `source`, committing any change, so a
/// benchmark can undo a run with `git reset --hard`. The first call creates
/// the repository; later calls first discard any leftover changes, then copy
/// only the files that differ and delete the ones the mod dropped.
///
/// The repository stores files byte for byte (no line-ending conversion or
/// `.gitattributes` filters), since hearty's output depends on them. A git
/// repository at `dest` that this function did not create is refused.
pub fn sync(source: &Path, dest: &Path) -> Result<Synced> {
    let git_dir = dest.join(".git");
    let created = !git_dir.exists();
    if created {
        std::fs::create_dir_all(dest)?;
        git(Some(dest), &["init", "-q"])?;
        git(Some(dest), &["config", "core.autocrlf", "false"])?;
        git(Some(dest), &["config", "core.safecrlf", "false"])?;
        // `info/attributes` outranks any `.gitattributes` in the mod.
        std::fs::create_dir_all(git_dir.join("info"))?;
        std::fs::write(
            git_dir.join("info").join("attributes"),
            "* -text -filter -diff\n",
        )?;
        std::fs::write(git_dir.join(MARKER), "")?;
    } else if !git_dir.join(MARKER).is_file() {
        return Err(format!(
            "{} is a git repository this benchmark did not create (no .git/{MARKER}); refusing to sync into it",
            dest.display()
        )
        .into());
    } else if git(Some(dest), &["rev-parse", "-q", "--verify", "HEAD"]).is_ok() {
        git(Some(dest), &["reset", "--hard", "-q", "HEAD"])?;
        git(Some(dest), &["clean", "-fdqx"])?;
    }

    let wanted = mod_files(source);
    let copied = wanted
        .par_iter()
        .map(|relative| -> std::io::Result<usize> {
            let from = source.join(relative);
            let to = dest.join(relative);
            let bytes = std::fs::read(&from)?;
            if std::fs::read(&to).is_ok_and(|existing| existing == bytes) {
                return Ok(0);
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&to, bytes)?;
            Ok(1)
        })
        .collect::<std::io::Result<Vec<usize>>>()?
        .into_iter()
        .sum();
    let mut deleted = 0;
    for relative in mod_files(dest).difference(&wanted) {
        std::fs::remove_file(dest.join(relative))?;
        deleted += 1;
    }

    git(Some(dest), &["add", "--all", "--force", "."])?;
    let status = git(Some(dest), &["status", "--porcelain"])?;
    if created || !status.trim().is_empty() {
        git(
            Some(dest),
            &[
                "-c",
                "user.name=hearty-bench",
                "-c",
                "user.email=hearty-bench@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "sync",
            ],
        )?;
    }
    Ok(Synced {
        copied,
        deleted,
        created,
    })
}

/// The mod files hearty reads under `root`, relative to it: its
/// `descriptor.mod` and every file under [`MOD_DIRS`].
fn mod_files(root: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    if root.join("descriptor.mod").is_file() {
        files.insert(PathBuf::from("descriptor.mod"));
    }
    for dir in MOD_DIRS {
        files.extend(
            walkdir::WalkDir::new(root.join(dir))
                .into_iter()
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .filter_map(|entry| entry.path().strip_prefix(root).ok().map(Path::to_path_buf)),
        );
    }
    files
}
