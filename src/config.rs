use crate::keybindings::KeybindingsConfig;
use crate::services::remote::RemoteProfile;
use crate::ui::theme::{Theme, DEFAULT_THEME_NAME};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::utils::format::strip_unc_prefix;

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
                    "read -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a && [ -n \"$a\" ] && [ \"$a\" != \"y\" ]".to_string(),
                    "/bin/bash -c \"$(cat '{{FILEPATH}}')\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                ],
            );
            extension_handler.insert(
                "py".to_string(),
                vec![
                    "read -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a && [ -n \"$a\" ] && [ \"$a\" != \"y\" ]".to_string(),
                    "python \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                    "python3 \"{{FILEPATH}}\" && echo 'Press any key to return...' && read -n 1 -s".to_string(),
                ],
            );
            extension_handler.insert(
                "js".to_string(),
                vec![
                    "read -p 'Run \"{{FILEPATH}}\"? (Y/n) ' a && [ -n \"$a\" ] && [ \"$a\" != \"y\" ]".to_string(),
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
    pub fn ensure_config_exists() {
        // Create ~/.cokacdir/
        if let Some(config_dir) = Self::config_dir() {
            if !config_dir.exists() {
                if fs::create_dir_all(&config_dir).is_ok() {
                    // Set directory permissions to user-only on Unix
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = fs::Permissions::from_mode(0o700);
                        let _ = fs::set_permissions(&config_dir, perms);
                    }
                }
            }
        }

        // Create ~/.cokacdir/themes/
        if let Some(themes_dir) = Self::themes_dir() {
            if !themes_dir.exists() {
                let _ = fs::create_dir_all(&themes_dir);
            }

            // Create default light.json if not exists
            let light_theme_path = themes_dir.join("light.json");
            if !light_theme_path.exists() {
                let _ = fs::write(&light_theme_path, Theme::light().to_json());
            }

            // Create default dark.json if not exists
            let dark_theme_path = themes_dir.join("dark.json");
            if !dark_theme_path.exists() {
                let _ = fs::write(&dark_theme_path, Theme::dark().to_json());
            }

            // Create default "dawn_of_coding.json" if not exists
            let dawn_theme_path = themes_dir.join("dawn_of_coding.json");
            if !dawn_theme_path.exists() {
                let _ = fs::write(&dawn_theme_path, Theme::dawn_of_coding().to_json());
            }
        }

        // Create default settings.json if not exists
        if let Some(config_path) = Self::config_path() {
            if !config_path.exists() {
                let default_settings = Self::default();
                let _ = default_settings.save();
            }
        }
    }

    /// Loads settings from the config file, returns default if not found or invalid
    pub fn load() -> Self {
        Self::load_with_error().unwrap_or_default()
    }

    /// Loads settings from the config file with error information
    /// Returns Ok(settings) on success, Err(error_message) on failure
    pub fn load_with_error() -> Result<Self, String> {
        // Ensure config directories and files exist
        Self::ensure_config_exists();

        let config_path =
            Self::config_path().ok_or_else(|| "Could not determine config path".to_string())?;

        let content = fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read settings file: {}", e))?;

        serde_json::from_str(&content).map_err(|e| format!("Invalid JSON in settings.json: {}", e))
    }

    /// Saves settings to the config file using atomic write pattern
    pub fn save(&self) -> io::Result<()> {
        let Some(config_dir) = Self::config_dir() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine config directory",
            ));
        };

        // Create config directory if it doesn't exist
        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)?;
            // Set directory permissions to user-only on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o700);
                let _ = fs::set_permissions(&config_dir, perms);
            }
        }

        let config_path = config_dir.join("settings.json");
        let temp_path = config_dir.join("settings.json.tmp");
        let content = serde_json::to_string_pretty(self)?;

        // Atomic write: write to temp file first, then rename
        fs::write(&temp_path, &content)?;
        fs::rename(&temp_path, &config_path)?;

        Ok(())
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
    }

    #[test]
    fn test_ensure_config_exists() {
        Settings::ensure_config_exists();
        if let Some(themes_dir) = Settings::themes_dir() {
            assert!(themes_dir.exists(), "themes directory should exist");
            let light_theme = themes_dir.join("light.json");
            assert!(light_theme.exists(), "light.json should exist");
        }
    }

    #[test]
    fn test_theme_to_json() {
        let json = Theme::light().to_json();
        assert!(json.contains("\"name\": \"light\""));
        assert!(json.contains("\"palette\""));
        assert!(json.contains("\"panel\""));
    }
}
