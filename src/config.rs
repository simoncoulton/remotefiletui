//! Saved connection profiles. Passwords are never written to disk.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

use crate::{backend::Protocol, theme::ThemeSpec};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// Path to a private key (SFTP only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// Directory to open after connecting; if it can't be opened the session
    /// stays in the server's default directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Folder this connection is filed under (one level deep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// One of [`COLORS`]; shown as a dot in the list and on the connected pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Lower-case labels used for filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// S3: region, pinned bucket and AWS profile (the endpoint is `host`, the
    /// access key id is `username`; the secret key is never stored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
}

/// Connection colours, mapped onto the active theme's palette when drawn.
pub const COLORS: [&str; 7] = ["red", "green", "yellow", "blue", "purple", "cyan", "gray"];

/// Normalise a user-typed tag list ("prod, EU db") into unique lower-case tags.
pub fn clean_tags(input: &str) -> Result<Vec<String>, String> {
    let mut tags: Vec<String> = Vec::new();
    for raw in input.split(|c: char| c == ',' || c.is_whitespace()) {
        let t = raw.trim().trim_start_matches('#').to_lowercase();
        if t.is_empty() {
            continue;
        }
        if t.chars().count() > 24 {
            return Err(format!("tag '{t}' is too long (24 characters max)"));
        }
        if !t
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "tag '{t}': use letters, digits, '-', '_' or '.' only"
            ));
        }
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    if tags.len() > 10 {
        return Err("at most 10 tags per connection".into());
    }
    Ok(tags)
}

/// A list filter: every selected tag must be a tag of the connection, and every
/// word of the text must appear in its name, host, user, folder or bucket.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filter {
    pub tags: Vec<String>,
    pub words: Vec<String>,
}

impl Filter {
    pub fn new<'a>(text: &str, tags: impl IntoIterator<Item = &'a String>) -> Self {
        Filter {
            tags: tags.into_iter().map(|t| t.to_lowercase()).collect(),
            words: text.split_whitespace().map(str::to_lowercase).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.words.is_empty()
    }

    pub fn matches(&self, p: &Profile) -> bool {
        if !self.tags.iter().all(|t| p.tags.contains(t)) {
            return false;
        }
        let haystack = [
            Some(p.name.as_str()),
            Some(p.host.as_str()),
            Some(p.username.as_str()),
            p.folder.as_deref(),
            p.bucket.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
        self.words.iter().all(|w| haystack.contains(w))
    }
}

/// Where connection details left empty are looked up. Only 1Password so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordManager {
    OnePassword,
}

impl PasswordManager {
    /// The managers the app supports, in picker order.
    pub const ALL: [PasswordManager; 1] = [PasswordManager::OnePassword];

    /// The value used in `config.yml` (`passwordmanager: onepassword`).
    pub fn id(self) -> &'static str {
        match self {
            PasswordManager::OnePassword => "onepassword",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PasswordManager::OnePassword => "1Password",
        }
    }
}

/// Everything the app keeps on disk, loaded from `config.yml` (settings) and
/// `profiles.yml` (saved connections and folders). The structs below are the
/// files; this is the in-memory view of both.
#[derive(Debug)]
pub struct Config {
    /// Theme name (see `theme`); `None` means `default`.
    pub theme: Option<String>,
    /// The password manager details are looked up in; `None` means none is used.
    pub password_manager: Option<PasswordManager>,
    /// Only search this 1Password vault (default: all vaults `op` can see).
    pub onepassword_vault: Option<String>,
    /// Don't paint the theme's background; let the terminal's show through.
    pub transparent: bool,
    /// The user's own `themes:` from `config.yml`.
    pub themes: BTreeMap<String, ThemeSpec>,
    /// Folder names, so empty folders persist.
    pub folders: Vec<String>,
    pub profiles: Vec<Profile>,
    /// Where the files live; `None` when the system has no config directory.
    dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_parts(Settings::default(), ProfilesFile::default(), None)
    }
}

/// `config.yml`. Unknown keys are errors, so a typo is never silently ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub theme: String,
    /// Paint nothing behind the text, so the terminal's own background shows.
    pub transparent_background: bool,
    /// `passwordmanager: onepassword`. Setting it is what enables lookups.
    pub passwordmanager: Option<PasswordManager>,
    /// The pre-`passwordmanager` switch; still read (`true` = 1Password).
    pub onepassword: Option<bool>,
    pub onepassword_vault: Option<String>,
    pub themes: BTreeMap<String, ThemeSpec>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "default".into(),
            transparent_background: true,
            passwordmanager: None,
            onepassword: None,
            onepassword_vault: None,
            themes: BTreeMap::new(),
        }
    }
}

/// `profiles.yml`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ProfilesFile {
    folders: Vec<String>,
    profiles: Vec<Profile>,
}

/// The pre-YAML `profiles.toml`.
#[derive(Debug, Default, Deserialize)]
struct OldTomlFile {
    theme: Option<String>,
    #[serde(default)]
    onepassword: bool,
    onepassword_vault: Option<String>,
    #[serde(default)]
    transparent: bool,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default, rename = "profile")]
    profiles: Vec<Profile>,
}

const APP_DIR: &str = "remotefiletui";
const OLD_APP_DIR: &str = "ftptui";
pub const SETTINGS_FILE: &str = "config.yml";
pub const PROFILES_FILE: &str = "profiles.yml";
const LEGACY_FILE: &str = "profiles.toml";

/// `config.yml`'s first-run content: every setting commented, with the
/// defaults filled in.
fn settings_template(s: &Settings) -> String {
    let vault = match &s.onepassword_vault {
        Some(v) => format!("onepassword_vault: {}", yaml_scalar(v)),
        None => "# onepassword_vault: Servers".to_string(),
    };
    format!(
        "\
# remotefiletui configuration. Saved connections are in profiles.yml.
# Everything here is optional; delete a line to get its default.

# Colour theme: default, tokyo-night, catppuccin-frappe, catppuccin-macchiato,
# catppuccin-mocha, catppuccin-latte, dracula, one-dark, gruvbox-dark, nord,
# solarized-dark, rose-pine, monokai, github-dark, everforest, kanagawa, or the
# name of a theme in the themes/ folder next to this file or under `themes:`
# below. Everything except `default` uses 24-bit colour (256 colours are used
# when the terminal doesn't advertise it). Press `t` in the app to preview and
# pick a theme; the choice is saved here.
theme: {theme}

# Paint the theme's own background (false), or leave the terminal's showing
# through (true) so its transparency and blur are kept. Press `t` then `b` in
# the app to toggle; the choice is saved here. `default` has no background of
# its own.
transparent_background: {transparent}

# Password manager. When set, connection details you leave empty (username,
# password, host, key file, ...) are looked up in an item named after the
# connection, and the Host is not required. Supported: onepassword (needs the
# `op` CLI). Ctrl-P picks one in the app; without one, passwords are typed
# each time and never saved.
{manager}
# 1Password only: search just this vault (default: every vault `op` can see).
{vault}

# Your own themes. Every colour is optional and inherits from `base` (one of
# the built-in themes, default: `default`). Colours are #rrggbb, an ANSI name
# (red, darkgray, reset, ...) or 0-255. The same can be done with one file per
# theme in the themes/ folder (the file name is the theme's name).
# themes:
#   my-theme:
#     base: dracula
#     accent: \"#ff8800\"
",
        theme = yaml_scalar(&s.theme),
        transparent = s.transparent_background,
        manager = match s.passwordmanager {
            Some(m) => format!("passwordmanager: {}", m.id()),
            None => "# passwordmanager: onepassword".to_string(),
        },
    )
}

/// `$XDG_CONFIG_HOME/remotefiletui`, else `~/.config/remotefiletui`. The same
/// layout on every platform (macOS included), except Windows' own config dir.
fn config_dir_from(xdg: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    if cfg!(windows) {
        return ProjectDirs::from("", "", APP_DIR).map(|d| d.config_dir().to_path_buf());
    }
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join(APP_DIR))
}

pub fn config_dir() -> Option<PathBuf> {
    let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    config_dir_from(std::env::var_os("XDG_CONFIG_HOME"), home.as_deref())
}

/// Config files written by earlier versions, newest first: the macOS
/// "Application Support" locations (including the pre-rename `ftptui` name) and
/// the old `ftptui` directory under the config base.
fn legacy_files(new: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let support = home
        .filter(|_| cfg!(target_os = "macos"))
        .map(|h| h.join("Library").join("Application Support"));
    if let Some(s) = &support {
        v.push(s.join(APP_DIR).join(LEGACY_FILE));
    }
    if let Some(base) = new.parent().and_then(Path::parent) {
        v.push(base.join(OLD_APP_DIR).join(LEGACY_FILE));
    }
    if let Some(s) = &support {
        v.push(s.join(OLD_APP_DIR).join(LEGACY_FILE));
    }
    v
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // e.g. across filesystems: copy, then remove the original only on success.
        Err(_) => {
            fs::copy(from, to)?;
            fs::remove_file(from)
        }
    }
}

/// Move earlier-version configs to `new`. Does nothing if `new` already exists.
/// The newest legacy file becomes `new`; any older ones are kept next to it as
/// `profiles.toml.old-N` (nothing is deleted), and emptied legacy directories are
/// removed. Returns the files that were moved.
pub fn migrate(new: &Path, legacy: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    if new.exists() {
        return Ok(Vec::new());
    }
    let found: Vec<&PathBuf> = legacy.iter().filter(|p| p.is_file()).collect();
    let Some((first, rest)) = found.split_first() else {
        return Ok(Vec::new());
    };
    if let Some(dir) = new.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut moved = vec![(*first).clone()];
    move_file(first, new)?;
    for (n, old) in rest.iter().enumerate() {
        let backup = new.with_file_name(format!("{LEGACY_FILE}.old-{}", n + 1));
        move_file(old, &backup)?;
        moved.push((*old).clone());
    }
    // `remove_dir` only succeeds on empty directories, so this can't remove data.
    for old in &moved {
        if let Some(dir) = old.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(moved)
}

/// Parse YAML where a file of only comments is the defaults.
fn parse_yaml<T: serde::de::DeserializeOwned + Default>(
    text: &str,
) -> Result<T, serde_yaml_ng::Error> {
    let blank = text
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
    if blank {
        return Ok(T::default());
    }
    serde_yaml_ng::from_str(text)
}

/// Write via a temp file + rename so a crash can't leave a half-written file.
/// Follows a symlinked file (e.g. managed by stow) instead of replacing the link.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .context("config path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

/// Quote a string as a YAML scalar if it needs it.
pub fn yaml_scalar(s: &str) -> String {
    serde_yaml_ng::to_string(s)
        .map(|t| t.trim_end().to_string())
        .unwrap_or_else(|_| format!("{s:?}"))
}

fn replace_key(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}:");
    let mut replaced = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && line.starts_with(&prefix) {
                replaced = true;
                // Keep a trailing comment (and the spacing before it), if any.
                let rest = &line[prefix.len()..];
                match rest.find(" #") {
                    Some(i) => format!("{key}: {value}{}", &rest[rest[..i].trim_end().len()..]),
                    None => format!("{key}: {value}"),
                }
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        // A top-level key at column 0 is valid after any preceding block.
        out.push(format!("{key}: {value}"));
    }
    out.join("\n") + "\n"
}

/// Set one top-level `key: value` in `config.yml`, keeping every other line and
/// comment. `value` is raw YAML. A file that already fails to parse is reported
/// as such and left alone, and the edit must still parse before it is written.
pub fn set_setting(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => settings_template(&Settings::default()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    parse_yaml::<Settings>(&existing).with_context(|| {
        format!(
            "{} has an error, so it was not changed (fix it first)",
            path.display()
        )
    })?;
    let updated = replace_key(&existing, key, value);
    parse_yaml::<Settings>(&updated)
        .with_context(|| format!("refusing to write invalid {key}: {value}"))?;
    write_atomic(path, &updated)
}

/// Remove a top-level `key:` line from `config.yml` (a no-op if it isn't there).
fn remove_setting(path: &Path, key: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    parse_yaml::<Settings>(&existing).with_context(|| {
        format!(
            "{} has an error, so it was not changed (fix it first)",
            path.display()
        )
    })?;
    let prefix = format!("{key}:");
    let kept: Vec<&str> = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .collect();
    if kept.len() == existing.lines().count() {
        return Ok(());
    }
    let updated = kept.join("\n") + "\n";
    parse_yaml::<Settings>(&updated)
        .with_context(|| format!("refusing to remove {key}: result would be invalid"))?;
    write_atomic(path, &updated)
}

/// Turn the old `profiles.toml` into `profiles.yml` (and `config.yml`, unless
/// one exists). The TOML file is renamed, never deleted.
fn convert_toml(dir: &Path) -> Result<()> {
    let old = dir.join(LEGACY_FILE);
    let text =
        fs::read_to_string(&old).with_context(|| format!("cannot read {}", old.display()))?;
    let parsed: OldTomlFile =
        toml::from_str(&text).with_context(|| format!("invalid config file {}", old.display()))?;

    let file = ProfilesFile {
        folders: parsed.folders,
        profiles: parsed.profiles,
    };
    write_atomic(&dir.join(PROFILES_FILE), &profiles_text(&file)?)?;

    let settings_path = dir.join(SETTINGS_FILE);
    if !settings_path.exists() {
        // Theme ids were renamed when themes became files, as in torrentui.
        let theme = match parsed.theme.as_deref() {
            None | Some("terminal") => "default".to_string(),
            Some("tokyo-night-storm") => "tokyo-night".to_string(),
            Some(t) => t.to_string(),
        };
        let settings = Settings {
            theme,
            transparent_background: parsed.transparent,
            passwordmanager: parsed.onepassword.then_some(PasswordManager::OnePassword),
            onepassword: None,
            onepassword_vault: parsed.onepassword_vault,
            themes: BTreeMap::new(),
        };
        write_atomic(&settings_path, &settings_template(&settings))?;
    }

    let mut done = dir.join(format!("{LEGACY_FILE}.migrated"));
    let mut n = 1;
    while done.exists() {
        n += 1;
        done = dir.join(format!("{LEGACY_FILE}.migrated-{n}"));
    }
    fs::rename(&old, &done).with_context(|| format!("cannot rename {}", old.display()))?;
    Ok(())
}

fn profiles_text(file: &ProfilesFile) -> Result<String> {
    let body = serde_yaml_ng::to_string(file).context("cannot serialise saved connections")?;
    Ok(format!(
        "# remotefiletui saved connections. Passwords are never stored.\n\
         # Edit freely while the app is closed; the app rewrites this file when you save.\n{body}"
    ))
}

impl Config {
    fn from_parts(settings: Settings, profiles: ProfilesFile, dir: Option<PathBuf>) -> Self {
        Config {
            theme: Some(settings.theme),
            password_manager: settings.passwordmanager.or_else(|| {
                (settings.onepassword == Some(true)).then_some(PasswordManager::OnePassword)
            }),
            onepassword_vault: settings.onepassword_vault,
            transparent: settings.transparent_background,
            themes: settings.themes,
            folders: profiles.folders,
            profiles: profiles.profiles,
            dir,
        }
    }

    /// Load from the default location, moving older files into place first.
    pub fn load() -> Result<Self> {
        let Some(dir) = config_dir() else {
            return Ok(Self::default());
        };
        if !dir.join(PROFILES_FILE).exists() {
            let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
            let legacy_toml = dir.join(LEGACY_FILE);
            let legacy = legacy_files(&legacy_toml, home.as_deref());
            // Couldn't move (read-only home?): the old file is simply not found
            // here, and nothing is lost.
            let _ = migrate(&legacy_toml, &legacy);
        }
        Self::load_dir(&dir)
    }

    /// Load `config.yml` and `profiles.yml` from `dir`. Missing files are the
    /// defaults, and a commented `config.yml` is written on first run.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        if !dir.join(PROFILES_FILE).exists() && dir.join(LEGACY_FILE).is_file() {
            convert_toml(dir)?;
        }
        let settings_path = dir.join(SETTINGS_FILE);
        let settings: Settings = match fs::read_to_string(&settings_path) {
            Ok(text) => parse_yaml(&text)
                .with_context(|| format!("invalid config {}", settings_path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Best effort: not being able to write the template isn't fatal.
                let _ = write_atomic(&settings_path, &settings_template(&Settings::default()));
                Settings::default()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", settings_path.display()));
            }
        };
        let profiles_path = dir.join(PROFILES_FILE);
        let profiles: ProfilesFile = match fs::read_to_string(&profiles_path) {
            Ok(text) => parse_yaml(&text)
                .with_context(|| format!("invalid connections file {}", profiles_path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => ProfilesFile::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", profiles_path.display()));
            }
        };
        Ok(Self::from_parts(
            settings,
            profiles,
            Some(dir.to_path_buf()),
        ))
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Write `profiles.yml`.
    pub fn save(&self) -> Result<()> {
        let dir = self
            .dir
            .as_deref()
            .context("no config directory available on this system")?;
        self.save_to(&dir.join(PROFILES_FILE))
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let file = ProfilesFile {
            folders: self.folders.clone(),
            profiles: self.profiles.clone(),
        };
        write_atomic(path, &profiles_text(&file)?)
    }

    /// Choose (or, with `None`, stop using) a password manager, in place. The
    /// old `onepassword:` switch is removed so it can't contradict the choice.
    pub fn save_password_manager(&self, manager: Option<PasswordManager>) -> Result<()> {
        let dir = self
            .dir
            .as_deref()
            .context("no config directory available on this system")?;
        let path = dir.join(SETTINGS_FILE);
        remove_setting(&path, "onepassword")?;
        match manager {
            Some(m) => set_setting(&path, "passwordmanager", m.id()),
            None => remove_setting(&path, "passwordmanager"),
        }
    }

    /// Change one setting in `config.yml`, in place (see [`set_setting`]).
    pub fn save_setting(&self, key: &str, value: &str) -> Result<()> {
        let dir = self
            .dir
            .as_deref()
            .context("no config directory available on this system")?;
        set_setting(&dir.join(SETTINGS_FILE), key, value)
    }

    /// Insert or replace (matched by name), keeping the list sorted by name.
    /// Replacing keeps the existing entry's folder; a new entry uses the folder
    /// it was built with.
    pub fn upsert(&mut self, mut profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(existing) => {
                // Saving the form keeps how the entry was organised.
                profile.folder = existing.folder.take();
                profile.color = existing.color.take();
                profile.tags = std::mem::take(&mut existing.tags);
                *existing = profile;
            }
            None => {
                if let Some(f) = &profile.folder {
                    self.ensure_folder(f);
                }
                self.profiles.push(profile);
            }
        }
        self.profiles.sort_by_key(|p| p.name.to_lowercase());
    }

    pub fn set_color(&mut self, name: &str, color: Option<&str>) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.name == name) {
            p.color = color.filter(|c| COLORS.contains(c)).map(str::to_string);
        }
    }

    pub fn set_tags(&mut self, name: &str, tags: Vec<String>) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.name == name) {
            p.tags = tags;
        }
    }

    /// Every tag in use with how many connections carry it, sorted by name.
    pub fn all_tags(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for t in self.profiles.iter().flat_map(|p| &p.tags) {
            *counts.entry(t.clone()).or_default() += 1;
        }
        counts.into_iter().collect()
    }

    pub fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
    }

    /// Rename a connection. Fails if the name is empty or already used.
    pub fn rename_profile(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        let new = Self::clean_profile_name(new)?;
        if new != old && self.profiles.iter().any(|p| p.name == new) {
            return Err("a connection with that name already exists");
        }
        if let Some(p) = self.profiles.iter_mut().find(|p| p.name == old) {
            p.name = new;
        }
        self.profiles.sort_by_key(|p| p.name.to_lowercase());
        Ok(())
    }

    /// Validate a connection name typed by the user.
    pub fn clean_profile_name(name: &str) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() {
            return Err("name cannot be empty");
        }
        if name.chars().count() > 80 {
            return Err("name is too long (80 characters max)");
        }
        if name.chars().any(char::is_control) {
            return Err("name cannot contain control characters");
        }
        Ok(name.to_string())
    }

    pub fn has_profile(&self, name: &str) -> bool {
        self.profiles.iter().any(|p| p.name == name)
    }

    // ───────── folders ─────────
    //
    // A folder is identified by its path: `Work` or `Work/Prod` (nested to any
    // depth). Names cannot contain `/`, so splitting on it is unambiguous.

    /// Validate and normalise one folder name (a single path segment).
    pub fn clean_folder_name(name: &str) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() {
            return Err("folder name cannot be empty");
        }
        if name.chars().count() > 40 {
            return Err("folder name is too long (40 characters max)");
        }
        if name.chars().any(char::is_control) {
            return Err("folder name cannot contain control characters");
        }
        if name.contains('/') {
            return Err("folder name cannot contain '/'");
        }
        Ok(name.to_string())
    }

    /// The folder containing `path`, or `None` at the top level.
    pub fn folder_parent(path: &str) -> Option<&str> {
        path.rsplit_once('/').map(|(parent, _)| parent)
    }

    /// The last segment of a folder path.
    pub fn folder_leaf(path: &str) -> &str {
        path.rsplit_once('/').map_or(path, |(_, leaf)| leaf)
    }

    pub fn folder_depth(path: &str) -> usize {
        path.matches('/').count()
    }

    /// Whether `path` is `ancestor` or lies inside it.
    pub fn folder_within(path: &str, ancestor: &str) -> bool {
        path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    fn has_folder(&self, path: &str) -> bool {
        self.folder_names()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(path))
    }

    /// Record a folder and (implicitly, via [`Config::folder_names`]) its parents.
    fn ensure_folder(&mut self, path: &str) {
        if !self.folders.iter().any(|f| f == path) {
            self.folders.push(path.to_string());
            self.sort_folders();
        }
    }

    fn sort_folders(&mut self) {
        self.folders.sort_by_key(|f| f.to_lowercase());
    }

    /// Every folder path, parents included even if only a child was recorded,
    /// sorted case-insensitively (a parent always precedes its children).
    pub fn folder_names(&self) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        let mut add = |path: &str| {
            let mut cur = String::new();
            for seg in path.split('/') {
                if !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(seg);
                if !all.iter().any(|a| a.eq_ignore_ascii_case(&cur)) {
                    all.push(cur.clone());
                }
            }
        };
        for f in &self.folders {
            add(f);
        }
        for p in &self.profiles {
            if let Some(f) = &p.folder {
                add(f);
            }
        }
        all.sort_by_key(|f| f.to_lowercase());
        all
    }

    /// Create `name` at the top level.
    #[cfg(test)]
    pub fn add_folder(&mut self, name: &str) -> Result<String, &'static str> {
        self.add_folder_in(None, name)
    }

    /// Create `name` inside `parent` (the top level for `None`); returns its path.
    pub fn add_folder_in(
        &mut self,
        parent: Option<&str>,
        name: &str,
    ) -> Result<String, &'static str> {
        let name = Self::clean_folder_name(name)?;
        let path = match parent {
            Some(p) => format!("{p}/{name}"),
            None => name,
        };
        if self.has_folder(&path) {
            return Err("a folder with that name already exists there");
        }
        self.folders.push(path.clone());
        self.sort_folders();
        Ok(path)
    }

    /// Rewrite every folder path `from` (and everything inside it) to live at
    /// `to`; `to` of `None` means "lift it out", handled by the callers.
    fn remap_folders(&mut self, from: &str, to: Option<&str>) {
        let map = |path: &str| -> Option<String> {
            if !Self::folder_within(path, from) {
                return Some(path.to_string());
            }
            let rest = &path[from.len()..];
            to.map(|t| format!("{t}{rest}"))
        };
        let mut folders: Vec<String> = Vec::new();
        for f in &self.folders {
            if let Some(m) = map(f)
                && !folders.contains(&m)
            {
                folders.push(m);
            }
        }
        self.folders = folders;
        for p in &mut self.profiles {
            if let Some(f) = &p.folder {
                p.folder = map(f);
            }
        }
        self.sort_folders();
    }

    /// Rename the folder at `old` (only its last segment changes).
    pub fn rename_folder(&mut self, old: &str, new: &str) -> Result<String, &'static str> {
        let leaf = Self::clean_folder_name(new)?;
        let path = match Self::folder_parent(old) {
            Some(parent) => format!("{parent}/{leaf}"),
            None => leaf,
        };
        if path != old && self.has_folder(&path) {
            return Err("a folder with that name already exists there");
        }
        self.remap_folders(old, Some(&path));
        Ok(path)
    }

    /// Move the folder at `path` (with everything inside) into `dest`
    /// (`None` = the top level). Returns its new path.
    pub fn move_folder(&mut self, path: &str, dest: Option<&str>) -> Result<String, &'static str> {
        if let Some(d) = dest
            && Self::folder_within(d, path)
        {
            return Err("a folder cannot be moved into itself");
        }
        let leaf = Self::folder_leaf(path);
        let new = match dest {
            Some(d) => format!("{d}/{leaf}"),
            None => leaf.to_string(),
        };
        if new != path && self.has_folder(&new) {
            return Err("that folder already has one with the same name");
        }
        self.remap_folders(path, Some(&new));
        Ok(new)
    }

    /// Remove a folder; what is inside it (connections and folders) moves up to
    /// its parent. Nothing is deleted.
    pub fn remove_folder(&mut self, path: &str) {
        let parent = Self::folder_parent(path).map(str::to_string);
        let map = |p: &str| -> Option<String> {
            if p == path {
                return parent.clone();
            }
            let rest = p.strip_prefix(path)?.strip_prefix('/')?;
            Some(match &parent {
                Some(par) => format!("{par}/{rest}"),
                None => rest.to_string(),
            })
        };
        let mut folders: Vec<String> = Vec::new();
        for f in &self.folders {
            let m = if f == path {
                None
            } else {
                Some(map(f).unwrap_or_else(|| f.clone()))
            };
            if let Some(m) = m
                && !folders.contains(&m)
            {
                folders.push(m);
            }
        }
        self.folders = folders;
        for p in &mut self.profiles {
            if let Some(f) = &p.folder
                && Self::folder_within(f, path)
            {
                p.folder = map(f);
            }
        }
        self.sort_folders();
    }

    pub fn set_folder(&mut self, profile: &str, folder: Option<&str>) {
        if let Some(f) = folder {
            self.ensure_folder(f);
        }
        if let Some(p) = self.profiles.iter_mut().find(|p| p.name == profile) {
            p.folder = folder.map(str::to_string);
        }
    }

    /// The visible rows of the saved-connections tree: each folder (sorted) with
    /// its sub-folders and connections unless collapsed, then top-level connections.
    pub fn tree(&self, collapsed: &HashSet<String>, filter: &Filter) -> Vec<TreeRow> {
        let filtering = !filter.is_empty();
        let all = self.folder_names();
        let mut rows = Vec::new();
        self.push_level(None, 0, &all, collapsed, filter, filtering, &mut rows);
        for (index, p) in self.profiles.iter().enumerate() {
            if p.folder.is_none() && filter.matches(p) {
                rows.push(TreeRow::Profile { index, depth: 0 });
            }
        }
        rows
    }

    /// Connections in `folder` or any folder below it that pass the filter.
    fn matching_in(&self, folder: &str, filter: &Filter) -> usize {
        self.profiles
            .iter()
            .filter(|p| {
                p.folder
                    .as_deref()
                    .is_some_and(|f| Self::folder_within(f, folder))
                    && filter.matches(p)
            })
            .count()
    }

    #[allow(clippy::too_many_arguments)]
    fn push_level(
        &self,
        parent: Option<&str>,
        depth: usize,
        all: &[String],
        collapsed: &HashSet<String>,
        filter: &Filter,
        filtering: bool,
        rows: &mut Vec<TreeRow>,
    ) {
        let children = all.iter().filter(|f| Self::folder_parent(f) == parent);
        for folder in children {
            let count = self.matching_in(folder, filter);
            // While filtering, only folders with matches are shown, expanded.
            if filtering && count == 0 {
                continue;
            }
            let is_collapsed = !filtering && collapsed.contains(folder);
            rows.push(TreeRow::Folder {
                name: folder.clone(),
                depth,
                count,
                collapsed: is_collapsed,
            });
            if is_collapsed {
                continue;
            }
            self.push_level(
                Some(folder),
                depth + 1,
                all,
                collapsed,
                filter,
                filtering,
                rows,
            );
            for (index, p) in self.profiles.iter().enumerate() {
                if p.folder.as_deref() == Some(folder.as_str()) && filter.matches(p) {
                    rows.push(TreeRow::Profile {
                        index,
                        depth: depth + 1,
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Folder {
        /// The folder's full path (`Work/Prod`).
        name: String,
        depth: usize,
        /// Connections inside it, sub-folders included.
        count: usize,
        collapsed: bool,
    },
    Profile {
        index: usize,
        depth: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.into(),
            protocol: Protocol::Sftp,
            host: "example.com".into(),
            port: 22,
            username: "me".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn roundtrip_and_upsert() {
        let dir = std::env::temp_dir().join(format!("remotefiletui-test-{}", std::process::id()));
        let path = dir.join("profiles.yml");
        let mut cfg = Config::default();
        cfg.upsert(profile("b"));
        cfg.upsert(profile("a"));
        let mut changed = profile("a");
        changed.port = 2222;
        cfg.upsert(changed);
        cfg.save_to(&path).unwrap();

        let loaded = Config::load_dir(&dir).unwrap();
        assert_eq!(loaded.profiles.len(), 2);
        assert_eq!(loaded.profiles[0].name, "a");
        assert_eq!(loaded.profiles[0].port, 2222);
        assert!(!fs::read_to_string(&path).unwrap().contains("password"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = scratch("empty");
        let cfg = Config::load_dir(&dir).unwrap();
        assert!(cfg.profiles.is_empty() && cfg.folders.is_empty());
        assert_eq!(cfg.theme.as_deref(), Some("default"));
        assert!(
            cfg.transparent,
            "transparent is the default, as in torrentui"
        );
        assert!(
            dir.join("config.yml").is_file(),
            "a commented config is written"
        );
        assert!(
            !dir.join("profiles.yml").exists(),
            "no profiles file until you save"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folders_tree_and_collapse() {
        let mut cfg = Config::default();
        cfg.add_folder("Work").unwrap();
        cfg.add_folder("alpha").unwrap();
        cfg.upsert(profile("zed"));
        cfg.upsert(profile("amy"));
        cfg.upsert(profile("bob"));
        cfg.set_folder("bob", Some("Work"));
        cfg.set_folder("zed", Some("Work"));

        let none = HashSet::new();
        let rows = cfg.tree(&none, &Filter::default());
        // folders sorted case-insensitively, then their members, then top level
        let desc: Vec<String> = rows
            .iter()
            .map(|r| match r {
                TreeRow::Folder { name, count, .. } => format!("F:{name}:{count}"),
                TreeRow::Profile { index, depth } => {
                    format!("P:{}:{}", cfg.profiles[*index].name, *depth > 0)
                }
            })
            .collect();
        assert_eq!(
            desc,
            [
                "F:alpha:0",
                "F:Work:2",
                "P:bob:true",
                "P:zed:true",
                "P:amy:false"
            ]
        );

        let collapsed: HashSet<String> = ["Work".to_string()].into();
        assert_eq!(cfg.tree(&collapsed, &Filter::default()).len(), 3); // alpha, Work (collapsed), amy
    }

    #[test]
    fn folder_rules() {
        let mut cfg = Config::default();
        assert!(cfg.add_folder("  ").is_err());
        assert!(cfg.add_folder("bad\u{1b}name").is_err());
        assert!(cfg.add_folder(&"x".repeat(41)).is_err());
        cfg.add_folder("Prod").unwrap();
        assert!(
            cfg.add_folder("prod").is_err(),
            "case-insensitive duplicate"
        );
        cfg.add_folder("Stage").unwrap();
        assert!(cfg.rename_folder("Stage", "PROD").is_err());
        cfg.upsert(profile("a"));
        cfg.set_folder("a", Some("Prod"));
        assert_eq!(
            cfg.rename_folder("Prod", "Production").unwrap(),
            "Production"
        );
        assert_eq!(cfg.profiles[0].folder.as_deref(), Some("Production"));
        // deleting a folder keeps its connections
        cfg.remove_folder("Production");
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.profiles[0].folder, None);
        assert!(!cfg.folder_names().contains(&"Production".to_string()));
    }

    #[test]
    fn upsert_keeps_folder_and_rename_profile() {
        let mut cfg = Config::default();
        cfg.upsert(profile("a"));
        cfg.set_folder("a", Some("F"));
        let mut again = profile("a");
        again.port = 2200;
        cfg.upsert(again);
        assert_eq!(cfg.profiles[0].port, 2200);
        assert_eq!(cfg.profiles[0].folder.as_deref(), Some("F"));

        cfg.upsert(profile("b"));
        assert!(cfg.rename_profile("a", "b").is_err());
        assert!(cfg.rename_profile("a", " ").is_err());
        cfg.rename_profile("a", "prod box").unwrap();
        assert!(cfg.profiles.iter().any(|p| p.name == "prod box"));
    }

    #[test]
    fn folders_roundtrip_with_empty_folder() {
        let dir =
            std::env::temp_dir().join(format!("remotefiletui-folders-{}", std::process::id()));
        let path = dir.join("profiles.yml");
        let mut cfg = Config::default();
        cfg.add_folder("Empty").unwrap();
        cfg.upsert(profile("a"));
        cfg.set_folder("a", Some("Work"));
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_dir(&dir).unwrap();
        assert_eq!(loaded.folder_names(), ["Empty", "Work"]);
        assert_eq!(loaded.profiles[0].folder.as_deref(), Some("Work"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tag_cleaning() {
        assert_eq!(
            clean_tags("Prod, EU #db  prod").unwrap(),
            ["prod", "eu", "db"]
        );
        assert_eq!(clean_tags("  ,, ").unwrap(), Vec::<String>::new());
        assert!(clean_tags("bad tag!").is_err());
        assert!(clean_tags("a/b").is_err());
        assert!(clean_tags(&"x".repeat(25)).is_err());
        assert!(clean_tags("a b c d e f g h i j k").is_err());
        assert_eq!(
            clean_tags("v1.2 my_tag a-b").unwrap(),
            ["v1.2", "my_tag", "a-b"]
        );
    }

    #[test]
    fn filter_parsing_and_matching() {
        let mut a = profile("sftp://alice@alpha");
        a.tags = vec!["prod".into(), "eu".into()];
        a.host = "alpha.example.com".into();
        let mut b = profile("sftp://bob@beta");
        b.tags = vec!["prod".into()];
        let c = profile("sftp://carol@gamma");

        let tags = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let f = Filter::new("", &tags(&["prod"]));
        assert!(f.matches(&a) && f.matches(&b) && !f.matches(&c));
        let f = Filter::new("", &tags(&["PROD", "eu"]));
        assert!(
            f.matches(&a) && !f.matches(&b),
            "tags are ANDed, case-insensitive"
        );
        let f = Filter::new("alpha", []);
        assert!(f.matches(&a) && !f.matches(&b), "words match host/name");
        let f = Filter::new("bob", &tags(&["prod"]));
        assert!(!f.matches(&a) && f.matches(&b));
        assert!(Filter::new("", []).is_empty() && Filter::new("  ", []).is_empty());
        // '#' has no meaning: it is just a character to look for.
        assert!(!Filter::new("#prod", []).matches(&a));
        assert!(Filter::default().matches(&c));
    }

    #[test]
    fn filtered_tree_hides_empty_folders_and_ignores_collapse() {
        let mut cfg = Config::default();
        cfg.add_folder("Work").unwrap();
        cfg.add_folder("Home").unwrap();
        cfg.upsert(profile("a"));
        cfg.upsert(profile("b"));
        cfg.upsert(profile("c"));
        cfg.set_folder("a", Some("Work"));
        cfg.set_folder("b", Some("Home"));
        cfg.set_tags("a", vec!["x".into()]);
        cfg.set_tags("c", vec!["x".into()]);
        let collapsed: HashSet<String> = ["Work".to_string()].into();
        let rows = cfg.tree(&collapsed, &Filter::new("", &["x".to_string()]));
        // Work stays (has a match) and is shown expanded; Home disappears; c is top level.
        assert_eq!(rows.len(), 3);
        assert!(
            matches!(&rows[0], TreeRow::Folder { name, collapsed: false, count: 1, .. } if name == "Work")
        );
        assert!(matches!(rows[1], TreeRow::Profile { depth: 1, .. }));
        assert!(matches!(rows[2], TreeRow::Profile { depth: 0, .. }));
        assert!(
            cfg.tree(&collapsed, &Filter::new("", &["nothing".to_string()]))
                .is_empty()
        );
    }

    #[test]
    fn colors_tags_survive_upsert_and_roundtrip() {
        let mut cfg = Config::default();
        cfg.upsert(profile("a"));
        cfg.set_color("a", Some("red"));
        cfg.set_color("a", Some("not-a-color"));
        assert_eq!(cfg.profiles[0].color, None, "unknown colours are rejected");
        cfg.set_color("a", Some("red"));
        cfg.set_tags("a", vec!["prod".into(), "eu".into()]);
        let mut again = profile("a");
        again.port = 2200;
        cfg.upsert(again);
        assert_eq!(cfg.profiles[0].color.as_deref(), Some("red"));
        assert_eq!(cfg.profiles[0].tags, ["prod", "eu"]);
        cfg.upsert(profile("b"));
        cfg.set_tags("b", vec!["prod".into()]);
        assert_eq!(
            cfg.all_tags(),
            [("eu".to_string(), 1), ("prod".to_string(), 2)]
        );

        let dir = std::env::temp_dir().join(format!("ftptui-tags-{}", std::process::id()));
        let path = dir.join("profiles.yml");
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_dir(&dir).unwrap();
        assert_eq!(loaded.profiles[0].color.as_deref(), Some("red"));
        assert_eq!(loaded.profiles[0].tags, ["prod", "eu"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn s3_profile_roundtrip_has_no_secret_field() {
        let p = Profile {
            name: "s3://bucket".into(),
            protocol: Protocol::S3,
            host: "http://127.0.0.1:5000".into(),
            port: 443,
            username: "AKIAEXAMPLE".into(),
            region: Some("eu-west-1".into()),
            bucket: Some("bucket".into()),
            aws_profile: None,
            ..Profile::default()
        };
        let mut cfg = Config::default();
        cfg.upsert(p);
        let file = ProfilesFile {
            folders: cfg.folders.clone(),
            profiles: cfg.profiles.clone(),
        };
        let text = profiles_text(&file).unwrap();
        assert!(text.contains("protocol: s3") && text.contains("bucket: bucket"));
        assert!(!text.contains("secret") && !text.contains("password"));
        let back: ProfilesFile = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.profiles[0].region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn profile_name_rules() {
        assert_eq!(
            Config::clean_profile_name("  Prod SFTP ").unwrap(),
            "Prod SFTP"
        );
        assert!(Config::clean_profile_name("   ").is_err());
        assert!(Config::clean_profile_name("a\u{1b}b").is_err());
        assert!(Config::clean_profile_name(&"x".repeat(81)).is_err());
        assert!(Config::clean_profile_name(&"x".repeat(80)).is_ok());
        let mut cfg = Config::default();
        cfg.upsert(profile("one"));
        cfg.upsert(profile("two"));
        assert!(cfg.has_profile("one") && !cfg.has_profile("three"));
        assert!(cfg.rename_profile("one", "two").is_err());
        assert!(cfg.rename_profile("one", &"y".repeat(81)).is_err());
        cfg.rename_profile("one", "  My Server ").unwrap();
        assert!(cfg.has_profile("My Server"));
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ftptui-mig-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn config_dir_prefers_xdg_then_dot_config() {
        if cfg!(windows) {
            return;
        }
        let home = Path::new("/home/u");
        assert_eq!(
            config_dir_from(None, Some(home)),
            Some(PathBuf::from("/home/u/.config/remotefiletui"))
        );
        assert_eq!(
            config_dir_from(Some("/x/cfg".into()), Some(home)),
            Some(PathBuf::from("/x/cfg/remotefiletui"))
        );
        // A relative XDG_CONFIG_HOME is ignored, per the spec.
        assert_eq!(
            config_dir_from(Some("rel".into()), Some(home)),
            Some(PathBuf::from("/home/u/.config/remotefiletui"))
        );
        assert_eq!(config_dir_from(None, None), None);
    }

    #[test]
    fn migrates_newest_and_keeps_older_as_backups() {
        let root = scratch("both");
        let new = root.join(".config/remotefiletui/profiles.toml");
        let newer = root.join("Support/remotefiletui/profiles.toml");
        let older = root.join("Support/ftptui/profiles.toml");
        write(&newer, "newer");
        write(&older, "older");

        let moved = migrate(&new, &[newer.clone(), older.clone()]).unwrap();
        assert_eq!(moved.len(), 2);
        assert_eq!(fs::read_to_string(&new).unwrap(), "newer");
        let backup = root.join(".config/remotefiletui/profiles.toml.old-1");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "older");
        assert!(!newer.exists() && !older.exists());
        // Emptied legacy directories are gone.
        assert!(!root.join("Support/remotefiletui").exists());
        assert!(!root.join("Support/ftptui").exists());
        // Idempotent: a second run changes nothing.
        assert!(migrate(&new, &[newer, older]).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrate_never_overwrites_or_removes_other_files() {
        let root = scratch("safe");
        let new = root.join(".config/remotefiletui/profiles.toml");
        let old = root.join("Support/ftptui/profiles.toml");
        write(&old, "old");
        write(&root.join("Support/ftptui/unrelated.txt"), "keep me");
        // An existing new config wins and the legacy file is left alone.
        write(&new, "current");
        assert!(
            migrate(&new, std::slice::from_ref(&old))
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&new).unwrap(), "current");
        assert!(old.exists());
        // When migrating, a directory holding other files is not removed.
        fs::remove_file(&new).unwrap();
        let moved = migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(fs::read_to_string(&new).unwrap(), "old");
        assert!(root.join("Support/ftptui/unrelated.txt").exists());
        // Nothing to migrate is not an error and creates nothing.
        let none = root.join("elsewhere/remotefiletui/profiles.toml");
        assert!(
            migrate(&none, &[root.join("missing.toml")])
                .unwrap()
                .is_empty()
        );
        assert!(!none.parent().unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_file_order_is_newest_first() {
        let new = Path::new("/h/.config/remotefiletui/profiles.toml");
        let v = legacy_files(new, Some(Path::new("/h")));
        let s: Vec<String> = v.iter().map(|p| p.display().to_string()).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(
                s,
                [
                    "/h/Library/Application Support/remotefiletui/profiles.toml",
                    "/h/.config/ftptui/profiles.toml",
                    "/h/Library/Application Support/ftptui/profiles.toml",
                ]
            );
        } else {
            assert_eq!(s, ["/h/.config/ftptui/profiles.toml"]);
        }
    }

    #[test]
    fn toml_is_converted_to_yaml_and_kept() {
        let dir = scratch("convert");
        write(
            &dir.join("profiles.toml"),
            "theme = \"tokyo-night-storm\"\ntransparent = true\nonepassword = true\nfolders = [\"Work\"]\n\n\
             [[profile]]\nname = \"a\"\nprotocol = \"sftp\"\nhost = \"h\"\nport = 22\nfolder = \"Work\"\ntags = [\"x\"]\n",
        );
        let cfg = Config::load_dir(&dir).unwrap();
        assert_eq!(cfg.profiles[0].name, "a");
        assert_eq!(cfg.profiles[0].tags, ["x"]);
        assert_eq!(cfg.folders, ["Work"]);
        assert_eq!(cfg.theme.as_deref(), Some("tokyo-night"));
        assert!(cfg.transparent && cfg.password_manager == Some(PasswordManager::OnePassword));
        assert!(dir.join("profiles.yml").is_file() && dir.join("config.yml").is_file());
        assert!(!dir.join("profiles.toml").exists());
        assert!(
            dir.join("profiles.toml.migrated").is_file(),
            "the original is kept"
        );
        // Loading again reads the YAML and converts nothing.
        let again = Config::load_dir(&dir).unwrap();
        assert_eq!(again.profiles.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_config_yml_is_not_overwritten_by_conversion() {
        let dir = scratch("keepcfg");
        write(&dir.join("config.yml"), "theme: nord\n");
        write(&dir.join("profiles.toml"), "theme = \"dracula\"\n");
        let cfg = Config::load_dir(&dir).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
        assert_eq!(
            fs::read_to_string(dir.join("config.yml")).unwrap(),
            "theme: nord\n"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_edits_keep_comments_and_other_lines() {
        let dir = scratch("settings");
        let path = dir.join("config.yml");
        write(
            &path,
            "# mine\ntheme: default   # pick one\nonepassword: false\nthemes:\n  a:\n    base: nord\n",
        );
        set_setting(&path, "theme", "dracula").unwrap();
        set_setting(&path, "transparent_background", "false").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mine\ntheme: dracula   # pick one\nonepassword: false\nthemes:\n  a:\n    base: nord\ntransparent_background: false\n"
        );
        // `themes:` is not mistaken for `theme:`.
        let cfg = Config::load_dir(&dir).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("dracula"));
        assert!(!cfg.transparent && cfg.themes.contains_key("a"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_or_unknown_settings_are_refused_and_untouched() {
        let dir = scratch("broken");
        let path = dir.join("config.yml");
        write(&path, "theme: [unclosed\n");
        assert!(set_setting(&path, "theme", "nord").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "theme: [unclosed\n");
        assert!(Config::load_dir(&dir).is_err());
        write(&path, "thme: nord\n");
        let e = format!("{:#}", Config::load_dir(&dir).unwrap_err());
        assert!(e.contains("config.yml") && e.contains("thme"), "{e}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_settings_file_is_created_by_the_first_edit() {
        let dir = scratch("firstedit");
        let path = dir.join("config.yml");
        set_setting(&path, "theme", "nord").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# remotefiletui configuration") && text.contains("theme: nord"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_template_parses_to_the_defaults() {
        let s: Settings = parse_yaml(&settings_template(&Settings::default())).unwrap();
        let d = Settings::default();
        assert_eq!(s.theme, d.theme);
        assert_eq!(s.transparent_background, d.transparent_background);
        assert!(s.passwordmanager.is_none() && s.themes.is_empty());
    }

    fn nested() -> Config {
        let mut cfg = Config::default();
        let work = cfg.add_folder("Work").unwrap();
        let prod = cfg.add_folder_in(Some(&work), "Prod").unwrap();
        cfg.add_folder_in(Some(&prod), "EU").unwrap();
        cfg.add_folder_in(Some(&work), "Dev").unwrap();
        for n in ["a", "b", "c", "d"] {
            cfg.upsert(profile(n));
        }
        cfg.set_folder("a", Some("Work"));
        cfg.set_folder("b", Some("Work/Prod"));
        cfg.set_folder("c", Some("Work/Prod/EU"));
        cfg
    }

    fn shape(cfg: &Config, collapsed: &HashSet<String>, f: &Filter) -> Vec<String> {
        cfg.tree(collapsed, f)
            .iter()
            .map(|r| match r {
                TreeRow::Folder {
                    name, depth, count, ..
                } => {
                    format!(
                        "{}F:{}:{count}",
                        " ".repeat(*depth),
                        Config::folder_leaf(name)
                    )
                }
                TreeRow::Profile { index, depth } => {
                    format!("{}P:{}", " ".repeat(*depth), cfg.profiles[*index].name)
                }
            })
            .collect()
    }

    #[test]
    fn folders_nest_to_any_depth() {
        let cfg = nested();
        assert_eq!(
            shape(&cfg, &HashSet::new(), &Filter::default()),
            [
                "F:Work:3",
                " F:Dev:0",
                " F:Prod:2",
                "  F:EU:1",
                "   P:c",
                "  P:b",
                " P:a",
                "P:d"
            ]
        );
        // Collapsing a folder hides everything inside it.
        let collapsed: HashSet<String> = ["Work/Prod".to_string()].into();
        let rows = shape(&cfg, &collapsed, &Filter::default());
        assert!(rows.contains(&" F:Prod:2".to_string()) && !rows.iter().any(|r| r.contains("EU")));
        // Filtering keeps the path down to a match, expanded, and drops the rest.
        let mut cfg = cfg;
        cfg.set_tags("c", vec!["x".into()]);
        let f = Filter::new("", &["x".to_string()]);
        assert_eq!(
            shape(&cfg, &collapsed, &f),
            ["F:Work:1", " F:Prod:1", "  F:EU:1", "   P:c"]
        );
    }

    #[test]
    fn folder_names_cannot_contain_a_slash_and_clash_per_parent() {
        let mut cfg = nested();
        assert!(cfg.add_folder("a/b").is_err());
        assert!(
            cfg.add_folder_in(Some("Work"), "prod").is_err(),
            "case-insensitive"
        );
        assert!(
            cfg.add_folder_in(Some("Work/Dev"), "Prod").is_ok(),
            "same name elsewhere"
        );
        assert!(cfg.add_folder("Prod").is_ok(), "and at the top level");
    }

    #[test]
    fn renaming_a_folder_moves_everything_inside() {
        let mut cfg = nested();
        let new = cfg.rename_folder("Work/Prod", "Live").unwrap();
        assert_eq!(new, "Work/Live");
        assert_eq!(
            cfg.profiles
                .iter()
                .find(|p| p.name == "b")
                .unwrap()
                .folder
                .as_deref(),
            Some("Work/Live")
        );
        assert_eq!(
            cfg.profiles
                .iter()
                .find(|p| p.name == "c")
                .unwrap()
                .folder
                .as_deref(),
            Some("Work/Live/EU")
        );
        assert!(cfg.folder_names().contains(&"Work/Live/EU".to_string()));
        assert!(!cfg.folder_names().iter().any(|f| f.contains("Prod")));
        assert!(cfg.rename_folder("Work/Live", "Dev").is_err());
    }

    #[test]
    fn moving_a_folder_keeps_its_contents_and_refuses_cycles() {
        let mut cfg = nested();
        assert!(cfg.move_folder("Work", Some("Work/Prod/EU")).is_err());
        assert!(cfg.move_folder("Work", Some("Work")).is_err());
        let new = cfg.move_folder("Work/Prod", None).unwrap();
        assert_eq!(new, "Prod");
        assert_eq!(
            cfg.profiles
                .iter()
                .find(|p| p.name == "c")
                .unwrap()
                .folder
                .as_deref(),
            Some("Prod/EU")
        );
        let new = cfg.move_folder("Prod", Some("Work/Dev")).unwrap();
        assert_eq!(new, "Work/Dev/Prod");
        assert!(cfg.folder_names().contains(&"Work/Dev/Prod/EU".to_string()));
    }

    #[test]
    fn removing_a_folder_lifts_its_contents_to_the_parent() {
        let mut cfg = nested();
        cfg.remove_folder("Work/Prod");
        let folder = |n: &str| {
            cfg.profiles
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .folder
                .clone()
        };
        assert_eq!(folder("b").as_deref(), Some("Work"));
        assert_eq!(folder("c").as_deref(), Some("Work/EU"));
        assert_eq!(cfg.profiles.len(), 4, "nothing deleted");
        cfg.remove_folder("Work");
        assert_eq!(folder_of(&cfg, "a"), None);
        assert_eq!(folder_of(&cfg, "c").as_deref(), Some("EU"));
    }

    fn folder_of(cfg: &Config, n: &str) -> Option<String> {
        cfg.profiles
            .iter()
            .find(|p| p.name == n)
            .unwrap()
            .folder
            .clone()
    }

    #[test]
    fn nested_folders_roundtrip_through_yaml() {
        let dir = scratch("nestedyaml");
        let cfg = nested();
        cfg.save_to(&dir.join("profiles.yml")).unwrap();
        let back = Config::load_dir(&dir).unwrap();
        assert_eq!(back.folder_names(), cfg.folder_names());
        assert_eq!(folder_of(&back, "c").as_deref(), Some("Work/Prod/EU"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn password_manager_setting_roundtrips_and_reads_the_old_switch() {
        let dir = scratch("pm");
        let path = dir.join("config.yml");
        write(&path, "passwordmanager: onepassword\n");
        assert_eq!(
            Config::load_dir(&dir).unwrap().password_manager,
            Some(PasswordManager::OnePassword)
        );
        // The pre-passwordmanager switch still works.
        write(&path, "onepassword: true\n");
        assert_eq!(
            Config::load_dir(&dir).unwrap().password_manager,
            Some(PasswordManager::OnePassword)
        );
        write(&path, "onepassword: false\n");
        assert_eq!(Config::load_dir(&dir).unwrap().password_manager, None);
        // Unknown managers are errors, not silently ignored.
        write(&path, "passwordmanager: lastpass\n");
        assert!(Config::load_dir(&dir).is_err());

        // Choosing one edits in place and retires the old switch; choosing none removes the key.
        write(&path, "# mine\nonepassword: true   # old\ntheme: nord\n");
        let cfg = Config::load_dir(&dir).unwrap();
        cfg.save_password_manager(Some(PasswordManager::OnePassword))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\ntheme: nord\npasswordmanager: onepassword\n"
        );
        cfg.save_password_manager(None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\ntheme: nord\n");
        assert_eq!(Config::load_dir(&dir).unwrap().password_manager, None);
        fs::remove_dir_all(dir).unwrap();
    }
}
