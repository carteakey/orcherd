//! Declarative Herdr UI Mod installer (`orcherd mod install`).
//!
//! Writes `mod.toml`, `main.luau`, and `icon.svg` into
//! `${XDG_CONFIG_HOME:-$HOME/.config}/herdr/gpui-mods/orcherd/`.

use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const MOD_NAME: &str = "orcherd";
pub const MOD_TOML: &str = include_str!("../../../mods/orcherd/mod.toml");
pub const MAIN_LUAU: &str = include_str!("../../../mods/orcherd/main.luau");
pub const ICON_SVG: &str = include_str!("../../../mods/orcherd/icon.svg");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledMod {
    pub files: Vec<PathBuf>,
    pub mod_dir: PathBuf,
    pub name: &'static str,
}

pub fn install(custom_mods_dir: Option<&Path>) -> io::Result<InstalledMod> {
    let base = match custom_mods_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_mods_dir()?,
    };
    let mod_dir = base.join(MOD_NAME);
    fs::create_dir_all(&mod_dir)?;

    let toml_path = mod_dir.join("mod.toml");
    let luau_path = mod_dir.join("main.luau");
    let icon_path = mod_dir.join("icon.svg");

    fs::write(&toml_path, MOD_TOML)?;
    fs::write(&luau_path, MAIN_LUAU)?;
    fs::write(&icon_path, ICON_SVG)?;

    Ok(InstalledMod {
        name: MOD_NAME,
        mod_dir,
        files: vec![toml_path, luau_path, icon_path],
    })
}

fn default_mods_dir() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("herdr").join("gpui-mods"));
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(appdata).join("herdr").join("gpui-mods"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(home.join(".config").join("herdr").join("gpui-mods"))
}
