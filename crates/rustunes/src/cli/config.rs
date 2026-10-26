//! This host's settings: the library directory `sync` mirrors and `push` and
//! `pull` default to. They live in `XDG_CONFIG_HOME/rustunes/config.json`
//! (`~/.config/rustunes/config.json`). Unknown keys are ignored, so a file
//! written by another version still reads.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub(super) struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) library: Option<PathBuf>,
}

/// The settings file.
pub(super) fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        ensure!(path.is_absolute(), "XDG_CONFIG_HOME must be absolute");
        return Ok(path.join("rustunes/config.json"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("set HOME or XDG_CONFIG_HOME")?);
    ensure!(home.is_absolute(), "HOME must be absolute");
    Ok(home.join(".config/rustunes/config.json"))
}

pub(super) fn load() -> Result<Settings> {
    read(&path()?)
}

fn read(path: &Path) -> Result<Settings> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("reading the settings in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replace the settings file whole: a reader sees the old file or the new.
fn write(path: &Path, settings: &Settings) -> Result<()> {
    let directory = path.parent().context("settings file has no directory")?;
    fs::create_dir_all(directory).with_context(|| format!("creating {}", directory.display()))?;
    let temporary = path.with_extension("json.new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(settings)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// The library directory `sync` mirrors. Without one, the error names the
/// command that sets it.
pub(super) fn library() -> Result<PathBuf> {
    load()?.library.context(
        "no library directory is set; set the directory `rustunes sync` mirrors with:\n  rustunes config library DIR\nor send tracks from any directory or file with `rustunes push PATH`",
    )
}

/// `config library [DIR]`: set the library directory, or print it.
pub(super) fn set_library(directory: Option<&Path>) -> Result<()> {
    let file = path()?;
    let mut settings = read(&file)?;
    let Some(directory) = directory else {
        if super::machine::enabled() {
            return super::machine::report(
                "config",
                &serde_json::json!({"file": file, "library": settings.library, "changed": false}),
            );
        }
        match &settings.library {
            Some(library) => println!("{}", library.display()),
            None => eprintln!(
                "No library directory is set; set one with `rustunes config library DIR`."
            ),
        }
        return Ok(());
    };
    let library =
        fs::canonicalize(directory).with_context(|| format!("opening {}", directory.display()))?;
    ensure!(
        library.is_dir(),
        "{} is not a directory; `push` sends a single file",
        library.display()
    );
    library
        .to_str()
        .context("the library directory's path is not UTF-8")?;
    settings.library = Some(library.clone());
    write(&file, &settings)?;
    if super::machine::enabled() {
        return super::machine::report(
            "config",
            &serde_json::json!({"file": file, "library": library, "changed": true}),
        );
    }
    println!("Library directory: {}", library.display());
    println!("`rustunes sync` now mirrors it to the phone.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_tolerate_other_versions_keys() {
        let directory =
            std::env::temp_dir().join(format!("rustunes-config-{}", std::process::id()));
        let file = directory.join("rustunes/config.json");
        assert!(read(&file).unwrap().library.is_none());
        write(
            &file,
            &Settings {
                library: Some("/music".into()),
            },
        )
        .unwrap();
        assert_eq!(
            read(&file).unwrap().library.as_deref(),
            Some(Path::new("/music"))
        );
        fs::write(&file, br#"{"library":"/other","added_later":{"x":1}}"#).unwrap();
        assert_eq!(
            read(&file).unwrap().library.as_deref(),
            Some(Path::new("/other"))
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}
