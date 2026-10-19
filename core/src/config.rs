//! User settings (config.json) and the data folder every other module writes to.

use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// One downloadable filter list in Adblock Plus syntax.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilterList {
    pub name:    String,
    pub url:     String,
    pub enabled: bool,
    /// Group shown in the app: ads, privacy, malware, general, regional, custom, ...
    #[serde(default = "custom_kind")]
    pub kind:    String,
    /// Trusted lists may use powerful scriptlets (trusted-*), like Zen's trusted flag.
    #[serde(default)]
    pub trusted: bool,
}

/// One userscript (`.user.js`) the user added. `name` comes from its `@name`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Userscript {
    pub name:    String,
    pub url:     String,
    pub enabled: bool,
}

/// An app whose connections are never decrypted or filtered: a program (by path) or a
/// Microsoft Store app (by package family name, which survives updates). Exactly one is set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExcludedApp {
    pub name:    String,
    #[serde(default)]
    pub path:    Option<String>,
    #[serde(default)]
    pub package: Option<String>,
}

fn custom_kind() -> String {
    "custom".into()
}

/// User settings, stored as JSON in the data directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub port:       u16,
    pub lists:      Vec<FilterList>,
    /// Hosts that are tunneled without decryption (subdomains included).
    pub exclusions: Vec<String>,
    #[serde(default)]
    pub userscripts: Vec<Userscript>,
    #[serde(default)]
    pub excluded_apps: Vec<ExcludedApp>,
    /// Write the detailed log (debug.log) for tracking down problems.
    #[serde(default)]
    pub detailed_log: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port:       18228,
            // The lists a new install starts with, grouped by kind; the rest are there to turn on
            lists:      serde_json::from_str(include_str!("default_lists.json")).expect("default_lists.json is valid"),
            exclusions: vec![],
            userscripts: vec![],
            excluded_apps: vec![],
            detailed_log: false,
        }
    }
}

/// `%LOCALAPPDATA%\CoffeeFilter`, created on first use. Per-user ACL'd by Windows.
pub fn data_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA not set"))?;
    let dir = PathBuf::from(base).join("CoffeeFilter");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Write a file so readers only ever see the old or the new contents, never half of
/// it: write a temp file next to it, then rename over (atomic on NTFS).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // name.ext.tmp, so two files with the same stem (engine.dat, engine.key) never share a temp file
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);

    // On disk before the swap: NTFS journals the rename, not the data, so a power cut right
    // after could otherwise leave an empty file in place of the old one
    let mut file = fs::File::create(&temp)?;
    io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)
}

impl Config {
    /// Load settings. A file that can't be read as settings is kept as `config.json.bad`
    /// and defaults are used, with a message for the app instead of silently losing it.
    pub fn load() -> (Self, Option<String>) {
        let Ok(path) = data_dir().map(|d| d.join("config.json")) else { return (Self::default(), None) };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => return (Self::default(), Some(format!("Couldn't read your settings ({e}). Using defaults."))),
        };
        match serde_json::from_str(&text) {
            Ok(config) => (config, None),
            Err(_) => {
                let _ = fs::rename(&path, path.with_extension("json.bad"));
                (Self::default(), Some("Your settings file was damaged. It was saved as config.json.bad and defaults are in use.".into()))
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(&data_dir()?.join("config.json"), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_without_kind_default_to_custom() {
        let config: Config = serde_json::from_str(
            r#"{"port":1,"exclusions":[],"lists":[{"name":"A","url":"https://a","enabled":true}]}"#,
        )
        .unwrap();
        assert_eq!(config.lists[0].kind, "custom");
    }

    #[test]
    fn atomic_write_replaces_whole_file() {
        let path = std::env::temp_dir().join(format!("cf-atomic-{}.json", std::process::id()));
        write_atomic(&path, b"first version, longer").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("tmp").exists());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn default_lists_are_https_and_unique() {
        let lists = Config::default().lists;
        assert!(lists.iter().any(|l| l.enabled), "a new install blocks something");
        for list in &lists {
            assert!(list.url.starts_with("https://"), "{} isn't https", list.name);
            assert_eq!(lists.iter().filter(|l| l.name == list.name || l.url == list.url).count(), 1, "{} is listed twice", list.name);
        }
    }

    #[test]
    fn config_roundtrips() {
        let config = Config::default();
        let back: Config = serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(back.port, config.port);
        assert_eq!(back.lists.len(), config.lists.len());
    }
}
