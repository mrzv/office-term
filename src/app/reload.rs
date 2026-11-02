use super::*;

impl AppDefaults {
    pub(super) fn from_config_with_themes(
        config: &Config,
        mut themes: Vec<(String, Palette)>,
        configured_theme: Palette,
    ) -> Self {
        let theme_index = themes
            .iter()
            .position(|(_, theme)| *theme == configured_theme)
            .unwrap_or_else(|| {
                themes.push(("built-in".to_owned(), configured_theme));
                themes.len() - 1
            });
        let theme = configured_theme;
        Self {
            fit: config.fit_mode(),
            invert: config.dark_mode(),
            dark_mode_style: DarkModeStyle::new(
                theme.document.background,
                theme.document.foreground,
            ),
            search_highlight: terminal_color_rgb(theme.yellow),
            link_highlight: terminal_color_rgb(theme.cyan),
            persistent_link_picker: config.persistent_link_picker(),
            link_picker_geometry: LinkPickerGeometry::new(
                config.link_picker_split_percent(),
                config.link_picker_layout(),
            ),
            forward_socket: config.forward_socket().map(str::to_owned),
            viewer: config.viewer.clone(),
            reload_context: config.reload_context(),
            theme,
            themes,
            theme_index,
        }
    }
}

struct PreparedReload {
    defaults: AppDefaults,
    max_file_size_mib: u32,
    endpoint_changed: bool,
    listener: Option<crate::ipc::ForwardListener>,
}

impl App {
    fn prepare_config_reload(&self) -> Result<PreparedReload, Box<dyn std::error::Error>> {
        let config = self.reload_context.load()?;
        let themes = crate::theme::load_for_reload(config.theme_catalog(), config.theme())?;
        let defaults =
            AppDefaults::from_config_with_themes(&config, themes.choices, themes.selected);
        let endpoint_changed = self.forward_socket != defaults.forward_socket;
        // Bind before dropping the old listener or applying any settings. A busy
        // replacement endpoint leaves the complete old configuration usable.
        let listener = if endpoint_changed {
            defaults
                .forward_socket
                .as_deref()
                .map(|path| crate::ipc::ForwardListener::bind(Path::new(path)))
                .transpose()?
        } else {
            None
        };
        Ok(PreparedReload {
            defaults,
            max_file_size_mib: config.max_file_size_mib,
            endpoint_changed,
            listener,
        })
    }

    pub(super) fn reload_config(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let prepared = match self.prepare_config_reload() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.draw_status(output, self.viewport()?, &format!("config error: {error}"))?;
                return Ok(());
            }
        };
        if let Err(error) = self
            .worker
            .set_max_file_size_mib(prepared.max_file_size_mib)
        {
            self.draw_status(output, self.viewport()?, &format!("config error: {error}"))?;
            return Ok(());
        }
        if prepared.endpoint_changed {
            self.forward_listener = prepared.listener;
        }
        self.apply_reload_defaults(prepared.defaults);
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)?;
        self.draw_status(output, self.viewport()?, "config reloaded")?;
        Ok(())
    }

    fn apply_reload_defaults(&mut self, defaults: AppDefaults) {
        let fit_changed = self.default_fit != defaults.fit;
        let invert_changed = self.default_invert != defaults.invert;
        self.default_fit = defaults.fit;
        self.default_invert = defaults.invert;
        self.persistent_link_picker = defaults.persistent_link_picker;
        self.link_picker_geometry = defaults.link_picker_geometry;
        if self.link_picker_geometry.layout == LinkPickerLayout::Floating {
            if let Some(state) = &mut self.link_picker {
                state.focus = LinkPickerFocus::Links;
            }
            if let Some(state) = &mut self.search_picker {
                state.focus = LinkPickerFocus::Links;
            }
        }
        self.viewer = defaults.viewer;
        self.title_document = None;
        self.smooth_scroll_remaining = 0;
        self.forward_socket = defaults.forward_socket;
        self.reload_context = defaults.reload_context;
        self.themes = defaults.themes;
        self.theme_index = defaults.theme_index;
        self.theme = defaults.theme;
        for tab in &mut self.session.tabs {
            if fit_changed {
                tab.fit = defaults.fit;
                if defaults.fit == FitMode::Content {
                    tab.center_content = true;
                }
            }
            if invert_changed {
                tab.invert = defaults.invert;
            }
            tab.dark_mode_style = defaults.dark_mode_style;
            tab.search_highlight = defaults.search_highlight;
            tab.link_highlight = defaults.link_highlight;
            tab.cache.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reload_app() -> (App, tempfile::TempDir, PathBuf) {
        let directory = tempfile::Builder::new()
            .prefix("oterm-reload-")
            .tempdir_in("/tmp")
            .unwrap();
        let config_path = directory.path().join("config.toml");
        let mut config = Config::load_path(&config_path).unwrap();
        config.select_session(Some("demo")).unwrap();
        let document = directory.path().join("document.pptx");
        std::fs::write(&document, []).unwrap();
        let revision = DocumentRevision::read(&document).unwrap();
        let mut app = App::new(
            PptxWorker::disconnected(),
            1,
            0,
            document.clone(),
            FileWatcher::new(&document).unwrap(),
            (Vec::new(), revision),
            AppDefaults::from_config(&config),
        );
        app.test_viewport = Some(Viewport {
            columns: 200,
            rows: 20,
            pixel_width: 2000,
            pixel_height: 200,
            top: 1,
            status_row: 21,
        });
        app.forward_listener = Some(
            crate::ipc::ForwardListener::bind(Path::new(app.forward_socket.as_ref().unwrap()))
                .unwrap(),
        );
        (app, directory, config_path)
    }

    #[test]
    fn empty_selected_theme_restores_builtin_with_an_external_catalog() {
        let config = toml::from_str("").unwrap();
        let builtin = crate::theme::TOKYO_NIGHT_MOON;
        let mut external = builtin;
        external.document.background = [1, 2, 3];
        let defaults = AppDefaults::from_config_with_themes(
            &config,
            vec![("external".to_owned(), external)],
            builtin,
        );
        assert_eq!(defaults.theme, builtin);
        assert_eq!(
            defaults.dark_mode_style.background,
            builtin.document.background
        );
        assert_eq!(defaults.themes[defaults.theme_index].1, builtin);
    }

    #[test]
    fn invalid_reload_keeps_live_settings_and_bound_session() {
        let (mut app, _directory, config_path) = reload_app();
        let socket = app.forward_socket.clone().unwrap();
        let theme = app.theme;
        for contents in [
            "dark_mode = true\n[viewer]\nscroll_frame_ms = 0\n".to_owned(),
            "dark_mode = true\ntheme = \"/missing/officeterm-theme.toml\"\n".to_owned(),
        ] {
            std::fs::write(&config_path, contents).unwrap();
            let mut output = Vec::new();
            app.reload_config(&mut output).unwrap();
            assert!(String::from_utf8_lossy(&output).contains("config error:"));
            assert!(!app.default_invert);
            assert!(!app.tab().invert);
            assert_eq!(app.viewer.scroll_frame_ms, 16);
            assert_eq!(app.theme, theme);
            assert_eq!(app.forward_socket.as_deref(), Some(socket.as_str()));
            assert!(app.forward_listener.is_some());
            assert!(Path::new(&socket).exists());
        }
    }

    #[test]
    fn busy_replacement_endpoint_keeps_old_listener_and_all_settings() {
        let (mut app, directory, config_path) = reload_app();
        let socket = app.forward_socket.clone().unwrap();
        let replacement = directory.path().join("run/demo-replacement.sock");
        let _occupied = crate::ipc::ForwardListener::bind(&replacement).unwrap();
        std::fs::write(
            &config_path,
            "forward_socket = \"replacement.sock\"\ndark_mode = true\npersistent_link_picker = true\n",
        )
        .unwrap();
        let mut output = Vec::new();
        app.reload_config(&mut output).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("config error:"));
        assert_eq!(app.forward_socket.as_deref(), Some(socket.as_str()));
        assert!(Path::new(&socket).exists());
        assert!(replacement.exists());
        assert!(!app.default_invert);
        assert!(!app.persistent_link_picker);
    }

    #[test]
    fn stopped_worker_rolls_back_prepared_endpoint_and_settings() {
        let (mut app, directory, config_path) = reload_app();
        let socket = app.forward_socket.clone().unwrap();
        let replacement = directory.path().join("run/demo-replacement.sock");
        std::fs::write(
            &config_path,
            "forward_socket = \"replacement.sock\"\ndark_mode = true\n",
        )
        .unwrap();
        let mut output = Vec::new();
        app.reload_config(&mut output).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("config error:"));
        assert_eq!(app.forward_socket.as_deref(), Some(socket.as_str()));
        assert!(Path::new(&socket).exists());
        assert!(!replacement.exists());
        assert!(!app.default_invert);
    }
}
