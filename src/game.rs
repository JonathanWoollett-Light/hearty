//! Finding the base game, Hearts of Iron IV itself. A mod's scripts may use
//! any localisation key the game defines: most mods reuse vanilla events,
//! focuses and ideas, whose keys are in the game's own localisation files,
//! so the lint counts those keys as defined.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The game's folder in a Steam library's `steamapps/common`.
const STEAM_FOLDER: &str = "Hearts of Iron IV";

/// The environment variable naming the game's folder.
pub const GAME_DIR_VAR: &str = "HEARTY_GAME_DIR";

/// Where the game's folder came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `HEARTY_GAME_DIR`.
    Env,
    /// `--game-dir`.
    Flag,
    /// A Steam library.
    Steam,
}

/// Finds the game's folder: `flag` (`--game-dir`), else `HEARTY_GAME_DIR`,
/// else the first Steam library holding the game. An empty flag or variable
/// means not to use the game (so runs don't depend on whether it is
/// installed); a folder they name is used as is, one found in Steam must
/// hold localisation.
pub fn find(flag: Option<&Path>) -> Option<(PathBuf, Source)> {
    if let Some(dir) = flag {
        return (!dir.as_os_str().is_empty()).then(|| (dir.to_path_buf(), Source::Flag));
    }
    if let Some(dir) = std::env::var_os(GAME_DIR_VAR) {
        return (!dir.is_empty()).then(|| (PathBuf::from(dir), Source::Env));
    }
    let in_library = |library: &Path| {
        let dir = library.join("steamapps").join("common").join(STEAM_FOLDER);
        dir.join("localisation").is_dir().then_some(dir)
    };
    // The registry is only asked (a process) when the usual places lack it.
    default_steam_roots()
        .into_iter()
        .flat_map(|root| library_folders(&root))
        .find_map(|library| in_library(&library))
        .or_else(|| {
            registry_steam_root()
                .into_iter()
                .flat_map(|root| library_folders(&root))
                .find_map(|library| in_library(&library))
        })
        .map(|dir| (dir, Source::Steam))
}

/// The usual Steam installation folders on this platform.
fn default_steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(windows) {
        roots.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        roots.push(PathBuf::from(r"C:\Program Files\Steam"));
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".local/share/Steam"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
        roots.push(home.join("Library/Application Support/Steam"));
    }
    roots.retain(|root| root.is_dir());
    roots
}

/// Steam's installation folder from the Windows registry, if `reg` can read
/// it (never elsewhere).
fn registry_steam_root() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let output = Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        let (_, path) = line.split_once("REG_SZ")?;
        let path = path.trim();
        (!path.is_empty()).then(|| PathBuf::from(path))
    })
}

/// The Steam library folders of the installation at `root`: `root` itself
/// and every library its `steamapps/libraryfolders.vdf` lists.
fn library_folders(root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![root.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        libraries.extend(parse_library_folders(&vdf));
    }
    libraries
}

/// The `"path"` values of a `libraryfolders.vdf`, with `\\` unescaped.
fn parse_library_folders(vdf: &str) -> Vec<PathBuf> {
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

/// The folders a mod's `descriptor.mod` replaces (`replace_path = "..."`),
/// whose files in the game are then ignored. As in the game, a replaced
/// folder's subfolders are not replaced with it.
pub fn replaced_folders(descriptor: &str) -> Vec<PathBuf> {
    let Ok(document) = crate::cst::parse(descriptor) else {
        return Vec::new();
    };
    document
        .root
        .entries
        .iter()
        .filter(|entry| entry.key_str(descriptor) == Some("replace_path"))
        .filter_map(|entry| match &entry.value {
            crate::cst::Value::Scalar(scalar) => Some(scalar.unquoted(descriptor)),
            crate::cst::Value::Block(_) | crate::cst::Value::Tagged { .. } => None,
        })
        .map(|path| path.trim_matches('/').split('/').collect::<PathBuf>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Source, find, parse_library_folders, replaced_folders};
    use std::path::{Path, PathBuf};

    #[test]
    fn library_folders_are_read_from_the_vdf() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"PATH\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            parse_library_folders(vdf),
            [
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary"),
            ]
        );
        assert_eq!(parse_library_folders(""), Vec::<PathBuf>::new());
    }

    #[test]
    fn replaced_folders_are_read_from_the_descriptor() {
        let descriptor = "name=\"Mod\"\nreplace_path=\"localisation/english\"\nreplace_path = \"common/ideas/\"\nreplace_path={ nonsense }\ntags={\n\t\"Gameplay\"\n}\n";
        assert_eq!(
            replaced_folders(descriptor),
            [
                ["localisation", "english"].iter().collect::<PathBuf>(),
                ["common", "ideas"].iter().collect::<PathBuf>(),
            ]
        );
        assert_eq!(replaced_folders("not { closed"), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_flag_wins() {
        let dir = Path::new("somewhere");
        assert_eq!(find(Some(dir)), Some((dir.to_path_buf(), Source::Flag)));
        // An empty flag opts out of the game.
        assert_eq!(find(Some(Path::new(""))), None);
    }
}
