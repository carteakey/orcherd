//! Herdr desktop app mod installer (`orcherd mod install`).
//!
//! Writes `mod.json`, `main.js`, and `icon.svg` into the Herdr app's mod folder:
//! `$HERDR_APP_HOME/mods/orcherd/`, else `${XDG_CONFIG_HOME:-$HOME/.config}/herdr-app/mods/orcherd/`.
//! The app hot-reloads the mod as soon as the files land; `herdr-app check <dir>` validates it.
//! It also removes the retired Luau mod from the third-party `herdr-gpui` mods folder.

use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const MOD_NAME: &str = "orcherd";
pub const MOD_JSON: &str = include_str!("../../../mods/orcherd/mod.json");
pub const MAIN_JS: &str = include_str!("../../../mods/orcherd/main.js");
pub const ICON_SVG: &str = include_str!("../../../mods/orcherd/icon.svg");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledMod {
    pub files: Vec<PathBuf>,
    pub mod_dir: PathBuf,
    pub name: &'static str,
    /// The retired Luau mod folder, if one was found and removed.
    pub removed_legacy: Option<PathBuf>,
}

pub fn install(custom_mods_dir: Option<&Path>) -> io::Result<InstalledMod> {
    let base = match custom_mods_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_mods_dir()?,
    };
    let mod_dir = base.join(MOD_NAME);
    fs::create_dir_all(&mod_dir)?;

    let mut files = Vec::with_capacity(3);
    for (name, body) in [
        ("mod.json", MOD_JSON),
        ("main.js", MAIN_JS),
        ("icon.svg", ICON_SVG),
    ] {
        let path = mod_dir.join(name);
        fs::write(&path, body)?;
        files.push(path);
    }
    // Files an older orcherd wrote here for the Luau runtime; the app ignores them.
    for stale in ["mod.toml", "main.luau"] {
        remove_if_present(&mod_dir.join(stale))?;
    }

    let removed_legacy = if custom_mods_dir.is_none() {
        remove_legacy_luau_mod()?
    } else {
        None
    };

    Ok(InstalledMod {
        name: MOD_NAME,
        mod_dir,
        files,
        removed_legacy,
    })
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Removes `~/.config/herdr/gpui-mods/orcherd/` only when it holds nothing but the
/// files older orcherd versions installed, so a user's own edits are never deleted.
fn remove_legacy_luau_mod() -> io::Result<Option<PathBuf>> {
    let dir = config_root()?
        .join("herdr")
        .join("gpui-mods")
        .join(MOD_NAME);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };
    let ours = ["mod.toml", "main.luau", "icon.svg"];
    for entry in entries {
        let name = entry?.file_name();
        if !ours.iter().any(|o| name == *o) {
            return Ok(None);
        }
    }
    fs::remove_dir_all(&dir)?;
    Ok(Some(dir))
}

fn default_mods_dir() -> io::Result<PathBuf> {
    if let Some(app_home) = std::env::var_os("HERDR_APP_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(app_home).join("mods"));
    }
    Ok(config_root()?.join("herdr-app").join("mods"))
}

fn config_root() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg));
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(appdata));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(home.join(".config"))
}
