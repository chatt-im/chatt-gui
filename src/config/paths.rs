use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetPlatform {
    Windows,
    MacOs,
    Unix,
}

pub(crate) fn config_path() -> Option<PathBuf> {
    config_path_with(TargetPlatform::current(), |name| std::env::var_os(name))
}

pub(crate) fn config_path_with(
    platform: TargetPlatform,
    mut env: impl FnMut(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = non_empty(env("CHATT_GUI_CONFIG")) {
        return Some(PathBuf::from(path));
    }

    if let Some(root) = non_empty(env("XDG_CONFIG_HOME")) {
        return Some(PathBuf::from(root).join("chatt").join("gui.toml"));
    }

    let root = match platform {
        TargetPlatform::Windows => non_empty(env("APPDATA")).map(PathBuf::from),
        TargetPlatform::MacOs => non_empty(env("HOME"))
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support")),
        TargetPlatform::Unix => non_empty(env("HOME"))
            .map(PathBuf::from)
            .map(|home| home.join(".config")),
    }?;
    Some(root.join("chatt").join("gui.toml"))
}

impl TargetPlatform {
    const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| value != OsStr::new(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve(platform: TargetPlatform, variables: &[(&str, &str)]) -> Option<PathBuf> {
        let variables: HashMap<&str, &str> = variables.iter().copied().collect();
        config_path_with(platform, |name| variables.get(name).map(OsString::from))
    }

    #[test]
    fn explicit_path_wins_and_can_be_relative() {
        assert_eq!(
            resolve(
                TargetPlatform::Unix,
                &[
                    ("CHATT_GUI_CONFIG", "portable/gui.toml"),
                    ("XDG_CONFIG_HOME", "/xdg"),
                    ("HOME", "/home/me"),
                ],
            ),
            Some(PathBuf::from("portable/gui.toml"))
        );
    }

    #[test]
    fn xdg_path_wins_on_every_platform() {
        for platform in [
            TargetPlatform::Windows,
            TargetPlatform::MacOs,
            TargetPlatform::Unix,
        ] {
            assert_eq!(
                resolve(
                    platform,
                    &[
                        ("CHATT_GUI_CONFIG", ""),
                        ("XDG_CONFIG_HOME", "/xdg"),
                        ("APPDATA", "C:\\Users\\me\\AppData\\Roaming"),
                        ("HOME", "/home/me"),
                    ],
                ),
                Some(PathBuf::from("/xdg/chatt/gui.toml"))
            );
        }
    }

    #[test]
    fn selects_each_platform_fallback() {
        assert_eq!(
            resolve(
                TargetPlatform::Windows,
                &[("APPDATA", "C:\\Users\\me\\AppData\\Roaming")],
            ),
            Some(
                PathBuf::from("C:\\Users\\me\\AppData\\Roaming")
                    .join("chatt")
                    .join("gui.toml")
            )
        );
        assert_eq!(
            resolve(TargetPlatform::MacOs, &[("HOME", "/Users/me")]),
            Some(PathBuf::from(
                "/Users/me/Library/Application Support/chatt/gui.toml"
            ))
        );
        assert_eq!(
            resolve(TargetPlatform::Unix, &[("HOME", "/home/me")]),
            Some(PathBuf::from("/home/me/.config/chatt/gui.toml"))
        );
    }

    #[test]
    fn empty_values_do_not_resolve_a_path() {
        assert_eq!(
            resolve(
                TargetPlatform::Unix,
                &[
                    ("CHATT_GUI_CONFIG", ""),
                    ("XDG_CONFIG_HOME", ""),
                    ("HOME", ""),
                ],
            ),
            None
        );
    }
}
