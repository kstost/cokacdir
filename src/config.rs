use crate::keybindings::KeybindingsConfig;
#[cfg(test)]
use crate::services::file_ops::remove_file_by_identity;
use crate::services::file_ops::{
    open_directory_for_read, stable_file_identity, stable_path_identity, DirectoryAccess,
    DirectoryFileOptions, StablePathIdentity,
};
use crate::services::remote::RemoteProfile;
use crate::ui::theme::{Theme, DEFAULT_THEME_NAME};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::utils::format::strip_unc_prefix;

#[derive(Debug)]
pub enum SettingsLoadError {
    Initialize(io::Error),
    Read(io::Error),
    Parse(serde_json::Error),
}

impl SettingsLoadError {
    pub fn is_parse_error(&self) -> bool {
        matches!(self, Self::Parse(_))
    }
}

impl std::fmt::Display for SettingsLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initialize(error) => write!(formatter, "Failed to initialize settings: {error}"),
            Self::Read(error) => write!(formatter, "Failed to read settings file: {error}"),
            Self::Parse(error) => write!(formatter, "Invalid JSON in settings.json: {error}"),
        }
    }
}

impl std::error::Error for SettingsLoadError {}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// A real config directory kept open for the entire operation.
///
/// Child entries are always accessed relative to `access`, never by appending
/// to a descriptor pseudo-path. The separately held handle also pins the
/// directory name on Windows.
#[derive(Debug)]
struct PrivateDirectory {
    original_path: PathBuf,
    access: DirectoryAccess,
    handle: fs::File,
    identity: StablePathIdentity,
}

impl PrivateDirectory {
    fn open_or_create(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match create_private_directory(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        Self::open_paths(path.to_path_buf(), path)
    }

    fn open_paths(original_path: PathBuf, access_path: &Path) -> io::Result<Self> {
        let before = fs::symlink_metadata(access_path)?;
        if !before.file_type().is_dir()
            || before.file_type().is_symlink()
            || metadata_is_reparse_point(&before)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a real directory", original_path.display()),
            ));
        }
        let before_identity = stable_path_identity(access_path)?;
        let (handle, access, opened) = open_directory_for_read(access_path)?;
        let identity = stable_file_identity(&handle)?;
        if !opened.is_dir()
            || metadata_is_reparse_point(&opened)
            || identity != before_identity
            || stable_path_identity(access_path)? != identity
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} changed while being opened", original_path.display()),
            ));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            handle.set_permissions(fs::Permissions::from_mode(0o700))?;
        }

        let directory = Self {
            original_path,
            access,
            handle,
            identity,
        };
        directory.validate_current()?;
        Ok(directory)
    }

    fn open_or_create_child(&self, name: &str) -> io::Result<Self> {
        self.validate_current()?;
        let original_path = self.child_path(name)?;
        let component = original_path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Config entry has no filename")
        })?;
        match self.access.child_metadata(component) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match self.access.create_directory(component, 0o700) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        let (handle, access, opened) = self.access.open_directory(component)?;
        let identity = stable_file_identity(&handle)?;
        if !opened.is_dir()
            || metadata_is_reparse_point(&opened)
            || stable_path_identity(&original_path)? != identity
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} changed while being opened", original_path.display()),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            handle.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        let child = Self {
            original_path,
            access,
            handle,
            identity,
        };
        self.validate_current()?;
        child.validate_current()?;
        Ok(child)
    }

    fn child_path(&self, name: &str) -> io::Result<PathBuf> {
        let component = Path::new(name);
        if component.components().count() != 1
            || component.file_name().and_then(|value| value.to_str()) != Some(name)
            || name == "."
            || name == ".."
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Config entry name must be one normal UTF-8 path component",
            ));
        }
        Ok(self.original_path.join(component))
    }

    fn validate_current(&self) -> io::Result<()> {
        let held = self.handle.metadata()?;
        if !held.is_dir()
            || metadata_is_reparse_point(&held)
            || stable_file_identity(&self.handle)? != self.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Config directory handle changed: {}",
                    self.original_path.display()
                ),
            ));
        }
        match stable_path_identity(&self.original_path) {
            Ok(identity) if identity == self.identity => Ok(()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Config directory changed during operation: {}",
                    self.original_path.display()
                ),
            )),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "Config directory became unavailable during operation ({}): {error}",
                    self.original_path.display()
                ),
            )),
        }
    }

    #[cfg(unix)]
    fn sync(&self) -> io::Result<()> {
        self.handle.sync_all()
    }
}

fn open_private_regular_file_in(directory: &PrivateDirectory, name: &str) -> io::Result<fs::File> {
    directory.validate_current()?;
    let path = directory.child_path(name)?;
    let component = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Config entry has no filename")
    })?;
    let (file, metadata) = directory.access.open_regular_file(component)?;
    let identity = stable_file_identity(&file)?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a real regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    if stable_file_identity(&file)? != identity
        || directory.access.child_identity(component)? != identity
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} changed while its permissions were secured",
                path.display()
            ),
        ));
    }
    directory.validate_current()?;
    Ok(file)
}

fn cleanup_created_child(
    directory: &PrivateDirectory,
    name: &str,
    identity: StablePathIdentity,
    primary_error: io::Error,
) -> io::Error {
    let display_path = directory.original_path.join(name);
    match directory
        .access
        .remove_file_if_identity(std::ffi::OsStr::new(name), identity)
    {
        Ok(()) => primary_error,
        Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => primary_error,
        Err(cleanup_error) => io::Error::new(
            primary_error.kind(),
            format!(
                "{primary_error}; additionally could not safely clean up '{}': {cleanup_error}",
                display_path.display()
            ),
        ),
    }
}

#[cfg(test)]
fn cleanup_created_file(
    path: &Path,
    identity: StablePathIdentity,
    primary_error: io::Error,
) -> io::Error {
    match remove_file_by_identity(path, identity) {
        Ok(()) => primary_error,
        Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => primary_error,
        Err(cleanup_error) => io::Error::new(
            primary_error.kind(),
            format!(
                "{primary_error}; additionally could not safely clean up '{}': {cleanup_error}",
                path.display()
            ),
        ),
    }
}

fn write_new_private_file_in(
    directory: &PrivateDirectory,
    name: &str,
    contents: &[u8],
) -> io::Result<()> {
    directory.validate_current()?;
    let path = directory.child_path(name)?;
    let component = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Config entry has no filename")
    })?;
    let mut file = directory.access.open_file(
        component,
        DirectoryFileOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600),
    )?;
    let identity = stable_file_identity(&file)?;
    if directory.access.child_identity(component)? != identity {
        drop(file);
        return Err(cleanup_created_child(
            directory,
            name,
            identity,
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} changed immediately after creation", path.display()),
            ),
        ));
    }
    if let Err(error) = directory
        .validate_current()
        .and_then(|_| file.write_all(contents))
        .and_then(|_| file.sync_all())
        .and_then(|_| directory.validate_current())
    {
        drop(file);
        return Err(cleanup_created_child(directory, name, identity, error));
    }
    drop(file);
    #[cfg(unix)]
    directory.sync()?;
    match directory.validate_current() {
        Ok(()) => Ok(()),
        Err(error) => Err(cleanup_created_child(directory, name, identity, error)),
    }
}

fn ensure_private_regular_file_in(
    directory: &PrivateDirectory,
    name: &str,
    default_contents: &[u8],
) -> io::Result<()> {
    directory.validate_current()?;
    directory.child_path(name)?;
    match directory.access.child_metadata(std::ffi::OsStr::new(name)) {
        Ok(_) => {
            open_private_regular_file_in(directory, name)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match write_new_private_file_in(directory, name, default_contents) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    open_private_regular_file_in(directory, name)?;
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Panel-specific settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSettings {
    #[serde(default)]
    pub start_path: Option<String>,
    #[serde(default = "default_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
}

fn default_sort_by() -> String {
    "name".to_string()
}

fn default_sort_order() -> String {
    "asc".to_string()
}

fn default_diff_compare_method() -> String {
    "content".to_string()
}

/// Content-verification policy for moves that must copy across filesystems.
///
/// Standard mode keeps the transactional staging, metadata checks, and
/// durability syncs, but avoids rereading file contents solely to hash them.
/// Strict mode opts into the more expensive SHA-256 verification passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CrossVolumeMoveVerification {
    #[default]
    Standard,
    Strict,
}

fn default_encrypt_split_size() -> u64 {
    1800
}

fn default_telegram_polling_time() -> u64 {
    3000
}

impl Default for PanelSettings {
    fn default() -> Self {
        Self {
            start_path: None,
            sort_by: default_sort_by(),
            sort_order: default_sort_order(),
        }
    }
}

/// Theme settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeSettings {
    #[serde(default = "default_theme_name")]
    pub name: String,
}

fn default_theme_name() -> String {
    DEFAULT_THEME_NAME.to_string()
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            name: default_theme_name(),
        }
    }
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemeSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tar_path: Option<String>,
    /// Extension handlers: maps file extensions to command arrays
    /// Example: {"jpg": ["imageviewer {{FILEPATH}}", "imgviewer {{FILEPATH}}"]}
    /// Commands are tried in order until one succeeds (fallback)
    /// {{FILEPATH}} is replaced with the actual file path
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extension_handler: HashMap<String, Vec<String>>,
    /// Bookmarked paths for quick navigation
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarked_path: Vec<String>,
    /// Panel settings (multi-panel support)
    #[serde(default)]
    pub panels: Vec<PanelSettings>,
    /// Active panel index
    #[serde(default)]
    pub active_panel_index: usize,
    /// DIFF compare method: "content", "modified_time", "content_and_time"
    #[serde(default = "default_diff_compare_method")]
    pub diff_compare_method: String,
    /// Verification policy for cross-volume cut/paste operations.
    #[serde(default)]
    pub cross_volume_move_verification: CrossVolumeMoveVerification,
    /// Remote server profiles for SSH/SFTP connections
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_profiles: Vec<RemoteProfile>,
    /// Keybindings configuration
    #[serde(default)]
    pub keybindings: KeybindingsConfig,
    /// Encryption split size in MB (0 = no split)
    #[serde(default = "default_encrypt_split_size")]
    pub encrypt_split_size: u64,
    /// Telegram API polling interval in milliseconds (minimum 2500, default 3000)
    #[serde(default = "default_telegram_polling_time")]
    pub telegram_polling_time: u64,
}

impl Default for Settings {
    fn default() -> Self {
        let mut extension_handler = HashMap::new();
        // First element: confirmation prompt with filepath - 'y' or Enter runs, anything else exits
        // Subsequent elements: actual execution commands with fallback
        #[cfg(unix)]
        {
            extension_handler.insert(
                "sh".to_string(),
                vec![
                    "read -r -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a; case \"$a\" in ''|[yY]) exit 1;; *) exit 0;; esac".to_string(),
                    "/bin/bash \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                ],
            );
            extension_handler.insert(
                "py".to_string(),
                vec![
                    "read -r -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a; case \"$a\" in ''|[yY]) exit 1;; *) exit 0;; esac".to_string(),
                    "python \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                    "python3 \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                ],
            );
            extension_handler.insert(
                "js".to_string(),
                vec![
                    "read -r -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a; case \"$a\" in ''|[yY]) exit 1;; *) exit 0;; esac".to_string(),
                    "node \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                ],
            );
        }
        #[cfg(windows)]
        {
            extension_handler.insert(
                "bat".to_string(),
                vec![
                    "choice /c YN /n /m \"Run {{FILEPATH}}? [Y/N] \" & if errorlevel 2 exit /b 0 & exit /b 1".to_string(),
                    "cmd /c \"{{FILEPATH}}\" & pause >nul".to_string(),
                ],
            );
            extension_handler.insert(
                "py".to_string(),
                vec![
                    "choice /c YN /n /m \"Run {{FILEPATH}}? [Y/N] \" & if errorlevel 2 exit /b 0 & exit /b 1".to_string(),
                    "python \"{{FILEPATH}}\" & pause >nul".to_string(),
                ],
            );
            extension_handler.insert(
                "js".to_string(),
                vec![
                    "choice /c YN /n /m \"Run {{FILEPATH}}? [Y/N] \" & if errorlevel 2 exit /b 0 & exit /b 1".to_string(),
                    "node \"{{FILEPATH}}\" & pause >nul".to_string(),
                ],
            );
        }

        Self {
            theme: ThemeSettings::default(),
            tar_path: None,
            extension_handler,
            bookmarked_path: Vec::new(),
            panels: vec![PanelSettings::default(), PanelSettings::default()],
            active_panel_index: 0,
            diff_compare_method: default_diff_compare_method(),
            cross_volume_move_verification: CrossVolumeMoveVerification::default(),
            remote_profiles: Vec::new(),
            keybindings: KeybindingsConfig::default(),
            encrypt_split_size: default_encrypt_split_size(),
            telegram_polling_time: default_telegram_polling_time(),
        }
    }
}

impl Settings {
    /// Returns the config directory path (~/.cokacdir)
    pub fn config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".cokacdir"))
    }

    /// Returns the themes directory path (~/.cokacdir/themes)
    pub fn themes_dir() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("themes"))
    }

    /// Returns the config file path (~/.cokacdir/settings.json)
    pub fn config_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("settings.json"))
    }

    /// Ensures config directories and default files exist
    /// Called on app startup to initialize configuration
    pub fn ensure_config_exists() -> io::Result<()> {
        let config_dir = Self::config_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine config directory",
            )
        })?;
        Self::ensure_config_exists_in(&config_dir)
    }

    fn ensure_config_exists_in(config_dir: &Path) -> io::Result<()> {
        Self::initialize_config_in(config_dir).map(drop)
    }

    fn initialize_config_in(config_dir: &Path) -> io::Result<PrivateDirectory> {
        let config_directory = PrivateDirectory::open_or_create(config_dir)?;
        let themes_directory = config_directory.open_or_create_child("themes")?;
        ensure_private_regular_file_in(
            &themes_directory,
            "light.json",
            Theme::light().to_json().as_bytes(),
        )?;
        ensure_private_regular_file_in(
            &themes_directory,
            "dark.json",
            Theme::dark().to_json().as_bytes(),
        )?;
        ensure_private_regular_file_in(
            &themes_directory,
            "dawn_of_coding.json",
            Theme::dawn_of_coding().to_json().as_bytes(),
        )?;

        config_directory.validate_current()?;
        config_directory.child_path("settings.json")?;
        match config_directory
            .access
            .child_metadata(std::ffi::OsStr::new("settings.json"))
        {
            Ok(_) => {
                open_private_regular_file_in(&config_directory, "settings.json")?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Self::default().save_to_private_dir(&config_directory)?;
            }
            Err(error) => return Err(error),
        }
        themes_directory.validate_current()?;
        config_directory.validate_current()?;
        Ok(config_directory)
    }

    /// Loads settings from the config file, returns default if not found or invalid
    pub fn load() -> Self {
        Self::load_with_error().unwrap_or_default()
    }

    /// Loads settings from the config file with error information
    /// Returns Ok(settings) on success, Err(error_message) on failure
    pub fn load_with_error() -> Result<Self, SettingsLoadError> {
        let config_dir = Self::config_dir().ok_or_else(|| {
            SettingsLoadError::Initialize(io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine config directory",
            ))
        })?;
        // Keep the validated directory object pinned through initialization
        // and the subsequent settings read.
        let config_directory =
            Self::initialize_config_in(&config_dir).map_err(SettingsLoadError::Initialize)?;

        const MAX_SETTINGS_BYTES: u64 = 8 * 1024 * 1024;
        let mut file = open_private_regular_file_in(&config_directory, "settings.json")
            .map_err(SettingsLoadError::Read)?;
        let settings_identity = stable_file_identity(&file).map_err(SettingsLoadError::Read)?;
        if file.metadata().map_err(SettingsLoadError::Read)?.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsLoadError::Read(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings.json exceeds the 8 MiB size limit",
            )));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(SettingsLoadError::Read)?;
        config_directory
            .child_path("settings.json")
            .map_err(SettingsLoadError::Read)?;
        if stable_file_identity(&file).map_err(SettingsLoadError::Read)? != settings_identity
            || config_directory
                .access
                .child_identity(std::ffi::OsStr::new("settings.json"))
                .map_err(SettingsLoadError::Read)?
                != settings_identity
        {
            return Err(SettingsLoadError::Read(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings.json changed while it was being read",
            )));
        }
        config_directory
            .validate_current()
            .map_err(SettingsLoadError::Read)?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(SettingsLoadError::Read(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings.json exceeds the 8 MiB size limit",
            )));
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            SettingsLoadError::Read(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;

        serde_json::from_str(&content).map_err(SettingsLoadError::Parse)
    }

    /// Saves settings to the config file using atomic write pattern
    pub fn save(&self) -> io::Result<()> {
        let Some(config_dir) = Self::config_dir() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine config directory",
            ));
        };

        self.save_to_dir(&config_dir)
    }

    fn save_to_dir(&self, config_dir: &Path) -> io::Result<()> {
        let config_directory = PrivateDirectory::open_or_create(config_dir)?;
        self.save_to_private_dir(&config_directory)
    }

    fn save_to_private_dir(&self, config_directory: &PrivateDirectory) -> io::Result<()> {
        config_directory.validate_current()?;
        let config_path = config_directory.child_path("settings.json")?;
        let content = serde_json::to_string_pretty(self)?;

        let (temp_name, temp_path, mut temp_file, temp_identity) = (0..100)
            .find_map(|_| {
                let mut random = [0u8; 8];
                rand::thread_rng().fill_bytes(&mut random);
                let name = format!(".settings.json.{}.tmp", hex::encode(random));
                let path = match config_directory.child_path(&name) {
                    Ok(path) => path,
                    Err(error) => return Some(Err(error)),
                };
                match config_directory.access.open_file(
                    std::ffi::OsStr::new(&name),
                    DirectoryFileOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600),
                ) {
                    Ok(file) => match stable_file_identity(&file) {
                        Ok(identity) => Some(Ok((name, path, file, identity))),
                        Err(error) => Some(Err(error)),
                    },
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .transpose()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "No unique temp path"))?;

        let write_result = (|| -> io::Result<()> {
            if config_directory
                .access
                .child_identity(std::ffi::OsStr::new(&temp_name))?
                != temp_identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Temporary settings file changed immediately after creation: {}",
                        temp_path.display()
                    ),
                ));
            }
            config_directory.validate_current()?;
            temp_file.write_all(content.as_bytes())?;
            temp_file.sync_all()?;
            if stable_file_identity(&temp_file)? != temp_identity
                || config_directory
                    .access
                    .child_identity(std::ffi::OsStr::new(&temp_name))?
                    != temp_identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Temporary settings file changed before publication: {}",
                        temp_path.display()
                    ),
                ));
            }
            config_directory.validate_current()?;
            config_directory.access.rename_replace(
                std::ffi::OsStr::new(&temp_name),
                std::ffi::OsStr::new("settings.json"),
            )?;
            if stable_file_identity(&temp_file)? != temp_identity
                || config_directory
                    .access
                    .child_identity(std::ffi::OsStr::new("settings.json"))?
                    != temp_identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Published settings file identity could not be verified: {}",
                        config_path.display()
                    ),
                ));
            }
            config_directory.validate_current()?;
            #[cfg(unix)]
            {
                // Persist the directory entry as well as the file contents.
                config_directory.sync()?;
            }
            config_directory.validate_current()?;
            Ok(())
        })();
        match write_result {
            Ok(()) => Ok(()),
            Err(error) => {
                drop(temp_file);
                // If publication did not happen, remove only the exact temporary
                // object we created. A concurrently substituted path is preserved.
                Err(cleanup_created_child(
                    config_directory,
                    &temp_name,
                    temp_identity,
                    error,
                ))
            }
        }
    }

    /// Resolves a path setting to a valid directory
    /// Security: Only accepts absolute paths and canonicalizes to resolve symlinks
    pub fn resolve_path<F>(&self, path_opt: &Option<String>, fallback: F) -> PathBuf
    where
        F: FnOnce() -> PathBuf,
    {
        if let Some(path_str) = path_opt {
            let path = PathBuf::from(path_str);

            // Security: Reject relative paths to prevent path traversal
            if !path.is_absolute() {
                return fallback();
            }

            // Canonicalize to resolve symlinks and verify the path exists
            if let Ok(canonical) = path.canonicalize() {
                let canonical = strip_unc_prefix(canonical);
                if canonical.is_dir() {
                    return canonical;
                }
            }

            // If canonicalize fails, try parent directories
            let mut current = path;
            while let Some(parent) = current.parent() {
                if let Ok(canonical_parent) = parent.canonicalize() {
                    let canonical_parent = strip_unc_prefix(canonical_parent);
                    if canonical_parent.is_dir() {
                        return canonical_parent;
                    }
                }
                if parent == current {
                    break;
                }
                current = parent.to_path_buf();
            }
        }
        fallback()
    }

    /// Gets the extension handler for a given file extension (case-insensitive)
    /// Supports pipe-separated extensions: "jpg|jpeg|png"
    /// Returns None if no handler is defined for this extension
    pub fn get_extension_handler(&self, extension: &str) -> Option<&Vec<String>> {
        let ext_lower = extension.to_lowercase();
        // Try to find a matching handler (case-insensitive, supports pipe-separated extensions)
        for (key, value) in &self.extension_handler {
            // Split by pipe and check each extension
            for key_ext in key.split('|') {
                if key_ext.trim().to_lowercase() == ext_lower {
                    return Some(value);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert_eq!(settings.panels.len(), 2);
        assert_eq!(settings.panels[0].sort_by, "name");
        assert_eq!(settings.panels[0].sort_order, "asc");
        assert_eq!(settings.active_panel_index, 0);
        assert_eq!(settings.theme.name, DEFAULT_THEME_NAME);
        assert_eq!(
            settings.cross_volume_move_verification,
            CrossVolumeMoveVerification::Standard
        );
    }

    #[test]
    fn test_parse_partial_json() {
        let test_path = std::env::temp_dir().display().to_string();
        let json = format!(
            r#"{{"panels":[{{"start_path":"{}"}}]}}"#,
            test_path.replace('\\', "\\\\")
        );
        let settings: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(settings.panels[0].start_path, Some(test_path));
        assert_eq!(settings.panels[0].sort_by, "name");
        assert_eq!(
            settings.cross_volume_move_verification,
            CrossVolumeMoveVerification::Standard
        );
    }

    #[test]
    fn cross_volume_move_verification_round_trips() {
        let mut settings = Settings::default();
        settings.cross_volume_move_verification = CrossVolumeMoveVerification::Strict;

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""cross_volume_move_verification":"strict""#));

        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.cross_volume_move_verification,
            CrossVolumeMoveVerification::Strict
        );
    }

    #[test]
    fn test_ensure_config_exists() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");

        Settings::ensure_config_exists_in(&config_dir).unwrap();

        let themes_dir = config_dir.join("themes");
        assert!(themes_dir.exists(), "themes directory should exist");
        assert!(themes_dir.join("light.json").exists());
        assert!(themes_dir.join("dark.json").exists());
        assert!(themes_dir.join("dawn_of_coding.json").exists());
        assert!(config_dir.join("settings.json").exists());
    }

    #[test]
    fn test_theme_to_json() {
        let json = Theme::light().to_json();
        assert!(json.contains("\"name\": \"light\""));
        assert!(json.contains("\"palette\""));
        assert!(json.contains("\"panel\""));
    }

    #[cfg(unix)]
    #[test]
    fn default_shell_handler_executes_the_file_directly_and_accepts_uppercase_yes() {
        let settings = Settings::default();
        let handlers = settings.extension_handler.get("sh").unwrap();
        assert!(handlers[0].contains("''|[yY]"));
        assert_eq!(
            handlers[1],
            "/bin/bash \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s"
        );
        assert!(!handlers[1].contains("$(cat"));
    }

    #[test]
    fn save_to_dir_replaces_settings_and_leaves_valid_json() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.encrypt_split_size = 10;
        settings.save_to_dir(temp.path()).unwrap();
        settings.encrypt_split_size = 20;
        settings.save_to_dir(temp.path()).unwrap();

        let saved: Settings =
            serde_json::from_slice(&fs::read(temp.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(saved.encrypt_split_size, 20);
        assert!(fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn save_to_dir_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        Settings::default().save_to_dir(temp.path()).unwrap();

        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(temp.path().join("settings.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_to_dir_replaces_settings_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim.txt");
        let settings_path = temp.path().join("settings.json");
        fs::write(&victim, b"must survive").unwrap();
        symlink(&victim, &settings_path).unwrap();

        Settings::default().save_to_dir(temp.path()).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"must survive");
        assert!(!fs::symlink_metadata(&settings_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_dir_rejects_a_symlinked_config_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let config_link = temp.path().join("config-link");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&shared, &config_link).unwrap();

        let error = Settings::default().save_to_dir(&config_link).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!shared.join("settings.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_check_rejects_a_symlink_without_chmodding_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let themes_link = temp.path().join("themes");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&shared, &themes_link).unwrap();

        let error = PrivateDirectory::open_or_create(&themes_link).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_initialization_rejects_a_symlinked_themes_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let shared = temp.path().join("shared-themes");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&shared, config.join("themes")).unwrap();

        let error = Settings::ensure_config_exists_in(&config).unwrap_err();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotADirectory | io::ErrorKind::InvalidInput
        ));
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(fs::read_dir(&shared).unwrap().next().is_none());
        assert!(!config.join("settings.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn default_file_creation_does_not_follow_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("outside.json");
        let link = temp.path().join("light.json");
        symlink(&victim, &link).unwrap();
        let directory = PrivateDirectory::open_or_create(temp.path()).unwrap();

        let error = write_new_private_file_in(&directory, "light.json", b"theme").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(victim).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_config_directory_rejects_a_path_swap_before_writing() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let detached = temp.path().join("detached-config");
        let victim = temp.path().join("victim");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&victim).unwrap();
        let directory = PrivateDirectory::open_or_create(&config).unwrap();

        fs::rename(&config, &detached).unwrap();
        symlink(&victim, &config).unwrap();
        let error = Settings::default()
            .save_to_private_dir(&directory)
            .unwrap_err();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::NotFound
        ));
        assert!(!victim.join("settings.json").exists());
        assert!(!detached.join("settings.json").exists());
        assert!(fs::read_dir(&detached)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn identity_cleanup_preserves_a_replacement_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let temp_path = temp.path().join("settings.tmp");
        let moved_path = temp.path().join("original.tmp");
        fs::write(&temp_path, b"original").unwrap();
        let identity = stable_path_identity(&temp_path).unwrap();
        fs::rename(&temp_path, &moved_path).unwrap();
        fs::write(&temp_path, b"replacement").unwrap();

        let primary = io::Error::other("simulated publication failure");
        let error = cleanup_created_file(&temp_path, identity, primary);

        assert!(error.to_string().contains("could not safely clean up"));
        assert_eq!(fs::read(&temp_path).unwrap(), b"replacement");
        assert_eq!(fs::read(&moved_path).unwrap(), b"original");
    }
}
