//! Platform directories (decisions.md D14): XDG-style layout, following
//! the gh model of using `~/.config` on macOS too. Deliberately NOT
//! `~/.audible` — the Python audible-cli stores its own, incompatible
//! `config.toml` there.
//!
//! * Config + auth files: `$AUDIBLE_CONFIG_DIR`, else `%APPDATA%\audible`
//!   on Windows, else `$XDG_CONFIG_HOME/audible`, else `~/.config/audible`.
//! * Data (library DB, M2): `%LOCALAPPDATA%\audible` on Windows, else
//!   `$XDG_DATA_HOME/audible`, else `~/.local/share/audible`.

use std::ffi::OsString;
use std::path::PathBuf;

/// Name of the config file inside [`config_dir`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Directory holding `config.toml` and the auth files.
pub fn config_dir() -> PathBuf {
    config_dir_from(|key: &str| std::env::var_os(key), home_dir())
}

/// Directory for large, reproducible data (the library DB).
pub fn data_dir() -> PathBuf {
    data_dir_from(|key: &str| std::env::var_os(key), home_dir())
}

/// Full path of the config file.
pub fn config_file() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

fn home_dir() -> PathBuf {
    #[allow(deprecated)] // un-deprecated in Rust 1.85; remove once MSRV docs catch up
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn config_dir_from(env: impl Fn(&str) -> Option<OsString>, home: PathBuf) -> PathBuf {
    if let Some(dir) = env("AUDIBLE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(windows)
        && let Some(appdata) = env("APPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(appdata).join("audible");
    }
    match env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("audible"),
        None => home.join(".config").join("audible"),
    }
}

fn data_dir_from(env: impl Fn(&str) -> Option<OsString>, home: PathBuf) -> PathBuf {
    if cfg!(windows)
        && let Some(local) = env("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local).join("audible");
    }
    match env("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("audible"),
        None => home.join(".local").join("share").join("audible"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<OsString> {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn explicit_override_wins() {
        let env = env_with(&[
            ("AUDIBLE_CONFIG_DIR", "/custom"),
            ("XDG_CONFIG_HOME", "/xdg"),
        ]);
        assert_eq!(
            config_dir_from(env, PathBuf::from("/home/x")),
            PathBuf::from("/custom")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn xdg_config_home_is_respected() {
        let env = env_with(&[("XDG_CONFIG_HOME", "/xdg-config")]);
        assert_eq!(
            config_dir_from(env, PathBuf::from("/home/x")),
            PathBuf::from("/xdg-config/audible")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn falls_back_to_dot_config() {
        assert_eq!(
            config_dir_from(env_with(&[]), PathBuf::from("/home/x")),
            PathBuf::from("/home/x/.config/audible")
        );
        assert_eq!(
            data_dir_from(env_with(&[]), PathBuf::from("/home/x")),
            PathBuf::from("/home/x/.local/share/audible")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn empty_env_values_are_ignored() {
        let env = env_with(&[("AUDIBLE_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "")]);
        assert_eq!(
            config_dir_from(env, PathBuf::from("/home/x")),
            PathBuf::from("/home/x/.config/audible")
        );
    }
}
