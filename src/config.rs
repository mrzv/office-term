use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pptx::FitMode;

/// Settings for the officeterm viewer.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    fit_mode: FitModeSetting,
    #[serde(alias = "invert")]
    dark_mode: bool,
    theme: Option<String>,
    theme_catalog: Option<String>,
    persistent_link_picker: bool,
    link_picker_split_percent: Option<u16>,
    link_picker_layout: LinkPickerLayout,
    forward_socket: Option<String>,
    pub max_file_size_mib: u32,
    pub viewer: ViewerSettings,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum FitModeSetting {
    #[default]
    Page,
    Width,
    Height,
    Content,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LinkPickerLayout {
    #[default]
    Auto,
    Vertical,
    Horizontal,
    Floating,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = config_path().ok_or("HOME or XDG_CONFIG_HOME must be set")?;
        Self::load_path(&path).map_err(|error| format!("{}: {error}", path.display()).into())
    }

    fn load_path(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(path.parent().ok_or("config path has no parent")?)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => file.write_all(include_bytes!("../config.default.toml"))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if config.max_file_size_mib == 0 {
            return Err("max_file_size_mib must be at least 1".into());
        }
        let viewer = &config.viewer;
        for (name, value, low, high) in [
            ("scroll_frame_ms", viewer.scroll_frame_ms, 1, 1000),
            ("scroll_ease_divisor", viewer.scroll_ease_divisor, 1, 100),
            ("scroll_step_percent", viewer.scroll_step_percent, 1, 100),
            ("page_scroll_percent", viewer.page_scroll_percent, 1, 100),
            ("prefetch_pages", viewer.prefetch_pages, 0, u32::MAX as u64),
        ] {
            if !(low..=high).contains(&value) {
                return Err(format!("viewer.{name} must be in {low}..={high}").into());
            }
        }
        let runtime = fs::canonicalize(path.parent().unwrap())?.join("run");
        if let Some(value) = config.forward_socket.as_mut()
            && !value.is_empty()
        {
            let configured = Path::new(value);
            let resolved = if configured.is_absolute() {
                configured.to_owned()
            } else {
                if configured.components().count() != 1
                    || !matches!(
                        configured.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                {
                    return Err("relative socket paths must be simple filenames".into());
                }
                runtime.join(configured)
            };
            *value = resolved
                .into_os_string()
                .into_string()
                .map_err(|_| "socket path must be UTF-8")?;
        }
        Ok(config)
    }

    /// Select a named session endpoint, or disable viewer control when no session is specified.
    pub fn select_session(&mut self, name: Option<&str>) -> io::Result<()> {
        let Some(name) = name else {
            if let Some(socket) = self.forward_socket.as_mut() {
                socket.clear();
            }
            return Ok(());
        };
        if name.is_empty()
            || name.len() > 24
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(io::Error::other(
                "session must contain 1..24 ASCII letters, digits, '-' or '_'",
            ));
        }
        if let Some(value) = self.forward_socket.as_mut()
            && !value.is_empty()
        {
            let path = Path::new(value);
            let file = path
                .file_name()
                .ok_or_else(|| io::Error::other("socket path has no filename"))?;
            let selected = path.with_file_name(format!("{name}-{}", file.to_string_lossy()));
            if selected.as_os_str().as_encoded_bytes().len() > 103 {
                return Err(io::Error::other(
                    "named session socket path exceeds 103 bytes",
                ));
            }
            crate::ipc::private_dir(
                selected
                    .parent()
                    .ok_or_else(|| io::Error::other("socket path has no parent"))?,
            )?;
            *value = selected
                .into_os_string()
                .into_string()
                .map_err(|_| io::Error::other("socket path is not UTF-8"))?;
        }
        Ok(())
    }

    pub fn fit_mode(&self) -> FitMode {
        match self.fit_mode {
            FitModeSetting::Page => FitMode::Page,
            FitModeSetting::Width => FitMode::Width,
            FitModeSetting::Height => FitMode::Height,
            FitModeSetting::Content => FitMode::Content,
        }
    }

    pub fn dark_mode(&self) -> bool {
        self.dark_mode
    }

    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref().filter(|value| !value.is_empty())
    }

    pub fn theme_catalog(&self) -> Option<&str> {
        self.theme_catalog
            .as_deref()
            .filter(|value| !value.is_empty())
    }

    pub fn persistent_link_picker(&self) -> bool {
        self.persistent_link_picker
    }

    pub fn link_picker_split_percent(&self) -> u16 {
        self.link_picker_split_percent.unwrap_or(50).clamp(20, 80)
    }

    pub fn link_picker_layout(&self) -> LinkPickerLayout {
        self.link_picker_layout
    }

    pub fn forward_socket(&self) -> Option<&str> {
        self.forward_socket
            .as_deref()
            .filter(|value| !value.is_empty())
    }
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

pub(crate) fn config_root() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base)
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    Some(config_root()?.join("officeterm"))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fit_mode: FitModeSetting::default(),
            dark_mode: false,
            theme: None,
            theme_catalog: None,
            persistent_link_picker: false,
            link_picker_split_percent: None,
            link_picker_layout: LinkPickerLayout::Auto,
            forward_socket: Some("forward.sock".into()),
            max_file_size_mib: 256,
            viewer: ViewerSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewerSettings {
    pub continuous_scroll: bool,
    pub prefetch_pages: u64,
    pub smooth_scroll: bool,
    pub scroll_frame_ms: u64,
    pub scroll_ease_divisor: u64,
    pub scroll_step_percent: u64,
    pub page_scroll_percent: u64,
    pub set_window_title: bool,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self {
            continuous_scroll: true,
            prefetch_pages: 5,
            smooth_scroll: false,
            scroll_frame_ms: 16,
            scroll_ease_divisor: 4,
            scroll_step_percent: 12,
            page_scroll_percent: 85,
            set_window_title: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, LinkPickerLayout};
    use crate::pptx::FitMode;
    use std::fs;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.fit_mode(), FitMode::Page);
        assert!(!config.dark_mode());
        assert_eq!(config.theme(), None);
        assert_eq!(config.theme_catalog(), None);
        assert!(!config.persistent_link_picker());
        assert_eq!(config.link_picker_split_percent(), 50);
        assert_eq!(config.link_picker_layout(), LinkPickerLayout::Auto);
    }

    #[test]
    fn parses_fit_mode_and_dark_mode() {
        let config: Config =
            toml::from_str("fit_mode = \"width\"\ndark_mode = true\n").expect("config");
        assert_eq!(config.fit_mode(), FitMode::Width);
        assert!(config.dark_mode());
    }

    #[test]
    fn accepts_legacy_invert_name() {
        let config: Config = toml::from_str("invert = true\n").expect("config");
        assert!(config.dark_mode());
    }

    #[test]
    fn parses_persistent_link_picker() {
        let config: Config = toml::from_str("persistent_link_picker = true\n").expect("config");
        assert!(config.persistent_link_picker());
    }

    #[test]
    fn parses_and_bounds_link_picker_split_percent() {
        let config: Config = toml::from_str("link_picker_split_percent = 65\n").expect("config");
        assert_eq!(config.link_picker_split_percent(), 65);

        let config: Config = toml::from_str("link_picker_split_percent = 100\n").expect("config");
        assert_eq!(config.link_picker_split_percent(), 80);
    }

    #[test]
    fn parses_link_picker_layouts() {
        for (value, expected) in [
            ("auto", LinkPickerLayout::Auto),
            ("vertical", LinkPickerLayout::Vertical),
            ("horizontal", LinkPickerLayout::Horizontal),
            ("floating", LinkPickerLayout::Floating),
        ] {
            let config: Config =
                toml::from_str(&format!("link_picker_layout = \"{value}\"\n")).expect("config");
            assert_eq!(config.link_picker_layout(), expected);
        }
    }

    #[test]
    fn parses_explicit_theme_paths() {
        let config: Config = toml::from_str(
            "theme = \"~/.config/themes/synthetic.toml\"\ntheme_catalog = \"~/.config/themes/catalog.toml\"\n",
        )
        .expect("config");
        assert_eq!(config.theme(), Some("~/.config/themes/synthetic.toml"));
        assert_eq!(
            config.theme_catalog(),
            Some("~/.config/themes/catalog.toml")
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("future_option = 42").is_err());
    }

    #[test]
    fn unused_socket_does_not_restrict_config_directory_length() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("x".repeat(120)).join("config.toml");
        let mut config = Config::load_path(&path).unwrap();
        assert!(!path.parent().unwrap().join("run").exists());
        assert!(config.select_session(Some("demo")).is_err());
        config.select_session(None).unwrap();
        assert_eq!(config.forward_socket(), None);
        assert!(!path.parent().unwrap().join("run").exists());
    }

    #[test]
    fn socket_parent_permissions_are_checked_only_for_named_sessions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::Builder::new()
            .prefix("oterm-config-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let path = directory.path().join("config.toml");
        let socket = directory.path().join("viewer.sock");
        fs::write(
            &path,
            format!("forward_socket = {:?}", socket.to_str().unwrap()),
        )
        .unwrap();
        let mut config = Config::load_path(&path).unwrap();
        assert!(config.select_session(Some("demo")).is_err());
        config.select_session(None).unwrap();
        assert_eq!(config.forward_socket(), None);
    }

    #[test]
    fn creates_documented_config_without_overwriting_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("officeterm/config.toml");
        Config::load_path(&path).unwrap();
        let template = fs::read_to_string(&path).unwrap();
        assert_eq!(template, include_str!("../config.default.toml"));
        assert!(!Config::load_path(&path).unwrap().viewer.smooth_scroll);
        fs::write(&path, "[viewer]\nsmooth_scroll = true\n").unwrap();
        assert!(Config::load_path(&path).unwrap().viewer.smooth_scroll);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[viewer]\nsmooth_scroll = true\n"
        );
        fs::write(&path, "[viewer]\nscroll_frame_ms = 0\n").unwrap();
        assert!(Config::load_path(&path).is_err());
    }

    #[test]
    fn file_size_limit_rejects_zero_and_accepts_large_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for invalid in ["0", "-1", "4294967296"] {
            fs::write(&path, format!("max_file_size_mib = {invalid}\n")).unwrap();
            assert!(Config::load_path(&path).is_err(), "accepted {invalid}");
        }
        fs::write(&path, "max_file_size_mib = 4294967295\n").unwrap();
        assert_eq!(
            Config::load_path(&path).unwrap().max_file_size_mib,
            u32::MAX
        );
    }
}
