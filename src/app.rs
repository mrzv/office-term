use crate::screenshot::Page as ScreenshotPage;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::Frame as RatatuiFrame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RatatuiColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear as RatatuiClear, Paragraph, Wrap};
use thiserror::Error;

use crate::browser::{BrowserEntry, BrowserEntrySource, BrowserState};
use crate::config::{Config, LinkPickerLayout, ViewerSettings};
use crate::kitty::{self, Placement};
use crate::pptx::{
    DarkModeStyle, DocumentId, DocumentLink, DocumentRevision, FitMode, Frame, LinkTarget,
    OutlineItem, PptxWorker, RenderKey, RenderRequest, SearchPageMatch, WorkerMessage,
};
use crate::terminal::{ImagePlacement, TerminalGuard, Viewport};
use crate::theme::Palette;
mod scale;
mod session;
mod status;
#[cfg(test)]
use scale::ZOOM_STEP;
use scale::{
    ZOOM_DEFAULT, ZOOM_MAX, ZOOM_MIN, centered_scaled_view, content_axis_scroll,
    content_horizontal_scroll, content_scroll, fitted_page_size, horizontal_scroll_to_reveal,
    link_at_cell, scale_zoom, stepped_zoom,
};
#[cfg(test)]
use session::FileFingerprint;
use session::{FileWatcher, Session, Tab};
#[cfg(test)]
use status::render_timing_status;
use status::{
    PerformanceSnapshot, StatusText, cycled_tab_index, numbered_tab_index, stale_status_row,
    write_clipboard_osc52,
};

const FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_STABLE_FOR: Duration = Duration::from_millis(150);
const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(500);
const LINK_PREVIEW_DELAY: Duration = Duration::from_millis(120);
const INITIAL_DOCUMENT_ID: DocumentId = 1;
const PAGE_IMAGE_Z_INDEX: i32 = i32::MIN / 2 - 2;
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

/// Defer inner flushes until synchronized_output has closed the frame.
struct FrameOutput<'a, W>(&'a mut W);

impl<W: Write> Write for FrameOutput<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn synchronized_output<W, T, E>(
    output: &mut W,
    operation: impl FnOnce(&mut FrameOutput<'_, W>) -> Result<T, E>,
) -> Result<T, E>
where
    W: Write,
    E: From<io::Error>,
{
    output
        .write_all(BEGIN_SYNCHRONIZED_UPDATE)
        .map_err(E::from)?;
    let operation_result = operation(&mut FrameOutput(output));
    let finish_result = output
        .write_all(END_SYNCHRONIZED_UPDATE)
        .and_then(|()| output.flush());
    match operation_result {
        Ok(value) => {
            finish_result.map_err(E::from)?;
            Ok(value)
        }
        Err(error) => {
            let _ = finish_result;
            Err(error)
        }
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("viewer quit requested")]
    Quit,
    #[error("oterm requires an interactive terminal")]
    NotInteractive,
    #[error("{0}")]
    Renderer(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn read_event() -> Result<Event, AppError> {
    let event = event::read()?;
    if matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Err(AppError::Quit);
    }
    Ok(event)
}

pub fn run(path: Option<PathBuf>, start_page: u32, config: &Config) -> Result<(), AppError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::NotInteractive);
    }
    crate::pptx::init_font_db();

    let mut output = io::BufWriter::new(io::stdout().lock());

    let defaults = AppDefaults::from_config(config);
    let theme = defaults.theme;
    let _terminal = TerminalGuard::enter(&mut output, theme)?;
    let path = match path {
        Some(path) => path.canonicalize()?,
        None => match pick_document(std::env::current_dir()?, &mut output, theme, || Ok(false))? {
            Some(path) => path,
            None => return Ok(()),
        },
    };
    let (worker, page_count, outline, revision) =
        PptxWorker::spawn(path.clone()).map_err(AppError::Renderer)?;
    crate::recent::record(&path);
    let watcher = FileWatcher::new(&path)?;
    let mut app = App::new(
        worker,
        page_count,
        start_page.min(page_count.saturating_sub(1)),
        path,
        watcher,
        (outline, revision),
        defaults,
    );
    app.request_current(&mut output)?;

    loop {
        while let Some(message) = app.worker.try_recv() {
            match message {
                WorkerMessage::Frame(frame) => app.receive_frame(frame, &mut output)?,
                WorkerMessage::Error(error) => {
                    return Err(AppError::Renderer(error));
                }
                WorkerMessage::Ready { .. } => {}
                WorkerMessage::Opened {
                    document_id,
                    pages,
                    outline,
                    revision,
                } => app.finish_open(document_id, pages, outline, revision, &mut output)?,
                WorkerMessage::OpenError { document_id, error } => {
                    app.fail_open(document_id, &error, &mut output)?
                }
                WorkerMessage::Text { content, .. } => app.copy_text(&content, &mut output)?,
                WorkerMessage::SearchProgress {
                    document_id,
                    request_id,
                    scanned,
                    total,
                    matches,
                    total_occurrences,
                } => app.receive_search_progress(
                    document_id,
                    SearchProgressUpdate {
                        request_id,
                        scanned,
                        total,
                        matches,
                        total_occurrences,
                    },
                    &mut output,
                )?,
                WorkerMessage::SearchResults {
                    document_id,
                    request_id,
                    matches,
                    total_occurrences,
                } => app.receive_search_results(
                    document_id,
                    request_id,
                    matches,
                    total_occurrences,
                    &mut output,
                )?,
                WorkerMessage::LinkIndexProgress {
                    document_id,
                    request_id,
                    links,
                    scanned,
                    total,
                    complete,
                } => app.receive_link_index_progress(
                    document_id,
                    LinkIndexUpdate {
                        request_id,
                        links,
                        scanned,
                        total,
                        complete,
                    },
                    &mut output,
                )?,
            }
        }
        app.poll_file_change(&mut output)?;
        app.poll_link_preview(&mut output)?;
        app.poll_search_preview(&mut output)?;
        app.poll_forward_socket(&mut output)?;
        app.poll_smooth_scroll(&mut output)?;

        if event::poll(app.input_wait())? {
            match read_event()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.pending_scale = None;
                    if app.handle_key(key, &mut output)? {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    app.tab_mut().center_content = true;
                    app.request_current(&mut output)?;
                }
                Event::Mouse(mouse) => {
                    if !matches!(mouse.kind, MouseEventKind::Moved | MouseEventKind::Up(_)) {
                        app.pending_scale = None;
                    }
                    app.handle_mouse(mouse, &mut output)?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkPickerGeometry {
    split_percent: u16,
    layout: LinkPickerLayout,
}

impl LinkPickerGeometry {
    const fn new(split_percent: u16, layout: LinkPickerLayout) -> Self {
        Self {
            split_percent,
            layout,
        }
    }
}

struct App {
    worker: PptxWorker,
    session: Session,
    generation: u64,
    desired_key: Option<RenderKey>,
    pending: HashSet<RenderKey>,
    visible_page: Option<VisiblePage>,
    submitted_key: Option<RenderKey>,
    visible_pages: Vec<VisiblePage>,
    missing_visible_page: Option<RenderKey>,
    pending_scale: Option<(RenderKey, FitMode, u16)>,
    canvas_viewport: Option<Viewport>,
    #[cfg(test)]
    test_viewport: Option<Viewport>,
    pending_vertical_scroll: i64,
    smooth_scroll_remaining: i64,
    smooth_scroll_tick: Instant,
    title_document: Option<DocumentId>,
    viewer: ViewerSettings,
    next_image_id: u32,
    last_status_row: Option<u16>,
    status_line: Vec<u8>,
    status_buffer: Vec<u8>,
    performance_snapshot: Option<PerformanceSnapshot>,
    default_fit: FitMode,
    default_invert: bool,
    goto_input: Option<String>,
    search_input: Option<String>,
    search_picker: Option<SearchPickerState>,
    next_search_request_id: u64,
    next_link_request_id: u64,
    link_mode: bool,
    forward_socket: Option<String>,
    forward_listener: Option<crate::ipc::ForwardListener>,
    pending_link_picker_open: bool,
    link_picker: Option<LinkPickerState>,
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
    show_performance: bool,
    pending_clipboard: Option<String>,
    theme: Palette,
    themes: Vec<(String, Palette)>,
    theme_index: usize,
}

struct AppDefaults {
    fit: FitMode,
    invert: bool,
    theme: Palette,
    dark_mode_style: DarkModeStyle,
    search_highlight: [u8; 3],
    link_highlight: [u8; 3],
    themes: Vec<(String, Palette)>,
    theme_index: usize,
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
    forward_socket: Option<String>,
    viewer: ViewerSettings,
}

impl AppDefaults {
    fn from_config(config: &Config) -> Self {
        let themes = crate::theme::available_themes(config.theme_catalog(), config.theme());
        let configured_theme = crate::theme::load_or_default(config.theme());
        let theme_index = themes
            .iter()
            .position(|(_, theme)| *theme == configured_theme)
            .unwrap_or(0);
        let theme = themes
            .get(theme_index)
            .map_or(configured_theme, |(_, theme)| *theme);
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
            theme,
            themes,
            theme_index,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewPosition {
    page: u32,
    scroll_x: i64,
    scroll_y: u32,
    center_content: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LinkDestination {
    page: u32,
    top_ratio: Option<f32>,
    left_ratio: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkPickerState {
    page: u32,
    selected: usize,
    number_input: String,
    filter: String,
    filtering: bool,
    focus: LinkPickerFocus,
    selection_key: Option<(u32, u32)>,
    awaiting_current_page: bool,
    pending_preview: Option<PendingLinkPreview>,
    preview_origin: Option<ViewPosition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingLinkPreview {
    selection_key: (u32, u32),
    ready_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkPickerFocus {
    Document,
    Links,
}

#[derive(Default)]
struct LinkIndexState {
    request_id: u64,
    links: Vec<DocumentLink>,
    scanned: u32,
    total_pages: u32,
    indexing: bool,
}

impl LinkIndexState {
    fn new(total_pages: u32) -> Self {
        Self {
            total_pages,
            ..Self::default()
        }
    }

    fn started(&self) -> bool {
        self.request_id != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkIndexProgress {
    scanned: u32,
    total_pages: u32,
    indexing: bool,
}

#[derive(Clone, Copy)]
struct LinkPickerDocument<'a> {
    links: &'a [DocumentLink],
    outline: &'a [OutlineItem],
}

impl<'a> LinkPickerDocument<'a> {
    const fn new(links: &'a [DocumentLink], outline: &'a [OutlineItem]) -> Self {
        Self { links, outline }
    }
}

struct LinkIndexUpdate {
    request_id: u64,
    links: Vec<DocumentLink>,
    scanned: u32,
    total: u32,
    complete: bool,
}

struct SearchProgressUpdate {
    request_id: u64,
    scanned: u32,
    total: u32,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
}

#[derive(Clone, Copy)]
struct PickerFilter<'a> {
    query: &'a str,
    active: bool,
}

impl From<&LinkIndexState> for LinkIndexProgress {
    fn from(index: &LinkIndexState) -> Self {
        Self {
            scanned: index.scanned,
            total_pages: index.total_pages,
            indexing: index.indexing,
        }
    }
}

impl LinkPickerState {
    fn new(page: u32) -> Self {
        Self {
            page,
            selected: 0,
            number_input: String::new(),
            filter: String::new(),
            filtering: false,
            focus: LinkPickerFocus::Links,
            selection_key: None,
            awaiting_current_page: true,
            pending_preview: None,
            preview_origin: None,
        }
    }

    fn sync(&mut self, page: u32, links: &[DocumentLink], indexing: bool) {
        if self.page != page {
            self.page = page;
            self.selected = 0;
            self.number_input.clear();
            self.selection_key = None;
            self.awaiting_current_page = true;
        }

        if self.awaiting_current_page {
            let target = links
                .iter()
                .position(|link| link.source_page == page)
                .or_else(|| {
                    (!indexing)
                        .then(|| links.iter().position(|link| link.source_page > page))
                        .flatten()
                });
            if let Some(index) = target {
                self.select(index, links);
            } else if !indexing {
                self.select(0, links);
            }
            return;
        }

        if let Some(key) = self.selection_key
            && let Some(index) = links.iter().position(|link| link_key(link) == key)
        {
            self.selected = index;
            return;
        }
        self.select(self.selected.min(links.len().saturating_sub(1)), links);
    }

    fn select(&mut self, selected: usize, links: &[DocumentLink]) {
        self.selected = selected.min(links.len().saturating_sub(1));
        self.selection_key = links.get(self.selected).map(link_key);
        self.awaiting_current_page = false;
    }
}

fn link_key(link: &DocumentLink) -> (u32, u32) {
    (link.source_page, link.ordinal)
}

#[derive(Clone, Default)]
struct SearchState {
    query: String,
    request_id: u64,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
    scanned: u32,
    total_pages: u32,
    searching: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchPickerState {
    page: u32,
    selected: usize,
    focus: LinkPickerFocus,
    awaiting_initial_result: bool,
    selection_page: Option<u32>,
    pending_preview: Option<PendingSearchPreview>,
    preview_origin: Option<ViewPosition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSearchPreview {
    page: u32,
    ready_at: Instant,
}

impl SearchPickerState {
    const fn new(page: u32) -> Self {
        Self {
            page,
            selected: 0,
            focus: LinkPickerFocus::Links,
            awaiting_initial_result: true,
            selection_page: None,
            pending_preview: None,
            preview_origin: None,
        }
    }

    fn sync(&mut self, page: u32, matches: &[SearchPageMatch]) {
        self.page = page;
        if matches.is_empty() {
            self.selected = 0;
            self.selection_page = None;
            return;
        }
        if self.awaiting_initial_result {
            self.selected = matches
                .iter()
                .position(|result| result.page >= page)
                .unwrap_or(0);
            self.awaiting_initial_result = false;
        } else {
            self.selected = self.selected.min(matches.len() - 1);
        }
        self.selection_page = matches.get(self.selected).map(|result| result.page);
    }
}

impl SearchState {
    fn highlight_request_id(&self, page: u32) -> u64 {
        if !self.searching && self.matches.iter().any(|result| result.page == page) {
            self.request_id
        } else {
            0
        }
    }

    fn status_label(&self, current_page: u32) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        let query = truncated_search_query(&self.query, 28);
        if self.searching {
            return Some(format!(
                "  search {}/{}  /{}",
                self.scanned, self.total_pages, query
            ));
        }
        if self.matches.is_empty() {
            return Some(format!("  no matches  /{query}"));
        }
        let position = self
            .matches
            .iter()
            .position(|result| result.page == current_page)
            .map(|index| format!("{}/{}", index + 1, self.matches.len()))
            .unwrap_or_else(|| format!("{} pages", self.matches.len()));
        Some(format!(
            "  search {position} · {} hits  /{query}",
            self.total_occurrences
        ))
    }
}

fn truncated_search_query(query: &str, max_chars: usize) -> String {
    let mut characters = query.chars();
    let mut truncated: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn search_target_page(
    matches: &[SearchPageMatch],
    current_page: u32,
    forward: bool,
) -> Option<u32> {
    if forward {
        matches
            .iter()
            .find(|result| result.page > current_page)
            .or_else(|| matches.first())
    } else {
        matches
            .iter()
            .rev()
            .find(|result| result.page < current_page)
            .or_else(|| matches.last())
    }
    .map(|result| result.page)
}

#[derive(Clone)]
struct VisiblePage {
    frame: Arc<Frame>,
    placement: ImagePlacement,
    top: u16,
    image_id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

fn terminal_color_rgb(color: crossterm::style::Color) -> [u8; 3] {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => [r, g, b],
        _ => [0xff, 0xc7, 0x77],
    }
}

impl App {
    fn new(
        worker: PptxWorker,
        page_count: u32,
        page: u32,
        path: PathBuf,
        watcher: FileWatcher,
        document: (Vec<OutlineItem>, DocumentRevision),
        defaults: AppDefaults,
    ) -> Self {
        let (outline, revision) = document;
        let default_fit = defaults.fit;
        let default_invert = defaults.invert;
        Self {
            worker,
            session: Session {
                pending_open: None,
                tabs: vec![Tab {
                    document_id: INITIAL_DOCUMENT_ID,
                    path,
                    revision,
                    watcher,
                    page_count,
                    page,
                    fit: default_fit,
                    zoom: ZOOM_DEFAULT,
                    invert: default_invert,
                    dark_mode_style: defaults.dark_mode_style,
                    search_highlight: defaults.search_highlight,
                    link_highlight: defaults.link_highlight,
                    scroll_x: 0,
                    scroll_y: 0,
                    center_content: true,
                    outline: Arc::new(outline),
                    cache: HashMap::new(),
                    search: SearchState::default(),
                    link_history: Vec::new(),
                    pending_destination: None,
                    link_index: LinkIndexState::new(page_count),
                }],
                active_tab: 0,
                next_document_id: INITIAL_DOCUMENT_ID + 1,
            },
            generation: 0,
            desired_key: None,
            pending: HashSet::new(),
            visible_page: None,
            submitted_key: None,
            visible_pages: Vec::new(),
            missing_visible_page: None,
            pending_scale: None,
            canvas_viewport: None,
            #[cfg(test)]
            test_viewport: None,
            pending_vertical_scroll: 0,
            smooth_scroll_remaining: 0,
            smooth_scroll_tick: Instant::now(),
            title_document: None,
            next_image_id: 1,
            last_status_row: None,
            status_line: Vec::new(),
            status_buffer: Vec::new(),
            performance_snapshot: None,
            default_fit,
            default_invert,
            goto_input: None,
            search_input: None,
            search_picker: None,
            next_search_request_id: 1,
            next_link_request_id: 1,
            link_mode: false,
            forward_socket: defaults.forward_socket,
            forward_listener: None,
            pending_link_picker_open: false,
            link_picker: None,
            persistent_link_picker: defaults.persistent_link_picker,
            link_picker_geometry: defaults.link_picker_geometry,
            show_performance: false,
            pending_clipboard: None,
            theme: defaults.theme,
            themes: defaults.themes,
            theme_index: defaults.theme_index,
            viewer: defaults.viewer,
        }
    }

    fn open_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let directory = self
            .tab()
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let theme = self.theme;
        match pick_document(directory, output, theme, || self.poll_modal_background())? {
            Some(path) => {
                let path = path.canonicalize()?;
                if let Some(index) = self.tab_index_for_path(&path) {
                    self.session.active_tab = index;
                    self.reset_render_state();
                    self.request_current(output)?;
                } else {
                    self.begin_open(path, output)?;
                }
            }
            None => self.request_current(output)?,
        }
        self.flush_pending_clipboard(output)
    }

    fn open_outline(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let outline = Arc::clone(&self.tab().outline);
        if outline.is_empty() {
            let viewport = self.prepare_viewport(output)?;
            self.draw_status(output, viewport, "no outline in this document")?;
            return Ok(());
        }
        self.clear_viewer(output)?;
        let current_page = self.tab().page;
        let theme = self.theme;
        let selection = pick_outline(&outline, current_page, output, theme, || {
            self.poll_modal_background()
        })?;
        if let Some(page) = selection {
            let page = page.min(self.tab().page_count.saturating_sub(1));
            self.tab_mut().page = page;
            self.tab_mut().scroll_x = 0;
            self.tab_mut().scroll_y = 0;
            self.tab_mut().center_content = true;
        }
        self.reset_render_state();
        self.request_current(output)?;
        self.flush_pending_clipboard(output)
    }

    fn open_theme_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let themes = self.themes.clone();
        let theme_index = self.theme_index;
        let selection = pick_theme(&themes, theme_index, output, || {
            self.poll_modal_background()
        })?;
        if let Some(index) = selection {
            self.apply_theme(index);
        }
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)?;
        self.flush_pending_clipboard(output)
    }

    fn open_help(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let theme = self.theme;
        show_help(output, theme, || self.poll_modal_background())?;
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)?;
        self.flush_pending_clipboard(output)
    }

    fn poll_modal_background(&mut self) -> Result<bool, AppError> {
        while let Some(message) = self.worker.try_recv() {
            self.handle_modal_worker_message(message)?;
        }
        self.forward_waiting().map_err(AppError::Io)
    }

    fn handle_modal_worker_message(&mut self, message: WorkerMessage) -> Result<(), AppError> {
        match message {
            WorkerMessage::Frame(frame) => {
                self.store_frame(frame);
            }
            WorkerMessage::Error(error) => return Err(AppError::Renderer(error)),
            WorkerMessage::Ready { .. }
            | WorkerMessage::Opened { .. }
            | WorkerMessage::OpenError { .. } => {}
            WorkerMessage::Text { content, .. } => {
                self.pending_clipboard = Some(content);
            }
            WorkerMessage::SearchProgress {
                document_id,
                request_id,
                scanned,
                total,
                matches,
                total_occurrences,
            } => {
                self.apply_search_progress(
                    document_id,
                    SearchProgressUpdate {
                        request_id,
                        scanned,
                        total,
                        matches,
                        total_occurrences,
                    },
                );
            }
            WorkerMessage::SearchResults {
                document_id,
                request_id,
                matches,
                total_occurrences,
            } => {
                self.apply_search_results(document_id, request_id, matches, total_occurrences);
            }
            WorkerMessage::LinkIndexProgress {
                document_id,
                request_id,
                links,
                scanned,
                total,
                complete,
            } => {
                self.apply_link_index_progress(
                    document_id,
                    LinkIndexUpdate {
                        request_id,
                        links,
                        scanned,
                        total,
                        complete,
                    },
                );
            }
        }
        Ok(())
    }

    fn flush_pending_clipboard(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if let Some(content) = self.pending_clipboard.take() {
            self.copy_text(&content, output)?;
        }
        Ok(())
    }

    fn apply_theme(&mut self, index: usize) {
        let Some((_, theme)) = self.themes.get(index) else {
            return;
        };
        let theme = *theme;
        self.theme_index = index;
        self.theme = theme;
        let style = DarkModeStyle::new(theme.document.background, theme.document.foreground);
        let search_highlight = terminal_color_rgb(theme.yellow);
        let link_highlight = terminal_color_rgb(theme.cyan);
        for tab in &mut self.session.tabs {
            tab.dark_mode_style = style;
            tab.search_highlight = search_highlight;
            tab.link_highlight = link_highlight;
            tab.cache.clear();
        }
    }

    fn begin_goto(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.goto_input = Some(String::new());
        let viewport = self.viewport()?;
        self.draw_goto(output, viewport)?;
        Ok(())
    }

    fn handle_goto_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<(), AppError> {
        match key.code {
            KeyCode::Esc => {
                self.goto_input = None;
                self.redraw_current(output)?;
            }
            KeyCode::Enter => {
                let input = self.goto_input.take().unwrap_or_default();
                let target = input
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|number| *number >= 1)
                    .map(|number| (number - 1).min(self.tab().page_count.saturating_sub(1)));
                match target {
                    Some(page) if page != self.tab().page => self.set_page(page, output)?,
                    _ => self.redraw_current(output)?,
                }
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.goto_input.as_mut() {
                    buffer.pop();
                }
                let viewport = self.viewport()?;
                self.draw_goto(output, viewport)?;
            }
            KeyCode::Char(character) if character.is_ascii_digit() => {
                if let Some(buffer) = self.goto_input.as_mut().filter(|buffer| buffer.len() < 9) {
                    buffer.push(character);
                }
                let viewport = self.viewport()?;
                self.draw_goto(output, viewport)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_goto(&mut self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        self.status_line.clear();
        let theme = self.theme;
        let input = self.goto_input.as_deref().unwrap_or_default();
        let hint = format!(
            "  (1-{}, enter to jump, esc to cancel)",
            self.tab().page_count
        );
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.yellow),
            Print("go to page: "),
            SetForegroundColor(theme.fg),
            Print(input),
            SetForegroundColor(theme.comment),
            Print(hint),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn begin_search(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.search_input = Some(String::new());
        self.draw_search(output, self.viewport()?)?;
        Ok(())
    }

    fn handle_search_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.search_input = None;
                self.redraw_current(output)?;
            }
            KeyCode::Enter => {
                let query = self.search_input.take().unwrap_or_default();
                let query = query.trim();
                if query.is_empty() {
                    self.redraw_current(output)?;
                } else {
                    self.start_search(query.to_string(), output)?;
                }
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.search_input.as_mut() {
                    buffer.pop();
                }
                self.draw_search(output, self.viewport()?)?;
            }
            KeyCode::Char('u') if control => {
                if let Some(buffer) = self.search_input.as_mut() {
                    buffer.clear();
                }
                self.draw_search(output, self.viewport()?)?;
            }
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(buffer) = self
                    .search_input
                    .as_mut()
                    .filter(|buffer| buffer.len() < 256)
                {
                    buffer.push(character);
                }
                self.draw_search(output, self.viewport()?)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_search(&mut self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        self.status_line.clear();
        let theme = self.theme;
        let input = self.search_input.as_deref().unwrap_or_default();
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.yellow),
            Print("search: "),
            SetForegroundColor(theme.fg),
            Print(input),
            SetForegroundColor(theme.comment),
            Print("  (enter to search, esc to cancel)"),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn start_search(&mut self, query: String, output: &mut impl Write) -> Result<(), AppError> {
        let request_id = self.next_search_request_id;
        self.next_search_request_id = self.next_search_request_id.wrapping_add(1).max(1);
        let (document_id, total_pages) = {
            let tab = self.tab();
            (tab.document_id, tab.page_count)
        };
        self.tab_mut().replace_search(SearchState {
            query: query.clone(),
            request_id,
            matches: Vec::new(),
            total_occurrences: 0,
            scanned: 0,
            total_pages,
            searching: true,
        });
        let _ = self.worker.search(document_id, request_id, query);
        self.search_picker = Some(SearchPickerState::new(self.tab().page));
        self.open_search_picker(output)
    }

    fn apply_search_progress(
        &mut self,
        document_id: DocumentId,
        update: SearchProgressUpdate,
    ) -> Option<usize> {
        let index = self.tab_index(document_id)?;
        if self.session.tabs[index].search.request_id != update.request_id {
            return None;
        }
        let search = &mut self.session.tabs[index].search;
        search.scanned = update.scanned;
        search.total_pages = update.total;
        search.matches = update.matches;
        search.total_occurrences = update.total_occurrences;
        Some(index)
    }

    fn receive_search_progress(
        &mut self,
        document_id: DocumentId,
        update: SearchProgressUpdate,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.apply_search_progress(document_id, update) else {
            return Ok(());
        };
        if index == self.session.active_tab {
            if self.search_picker.is_some() {
                self.redraw_search_picker(output)?;
            }
            self.draw_status(output, self.viewport()?, "")?;
        }
        Ok(())
    }

    fn apply_search_results(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    ) -> Option<(usize, Option<u32>)> {
        let index = self.tab_index(document_id)?;
        if self.session.tabs[index].search.request_id != request_id {
            return None;
        }
        let current_page = self.session.tabs[index].page;
        let target_page = matches
            .iter()
            .find(|result| result.page >= current_page)
            .or_else(|| matches.first())
            .map(|result| result.page);
        let search = &mut self.session.tabs[index].search;
        search.matches = matches;
        search.total_occurrences = total_occurrences;
        search.scanned = search.total_pages;
        search.searching = false;
        self.session.tabs[index].retain_search_frames();

        if index == self.session.active_tab {
            if let Some(state) = &mut self.search_picker {
                state.sync(current_page, &self.session.tabs[index].search.matches);
            }
            if let Some(page) = target_page {
                let tab = &mut self.session.tabs[index];
                tab.page = page;
                tab.scroll_x = 0;
                tab.scroll_y = 0;
                tab.center_content = true;
            }
        }
        Some((index, target_page))
    }

    fn receive_search_results(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some((index, target_page)) =
            self.apply_search_results(document_id, request_id, matches, total_occurrences)
        else {
            return Ok(());
        };
        if index != self.session.active_tab {
            return Ok(());
        }
        if target_page.is_some() {
            self.request_current(output)?;
        } else {
            self.draw_status(output, self.viewport()?, "")?;
        }
        Ok(())
    }

    fn apply_link_index_progress(
        &mut self,
        document_id: DocumentId,
        update: LinkIndexUpdate,
    ) -> Option<usize> {
        let index = self.tab_index(document_id)?;
        let link_index = &mut self.session.tabs[index].link_index;
        if link_index.request_id != update.request_id {
            return None;
        }
        link_index.links.extend(update.links);
        link_index.scanned = update.scanned;
        link_index.total_pages = update.total;
        link_index.indexing = !update.complete;
        Some(index)
    }

    fn receive_link_index_progress(
        &mut self,
        document_id: DocumentId,
        update: LinkIndexUpdate,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.apply_link_index_progress(document_id, update) else {
            return Ok(());
        };
        if index == self.session.active_tab && self.link_picker.is_some() {
            self.redraw_link_picker(output)?;
        }
        Ok(())
    }

    fn navigate_search(&mut self, forward: bool, output: &mut impl Write) -> Result<(), AppError> {
        let tab = self.tab();
        if tab.search.query.is_empty() {
            self.draw_status(output, self.viewport()?, "no active search")?;
            return Ok(());
        }
        if tab.search.searching {
            self.draw_status(output, self.viewport()?, "searching")?;
            return Ok(());
        }
        let Some(page) = search_target_page(&tab.search.matches, tab.page, forward) else {
            self.draw_status(output, self.viewport()?, "")?;
            return Ok(());
        };
        self.tab_mut().page = page;
        self.tab_mut().scroll_x = 0;
        self.tab_mut().scroll_y = 0;
        self.tab_mut().center_content = true;
        self.request_current(output)
    }

    fn clear_search(&mut self, output: &mut impl Write) -> Result<bool, AppError> {
        if self.tab().search.query.is_empty() {
            return Ok(false);
        }
        let document_id = self.tab().document_id;
        let request_id = self.tab().search.request_id;
        self.worker.cancel_search(document_id, request_id);
        if self.search_picker.is_some() {
            self.close_search_picker(output)?;
        }
        self.tab_mut().replace_search(SearchState::default());
        self.request_current(output)?;
        Ok(true)
    }

    fn toggle_link_mode(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.set_link_mode(!self.link_mode, output)
    }

    fn set_link_mode(&mut self, enabled: bool, output: &mut impl Write) -> Result<(), AppError> {
        if enabled == self.link_mode {
            return Ok(());
        }
        self.link_mode = enabled;
        self.pending_link_picker_open = enabled;
        self.ensure_link_index();
        self.request_current(output)?;
        self.open_pending_link_picker(output)
    }

    fn ensure_link_index(&mut self) {
        if self.link_mode || self.link_picker.is_some() {
            self.start_link_index();
        }
    }

    fn start_link_index(&mut self) {
        if self.tab().link_index.started() {
            return;
        }
        let request_id = self.next_link_request_id;
        self.next_link_request_id = self.next_link_request_id.wrapping_add(1).max(1);
        let document_id = self.tab().document_id;
        let total_pages = self.tab().page_count;
        self.tab_mut().link_index = LinkIndexState {
            request_id,
            links: Vec::new(),
            scanned: 0,
            total_pages,
            indexing: true,
        };
        let _ = self.worker.index_links(document_id, request_id);
    }

    fn schedule_link_preview(&mut self) {
        let Some(state) = &mut self.link_picker else {
            return;
        };
        let Some(selection_key) = state.selection_key else {
            return;
        };
        state.pending_preview = Some(PendingLinkPreview {
            selection_key,
            ready_at: Instant::now() + LINK_PREVIEW_DELAY,
        });
    }

    fn poll_link_preview(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let Some(pending) = self
            .link_picker
            .as_ref()
            .and_then(|state| state.pending_preview)
        else {
            return Ok(());
        };
        if Instant::now() < pending.ready_at {
            return Ok(());
        }

        let selected = {
            let Some(state) = &mut self.link_picker else {
                return Ok(());
            };
            state.pending_preview = None;
            if state.selection_key != Some(pending.selection_key) {
                return Ok(());
            }
            state.selected
        };
        let Some(link) = self.tab().link_index.links.get(selected).cloned() else {
            return Ok(());
        };
        let origin = ViewPosition {
            page: self.tab().page,
            scroll_x: self.tab().scroll_x,
            scroll_y: self.tab().scroll_y,
            center_content: self.tab().center_content,
        };
        let state = self.link_picker.as_mut().expect("link picker state");
        state.preview_origin.get_or_insert(origin);
        state.page = link.source_page;
        state.awaiting_current_page = false;

        let tab = self.tab_mut();
        tab.page = link.source_page;
        tab.scroll_x = 0;
        tab.scroll_y = 0;
        tab.center_content = true;
        tab.pending_destination = Some(LinkDestination {
            page: link.source_page,
            top_ratio: Some((link.source_top_ratio - 0.08).max(0.0)),
            left_ratio: None,
        });
        self.request_current(output)
    }

    fn schedule_search_preview(&mut self) {
        let Some(state) = &mut self.search_picker else {
            return;
        };
        let Some(page) = state.selection_page else {
            return;
        };
        state.pending_preview = Some(PendingSearchPreview {
            page,
            ready_at: Instant::now() + LINK_PREVIEW_DELAY,
        });
    }

    fn forward_waiting(&self) -> io::Result<bool> {
        self.forward_listener
            .as_ref()
            .map_or(Ok(false), crate::ipc::ForwardListener::has_pending)
    }

    fn poll_forward_socket(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(path) = self.forward_socket.clone() else {
            return Ok(());
        };
        if self.forward_listener.is_none() {
            self.forward_listener = Some(crate::ipc::ForwardListener::bind(Path::new(&path))?);
        }
        let ready = self
            .forward_listener
            .as_mut()
            .expect("listener bound")
            .poll()?;
        for (result, mut reply) in ready {
            match result {
                Ok(crate::ipc::ViewerRequest::Screenshot(path)) => {
                    let result = self.save_screenshot(&path);
                    let error = result.as_ref().err().map(ToString::to_string);
                    reply.finish(error.clone());
                    if let Some(error) = error {
                        self.draw_status(
                            output,
                            self.viewport()?,
                            &format!("screenshot: {error}"),
                        )?;
                    } else {
                        self.draw_status(output, self.viewport()?, "screenshot saved")?;
                    }
                }
                Err(error) => {
                    reply.finish(Some(error.to_string()));
                    self.draw_status(
                        output,
                        self.viewport()?,
                        &format!("socket request: {error}"),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn save_screenshot(&self, path: &Path) -> io::Result<()> {
        if self.link_picker.is_some() || self.search_picker.is_some() {
            return Err(io::Error::other(
                "screenshot is unavailable while a link or search picker is open",
            ));
        }
        let viewport = self
            .viewport()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let pages = self.screenshot_pages(viewport)?;
        let overlay = crate::screenshot::blank_overlay(viewport)?;
        crate::screenshot::save(
            path,
            viewport,
            &pages,
            &overlay,
            terminal_color_rgb(self.theme.bg),
        )
    }

    fn current_visible_page(&self, viewport: Viewport) -> Option<&VisiblePage> {
        let page = self.visible_page.as_ref()?;
        (self.canvas_viewport == Some(viewport)
            && self.submitted_key == Some(self.render_key(viewport))
            && page.frame.revision == self.tab().revision
            && same_render_view(page.frame.key, self.render_key(viewport)))
        .then_some(page)
    }

    fn picker_image(&self, viewport: Viewport) -> Option<LinkPickerImage> {
        let page = self.current_visible_page(viewport)?;
        let mut image = LinkPickerImage::new(page.image_id, &page.frame, page.placement, viewport);
        image.original.top = page.top;
        Some(image)
    }

    fn screenshot_pages(&self, viewport: Viewport) -> io::Result<Vec<ScreenshotPage<'_>>> {
        let unavailable =
            || io::Error::other("viewer frame is still rendering; retry the screenshot");
        if self.missing_visible_page.is_some()
            || self.canvas_viewport != Some(viewport)
            || self.submitted_key != Some(self.render_key(viewport))
        {
            return Err(unavailable());
        }
        let mut pages = Vec::new();
        if self.visible_pages.is_empty() {
            let page = self
                .current_visible_page(viewport)
                .ok_or_else(unavailable)?;
            pages.push(ScreenshotPage {
                frame: &page.frame,
                placement: page.placement,
                row: page.top.saturating_sub(viewport.top),
            });
        } else {
            let current_key = self.render_key(viewport);
            for page in &self.visible_pages {
                if page.frame.revision != self.tab().revision
                    || !same_render_view(page.frame.key, current_key)
                {
                    return Err(unavailable());
                }
                pages.push(ScreenshotPage {
                    frame: &page.frame,
                    placement: page.placement,
                    row: page.top.saturating_sub(viewport.top),
                });
            }
        }
        Ok(pages)
    }

    fn poll_search_preview(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let Some(pending) = self
            .search_picker
            .as_ref()
            .and_then(|state| state.pending_preview)
        else {
            return Ok(());
        };
        if Instant::now() < pending.ready_at {
            return Ok(());
        }

        {
            let Some(state) = &mut self.search_picker else {
                return Ok(());
            };
            state.pending_preview = None;
            if state.selection_page != Some(pending.page) {
                return Ok(());
            }
        }
        if pending.page == self.tab().page {
            return Ok(());
        }
        let origin = ViewPosition {
            page: self.tab().page,
            scroll_x: self.tab().scroll_x,
            scroll_y: self.tab().scroll_y,
            center_content: self.tab().center_content,
        };
        let state = self.search_picker.as_mut().expect("search picker state");
        state.preview_origin.get_or_insert(origin);
        state.page = pending.page;

        let tab = self.tab_mut();
        tab.page = pending.page;
        tab.scroll_x = 0;
        tab.scroll_y = 0;
        tab.center_content = true;
        self.request_current(output)
    }

    fn handle_link_picker_pointer(
        &mut self,
        mouse: MouseEvent,
        output: &mut impl Write,
    ) -> Result<bool, AppError> {
        let area = link_picker_area(self.viewport()?);
        let links = self.tab().link_index.links.clone();
        let outline = Arc::clone(&self.tab().outline);
        let Some(state) = self.link_picker.as_ref() else {
            return Ok(false);
        };
        let Some(selected) = link_picker_link_at_position(
            area,
            LinkPickerDocument::new(&links, &outline),
            state,
            self.link_picker_geometry,
            mouse.column,
            mouse.row,
        ) else {
            return Ok(false);
        };
        if state.selected == selected && state.focus == LinkPickerFocus::Links {
            if state.pending_preview.is_none() && state.preview_origin.is_none() {
                self.schedule_link_preview();
            }
            return Ok(true);
        }
        let state = self.link_picker.as_mut().expect("link picker state");
        state.focus = LinkPickerFocus::Links;
        state.select(selected, &links);
        self.schedule_link_preview();
        self.redraw_link_picker(output)?;
        Ok(true)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let scroll = match mouse.kind {
            MouseEventKind::ScrollUp => Some((Axis::Vertical, false)),
            MouseEventKind::ScrollDown => Some((Axis::Vertical, true)),
            MouseEventKind::ScrollLeft => Some((Axis::Horizontal, false)),
            MouseEventKind::ScrollRight => Some((Axis::Horizontal, true)),
            _ => None,
        };
        if let Some((axis, forward)) = scroll {
            if self.link_picker.is_none()
                && self.search_picker.is_none()
                && self.search_input.is_none()
                && self.goto_input.is_none()
            {
                self.move_view(axis, forward, false, axis == Axis::Vertical, output)?;
            }
            return Ok(());
        }
        if self.link_picker.is_some()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.handle_link_picker_pointer(mouse, output)?
        {
            return Ok(());
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let Some((frame, placement, image_top)) = self.page_at_mouse(mouse, viewport) else {
            return Ok(());
        };
        let Some(target) = link_at_cell(
            &frame.links,
            placement,
            frame.width,
            frame.height,
            image_top,
            mouse.column,
            mouse.row,
        ) else {
            self.draw_status(output, viewport, "no link here")?;
            return Ok(());
        };
        if let Some(state) = &mut self.link_picker
            && matches!(&target, LinkTarget::Internal { .. })
        {
            state.pending_preview = None;
            state.preview_origin = None;
        }
        if self.link_picker.is_some() && !self.persistent_link_picker {
            self.close_link_picker(output)?;
        }
        self.follow_link(target, output)
    }

    fn follow_link(&mut self, target: LinkTarget, output: &mut impl Write) -> Result<(), AppError> {
        match target {
            LinkTarget::Internal {
                page,
                top_ratio,
                left_ratio,
            } => {
                let current = ViewPosition {
                    page: self.tab().page,
                    scroll_x: self.tab().scroll_x,
                    scroll_y: self.tab().scroll_y,
                    center_content: self.tab().center_content,
                };
                let page = page.min(self.tab().page_count.saturating_sub(1));
                let tab = self.tab_mut();
                if tab.link_history.len() == 100 {
                    tab.link_history.remove(0);
                }
                tab.link_history.push(current);
                tab.page = page;
                tab.center_content = true;
                if top_ratio.is_some() {
                    tab.scroll_y = 0;
                }
                tab.pending_destination = Some(LinkDestination {
                    page,
                    top_ratio,
                    left_ratio,
                });
                self.request_current(output)?;
            }
            LinkTarget::Uri(uri) => {
                if uri.trim().is_empty() {
                    self.draw_status(output, self.viewport()?, "empty link")?;
                } else {
                    write_clipboard_osc52(output, &uri)?;
                    self.draw_status(output, self.viewport()?, "copied link to clipboard")?;
                }
            }
        }
        Ok(())
    }

    fn follow_link_back(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(previous) = self.tab_mut().link_history.pop() else {
            self.draw_status(output, self.viewport()?, "no link history")?;
            return Ok(());
        };
        let tab = self.tab_mut();
        tab.page = previous.page.min(tab.page_count.saturating_sub(1));
        tab.scroll_x = previous.scroll_x;
        tab.scroll_y = previous.scroll_y;
        tab.center_content = previous.center_content;
        tab.pending_destination = None;
        self.request_current(output)
    }

    fn open_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.start_link_index();
        let viewport = self.viewport()?;
        let Some(image) = self.picker_image(viewport) else {
            self.draw_status(output, viewport, "page is still rendering")?;
            return Ok(());
        };
        let page = self
            .visible_page
            .as_ref()
            .expect("picker has a visible image")
            .frame
            .key
            .page;
        self.retain_primary_image(output)?;

        self.pending_link_picker_open = false;
        self.link_picker = Some(LinkPickerState::new(page));
        show_link_picker_split(
            output,
            link_picker_area(viewport),
            image,
            self.link_picker_geometry,
            self.theme,
        )?;
        self.redraw_link_picker(output)
    }

    fn open_pending_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if !self.pending_link_picker_open
            || !self.link_mode
            || self.link_picker.is_some()
            || self.session.pending_open.is_some()
        {
            return Ok(());
        }
        let viewport = self.viewport()?;
        if self.picker_image(viewport).is_none() {
            return Ok(());
        }
        self.open_link_picker(output)
    }

    fn redraw_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(_) = self.link_picker else {
            return Ok(());
        };
        let viewport = self.viewport()?;
        let page = self
            .current_visible_page(viewport)
            .map_or(self.tab().page, |page| page.frame.key.page);
        let links = self.tab().link_index.links.clone();
        let outline = Arc::clone(&self.tab().outline);
        let progress = LinkIndexProgress::from(&self.tab().link_index);
        let state = self.link_picker.as_mut().expect("link picker state");
        let selection_before = state.selection_key;
        state.sync(page, &links, progress.indexing);
        let schedule_preview =
            selection_before != state.selection_key && state.focus == LinkPickerFocus::Links;
        let state = state.clone();
        if schedule_preview {
            self.schedule_link_preview();
        }
        draw_link_picker_terminal(
            output,
            link_picker_area(viewport),
            LinkPickerDocument::new(&links, &outline),
            &state,
            progress,
            self.link_picker_geometry,
            self.theme,
        )?;
        Ok(())
    }

    fn set_link_picker_layout(
        &mut self,
        layout: LinkPickerLayout,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.link_picker.is_none() && self.search_picker.is_none() {
            return Ok(());
        }
        if self.link_picker_geometry.layout == layout {
            return self.redraw_active_side_picker(output);
        }
        let viewport = self.viewport()?;
        let area = link_picker_area(viewport);
        let previous = self.link_picker_geometry;
        let image = self.picker_image(viewport);
        self.link_picker_geometry.layout = layout;
        if layout == LinkPickerLayout::Floating
            && let Some(state) = &mut self.link_picker
        {
            state.focus = LinkPickerFocus::Links;
        }
        if layout == LinkPickerLayout::Floating
            && let Some(state) = &mut self.search_picker
        {
            state.focus = LinkPickerFocus::Links;
        }
        if let Some(image) = image {
            restore_link_picker_split(output, area, image, previous, self.theme)?;
            show_link_picker_split(output, area, image, self.link_picker_geometry, self.theme)?;
            self.redraw_active_side_picker(output)
        } else {
            self.reset_render_state();
            self.request_current(output)
        }
    }

    fn cycle_link_picker_layout(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let area = link_picker_area(self.viewport()?);
        let layout = next_link_picker_layout(area, self.link_picker_geometry.layout);
        self.set_link_picker_layout(layout, output)
    }

    fn redraw_active_side_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.is_some() {
            self.redraw_link_picker(output)
        } else {
            self.redraw_search_picker(output)
        }
    }

    fn close_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(state) = self.link_picker.take() else {
            return Ok(());
        };
        if let Some(origin) = state.preview_origin {
            let tab = self.tab_mut();
            tab.page = origin.page.min(tab.page_count.saturating_sub(1));
            tab.scroll_x = origin.scroll_x;
            tab.scroll_y = origin.scroll_y;
            tab.center_content = origin.center_content;
            tab.pending_destination = None;
            self.clear_viewer(output)?;
            self.reset_render_state();
            self.request_current(output)?;
            return Ok(());
        }
        self.request_current(output)
    }

    fn close_link_picker_and_exit_link_mode(
        &mut self,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.close_link_picker(output)?;
        if self.link_mode {
            self.set_link_mode(false, output)?;
        }
        Ok(())
    }

    fn open_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.retain_primary_image(output)?;
        let viewport = self.viewport()?;
        if let Some(image) = self.picker_image(viewport) {
            show_link_picker_split(
                output,
                link_picker_area(viewport),
                image,
                self.link_picker_geometry,
                self.theme,
            )?;
            self.redraw_search_picker(output)
        } else {
            self.reset_render_state();
            self.request_current(output)
        }
    }

    fn redraw_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(_) = self.search_picker else {
            return Ok(());
        };
        let viewport = self.viewport()?;
        let page = self
            .current_visible_page(viewport)
            .map_or(self.tab().page, |page| page.frame.key.page);
        let search = self.tab().search.clone();
        let outline = Arc::clone(&self.tab().outline);
        let state = self.search_picker.as_mut().expect("search picker state");
        state.sync(page, &search.matches);
        let state = state.clone();
        draw_search_picker_terminal(
            output,
            link_picker_area(viewport),
            &search,
            &outline,
            &state,
            self.link_picker_geometry,
            self.theme,
        )?;
        Ok(())
    }

    fn close_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(state) = self.search_picker.take() else {
            return Ok(());
        };
        if let Some(origin) = state.preview_origin {
            let tab = self.tab_mut();
            tab.page = origin.page.min(tab.page_count.saturating_sub(1));
            tab.scroll_x = origin.scroll_x;
            tab.scroll_y = origin.scroll_y;
            tab.center_content = origin.center_content;
            tab.pending_destination = None;
            self.clear_viewer(output)?;
            self.reset_render_state();
            self.request_current(output)?;
            return Ok(());
        }
        self.request_current(output)
    }

    fn handle_search_picker_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let area = link_picker_area(self.viewport()?);
        let layout = resolved_link_picker_layout(area, self.link_picker_geometry.layout);
        if let Some(focus) = link_picker_focus_for_key(
            self.search_picker
                .as_ref()
                .expect("search picker state")
                .focus,
            layout,
            key,
        ) {
            let state = self.search_picker.as_mut().expect("search picker state");
            state.focus = focus;
            if focus == LinkPickerFocus::Document {
                state.pending_preview = None;
            } else {
                self.schedule_search_preview();
            }
            return self.redraw_search_picker(output);
        }
        match key.code {
            KeyCode::Esc => return self.close_search_picker(output),
            KeyCode::Char('/') => {
                self.close_search_picker(output)?;
                return self.begin_search(output);
            }
            KeyCode::Char('s') => return self.cycle_link_picker_layout(output),
            KeyCode::Char('a') => {
                return self.set_link_picker_layout(LinkPickerLayout::Auto, output);
            }
            _ => {}
        }
        if self
            .search_picker
            .as_ref()
            .is_some_and(|state| state.focus == LinkPickerFocus::Document)
        {
            return self.handle_link_picker_document_key(key, output);
        }

        let visible_height = link_picker_visible_height(area, self.link_picker_geometry);
        let matches = self.tab().search.matches.clone();
        let selected = self
            .search_picker
            .as_ref()
            .expect("search picker state")
            .selected;
        if let Some(next) =
            link_picker_navigation_index(selected, matches.len(), key, visible_height)
        {
            let state = self.search_picker.as_mut().expect("search picker state");
            state.selected = next;
            state.selection_page = matches.get(next).map(|result| result.page);
            self.schedule_search_preview();
            self.redraw_search_picker(output)?;
        } else if key.code == KeyCode::Enter
            && let Some(result) = matches.get(selected)
        {
            let state = self.search_picker.as_mut().expect("search picker state");
            state.pending_preview = None;
            state.preview_origin = None;
            self.set_page(result.page, output)?;
        }
        Ok(())
    }

    fn handle_link_picker_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let filtering = self
            .link_picker
            .as_ref()
            .is_some_and(|state| state.filtering);
        let has_filter = self
            .link_picker
            .as_ref()
            .is_some_and(|state| !state.filter.is_empty());
        let area = link_picker_area(self.viewport()?);
        let layout = resolved_link_picker_layout(area, self.link_picker_geometry.layout);
        if !filtering
            && let Some(focus) = link_picker_focus_for_key(
                self.link_picker.as_ref().expect("link picker state").focus,
                layout,
                key,
            )
        {
            let state = self.link_picker.as_mut().expect("link picker state");
            state.focus = focus;
            if focus == LinkPickerFocus::Document {
                state.pending_preview = None;
            } else {
                self.schedule_link_preview();
            }
            return self.redraw_link_picker(output);
        }
        match key.code {
            KeyCode::Esc if self.link_mode => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Esc if filtering || has_filter => {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.filter.clear();
                state.filtering = false;
                state.number_input.clear();
                state.awaiting_current_page = true;
                return self.redraw_link_picker(output);
            }
            KeyCode::Esc => return self.close_link_picker(output),
            KeyCode::Char('/') if !filtering && !control && !alt => {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.filtering = true;
                state.focus = LinkPickerFocus::Links;
                state.number_input.clear();
                return self.redraw_link_picker(output);
            }
            KeyCode::Char('s') if !filtering => return self.cycle_link_picker_layout(output),
            KeyCode::Char('a') if !filtering => {
                return self.set_link_picker_layout(LinkPickerLayout::Auto, output);
            }
            KeyCode::Char('L') if !filtering => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Char('q') if !filtering => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Char('b') if !filtering && !control && !alt => {
                self.follow_link_back(output)?;
                return Ok(());
            }
            _ => {}
        }

        if self
            .link_picker
            .as_ref()
            .is_some_and(|state| state.focus == LinkPickerFocus::Document)
        {
            return self.handle_link_picker_document_key(key, output);
        }

        let viewport = self.viewport()?;
        let links = self.tab().link_index.links.clone();
        let indexing = self.tab().link_index.indexing;
        let page = self
            .current_visible_page(viewport)
            .map_or(self.tab().page, |page| page.frame.key.page);
        let visible_height =
            link_picker_visible_height(link_picker_area(viewport), self.link_picker_geometry);
        let state = self.link_picker.as_mut().expect("link picker state");
        state.sync(page, &links, indexing);
        let selected_before = state.selected;

        let input_consumed = if state.filtering {
            match key.code {
                KeyCode::Enter => {
                    state.filtering = false;
                    false
                }
                KeyCode::Backspace => {
                    state.filter.pop();
                    true
                }
                KeyCode::Char(character) if !control && !alt => {
                    state.filter.push(character);
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        let filtered = filter_document_links(&links, &state.filter);
        if !filtered.contains(&state.selected)
            && let Some(first) = filtered.first().copied()
        {
            state.select(first, &links);
        }
        let selected_position = filtered
            .iter()
            .position(|index| *index == state.selected)
            .unwrap_or(0);
        let navigation = (!input_consumed)
            .then(|| {
                link_picker_navigation_index(selected_position, filtered.len(), key, visible_height)
            })
            .flatten();
        let mut explicit_selection = navigation.is_some();
        let mut redraw = navigation.is_some();
        let mut target = None;
        if input_consumed {
            redraw = true;
        } else if let Some(selected) = navigation {
            state.select(filtered[selected], &links);
            state.number_input.clear();
        } else {
            match key.code {
                KeyCode::Enter => {
                    target = filtered
                        .get(selected_position)
                        .and_then(|index| links.get(*index))
                        .map(|link| link.target.clone());
                    state.number_input.clear();
                    redraw = false;
                }
                KeyCode::Backspace => {
                    state.number_input.pop();
                    if let Some(index) = link_number_index(&state.number_input, filtered.len()) {
                        state.select(filtered[index], &links);
                        explicit_selection = true;
                    }
                    redraw = true;
                }
                KeyCode::Char(digit) if digit.is_ascii_digit() => {
                    if let Some(index) =
                        update_link_number_selection(&mut state.number_input, digit, filtered.len())
                    {
                        state.select(filtered[index], &links);
                        explicit_selection = true;
                    }
                    redraw = true;
                }
                _ => redraw = false,
            }
        }
        let selection_changed = state.selected != selected_before;

        if let Some(target) = target {
            if matches!(&target, LinkTarget::Internal { .. }) {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.pending_preview = None;
                state.preview_origin = None;
            }
            if !self.persistent_link_picker {
                self.close_link_picker(output)?;
            }
            self.follow_link(target, output)?;
        } else if redraw {
            if selection_changed || explicit_selection {
                self.schedule_link_preview();
            }
            self.redraw_link_picker(output)?;
        }
        Ok(())
    }

    fn handle_link_picker_document_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if matches!(
            key.code,
            KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::Up
                | KeyCode::Char('k')
                | KeyCode::PageDown
                | KeyCode::Char(' ')
                | KeyCode::PageUp
                | KeyCode::Backspace
                | KeyCode::Right
                | KeyCode::Left
                | KeyCode::Char('g')
                | KeyCode::Home
                | KeyCode::Char('G')
                | KeyCode::End
                | KeyCode::Char('m')
                | KeyCode::Char('i')
        ) {
            if let Some(state) = self.link_picker.as_mut() {
                state.pending_preview = None;
                state.preview_origin = None;
            }
            if let Some(state) = self.search_picker.as_mut() {
                state.pending_preview = None;
                state.preview_origin = None;
            }
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_view(Axis::Vertical, true, false, true, output)?
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_view(Axis::Vertical, false, false, true, output)?
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.move_view(Axis::Vertical, true, true, true, output)?
            }
            KeyCode::PageUp | KeyCode::Backspace => {
                self.move_view(Axis::Vertical, false, true, true, output)?
            }
            KeyCode::Right => self.move_view(Axis::Horizontal, true, false, true, output)?,
            KeyCode::Left => self.move_view(Axis::Horizontal, false, false, true, output)?,
            KeyCode::Char('g') | KeyCode::Home => self.set_page_vertically(0, output)?,
            KeyCode::Char('G') | KeyCode::End => {
                self.set_page_vertically(self.tab().page_count.saturating_sub(1), output)?
            }
            KeyCode::Char('m') => self.cycle_fit(output)?,
            KeyCode::Char('i') => self.toggle_invert(output)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
        if self.search_picker.is_some() {
            self.handle_search_picker_key(key, output)?;
            return Ok(false);
        }
        if self.link_picker.is_some() {
            self.handle_link_picker_key(key, output)?;
            return Ok(false);
        }
        if self.search_input.is_some() {
            self.handle_search_key(key, output)?;
            return Ok(false);
        }
        if self.goto_input.is_some() {
            self.handle_goto_key(key, output)?;
            return Ok(false);
        }
        if let Some(index) = numbered_tab_index(key) {
            self.select_tab(index, output)?;
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('?') => self.open_help(output)?,
            KeyCode::Char('q') if self.link_mode => self.set_link_mode(false, output)?,
            KeyCode::Char('q') => return self.close_current(output),
            KeyCode::Char(':') => self.begin_goto(output)?,
            KeyCode::Char('/') => self.begin_search(output)?,
            KeyCode::Char('n') => self.navigate_search(true, output)?,
            KeyCode::Char('N') => self.navigate_search(false, output)?,
            KeyCode::Char('L') => self.toggle_link_mode(output)?,
            KeyCode::Char('b') => self.follow_link_back(output)?,
            KeyCode::Enter => self.open_link_picker(output)?,
            KeyCode::Esc if self.link_mode => self.set_link_mode(false, output)?,
            KeyCode::Esc if self.clear_search(output)? => {}
            KeyCode::Esc => return Ok(true),
            KeyCode::Char('f') => self.open_picker(output)?,
            KeyCode::Char('D')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.duplicate_tab(output)?
            }
            KeyCode::Tab => self.switch_tab(1, output)?,
            KeyCode::BackTab => self.switch_tab(-1, output)?,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_view(Axis::Vertical, true, false, true, output)?
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_view(Axis::Vertical, false, false, true, output)?
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.move_view(Axis::Vertical, true, true, true, output)?
            }
            KeyCode::PageUp | KeyCode::Backspace => {
                self.move_view(Axis::Vertical, false, true, true, output)?
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_view(Axis::Horizontal, true, false, true, output)?
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_view(Axis::Horizontal, false, false, true, output)?
            }
            KeyCode::Char('g') | KeyCode::Home => self.set_page_vertically(0, output)?,
            KeyCode::Char('G') | KeyCode::End => {
                self.set_page_vertically(self.tab().page_count.saturating_sub(1), output)?
            }
            KeyCode::Char('m') => self.cycle_fit(output)?,
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_in(output)?,
            KeyCode::Char('-') | KeyCode::Char('_') => self.zoom_out(output)?,
            KeyCode::Char('0') => self.reset_zoom(output)?,
            KeyCode::Char('i') => self.toggle_invert(output)?,
            KeyCode::Char('S')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.toggle_smooth_scroll(output)?
            }
            KeyCode::Char('s') if key.modifiers == KeyModifiers::SHIFT => {
                self.toggle_smooth_scroll(output)?
            }
            KeyCode::Char('p') => self.toggle_performance(output)?,
            KeyCode::Char('t') => self.open_outline(output)?,
            KeyCode::Char('T') => self.open_theme_picker(output)?,
            KeyCode::Char('y') => self.request_copy(output)?,
            _ => {}
        }
        Ok(false)
    }

    fn set_page(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        self.tab_mut().scroll_x = 0;
        self.set_page_vertically(page, output)
    }

    fn set_page_vertically(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        let page = page.min(self.tab().page_count.saturating_sub(1));
        let tab = self.tab_mut();
        tab.page = page;
        tab.scroll_y = 0;
        tab.pending_destination = None;
        tab.center_content = true;
        self.request_current(output)
    }

    /// Vertical movement crosses page boundaries without discarding the remainder.
    fn move_view(
        &mut self,
        axis: Axis,
        forward: bool,
        large: bool,
        allow_page_step: bool,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        if self.viewer.continuous_scroll
            && axis == Axis::Vertical
            && self.link_picker.is_none()
            && self.search_picker.is_none()
        {
            let pixels = if large {
                u32::from(viewport.pixel_height) * self.viewer.page_scroll_percent as u32 / 100
            } else {
                u32::from(viewport.pixel_height) * self.viewer.scroll_step_percent as u32 / 100
            };
            let step = i64::from(pixels.max(1));
            self.smooth_scroll_remaining += if forward { step } else { -step };
            if !self.viewer.smooth_scroll {
                self.pending_vertical_scroll += std::mem::take(&mut self.smooth_scroll_remaining);
                if self.apply_vertical_scroll(viewport)? {
                    self.request_current(output)?;
                }
            }
            return Ok(());
        }
        let key = self.render_key(viewport);
        let Some(frame) = self.tab().cache.get(&key).cloned() else {
            return if allow_page_step {
                self.page_step(forward, output)
            } else {
                Ok(())
            };
        };
        let (max_x, max_y) = viewport.max_scroll(frame.width, frame.height);
        let mut min_x = 0_i64;
        let mut max_x = i64::from(max_x);
        if self.viewer.continuous_scroll
            && self.link_picker.is_none()
            && self.search_picker.is_none()
        {
            for page in &self.visible_pages {
                let origin = content_horizontal_scroll(0, &frame, &page.frame, viewport);
                min_x = min_x.min(-origin);
                max_x = max_x.max(
                    i64::from(
                        page.frame
                            .width
                            .saturating_sub(u32::from(viewport.pixel_width)),
                    ) - origin,
                );
            }
        }
        let (axis_min, axis_max, current) = match axis {
            Axis::Vertical => (0, i64::from(max_y), i64::from(self.tab().scroll_y)),
            Axis::Horizontal => (min_x, max_x, self.tab().scroll_x),
        };
        if axis_min == axis_max {
            return if allow_page_step {
                self.page_step(forward, output)
            } else {
                Ok(())
            };
        }

        let span = match axis {
            Axis::Vertical => u32::from(viewport.pixel_height),
            Axis::Horizontal => u32::from(viewport.pixel_width),
        };
        let step = if large {
            (span * 85 / 100).max(1)
        } else {
            (span / 8).max(1)
        };

        let next = if forward {
            if current >= axis_max {
                return if allow_page_step {
                    self.page_step(true, output)
                } else {
                    Ok(())
                };
            }
            (current + i64::from(step)).min(axis_max)
        } else {
            if current <= axis_min {
                return if allow_page_step {
                    self.page_step(false, output)
                } else {
                    Ok(())
                };
            }
            (current - i64::from(step)).max(axis_min)
        };
        match axis {
            Axis::Vertical => self.tab_mut().scroll_y = next as u32,
            Axis::Horizontal => self.tab_mut().scroll_x = next,
        }
        self.redraw_current(output)
    }

    fn input_wait(&self) -> Duration {
        let background_wait = Duration::from_millis(10);
        if self.smooth_scroll_remaining == 0 || self.pending_vertical_scroll != 0 {
            return background_wait;
        }
        Duration::from_millis(self.viewer.scroll_frame_ms)
            .saturating_sub(self.smooth_scroll_tick.elapsed())
            .min(background_wait)
    }

    fn poll_smooth_scroll(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.is_some()
            || self.search_picker.is_some()
            || self.search_input.is_some()
            || self.goto_input.is_some()
        {
            self.smooth_scroll_remaining = 0;
            return Ok(());
        }
        let now = Instant::now();
        if self.smooth_scroll_remaining == 0
            || self.pending_vertical_scroll != 0
            || now.duration_since(self.smooth_scroll_tick)
                < Duration::from_millis(self.viewer.scroll_frame_ms)
        {
            return Ok(());
        }
        self.smooth_scroll_tick = now;
        let viewport = self.viewport()?;
        // Ease toward the accumulated wheel/key target without queuing animations.
        let remaining = self.smooth_scroll_remaining;
        let step = remaining
            .unsigned_abs()
            .div_ceil(self.viewer.scroll_ease_divisor) as i64
            * remaining.signum();
        self.smooth_scroll_remaining -= step;
        self.pending_vertical_scroll = step;
        if self.apply_vertical_scroll(viewport)? {
            let key = self.render_key(viewport);
            if self.desired_key == Some(key)
                && let Some(frame) = self.tab().cache.get(&key).cloned()
            {
                // An offset-only tick needs placement, not a new render generation
                // or another scan of the prefetch window.
                self.draw_frame(&frame, viewport, output)?;
            } else {
                let rest = self.smooth_scroll_remaining;
                self.request_current(output)?;
                self.smooth_scroll_remaining = rest;
            }
        }
        Ok(())
    }

    fn page_key(&self, page: u32, viewport: Viewport) -> RenderKey {
        RenderKey {
            page,
            search_request_id: self.tab().search.highlight_request_id(page),
            selected_link_ordinal: None,
            ..self.render_key(viewport)
        }
    }

    fn request_visible_page(&mut self, key: RenderKey) -> Result<(), AppError> {
        if self.pending.insert(key) {
            self.worker
                .render(RenderRequest {
                    key,
                    generation: self.generation,
                })
                .map_err(AppError::Renderer)?;
        }
        Ok(())
    }

    fn apply_vertical_scroll(&mut self, viewport: Viewport) -> Result<bool, AppError> {
        let cell = (u32::from(viewport.pixel_height) / u32::from(viewport.rows)).max(1);
        while self.pending_vertical_scroll != 0 {
            let page = self.tab().page;
            let key = self.page_key(page, viewport);
            let Some(frame) = self.tab().cache.get(&key) else {
                self.request_visible_page(key)?;
                return Ok(false);
            };
            let offset = i64::from(self.tab().scroll_y);
            let delta = self.pending_vertical_scroll;
            if delta > 0 {
                if page + 1 >= self.tab().page_count {
                    self.tab_mut().scroll_y = (offset + delta).min(i64::from(
                        frame
                            .height
                            .saturating_sub(u32::from(viewport.pixel_height)),
                    )) as u32;
                    self.pending_vertical_scroll = 0;
                } else {
                    let span = i64::from(frame.height) + i64::from(cell);
                    if offset + delta < span {
                        self.tab_mut().scroll_y = (offset + delta) as u32;
                        self.pending_vertical_scroll = 0;
                    } else {
                        if frame.key.fit == FitMode::Content {
                            let next = self.page_key(page + 1, viewport);
                            let Some(next) = self.tab().cache.get(&next) else {
                                self.request_visible_page(next)?;
                                return Ok(false);
                            };
                            let scroll_x = content_horizontal_scroll(
                                self.tab().scroll_x,
                                frame,
                                next,
                                viewport,
                            );
                            self.tab_mut().scroll_x = scroll_x;
                        }
                        self.pending_vertical_scroll = offset + delta - span;
                        self.tab_mut().page += 1;
                        self.tab_mut().scroll_y = 0;
                    }
                }
            } else if offset + delta >= 0 || page == 0 {
                self.tab_mut().scroll_y = (offset + delta).max(0) as u32;
                self.pending_vertical_scroll = 0;
            } else {
                let previous = self.page_key(page - 1, viewport);
                let Some(previous) = self.tab().cache.get(&previous) else {
                    self.request_visible_page(previous)?;
                    return Ok(false);
                };
                let span = previous.height + cell;
                if frame.key.fit == FitMode::Content {
                    let scroll_x =
                        content_horizontal_scroll(self.tab().scroll_x, frame, previous, viewport);
                    self.tab_mut().scroll_x = scroll_x;
                }
                self.pending_vertical_scroll += offset;
                self.tab_mut().page -= 1;
                self.tab_mut().scroll_y = span;
            }
        }
        Ok(true)
    }

    fn page_step(&mut self, forward: bool, output: &mut impl Write) -> Result<(), AppError> {
        let page = if forward {
            self.tab().page.saturating_add(1)
        } else {
            self.tab().page.saturating_sub(1)
        };
        self.set_page(page, output)
    }

    /// Redraws the current page from cache with the current scroll offset,
    /// without asking the worker to render again.
    fn redraw_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        let key = self.render_key(viewport);
        if let Some(frame) = self.tab().cache.get(&key).cloned() {
            self.draw_frame(&frame, viewport, output)?;
        }
        Ok(())
    }

    fn request_copy(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let (document_id, page) = {
            let tab = self.tab();
            (tab.document_id, tab.page)
        };
        let _ = self.worker.extract_text(document_id, page);
        let viewport = self.viewport()?;
        self.draw_status(output, viewport, "copying page text...")?;
        Ok(())
    }

    fn copy_text(&mut self, content: &str, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        if content.trim().is_empty() {
            self.draw_status(output, viewport, "no selectable text on this page")?;
            return Ok(());
        }
        write_clipboard_osc52(output, content)?;
        let message = format!(
            "copied {} characters to the clipboard",
            content.chars().count()
        );
        self.draw_status(output, viewport, &message)?;
        Ok(())
    }

    fn cycle_fit(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.set_view_scale(self.tab().fit.cycle(), self.tab().zoom, output)
    }

    fn set_zoom(&mut self, zoom: u16, output: &mut impl Write) -> Result<(), AppError> {
        self.set_view_scale(self.tab().fit, zoom, output)
    }

    fn set_view_scale(
        &mut self,
        fit: FitMode,
        zoom: u16,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let old_zoom = self.tab().zoom;
        if zoom == old_zoom && fit == self.tab().fit {
            return Ok(());
        }
        // Anchor to the displayed position, not an unfinished scroll target.
        self.smooth_scroll_remaining = 0;
        self.pending_vertical_scroll = 0;
        if fit == FitMode::Content && self.tab().fit != FitMode::Content {
            let tab = self.tab_mut();
            tab.fit = fit;
            tab.zoom = zoom;
            tab.scroll_x = 0;
            tab.scroll_y = 0;
            tab.center_content = true;
            return self.request_current(output);
        }
        let viewport = self.viewport()?;
        let tab = self.tab();
        let current_key = self.render_key(viewport);
        let page_frame = |page| {
            let key = self.page_key(page, viewport);
            let frame = tab
                .cache
                .get(&key)
                .map(Arc::as_ref)
                .or_else(|| {
                    self.visible_pages
                        .iter()
                        .find(|visible| {
                            visible.frame.key.page == page
                                && visible.frame.key.fit == tab.fit
                                && visible.frame.key.width == viewport.pixel_width
                                && visible.frame.key.height == viewport.pixel_height
                                && visible.frame.revision == tab.revision
                        })
                        .map(|visible| visible.frame.as_ref())
                })
                .or_else(|| {
                    tab.cache
                        .values()
                        .find(|frame| {
                            frame.key.page == page
                                && frame.key.width == current_key.width
                                && frame.key.height == current_key.height
                                && frame.key.fit == current_key.fit
                                && frame.revision == tab.revision
                        })
                        .map(Arc::as_ref)
                })?;
            if frame.key.document_id != tab.document_id
                || frame.key.fit != tab.fit
                || frame.key.width != viewport.pixel_width
                || frame.key.height != viewport.pixel_height
                || frame.revision != tab.revision
            {
                return None;
            }
            Some(frame)
        };
        let page_size = |page| {
            page_frame(page).map(|frame| {
                let (width, height) = if tab.fit == FitMode::Content {
                    fitted_page_size(
                        viewport,
                        frame.page_width_pt,
                        frame.page_height_pt,
                        frame.content_bounds,
                        tab.fit,
                        old_zoom,
                    )
                } else {
                    (
                        scale_zoom(frame.width, frame.key.zoom, old_zoom),
                        scale_zoom(frame.height, frame.key.zoom, old_zoom),
                    )
                };
                let origin = (tab.fit == FitMode::Content).then(|| {
                    content_axis_scroll(
                        frame.content_bounds[0],
                        frame.content_bounds[2],
                        width,
                        viewport.pixel_width,
                    )
                });
                (width, height, origin)
            })
        };
        let position = centered_scaled_view(
            viewport,
            tab.page,
            tab.page_count,
            (tab.scroll_x, tab.scroll_y),
            |page, width, height| {
                let frame = page_frame(page)?;
                let (width, height) = if fit == tab.fit && fit != FitMode::Content {
                    (
                        scale_zoom(width, old_zoom, zoom),
                        scale_zoom(height, old_zoom, zoom),
                    )
                } else {
                    fitted_page_size(
                        viewport,
                        frame.page_width_pt,
                        frame.page_height_pt,
                        frame.content_bounds,
                        fit,
                        zoom,
                    )
                };
                let origin = (fit == FitMode::Content).then(|| {
                    content_axis_scroll(
                        frame.content_bounds[0],
                        frame.content_bounds[2],
                        width,
                        viewport.pixel_width,
                    )
                });
                Some((width, height, origin))
            },
            self.viewer.continuous_scroll
                && self.link_picker.is_none()
                && self.search_picker.is_none(),
            page_size,
        );
        let (page, scroll_x, scroll_y) = match position {
            Ok(position) => position,
            Err(missing_page) => {
                let key = self.page_key(missing_page, viewport);
                self.pending_scale = Some((key, fit, zoom));
                self.request_visible_page(key)?;
                self.draw_status(output, viewport, "loading page geometry")?;
                return Ok(());
            }
        };
        let tab = self.tab_mut();
        tab.page = page;
        tab.scroll_x = scroll_x;
        tab.scroll_y = scroll_y;
        tab.zoom = zoom;
        tab.fit = fit;
        self.request_current(output)
    }

    fn zoom_in(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let zoom = stepped_zoom(self.tab().zoom, true);
        self.set_zoom(zoom, output)
    }

    fn zoom_out(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let zoom = stepped_zoom(self.tab().zoom, false);
        self.set_zoom(zoom, output)
    }

    fn reset_zoom(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.set_zoom(ZOOM_DEFAULT, output)
    }

    fn toggle_invert(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let inverted = !self.tab().invert;
        self.tab_mut().invert = inverted;
        self.request_current(output)
    }

    fn toggle_smooth_scroll(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.viewer.smooth_scroll = !self.viewer.smooth_scroll;
        if !self.viewer.smooth_scroll && self.smooth_scroll_remaining != 0 {
            self.pending_vertical_scroll += std::mem::take(&mut self.smooth_scroll_remaining);
            if self.apply_vertical_scroll(self.viewport()?)? {
                self.request_current(output)?;
            }
        }
        let state = if self.viewer.smooth_scroll {
            "smooth scroll on"
        } else {
            "smooth scroll off"
        };
        self.draw_status(output, self.viewport()?, state)?;
        Ok(())
    }

    fn toggle_performance(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.show_performance = !self.show_performance;
        let Some(snapshot) = self.performance_snapshot else {
            return Ok(());
        };
        let state = snapshot.status(self.show_performance, self.link_mode);
        self.draw_status(output, self.viewport()?, &state)?;
        Ok(())
    }

    fn request_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
        self.pending_scale = None;
        self.missing_visible_page = None;
        self.status_line.clear();
        if self.viewer.set_window_title && self.title_document != Some(self.tab().document_id) {
            let title: String = self
                .tab()
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            execute!(output, crossterm::terminal::SetTitle(title))?;
            self.title_document = Some(self.tab().document_id);
        }
        let viewport = self.prepare_viewport(output)?;
        self.draw_tab_bar(output)?;
        let key = self.render_key(viewport);
        self.desired_key = Some(key);
        self.generation = self.generation.wrapping_add(1);
        self.worker.begin_generation(self.generation);
        self.pending.clear();
        self.performance_snapshot = None;

        if let Some(frame) = self.tab().cache.get(&key).cloned() {
            self.position_content_frame(&frame, viewport);
            self.draw_frame(&frame, viewport, output)?;
            self.prefetch_neighbors(key);
        } else {
            self.draw_status(output, viewport, "rendering")?;
            if self.pending.insert(key) {
                self.worker
                    .render(RenderRequest {
                        key,
                        generation: self.generation,
                    })
                    .map_err(AppError::Renderer)?;
            }
        }
        Ok(())
    }

    fn store_frame(&mut self, frame: Frame) -> Option<(usize, Arc<Frame>)> {
        if frame.generation != self.generation {
            return None;
        }
        let index = self.tab_index(frame.key.document_id)?;
        let tab = &self.session.tabs[index];
        if frame.key.page >= tab.page_count
            || frame.revision != tab.revision
            || frame.key.search_request_id != tab.search.highlight_request_id(frame.key.page)
        {
            return None;
        }
        let key = frame.key;
        self.pending.remove(&key);
        let frame = Arc::new(frame);
        let current_page = self.session.tabs[index].page;
        // Keep one on-demand neighbor even with prefetch disabled: continuous
        // scrolling may need its dimensions before it becomes visible.
        // Config validation bounds prefetch_pages at u32::MAX.
        let cache_radius = u32::try_from(self.viewer.prefetch_pages)
            .expect("validated prefetch_pages exceeds u32")
            .max(1)
            .max(
                self.pending_scale
                    .filter(|(wanted, _, _)| wanted.document_id == key.document_id)
                    .map_or(0, |(wanted, _, _)| wanted.page.abs_diff(current_page)),
            );
        let visible_end = self
            .visible_pages
            .last()
            .filter(|page| page.frame.key.document_id == key.document_id)
            .map_or(current_page, |page| page.frame.key.page)
            .max(current_page)
            .saturating_add(cache_radius);
        self.session.tabs[index]
            .cache
            .insert(key, Arc::clone(&frame));
        let tab = &mut self.session.tabs[index];
        let search = &tab.search;
        tab.cache.retain(|cached, _| {
            cached.search_request_id == search.highlight_request_id(cached.page)
                && cached.width == key.width
                && cached.height == key.height
                && cached.zoom == key.zoom
                && cached.fit == key.fit
                && cached.invert == key.invert
                && cached.dark_mode_style == key.dark_mode_style
                && cached.page >= current_page.saturating_sub(cache_radius)
                && cached.page <= visible_end
                && (cached.selected_link_ordinal.is_none()
                    || cached.selected_link_ordinal == key.selected_link_ordinal)
        });
        Some((index, frame))
    }

    fn receive_frame(&mut self, frame: Frame, output: &mut impl Write) -> Result<(), AppError> {
        let Some((_index, frame)) = self.store_frame(frame) else {
            return Ok(());
        };
        let key = frame.key;
        if let Some((waiting, fit, zoom)) = self.pending_scale
            && waiting == key
        {
            self.pending_scale = None;
            self.set_view_scale(fit, zoom, output)?;
            return Ok(());
        }
        if self.desired_key == Some(key) {
            self.position_content_frame(&frame, self.viewport()?);
        }

        if key.document_id == self.tab().document_id && self.pending_vertical_scroll != 0 {
            if self.apply_vertical_scroll(self.viewport()?)? {
                let rest = self.smooth_scroll_remaining;
                self.request_current(output)?;
                self.smooth_scroll_remaining = rest;
            }
            return Ok(());
        }
        if self.viewer.continuous_scroll
            && self.link_picker.is_none()
            && self.search_picker.is_none()
            && self.desired_key != Some(key)
            && (self.missing_visible_page == Some(key)
                || self.visible_pages.iter().any(|page| page.frame.key == key))
        {
            self.redraw_current(output)?;
        }

        if self.desired_key == Some(key) {
            let viewport = self.viewport()?;
            let current_key = self.render_key(viewport);
            if current_key == key {
                self.draw_frame(&frame, viewport, output)?;
                self.prefetch_neighbors(key);
                self.open_pending_link_picker(output)?;
            }
        }
        Ok(())
    }

    fn position_content_frame(&mut self, frame: &Frame, viewport: Viewport) {
        if self.tab().fit != FitMode::Content || !self.tab().center_content {
            return;
        }
        let (scroll_x, scroll_y) = content_scroll(frame, viewport);
        let tab = self.tab_mut();
        tab.scroll_x = scroll_x;
        tab.scroll_y = scroll_y;
        tab.center_content = false;
    }

    fn draw_frame(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        synchronized_output(output, |output| {
            self.draw_frame_unsynchronized(frame, viewport, output)
        })
    }

    fn draw_frame_unsynchronized(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if let Some(destination) = self.tab_mut().pending_destination.take()
            && destination.page == frame.key.page
        {
            if let Some(left_ratio) = destination.left_ratio {
                let target_x = left_ratio * frame.width as f32;
                self.tab_mut().scroll_x = horizontal_scroll_to_reveal(
                    self.tab().scroll_x.max(0) as u32,
                    frame.width,
                    u32::from(viewport.pixel_width),
                    target_x,
                    target_x,
                ) as i64;
            }
            if let Some(top_ratio) = destination.top_ratio {
                self.tab_mut().scroll_y = (top_ratio * frame.height as f32).round() as u32;
            }
        }
        if self.viewer.continuous_scroll
            && self.link_picker.is_none()
            && self.search_picker.is_none()
        {
            return self.draw_continuous(frame, viewport, output);
        }
        self.retain_primary_image(output)?;
        let tab = self.tab();
        let placement = viewport.place(
            frame.width,
            frame.height,
            tab.scroll_x.max(0) as u32,
            tab.scroll_y,
        );
        self.tab_mut().scroll_x = i64::from(placement.scroll_x);
        self.tab_mut().scroll_y = placement.scroll_y;
        let image_id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        let original = PositionedImage {
            left: placement.left,
            top: viewport.top,
            placement: Placement {
                image_id,
                columns: placement.columns,
                rows: placement.rows,
                offset_y: 0,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: placement.crop,
            },
        };
        let positioned = if self.link_picker.is_some() || self.search_picker.is_some() {
            let area = link_picker_area(viewport);
            let (preview, _) = link_picker_panes(area, self.link_picker_geometry);
            position_link_picker_image(
                LinkPickerImage::new(image_id, frame, placement, viewport),
                preview,
                self.link_picker_geometry.layout,
            )
        } else {
            original
        };

        self.prepare_image_canvas(output, viewport)?;
        execute!(output, MoveTo(positioned.left, positioned.top))?;
        let transfer_started = Instant::now();
        kitty::transmit_compressed_rgba(
            output,
            &frame.compressed_rgba,
            frame.width,
            frame.height,
            positioned.placement,
        )?;
        let transfer_elapsed = transfer_started.elapsed();
        if let Some(previous) = self.visible_page.replace(VisiblePage {
            frame: Arc::clone(frame),
            placement,
            top: viewport.top,
            image_id,
        }) {
            kitty::delete_image(output, previous.image_id)?;
        }
        self.submitted_key = Some(frame.key);
        let snapshot = PerformanceSnapshot {
            render_ms: frame.render_elapsed.as_millis(),
            dark_mode_ms: frame.dark_mode_elapsed.map(|elapsed| elapsed.as_millis()),
            highlight_ms: frame.highlight_elapsed.map(|elapsed| elapsed.as_millis()),
            compression_ms: frame.compression_elapsed.as_millis(),
            transfer_ms: transfer_elapsed.as_millis(),
            link_count: frame.links.len(),
        };
        self.performance_snapshot = Some(snapshot);
        let state = snapshot.status(self.show_performance, self.link_mode);
        let links = self.tab().link_index.links.clone();
        let outline = Arc::clone(&self.tab().outline);
        let progress = LinkIndexProgress::from(&self.tab().link_index);
        let search = self.tab().search.clone();
        let mut schedule_link_preview = false;
        if let Some(link_picker) = &mut self.link_picker {
            let selection_before = link_picker.selection_key;
            link_picker.sync(frame.key.page, &links, progress.indexing);
            schedule_link_preview = selection_before != link_picker.selection_key
                && link_picker.focus == LinkPickerFocus::Links;
            let link_picker = link_picker.clone();
            draw_link_picker_terminal_unsynchronized(
                output,
                link_picker_area(viewport),
                LinkPickerDocument::new(&links, &outline),
                &link_picker,
                progress,
                self.link_picker_geometry,
                self.theme,
            )?;
        } else if let Some(search_picker) = &mut self.search_picker {
            search_picker.sync(frame.key.page, &search.matches);
            let search_picker = search_picker.clone();
            draw_search_picker_terminal_unsynchronized(
                output,
                link_picker_area(viewport),
                &search,
                &outline,
                &search_picker,
                self.link_picker_geometry,
                self.theme,
            )?;
        }
        if schedule_link_preview {
            self.schedule_link_preview();
        }
        self.draw_status(output, viewport, &state)?;
        Ok(())
    }

    fn retain_primary_image(&mut self, output: &mut impl Write) -> io::Result<()> {
        for page in self.visible_pages.drain(..) {
            if Some(page.image_id) != self.visible_page.as_ref().map(|page| page.image_id) {
                kitty::delete_image(output, page.image_id)?;
            }
        }
        Ok(())
    }

    fn draw_continuous(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let cell = (u32::from(viewport.pixel_height) / u32::from(viewport.rows)).max(1);
        let max_y = if self.tab().page + 1 == self.tab().page_count {
            frame
                .height
                .saturating_sub(u32::from(viewport.pixel_height))
        } else {
            // The inter-page gap belongs to this page; the next page starts at
            // height + cell. This is the inclusive clamp for that half-open span.
            frame.height + cell - 1
        };
        self.tab_mut().scroll_y = self.tab().scroll_y.min(max_y);
        let old_pages = std::mem::take(&mut self.visible_pages);
        if old_pages.is_empty()
            && let Some(previous) = self.visible_page.take()
        {
            kitty::delete_image(output, previous.image_id)?;
        }
        // Kitty (image id, p=1) replaces the old placement, including its crop.
        // Only text/UI or geometry changes require clearing the canvas.
        if old_pages.is_empty() || self.canvas_viewport != Some(viewport) {
            self.prepare_image_canvas(output, viewport)?;
        }
        let started = Instant::now();
        self.missing_visible_page = None;
        let mut page = self.tab().page;
        let mut y = -i64::from(self.tab().scroll_y);
        while y < i64::from(viewport.pixel_height) && page < self.tab().page_count {
            let key = self.page_key(page, viewport);
            let rendered = if let Some(rendered) = self.tab().cache.get(&key).cloned() {
                rendered
            } else {
                self.missing_visible_page.get_or_insert(key);
                self.request_visible_page(key)?;
                // Overlay expiry can evict a frame that is still on screen. Keep
                // its image until the replacement arrives, but only for this
                // document revision and exact render settings. Run placement
                // normally so scrolling still updates crops and removes pages.
                let Some(old) = old_pages
                    .iter()
                    .find(|old| old.frame.key == key && old.frame.revision == self.tab().revision)
                else {
                    break;
                };
                Arc::clone(&old.frame)
            };
            let offset = (-y).max(0) as u32;
            let top = y.max(0) as u32;
            let span = i64::from(rendered.height) + i64::from(cell);
            if let Some(placement) = viewport.place_continuous(
                rendered.width,
                rendered.height,
                content_horizontal_scroll(self.tab().scroll_x, frame, &rendered, viewport).clamp(
                    0,
                    i64::from(
                        rendered
                            .width
                            .saturating_sub(u32::from(viewport.pixel_width)),
                    ),
                ) as u32,
                offset,
                top,
            ) {
                let row = (top / cell) as u16;
                let retained = old_pages
                    .iter()
                    .find(|old| Arc::ptr_eq(&old.frame, &rendered));
                let image_id = if let Some(old) = retained {
                    old.image_id
                } else {
                    let id = self.next_image_id;
                    self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
                    id
                };
                let kitty_placement = Placement {
                    image_id,
                    columns: 0,
                    rows: 0,
                    offset_y: placement.offset_y,
                    z_index: PAGE_IMAGE_Z_INDEX,
                    crop: placement.crop,
                };
                execute!(output, MoveTo(placement.left, viewport.top + row))?;
                if retained.is_some() {
                    kitty::place_image(output, kitty_placement)?;
                } else {
                    kitty::transmit_compressed_rgba(
                        output,
                        &rendered.compressed_rgba,
                        rendered.width,
                        rendered.height,
                        kitty_placement,
                    )?;
                }
                self.visible_pages.push(VisiblePage {
                    frame: rendered,
                    placement,
                    top: viewport.top + row,
                    image_id,
                });
            }
            y += span; // Keep a one-cell-high gap, without rounding page heights.
            page += 1;
        }
        // A narrow preceding page must not hide a wider visible jump or zoom target.
        // Each placement clamps independently, including retained overlay frames.
        // An uncached visible page may be wider; retain the requested offset until it renders.
        if frame.key.fit != FitMode::Content
            && self.missing_visible_page.is_none()
            && let Some(scroll_x) = self
                .visible_pages
                .iter()
                .map(|page| page.placement.scroll_x)
                .max()
        {
            self.tab_mut().scroll_x = i64::from(scroll_x);
        }
        for old in old_pages {
            if !self
                .visible_pages
                .iter()
                .any(|page| page.image_id == old.image_id)
            {
                kitty::delete_image(output, old.image_id)?;
            }
        }
        self.visible_page = self.visible_pages.first().cloned();
        self.submitted_key = Some(frame.key);
        let snapshot = PerformanceSnapshot {
            render_ms: frame.render_elapsed.as_millis(),
            dark_mode_ms: frame.dark_mode_elapsed.map(|elapsed| elapsed.as_millis()),
            highlight_ms: frame.highlight_elapsed.map(|elapsed| elapsed.as_millis()),
            compression_ms: frame.compression_elapsed.as_millis(),
            transfer_ms: started.elapsed().as_millis(),
            link_count: frame.links.len(),
        };
        self.performance_snapshot = Some(snapshot);
        self.draw_status(
            output,
            viewport,
            &snapshot.status(self.show_performance, self.link_mode),
        )?;
        Ok(())
    }

    fn page_at_mouse(
        &self,
        mouse: MouseEvent,
        viewport: Viewport,
    ) -> Option<(Arc<Frame>, ImagePlacement, u16)> {
        if self.viewer.continuous_scroll
            && self.link_picker.is_none()
            && self.search_picker.is_none()
        {
            return self
                .visible_pages
                .iter()
                .find(|page| {
                    mouse.row.checked_sub(page.top).is_some_and(|row| {
                        page.placement
                            .source_cell(mouse.column, row, page.frame.width, page.frame.height)
                            .is_some()
                    })
                })
                .map(|page| (Arc::clone(&page.frame), page.placement, page.top));
        }
        let page = self.current_visible_page(viewport)?;
        let frame = Arc::clone(&page.frame);
        if (self.link_picker.is_none() && self.search_picker.is_none())
            || self.link_picker_geometry.layout == LinkPickerLayout::Floating
        {
            return Some((frame, page.placement, page.top));
        }
        let (preview, _) = link_picker_panes(link_picker_area(viewport), self.link_picker_geometry);
        let positioned = position_link_picker_image(
            self.picker_image(viewport)?,
            preview,
            self.link_picker_geometry.layout,
        );
        Some((
            frame,
            ImagePlacement {
                left: positioned.left,
                columns: positioned.placement.columns,
                rows: positioned.placement.rows,
                offset_y: 0,
                native_cell: None,
                crop: positioned.placement.crop,
                ..page.placement
            },
            positioned.top,
        ))
    }

    fn prepare_image_canvas(
        &mut self,
        output: &mut impl Write,
        viewport: Viewport,
    ) -> io::Result<()> {
        clear_image_canvas(output, viewport)?;
        self.canvas_viewport = Some(viewport);
        self.status_line.clear();
        Ok(())
    }

    fn draw_status(
        &mut self,
        output: &mut impl Write,
        viewport: Viewport,
        state: &str,
    ) -> io::Result<()> {
        let theme = self.theme;
        let tab = self.tab();
        let mut mode = String::new();
        if tab.fit != FitMode::Page {
            mode.push_str(tab.fit.label());
        }
        if tab.invert {
            if !mode.is_empty() {
                mode.push(' ');
            }
            mode.push_str("dark");
        }
        if tab.zoom != ZOOM_DEFAULT {
            if !mode.is_empty() {
                mode.push(' ');
            }
            mode.push_str(&format!("{}%", tab.zoom));
        }
        if !mode.is_empty() {
            mode.push_str("  ");
        }
        let search_status = tab.search.status_label(tab.page);
        let link_status = self.link_mode.then_some("  click/enter: open  esc: close");
        let page = tab.page + 1;
        let page_count = tab.page_count;
        self.status_buffer.clear();
        // Leave the last cell unused even if a terminal has re-enabled wrapping.
        let columns = Cell::new(usize::from(viewport.columns.saturating_sub(1)));
        execute!(
            self.status_buffer,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            SetForegroundColor(theme.fg),
            Clear(ClearType::CurrentLine),
            Print(StatusText(" ", &columns)),
            SetForegroundColor(theme.blue),
            Print(StatusText(page, &columns)),
            SetForegroundColor(theme.fg_dark),
            Print(StatusText("/", &columns)),
            SetForegroundColor(theme.magenta),
            Print(StatusText(page_count, &columns)),
            Print(StatusText("  ", &columns)),
            SetForegroundColor(theme.yellow),
            Print(StatusText(&mode, &columns)),
            SetForegroundColor(theme.green),
            Print(StatusText(state, &columns)),
            SetForegroundColor(theme.magenta),
            Print(StatusText(
                search_status.as_deref().unwrap_or_default(),
                &columns
            )),
            SetForegroundColor(theme.cyan),
            Print(StatusText(link_status.unwrap_or_default(), &columns)),
            SetForegroundColor(theme.comment),
            Print(StatusText("  ?: help", &columns)),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        if self.status_buffer != self.status_line {
            output.write_all(&self.status_buffer)?;
            output.flush()?;
            std::mem::swap(&mut self.status_line, &mut self.status_buffer);
        }
        Ok(())
    }

    fn prefetch_neighbors(&mut self, key: RenderKey) {
        let page_count = self.tab().page_count;
        for page in (1..=u32::try_from(self.viewer.prefetch_pages)
            .expect("validated prefetch_pages exceeds u32"))
            .flat_map(|distance| {
                [
                    key.page.checked_sub(distance),
                    key.page.checked_add(distance),
                ]
            })
            .flatten()
            .filter(|page| *page < page_count)
        {
            let neighbor = RenderKey {
                page,
                search_request_id: self.tab().search.highlight_request_id(page),
                selected_link_ordinal: None,
                ..key
            };
            let cached = self.tab().cache.contains_key(&neighbor);
            if !cached && self.pending.insert(neighbor) {
                let _ = self.worker.prefetch(RenderRequest {
                    key: neighbor,
                    generation: self.generation,
                });
            }
        }
    }

    fn clear_viewer(&mut self, output: &mut impl Write) -> io::Result<()> {
        let theme = self.theme;
        kitty::delete_all(output)?;
        self.visible_page = None;
        self.submitted_key = None;
        self.visible_pages.clear();
        self.missing_visible_page = None;
        self.canvas_viewport = None;
        self.status_line.clear();
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
        self.last_status_row = None;
        execute!(
            output,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg),
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        output.flush()
    }

    fn prepare_viewport(&mut self, output: &mut impl Write) -> io::Result<Viewport> {
        let viewport = self.viewport()?;
        if let Some(row) = stale_status_row(self.last_status_row, viewport.status_row) {
            let theme = self.theme;
            execute!(
                output,
                MoveTo(0, row),
                SetBackgroundColor(theme.bg),
                SetForegroundColor(theme.fg),
                Clear(ClearType::CurrentLine)
            )?;
        }
        self.last_status_row = Some(viewport.status_row);
        Ok(viewport)
    }

    fn draw_tab_bar(&self, output: &mut impl Write) -> io::Result<()> {
        if self.session.tabs.len() < 2 {
            return Ok(());
        }
        self.draw_tab_bar_at_width(output, usize::from(crossterm::terminal::size()?.0))
    }

    fn draw_tab_bar_at_width(&self, output: &mut impl Write, columns: usize) -> io::Result<()> {
        let theme = self.theme;
        execute!(
            output,
            MoveTo(0, 0),
            SetBackgroundColor(theme.bg_dark1),
            SetForegroundColor(theme.dark3),
            Clear(ClearType::CurrentLine)
        )?;
        let remaining = Cell::new(columns);
        for (index, tab) in self.session.tabs.iter().enumerate() {
            let name = tab
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| tab.path.to_string_lossy());
            if remaining.get() == 0 {
                break;
            }
            if index == self.session.active_tab {
                execute!(
                    output,
                    SetBackgroundColor(theme.blue),
                    SetForegroundColor(theme.bg_dark),
                    SetAttribute(Attribute::Bold),
                    Print(StatusText(
                        format_args!(" {}:{} ", index + 1, name),
                        &remaining
                    )),
                    SetAttribute(Attribute::NormalIntensity)
                )?;
            } else {
                execute!(
                    output,
                    SetBackgroundColor(theme.bg),
                    SetForegroundColor(theme.dark3),
                    Print(StatusText(
                        format_args!(" {}:{} ", index + 1, name),
                        &remaining
                    ))
                )?;
            }
        }
        execute!(
            output,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn reset_render_state(&mut self) {
        self.desired_key = None;
        self.pending.clear();
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
    }

    fn viewport(&self) -> io::Result<Viewport> {
        #[cfg(test)]
        if let Some(viewport) = self.test_viewport {
            return Ok(viewport);
        }
        Viewport::detect(u16::from(self.session.tabs.len() > 1))
    }

    fn render_key(&self, viewport: Viewport) -> RenderKey {
        let selected_link_ordinal = self
            .link_picker
            .as_ref()
            .and_then(|state| state.selection_key)
            .and_then(|(page, ordinal)| (page == self.tab().page).then_some(ordinal));
        self.tab()
            .render_key(viewport, self.link_mode, selected_link_ordinal)
    }
}

fn same_render_view(a: RenderKey, b: RenderKey) -> bool {
    a.document_id == b.document_id
        && a.width == b.width
        && a.height == b.height
        && a.zoom == b.zoom
        && a.fit == b.fit
        && a.invert == b.invert
        && a.dark_mode_style == b.dark_mode_style
}

fn pick_document(
    directory: PathBuf,
    output: &mut impl Write,
    theme: Palette,
    mut interrupt: impl FnMut() -> Result<bool, AppError>,
) -> Result<Option<PathBuf>, AppError> {
    let mut browser = BrowserState::new(directory);
    browser.set_recents(crate::recent::load());
    browser.preload_recursive();
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;
    let mut filtering = false;

    let selection = loop {
        if interrupt()? {
            break None;
        }
        if browser.poll_recursive() {
            redraw = true;
        }
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(4).max(1));
            browser.adjust_scroll(visible_height);
            terminal.draw(|frame| draw_picker(frame, &browser, filtering, theme))?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match read_event()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if apply_picker_navigation(&mut browser, key, visible_height, filtering) {
            redraw = true;
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if filtering => {
                clear_picker_filter(&mut browser);
                filtering = false;
            }
            KeyCode::Esc if !clear_picker_filter(&mut browser) => break None,
            KeyCode::Esc => {}
            KeyCode::Char('/') if !control && !alt => filtering = true,
            KeyCode::Enter => {
                if let Some(path) = browser.enter_selected() {
                    break Some(path);
                }
            }
            KeyCode::Backspace if filtering => {
                browser.filter.pop();
                browser.rebuild_filter();
                browser.select_first();
                browser.scroll_offset = 0;
            }
            KeyCode::Char(character) if filtering && !control && !alt => {
                browser.filter.push(character);
                browser.rebuild_filter();
                browser.select_first();
                browser.scroll_offset = 0;
            }
            _ => {}
        }
        redraw = true;
    };
    drop(terminal);
    clear_picker(output, theme)?;
    Ok(selection)
}

fn clear_picker(output: &mut impl Write, theme: Palette) -> io::Result<()> {
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Hide,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    output.flush()
}

fn show_help(
    output: &mut impl Write,
    theme: Palette,
    mut interrupt: impl FnMut() -> Result<bool, AppError>,
) -> Result<(), AppError> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;

    loop {
        if interrupt()? {
            break;
        }
        if redraw {
            terminal.autoresize()?;
            terminal.draw(|frame| draw_help_menu(frame, theme))?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match read_event()? {
            Event::Resize(_, _) => redraw = true,
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && matches!(
                        key.code,
                        KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc
                    ) =>
            {
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn pick_theme(
    themes: &[(String, Palette)],
    current: usize,
    output: &mut impl Write,
    mut interrupt: impl FnMut() -> Result<bool, AppError>,
) -> Result<Option<usize>, AppError> {
    let mut filter = String::new();
    let mut filtered = filter_theme_indices(themes, &filter);
    let mut selected = filtered
        .iter()
        .position(|index| *index == current)
        .unwrap_or(0);
    let mut filtering = false;
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;

    let selection = loop {
        if interrupt()? {
            break None;
        }
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(4).max(1));
            terminal.draw(|frame| {
                draw_theme_picker(frame, themes, &filtered, selected, &filter, filtering)
            })?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match read_event()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let last = filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc if filtering => {
                let actual = filtered.get(selected).copied().unwrap_or(current);
                filter.clear();
                filtered = filter_theme_indices(themes, &filter);
                selected = filtered
                    .iter()
                    .position(|index| *index == actual)
                    .unwrap_or(0);
                filtering = false;
            }
            KeyCode::Esc => break None,
            KeyCode::Char('/') if !control && !alt => filtering = true,
            KeyCode::Enter => break filtered.get(selected).copied(),
            KeyCode::Down => {
                selected = if selected == last { 0 } else { selected + 1 };
            }
            KeyCode::Up => {
                selected = if selected == 0 { last } else { selected - 1 };
            }
            KeyCode::Char('j') if !filtering => {
                selected = if selected == last { 0 } else { selected + 1 };
            }
            KeyCode::Char('k') if !filtering => {
                selected = if selected == 0 { last } else { selected - 1 };
            }
            KeyCode::Char('f') if control => {
                selected = (selected + visible_height).min(last);
            }
            KeyCode::Char('b') if control => selected = selected.saturating_sub(visible_height),
            KeyCode::Char('g') if !filtering && !control && !alt => selected = 0,
            KeyCode::Char('G') if !filtering && !control && !alt => selected = last,
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = last,
            KeyCode::PageDown => selected = (selected + visible_height).min(last),
            KeyCode::PageUp => selected = selected.saturating_sub(visible_height),
            KeyCode::Backspace if filtering => {
                filter.pop();
                filtered = filter_theme_indices(themes, &filter);
                selected = 0;
            }
            KeyCode::Char(character) if filtering && !control && !alt => {
                filter.push(character);
                filtered = filter_theme_indices(themes, &filter);
                selected = 0;
            }
            _ => continue,
        }
        redraw = true;
    };
    drop(terminal);
    Ok(selection)
}

fn draw_link_picker_terminal(
    output: &mut impl Write,
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    synchronized_output(output, |output| {
        draw_link_picker_terminal_unsynchronized(
            output, area, document, state, progress, geometry, theme,
        )
    })
}

fn draw_link_picker_terminal_unsynchronized(
    output: &mut impl Write,
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    terminal
        .draw(|frame| draw_link_picker(frame, area, document, state, progress, geometry, theme))?;
    drop(terminal);
    execute!(output, Hide)?;
    Ok(())
}

fn draw_search_picker_terminal(
    output: &mut impl Write,
    area: Rect,
    search: &SearchState,
    outline: &[OutlineItem],
    state: &SearchPickerState,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    synchronized_output(output, |output| {
        draw_search_picker_terminal_unsynchronized(
            output, area, search, outline, state, geometry, theme,
        )
    })
}

fn draw_search_picker_terminal_unsynchronized(
    output: &mut impl Write,
    area: Rect,
    search: &SearchState,
    outline: &[OutlineItem],
    state: &SearchPickerState,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    terminal
        .draw(|frame| draw_search_picker(frame, area, search, outline, state, geometry, theme))?;
    drop(terminal);
    execute!(output, Hide)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PositionedImage {
    left: u16,
    top: u16,
    placement: Placement,
}

fn clear_image_canvas(output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
    execute!(output, ResetColor)?;
    for row in viewport.top..viewport.top.saturating_add(viewport.rows) {
        execute!(output, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkPickerImage {
    image_id: u32,
    source_width: u32,
    source_height: u32,
    crop: Option<kitty::Crop>,
    cell_width: u32,
    cell_height: u32,
    original: PositionedImage,
}

impl LinkPickerImage {
    fn new(image_id: u32, frame: &Frame, placement: ImagePlacement, viewport: Viewport) -> Self {
        let (source_width, source_height) = placement
            .crop
            .map(|crop| (crop.width, crop.height))
            .unwrap_or((frame.width, frame.height));
        Self {
            image_id,
            source_width,
            source_height,
            crop: placement.crop,
            cell_width: (u32::from(viewport.pixel_width) / u32::from(viewport.columns)).max(1),
            cell_height: (u32::from(viewport.pixel_height) / u32::from(viewport.rows)).max(1),
            original: PositionedImage {
                left: placement.left,
                top: viewport.top,
                placement: Placement {
                    image_id,
                    columns: if placement.native_cell.is_some() {
                        0
                    } else {
                        placement.columns
                    },
                    rows: if placement.native_cell.is_some() {
                        0
                    } else {
                        placement.rows
                    },
                    offset_y: placement.offset_y,
                    z_index: PAGE_IMAGE_Z_INDEX,
                    crop: placement.crop,
                },
            },
        }
    }

    fn fit(self, pane: Rect) -> PositionedImage {
        let max_width = u32::from(pane.width).saturating_mul(self.cell_width);
        let max_height = u32::from(pane.height).saturating_mul(self.cell_height);
        let scale = (max_width as f64 / f64::from(self.source_width))
            .min(max_height as f64 / f64::from(self.source_height))
            .min(1.0);
        let pixel_width = (f64::from(self.source_width) * scale).round().max(1.0) as u32;
        let pixel_height = (f64::from(self.source_height) * scale).round().max(1.0) as u32;
        let columns = pixel_width
            .div_ceil(self.cell_width)
            .min(u32::from(pane.width))
            .max(1) as u16;
        let rows = pixel_height
            .div_ceil(self.cell_height)
            .min(u32::from(pane.height))
            .max(1) as u16;
        PositionedImage {
            left: pane.x + pane.width.saturating_sub(columns) / 2,
            top: pane.y + pane.height.saturating_sub(rows) / 2,
            placement: Placement {
                image_id: self.image_id,
                columns,
                rows,
                offset_y: 0,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: self.crop,
            },
        }
    }
}

fn position_link_picker_image(
    image: LinkPickerImage,
    preview: Rect,
    layout: LinkPickerLayout,
) -> PositionedImage {
    if layout == LinkPickerLayout::Floating {
        image.original
    } else {
        image.fit(preview)
    }
}

fn link_picker_area(viewport: Viewport) -> Rect {
    Rect::new(0, viewport.top, viewport.columns, viewport.rows)
}

fn resolved_link_picker_layout(area: Rect, layout: LinkPickerLayout) -> LinkPickerLayout {
    match layout {
        LinkPickerLayout::Auto if u32::from(area.width) >= u32::from(area.height) * 2 => {
            LinkPickerLayout::Vertical
        }
        LinkPickerLayout::Auto => LinkPickerLayout::Horizontal,
        layout => layout,
    }
}

fn next_link_picker_layout(area: Rect, layout: LinkPickerLayout) -> LinkPickerLayout {
    match resolved_link_picker_layout(area, layout) {
        LinkPickerLayout::Vertical => LinkPickerLayout::Horizontal,
        LinkPickerLayout::Horizontal => LinkPickerLayout::Floating,
        LinkPickerLayout::Floating => LinkPickerLayout::Vertical,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    }
}

const fn link_picker_layout_name(layout: LinkPickerLayout) -> &'static str {
    match layout {
        LinkPickerLayout::Auto => "auto",
        LinkPickerLayout::Vertical => "vertical",
        LinkPickerLayout::Horizontal => "horizontal",
        LinkPickerLayout::Floating => "floating",
    }
}

fn link_picker_layout_status(area: Rect, layout: LinkPickerLayout) -> String {
    if layout == LinkPickerLayout::Auto {
        format!(
            "auto/{}",
            link_picker_layout_name(resolved_link_picker_layout(area, layout))
        )
    } else {
        link_picker_layout_name(layout).to_string()
    }
}

fn link_picker_focus_for_key(
    current: LinkPickerFocus,
    layout: LinkPickerLayout,
    key: KeyEvent,
) -> Option<LinkPickerFocus> {
    if layout == LinkPickerLayout::Floating {
        return None;
    }
    match key.code {
        KeyCode::Char('h') => Some(LinkPickerFocus::Document),
        KeyCode::Char('l') => Some(LinkPickerFocus::Links),
        KeyCode::Tab | KeyCode::BackTab => Some(match current {
            LinkPickerFocus::Document => LinkPickerFocus::Links,
            LinkPickerFocus::Links => LinkPickerFocus::Document,
        }),
        _ => None,
    }
}

fn link_picker_panes(area: Rect, geometry: LinkPickerGeometry) -> (Rect, Rect) {
    let picker_percent = geometry.split_percent.clamp(20, 80);
    let document_percent = 100 - picker_percent;
    let panes = match resolved_link_picker_layout(area, geometry.layout) {
        LinkPickerLayout::Vertical => Layout::horizontal([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Horizontal => Layout::vertical([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Floating => return (area, picker_rect(area)),
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    (panes[0], panes[1])
}

fn place_positioned_image(output: &mut impl Write, image: PositionedImage) -> io::Result<()> {
    execute!(output, MoveTo(image.left, image.top))?;
    kitty::place_image(output, image.placement)
}

fn show_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let (preview, pane) = link_picker_panes(area, geometry);
    execute!(
        output,
        SetBackgroundColor(theme.bg_dark),
        SetForegroundColor(theme.fg)
    )?;
    for row in pane.y..pane.y.saturating_add(pane.height) {
        execute!(
            output,
            MoveTo(pane.x, row),
            Print(" ".repeat(usize::from(pane.width)))
        )?;
    }
    if geometry.layout != LinkPickerLayout::Floating {
        place_positioned_image(output, image.fit(preview))?;
    }
    execute!(output, Hide)?;
    output.flush()
}

fn clear_link_picker_pane(
    output: &mut impl Write,
    area: Rect,
    geometry: LinkPickerGeometry,
) -> io::Result<()> {
    let (_, pane) = link_picker_panes(area, geometry);
    execute!(output, ResetColor)?;
    for row in pane.y..pane.y.saturating_add(pane.height) {
        execute!(
            output,
            MoveTo(pane.x, row),
            Print(" ".repeat(usize::from(pane.width)))
        )?;
    }
    output.flush()
}

fn restore_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    clear_link_picker_pane(output, area, geometry)?;
    place_positioned_image(output, image.original)?;
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Hide
    )?;
    output.flush()
}

fn link_number_index(input: &str, link_count: usize) -> Option<usize> {
    input
        .parse::<usize>()
        .ok()
        .filter(|number| (1..=link_count).contains(number))
        .map(|number| number - 1)
}

fn update_link_number_selection(
    input: &mut String,
    digit: char,
    link_count: usize,
) -> Option<usize> {
    let mut candidate = input.clone();
    candidate.push(digit);
    if let Some(index) = link_number_index(&candidate, link_count) {
        *input = candidate;
        return Some(index);
    }

    input.clear();
    input.push(digit);
    if let Some(index) = link_number_index(input, link_count) {
        Some(index)
    } else {
        input.clear();
        None
    }
}

fn link_picker_navigation_index(
    selected: usize,
    link_count: usize,
    key: KeyEvent,
    visible_height: usize,
) -> Option<usize> {
    if link_count == 0 {
        return None;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => Some((selected + 1) % link_count),
        KeyCode::Up | KeyCode::Char('k') => Some(selected.checked_sub(1).unwrap_or(link_count - 1)),
        KeyCode::Char('f') if control => Some((selected + visible_height).min(link_count - 1)),
        KeyCode::Char('b') if control => Some(selected.saturating_sub(visible_height)),
        KeyCode::Char('g') if !control && !alt => Some(0),
        KeyCode::Char('G') if !control && !alt => Some(link_count - 1),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(link_count - 1),
        KeyCode::PageDown => Some((selected + visible_height).min(link_count - 1)),
        KeyCode::PageUp => Some(selected.saturating_sub(visible_height)),
        _ => None,
    }
}

fn filter_document_links(links: &[DocumentLink], filter: &str) -> Vec<usize> {
    let terms: Vec<_> = filter
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    links
        .iter()
        .enumerate()
        .filter_map(|(index, link)| {
            if terms.is_empty() {
                return Some(index);
            }
            let target = match &link.target {
                LinkTarget::Internal { page, .. } => format!("slide {}", page + 1),
                LinkTarget::Uri(uri) => uri.clone(),
            };
            let haystack = format!(
                "{} source page {} {}",
                link.label,
                link.source_page + 1,
                target
            )
            .to_lowercase();
            terms
                .iter()
                .all(|term| haystack.contains(term))
                .then_some(index)
        })
        .collect()
}

fn pick_outline(
    items: &[OutlineItem],
    current_page: u32,
    output: &mut impl Write,
    theme: Palette,
    mut interrupt: impl FnMut() -> Result<bool, AppError>,
) -> Result<Option<u32>, AppError> {
    let mut filter = String::new();
    let mut filtered: Vec<usize> = (0..items.len()).collect();
    let mut selected = outline_start_index(items, current_page);
    let mut scroll_offset = 0usize;
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;
    let mut filtering = false;

    let selection = loop {
        if interrupt()? {
            break None;
        }
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(4).max(1));
            if selected < scroll_offset {
                scroll_offset = selected;
            } else if selected >= scroll_offset + visible_height {
                scroll_offset = selected - visible_height + 1;
            }
            terminal.draw(|frame| {
                draw_outline(
                    frame,
                    items,
                    &filtered,
                    selected,
                    scroll_offset,
                    PickerFilter {
                        query: &filter,
                        active: filtering,
                    },
                    theme,
                )
            })?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match read_event()? {
            Event::Key(key) => key,
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                MouseEventKind::ScrollDown => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                _ => continue,
            },
            Event::Resize(_, _) => {
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let last = filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc if filtering => {
                filter.clear();
                filtered = filter_outline(items, &filter);
                selected = outline_start_index(items, current_page);
                scroll_offset = 0;
                filtering = false;
            }
            KeyCode::Esc => break None,
            KeyCode::Char('/') if !control && !alt => filtering = true,
            KeyCode::Enter => {
                if let Some(index) = filtered.get(selected) {
                    break Some(items[*index].page);
                }
            }
            KeyCode::Down => selected = (selected + 1).min(last),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('j') if !filtering => selected = (selected + 1).min(last),
            KeyCode::Char('k') if !filtering => selected = selected.saturating_sub(1),
            KeyCode::Char('f') if control => {
                selected = (selected + visible_height).min(last);
            }
            KeyCode::Char('b') if control => selected = selected.saturating_sub(visible_height),
            KeyCode::Char('g') if !filtering && !control && !alt => selected = 0,
            KeyCode::Char('G') if !filtering && !control && !alt => selected = last,
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = last,
            KeyCode::PageDown => selected = (selected + visible_height).min(last),
            KeyCode::PageUp => selected = selected.saturating_sub(visible_height),
            KeyCode::Backspace if filtering => {
                filter.pop();
                filtered = filter_outline(items, &filter);
                selected = 0;
                scroll_offset = 0;
            }
            KeyCode::Char(character) if filtering && !control && !alt => {
                filter.push(character);
                filtered = filter_outline(items, &filter);
                selected = 0;
                scroll_offset = 0;
            }
            _ => {}
        }
        redraw = true;
    };
    drop(terminal);
    clear_picker(output, theme)?;
    Ok(selection)
}

fn outline_start_index(items: &[OutlineItem], current_page: u32) -> usize {
    items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| item.page <= current_page)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn filter_outline(items: &[OutlineItem], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let haystack = Utf32Str::new(&item.title, &mut buffer);
            pattern
                .score(haystack, &mut matcher)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(index, _)| index).collect()
}

fn draw_outline(
    frame: &mut RatatuiFrame,
    items: &[OutlineItem],
    filtered: &[usize],
    selected: usize,
    scroll_offset: usize,
    filter: PickerFilter<'_>,
    theme: Palette,
) {
    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Outline ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter_line = if filter.query.is_empty() && !filter.active {
        Line::from(Span::styled(
            " / to filter",
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(
                filter.query.to_string(),
                Style::default().fg(colors.text).bg(colors.chrome),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter_line).style(Style::default().bg(colors.chrome)),
        rows[0],
    );

    let visible_height = usize::from(rows[1].height);
    let width = usize::from(rows[1].width);
    let mut lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(position, item_index)| {
            let item = &items[*item_index];
            let is_selected = position == selected;
            let style = if is_selected {
                Style::default().fg(colors.text).bg(colors.selection)
            } else {
                Style::default().fg(colors.text).bg(colors.surface)
            };
            let indent = "  ".repeat(usize::from(item.depth).min(6) + 1);
            let page_label = format!(" {} ", item.page + 1);
            let mut line = Line::from(vec![
                Span::styled(if is_selected { "▌" } else { " " }, style.fg(colors.accent)),
                Span::styled(indent, style),
                Span::styled(item.title.clone(), style.add_modifier(Modifier::BOLD)),
            ]);
            let used = line.width();
            let page_width = page_label.chars().count();
            if used + page_width < width {
                let page_style = if is_selected {
                    style
                } else {
                    Style::default().fg(colors.muted).bg(colors.surface)
                };
                line.spans
                    .push(Span::styled(" ".repeat(width - used - page_width), style));
                line.spans.push(Span::styled(page_label, page_style));
            }
            line
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No matches",
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[1],
    );

    frame.render_widget(
        Paragraph::new(picker_hint_line(
            &[
                ("j/k/wheel", "select"),
                ("^b/^f", "page"),
                ("g/G", "ends"),
                ("/", "filter"),
                ("enter", "jump"),
                ("esc", if filter.active { "clear" } else { "close" }),
            ],
            None,
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

fn apply_picker_navigation(
    browser: &mut BrowserState,
    key: KeyEvent,
    visible_height: usize,
    filtering: bool,
) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Down => browser.select_down(),
        KeyCode::Up => browser.select_up(),
        KeyCode::Char('j') if !filtering => browser.select_down(),
        KeyCode::Char('k') if !filtering => browser.select_up(),
        KeyCode::Char('f') if control => browser.page_down(visible_height),
        KeyCode::Char('b') if control => browser.page_up(visible_height),
        KeyCode::Char('g')
            if !control && !key.modifiers.contains(KeyModifiers::ALT) && !filtering =>
        {
            browser.select_first();
        }
        KeyCode::Char('G')
            if !control && !key.modifiers.contains(KeyModifiers::ALT) && !filtering =>
        {
            browser.select_last();
        }
        KeyCode::Home => browser.select_first(),
        KeyCode::End => browser.select_last(),
        KeyCode::PageDown => browser.page_down(visible_height),
        KeyCode::PageUp => browser.page_up(visible_height),
        _ => return false,
    }
    true
}

fn clear_picker_filter(browser: &mut BrowserState) -> bool {
    if browser.filter.is_empty() {
        return false;
    }
    browser.filter.clear();
    browser.rebuild_filter();
    browser.select_first();
    browser.scroll_offset = 0;
    true
}

fn draw_picker(frame: &mut RatatuiFrame, browser: &BrowserState, filtering: bool, theme: Palette) {
    let colors = PickerTheme::from(theme);
    let entries: Vec<_> = browser.filtered_entries().collect();
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let directory = shorten_path(&browser.current_dir.display().to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(format!(" {directory} "))
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter = if browser.filter.is_empty() && !filtering {
        Line::from(Span::styled(
            " / to filter",
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(
                browser.filter.as_str(),
                Style::default().fg(colors.text).bg(colors.chrome),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter).style(Style::default().bg(colors.chrome)),
        rows[0],
    );

    let visible_height = usize::from(rows[1].height);
    let recent_heading_index = browser.recent_heading_index();
    let mut lines = Vec::with_capacity(visible_height);
    for (index, entry) in entries.iter().enumerate().skip(browser.scroll_offset) {
        if Some(index) == recent_heading_index && lines.len() + 1 < visible_height {
            lines.push(picker_recent_heading_line(
                usize::from(rows[1].width),
                colors,
            ));
        }
        if lines.len() >= visible_height {
            break;
        }
        lines.push(picker_entry_line(
            entry,
            browser,
            index == browser.selected,
            usize::from(rows[1].width),
            colors,
        ));
        if lines.len() >= visible_height {
            break;
        }
    }
    if lines.is_empty() {
        let message = if browser.filter.is_empty() {
            "   No Office files found"
        } else {
            "   No matches"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[1],
    );

    let status = if browser.recursive_loading() {
        Some((format!("scan • {}", entries.len()), colors.loading))
    } else {
        let position = if entries.is_empty() {
            0
        } else {
            browser.selected + 1
        };
        Some((format!("{position}/{}", entries.len()), colors.muted))
    };
    let escape_action = if filtering || !browser.filter.is_empty() {
        "clear"
    } else {
        "close"
    };
    let mut bindings = vec![("j/k", ""), ("^b/^f", "page"), ("g/G", "ends")];
    bindings.push(("/", "filter"));
    bindings.push(("enter", "open"));
    bindings.push(("esc", escape_action));
    frame.render_widget(
        Paragraph::new(picker_hint_line(&bindings, status, colors))
            .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SearchPickerDisplayRow {
    Section(usize),
    Match(usize),
    Context(usize),
}

fn search_picker_display_rows(
    matches: &[SearchPageMatch],
    outline: &[OutlineItem],
) -> Vec<SearchPickerDisplayRow> {
    let mut rows = Vec::with_capacity(matches.len().saturating_mul(3));
    let mut previous_section = None;
    for (index, result) in matches.iter().enumerate() {
        let section = link_picker_section_index(outline, result.page);
        if section != previous_section {
            if let Some(section) = section {
                rows.push(SearchPickerDisplayRow::Section(section));
            }
            previous_section = section;
        }
        rows.push(SearchPickerDisplayRow::Match(index));
        if !result.context.is_empty() {
            rows.push(SearchPickerDisplayRow::Context(index));
        }
    }
    rows
}

fn search_picker_row_window(
    rows: &[SearchPickerDisplayRow],
    selected: usize,
    visible_height: usize,
) -> (usize, usize) {
    let selected_row = rows
        .iter()
        .position(|row| matches!(row, SearchPickerDisplayRow::Match(index) if *index == selected))
        .unwrap_or(0);
    let height = visible_height.max(1).min(rows.len());
    let start = selected_row
        .saturating_sub(height / 3)
        .min(rows.len().saturating_sub(height));
    (start, start + height)
}

fn draw_search_picker(
    frame: &mut RatatuiFrame,
    area: Rect,
    search: &SearchState,
    outline: &[OutlineItem],
    state: &SearchPickerState,
    geometry: LinkPickerGeometry,
    theme: Palette,
) {
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let results_focused =
        state.focus == LinkPickerFocus::Links || layout == LinkPickerLayout::Floating;
    let mut colors = PickerTheme::from(theme);
    colors.surface = picker_color(theme.bg_dark);
    colors.chrome = picker_color(theme.bg_dark1);
    let (_, pane) = link_picker_panes(area, geometry);
    frame.render_widget(RatatuiClear, pane);
    let borders = match layout {
        LinkPickerLayout::Vertical => Borders::LEFT,
        LinkPickerLayout::Horizontal => Borders::TOP,
        LinkPickerLayout::Floating => Borders::ALL,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let block = Block::default()
        .borders(borders)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(if results_focused {
            colors.accent
        } else {
            colors.border
        }));
    let inner = block.inner(pane);
    frame.render_widget(block, pane);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(u16::from(layout == LinkPickerLayout::Floating)),
    ])
    .split(inner);

    let mut header = vec![
        Span::styled(
            " Search ",
            Style::default()
                .fg(if results_focused {
                    colors.accent
                } else {
                    colors.muted
                })
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncated_search_query(&search.query, 36),
            Style::default().fg(colors.text).bg(colors.chrome),
        ),
        Span::styled(
            format!(
                "  · {} hits on {} pages",
                search.total_occurrences,
                search.matches.len()
            ),
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
    ];
    if search.searching {
        header.push(Span::styled(
            format!("  scanning {}/{}", search.scanned, search.total_pages),
            Style::default().fg(colors.loading).bg(colors.chrome),
        ));
    }
    if !results_focused {
        header.push(Span::styled(
            "  Slide focused  · l: results",
            Style::default()
                .fg(colors.loading)
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(header))
            .style(Style::default().bg(colors.chrome))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(colors.border)),
            ),
        rows[0],
    );

    if search.matches.is_empty() {
        let message = if search.searching {
            "Scanning document text…"
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(colors.muted).bg(colors.surface)),
            rows[1],
        );
    } else {
        let display_rows = search_picker_display_rows(&search.matches, outline);
        let (start, end) = search_picker_row_window(
            &display_rows,
            state.selected,
            usize::from(rows[1].height).max(1),
        );
        let width = usize::from(rows[1].width);
        let number_width = search.matches.len().to_string().len().max(1);
        let lines: Vec<_> = display_rows[start..end]
            .iter()
            .map(|row| match row {
                SearchPickerDisplayRow::Section(index) => {
                    link_picker_section_heading_line(&outline[*index].title, width, colors)
                }
                SearchPickerDisplayRow::Match(index) => search_picker_match_line(
                    *index,
                    &search.matches[*index],
                    state,
                    number_width,
                    width,
                    results_focused,
                    colors,
                ),
                SearchPickerDisplayRow::Context(index) => search_picker_context_line(
                    *index,
                    &search.matches[*index].context,
                    state,
                    width,
                    results_focused,
                    colors,
                ),
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(colors.surface)),
            rows[1],
        );
    }

    if layout == LinkPickerLayout::Floating {
        let position = usize::from(!search.matches.is_empty()) * (state.selected + 1);
        frame.render_widget(
            Paragraph::new(picker_hint_line(
                &[
                    ("j/k C-b/f g/G", "navigate"),
                    ("/", "new search"),
                    ("enter", "jump"),
                    ("s/a", "layout"),
                    ("esc", "close"),
                ],
                Some((format!("{position}/{}", search.matches.len()), colors.muted)),
                colors,
            ))
            .style(Style::default().bg(colors.chrome)),
            rows[2],
        );
    }
}

fn search_picker_match_line(
    index: usize,
    result: &SearchPageMatch,
    state: &SearchPickerState,
    number_width: usize,
    width: usize,
    focused: bool,
    colors: PickerTheme,
) -> Line<'static> {
    let selected = focused && index == state.selected;
    let background = if selected {
        colors.selection
    } else {
        colors.surface
    };
    let hits = if result.occurrences == 1 {
        "hit"
    } else {
        "hits"
    };
    let current = if result.page == state.page {
        " · current"
    } else {
        ""
    };
    let label = format!(
        "{:>number_width$}  Page {} · {} {hits}{current}",
        index + 1,
        result.page + 1,
        result.occurrences
    );
    Line::from(vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::default().fg(colors.accent).bg(background),
        ),
        Span::styled(
            truncate_right(&label, width.saturating_sub(2)),
            Style::default()
                .fg(colors.text)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn search_picker_context_line(
    index: usize,
    context: &str,
    state: &SearchPickerState,
    width: usize,
    focused: bool,
    colors: PickerTheme,
) -> Line<'static> {
    let background = if focused && index == state.selected {
        colors.selection
    } else {
        colors.surface
    };
    Line::from(vec![
        Span::styled("    ", Style::default().bg(background)),
        Span::styled(
            truncate_right(context, width.saturating_sub(4)),
            Style::default().fg(colors.text_dim).bg(background),
        ),
    ])
}

fn draw_link_picker(
    frame: &mut RatatuiFrame,
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) {
    let LinkPickerDocument { links, outline } = document;
    let selected = state.selected;
    let page = state.page;
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let links_focused =
        state.focus == LinkPickerFocus::Links || layout == LinkPickerLayout::Floating;
    let filtered = filter_document_links(links, &state.filter);
    let selected_position = filtered.iter().position(|index| *index == selected);
    let mut colors = PickerTheme::from(theme);
    colors.surface = picker_color(theme.bg_dark);
    colors.chrome = picker_color(theme.bg_dark1);
    let (_, pane) = link_picker_panes(area, geometry);
    frame.render_widget(RatatuiClear, pane);
    let pane_borders = match layout {
        LinkPickerLayout::Vertical => Borders::LEFT,
        LinkPickerLayout::Horizontal => Borders::TOP,
        LinkPickerLayout::Floating => Borders::ALL,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let pane_block = Block::default()
        .borders(pane_borders)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(if links_focused {
            colors.accent
        } else {
            colors.border
        }));
    let inner = pane_block.inner(pane);
    frame.render_widget(pane_block, pane);
    let detail_height = link_picker_detail_height(inner.height);
    let footer_height = u16::from(layout == LinkPickerLayout::Floating);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(detail_height),
        Constraint::Length(footer_height),
    ])
    .split(inner);

    let mut header = vec![
        Span::styled(
            " Links ",
            Style::default()
                .fg(if links_focused {
                    colors.accent
                } else {
                    colors.muted
                })
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if state.filter.is_empty() {
                links.len().to_string()
            } else {
                format!("{}/{}", filtered.len(), links.len())
            },
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
        Span::styled(
            format!("  · {}", link_picker_layout_status(area, geometry.layout)),
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
    ];
    if !links_focused {
        header.push(Span::styled(
            "  Slide focused  · l: links",
            Style::default()
                .fg(colors.loading)
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if progress.indexing {
        header.push(Span::styled(
            format!("  indexing {}/{}", progress.scanned, progress.total_pages),
            Style::default().fg(colors.loading).bg(colors.chrome),
        ));
    }
    if state.filtering || !state.filter.is_empty() {
        header.push(Span::styled(
            "  / ",
            Style::default()
                .fg(colors.accent)
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::styled(
            if state.filter.is_empty() {
                "type to filter…".to_string()
            } else {
                state.filter.clone()
            },
            Style::default().fg(colors.text).bg(colors.chrome),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(header))
            .style(Style::default().bg(colors.chrome))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(colors.border)),
            ),
        rows[0],
    );

    if filtered.is_empty() {
        let message = if progress.indexing {
            format!(
                "Indexing document links · {}/{} pages",
                progress.scanned, progress.total_pages
            )
        } else if !state.filter.is_empty() {
            format!("No links match ‘{}’", state.filter)
        } else {
            "No annotated links in this document".to_string()
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(colors.muted).bg(colors.surface)),
            rows[1],
        );
    } else {
        let display_rows = link_picker_display_rows(links, &filtered, outline);
        let visible_height = usize::from(rows[1].height).max(1);
        let (start, end) = link_picker_row_window(&display_rows, selected, visible_height);
        let width = usize::from(rows[1].width);
        let number_width = filtered.len().to_string().len().max(1);
        let lines: Vec<_> = display_rows[start..end]
            .iter()
            .map(|display_row| match display_row {
                LinkPickerDisplayRow::Section(index) => {
                    link_picker_section_heading_line(&outline[*index].title, width, colors)
                }
                LinkPickerDisplayRow::Page(source_page) => {
                    link_picker_page_heading_line(*source_page, *source_page == page, width, colors)
                }
                LinkPickerDisplayRow::Link { index, ordinal } => link_picker_entry_line(
                    *ordinal,
                    *index,
                    &links[*index],
                    if links_focused { selected } else { usize::MAX },
                    number_width,
                    width,
                    colors,
                ),
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(colors.surface)),
            rows[1],
        );
    }

    if detail_height > 0
        && let Some(position) = selected_position
        && let Some(link) = links.get(selected)
    {
        draw_link_picker_detail(frame, rows[2], link, position, colors);
    }

    if layout == LinkPickerLayout::Floating {
        let status = if filtered.is_empty() {
            if state.filter.is_empty() {
                "no links".to_string()
            } else {
                "no matches".to_string()
            }
        } else {
            format!("{}/{}", selected_position.unwrap_or(0) + 1, filtered.len())
        };
        let action = match selected_position
            .and_then(|_| links.get(selected))
            .map(|link| &link.target)
        {
            Some(LinkTarget::Uri(_)) => "copy URL",
            _ => "jump",
        };
        let escape_action = if state.filtering || !state.filter.is_empty() {
            "clear"
        } else {
            "close"
        };
        let compact_bindings = [("j/k", ""), ("/", ""), ("#", ""), ("↵", ""), ("esc", "")];
        let medium_bindings = [
            ("j/k C-b/f g/G", ""),
            ("/", ""),
            ("#", ""),
            ("enter", action),
            ("esc", ""),
        ];
        let full_bindings = [
            ("j/k C-b/f g/G", "navigate"),
            ("/", "filter"),
            ("#", "select"),
            ("enter", action),
            ("s/a", "layout"),
            ("esc", escape_action),
        ];
        let bindings = if rows[3].width < 48 {
            compact_bindings.as_slice()
        } else if rows[3].width < 110 {
            medium_bindings.as_slice()
        } else {
            full_bindings.as_slice()
        };
        frame.render_widget(
            Paragraph::new(picker_hint_line(
                bindings,
                Some((status, colors.muted)),
                colors,
            ))
            .style(Style::default().bg(colors.chrome)),
            rows[3],
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LinkPickerDisplayRow {
    Section(usize),
    Page(u32),
    Link { index: usize, ordinal: usize },
}

fn link_picker_display_rows(
    links: &[DocumentLink],
    filtered: &[usize],
    outline: &[OutlineItem],
) -> Vec<LinkPickerDisplayRow> {
    let mut rows = Vec::with_capacity(filtered.len().saturating_mul(2));
    let mut previous_section = None;
    let mut previous_page = None;
    for (ordinal, index) in filtered.iter().copied().enumerate() {
        let link = &links[index];
        let section = link_picker_section_index(outline, link.source_page);
        if section != previous_section {
            if let Some(section) = section {
                rows.push(LinkPickerDisplayRow::Section(section));
            }
            previous_section = section;
            previous_page = None;
        }
        if previous_page != Some(link.source_page) {
            rows.push(LinkPickerDisplayRow::Page(link.source_page));
            previous_page = Some(link.source_page);
        }
        rows.push(LinkPickerDisplayRow::Link { index, ordinal });
    }
    rows
}

fn link_picker_link_at_position(
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    geometry: LinkPickerGeometry,
    column: u16,
    row: u16,
) -> Option<usize> {
    let list_area = link_picker_list_area(area, geometry);
    if column < list_area.x
        || column >= list_area.x.saturating_add(list_area.width)
        || row < list_area.y
        || row >= list_area.y.saturating_add(list_area.height)
    {
        return None;
    }
    let filtered = filter_document_links(document.links, &state.filter);
    let display_rows = link_picker_display_rows(document.links, &filtered, document.outline);
    let (start, end) = link_picker_row_window(
        &display_rows,
        state.selected,
        usize::from(list_area.height).max(1),
    );
    let offset = usize::from(row.saturating_sub(list_area.y));
    match display_rows
        .get(start + offset)
        .filter(|_| start + offset < end)
    {
        Some(LinkPickerDisplayRow::Link { index, .. }) => Some(*index),
        _ => None,
    }
}

fn link_picker_list_area(area: Rect, geometry: LinkPickerGeometry) -> Rect {
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let (_, pane) = link_picker_panes(area, geometry);
    let borders = match layout {
        LinkPickerLayout::Vertical => Borders::LEFT,
        LinkPickerLayout::Horizontal => Borders::TOP,
        LinkPickerLayout::Floating => Borders::ALL,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let inner = Block::default().borders(borders).inner(pane);
    let detail_height = link_picker_detail_height(inner.height);
    let footer_height = u16::from(layout == LinkPickerLayout::Floating);
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(detail_height),
        Constraint::Length(footer_height),
    ])
    .split(inner)[1]
}

fn link_picker_section_index(outline: &[OutlineItem], page: u32) -> Option<usize> {
    outline
        .iter()
        .enumerate()
        .filter(|(_, item)| item.page <= page)
        .max_by_key(|(index, item)| (item.page, *index))
        .map(|(index, _)| index)
}

fn link_picker_row_window(
    rows: &[LinkPickerDisplayRow],
    selected: usize,
    visible_height: usize,
) -> (usize, usize) {
    let selected_row = rows
        .iter()
        .position(|row| {
            matches!(
                row,
                LinkPickerDisplayRow::Link { index, .. } if *index == selected
            )
        })
        .unwrap_or(0);
    let height = visible_height.max(1).min(rows.len());
    let max_start = rows.len().saturating_sub(height);
    let start = selected_row.saturating_sub(height / 2).min(max_start);
    (start, start + height)
}

fn link_picker_page_heading_line(
    page: u32,
    current: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let label = if current {
        format!(" Page {} · current ", page + 1)
    } else {
        format!(" Page {} ", page + 1)
    };
    let used = 2 + Line::raw(label.as_str()).width();
    let mut spans = vec![
        Span::styled("  ", Style::default().bg(colors.surface)),
        Span::styled(
            label,
            Style::default()
                .fg(if current { colors.recent } else { colors.muted })
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if used < width {
        spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    Line::from(spans)
}

fn link_picker_section_heading_line(
    title: &str,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let prefix = " Section  ";
    let label = truncate_right(title, width.saturating_sub(Line::raw(prefix).width() + 1));
    let mut line = Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(colors.loading)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{label} "),
            Style::default()
                .fg(colors.text)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let used = line.width();
    if used < width {
        line.spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    line
}

fn link_picker_entry_line(
    ordinal: usize,
    index: usize,
    link: &DocumentLink,
    selected: usize,
    number_width: usize,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let is_selected = index == selected;
    let background = if is_selected {
        colors.selection
    } else {
        colors.surface
    };
    let style = Style::default().fg(colors.text).bg(background);
    let number = format!("{:>number_width$}  ", ordinal + 1);
    let prefix_width = 2 + Line::raw(number.as_str()).width();
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(prefix_width),
    );
    let mut line = Line::from(vec![
        Span::styled(
            if is_selected { "▌ " } else { "  " },
            style.fg(colors.accent),
        ),
        Span::styled(number, style.fg(colors.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            label,
            if is_selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        ),
    ]);
    let used = line.width();
    if used < width {
        line.spans
            .push(Span::styled(" ".repeat(width - used), style));
    }
    line
}

fn draw_link_picker_detail(
    frame: &mut RatatuiFrame,
    area: Rect,
    link: &DocumentLink,
    selected: usize,
    colors: PickerTheme,
) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(colors.border))
        .style(Style::default().bg(colors.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = usize::from(inner.width);
    let number = format!("{}  ", selected + 1);
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(Line::raw(number.as_str()).width()),
    );
    let target = match &link.target {
        LinkTarget::Internal { page, .. } => format!("Slide {}", page + 1),
        LinkTarget::Uri(uri) => uri.clone(),
    };
    let source = format!("Page {}", link.source_page + 1);
    let fixed_width = Line::raw(source.as_str()).width() + 3;
    let target = truncate_right(&target, width.saturating_sub(fixed_width));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                number,
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label,
                Style::default()
                    .fg(colors.text)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                source,
                Style::default().fg(colors.recent).bg(colors.surface),
            ),
            Span::styled(" → ", Style::default().fg(colors.muted).bg(colors.surface)),
            Span::styled(
                target,
                Style::default().fg(colors.text_dim).bg(colors.surface),
            ),
        ]),
    ];
    let mut lines = lines;
    let truncate_context = inner.height <= 4;
    if let Some(context) = link.source_context.as_deref() {
        lines.push(link_picker_context_line(
            "Context",
            context,
            width,
            truncate_context,
            colors.loading,
            colors,
        ));
    }
    if let Some(context) = link.reference_context.as_deref() {
        lines.push(link_picker_context_line(
            "Reference",
            context,
            width,
            truncate_context,
            colors.recent,
            colors,
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn link_picker_context_line(
    label: &'static str,
    context: &str,
    width: usize,
    truncate: bool,
    label_color: RatatuiColor,
    colors: PickerTheme,
) -> Line<'static> {
    let label = format!("{label}  ");
    let context = if truncate {
        truncate_right(
            context,
            width.saturating_sub(Line::raw(label.as_str()).width()),
        )
    } else {
        context.to_string()
    };
    Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(context, Style::default().fg(colors.text).bg(colors.surface)),
    ])
}

fn link_picker_label(label: &str) -> String {
    let citation = label.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '[' | ']' | ',' | ';')
    });
    if !citation.is_empty() && citation.chars().all(|character| character.is_ascii_digit()) {
        format!("citation [{citation}]")
    } else {
        label.to_string()
    }
}

fn draw_help_menu(frame: &mut RatatuiFrame, theme: Palette) {
    const NAVIGATION: &[(&str, &str)] = &[
        ("j/k · ↑/↓", "scroll vertically"),
        ("h/l · ←/→", "move horizontally"),
        ("Space · PgDn", "page viewport forward"),
        ("Backspace · PgUp", "page viewport backward"),
        ("g / G", "first / last page"),
        (":", "go to page"),
        ("/", "search document"),
        ("n / N", "next / prev match"),
        ("Mouse wheel", "scroll vertically"),
        ("Enter", "browse slide links"),
        ("Click", "follow slide link"),
        ("h / l (split)", "focus slide/pane"),
        ("/ (pane)", "filter/new search"),
        ("Ctrl-b/f (pane)", "page results"),
        ("g / G (pane)", "first/last"),
        ("Tab / Shift-Tab", "switch tabs"),
        ("Alt-1 … Alt-9", "select tab"),
    ];
    const VIEWER: &[(&str, &str)] = &[
        ("m", "fit page / width / height / content"),
        ("+ / -", "zoom in / out"),
        ("0", "reset zoom"),
        ("i", "toggle dark mode"),
        ("S", "toggle smooth scroll"),
        ("p", "performance timings"),
        ("t", "table of contents"),
        ("T", "choose theme"),
        ("y", "copy page text"),
        ("L", "link mode + browser"),
        ("s / a", "cycle / auto layout"),
        ("b", "back from link"),
        ("f", "open deck in new tab"),
        ("D", "duplicate tab at current view"),
        ("q", "leave mode / close tab"),
        ("Esc", "leave mode / clear / exit"),
        ("?", "open help"),
    ];

    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = if area.width < 110 {
        area
    } else {
        picker_rect(area)
    };
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Help ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if inner.width < 80 { 10 } else { 6 }),
        Constraint::Length(1),
    ])
    .split(inner);
    let columns =
        if inner.width < 90 && usize::from(rows[0].height) >= NAVIGATION.len() + VIEWER.len() + 4 {
            Layout::vertical([
                Constraint::Length(NAVIGATION.len() as u16 + 2),
                Constraint::Min(1),
            ])
            .split(rows[0])
        } else {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(2)
                .split(rows[0])
        };

    frame.render_widget(
        Paragraph::new(help_lines("Navigation", NAVIGATION, 18, colors))
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(help_lines("Viewer", VIEWER, 18, colors))
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        columns[1],
    );
    let path = crate::config::config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "HOME is unset".into());
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Config: {path}")),
            Line::from("[viewer]: smooth_scroll, scroll_frame_ms, scroll_ease_divisor"),
            Line::from("[viewer]: continuous_scroll, prefetch_pages, set_window_title"),
            Line::from("Commented defaults on first launch. Edit config, then restart."),
        ])
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .wrap(Wrap { trim: true }),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(picker_hint_line(&[("? / esc / q", "close")], None, colors))
            .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

fn help_lines(
    title: &'static str,
    bindings: &[(&str, &str)],
    key_width: usize,
    colors: PickerTheme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(bindings.len() + 2);
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(colors.directory)
            .bg(colors.surface)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(bindings.iter().map(|(key, action)| {
        Line::from(vec![
            Span::styled(
                format!(" {key:<key_width$}"),
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.selection)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {action}"),
                Style::default().fg(colors.text).bg(colors.surface),
            ),
        ])
    }));
    lines
}

fn filter_theme_indices(themes: &[(String, Palette)], filter: &str) -> Vec<usize> {
    let filter = filter.trim().to_lowercase();
    themes
        .iter()
        .enumerate()
        .filter_map(|(index, (name, _))| {
            (filter.is_empty() || name.to_lowercase().contains(&filter)).then_some(index)
        })
        .collect()
}

fn draw_theme_picker(
    frame: &mut RatatuiFrame,
    themes: &[(String, Palette)],
    filtered: &[usize],
    selected: usize,
    filter: &str,
    filtering: bool,
) {
    let selected_theme = filtered.get(selected).copied().unwrap_or(0);
    let theme = themes
        .get(selected_theme)
        .map_or(crate::theme::TOKYO_NIGHT_MOON, |(_, theme)| *theme);
    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Themes ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter_line = if filter.is_empty() && !filtering {
        Line::from(Span::styled(
            " / to filter",
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(
                filter.to_string(),
                Style::default().fg(colors.text).bg(colors.chrome),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter_line).style(Style::default().bg(colors.chrome)),
        rows[0],
    );

    let visible_height = usize::from(rows[1].height).max(1);
    let first_visible = selected.saturating_add(1).saturating_sub(visible_height);
    let width = usize::from(rows[1].width);
    let lines: Vec<_> = filtered
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_height)
        .map(|(position, index)| {
            let is_selected = position == selected;
            let name = &themes[*index].0;
            let background = if is_selected {
                colors.selection
            } else {
                colors.surface
            };
            let mut style = Style::default().fg(colors.text).bg(background);
            if is_selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            let mut line = Line::from(vec![
                Span::styled(
                    if is_selected { "▌ " } else { "  " },
                    Style::default().fg(colors.accent).bg(background),
                ),
                Span::styled(name.clone(), style),
            ]);
            let used = line.width();
            if used < width {
                line.spans.push(Span::styled(
                    " ".repeat(width - used),
                    Style::default().bg(background),
                ));
            }
            line
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[1],
    );

    let status = Some((
        format!(
            "{}/{}",
            usize::from(!filtered.is_empty()) * (selected + 1),
            filtered.len()
        ),
        colors.muted,
    ));
    frame.render_widget(
        Paragraph::new(picker_hint_line(
            &[
                ("j/k", "select"),
                ("^b/^f", "page"),
                ("g/G", "ends"),
                ("/", "filter"),
                ("enter", "apply"),
                ("esc", if filtering { "clear" } else { "cancel" }),
            ],
            status,
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

fn picker_recent_heading_line(width: usize, colors: PickerTheme) -> Line<'static> {
    let label = " Most Recent ";
    let mut spans = vec![
        Span::styled("  ", Style::default().bg(colors.surface)),
        Span::styled(
            label,
            Style::default()
                .fg(colors.recent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let used = 2 + label.chars().count();
    if used < width {
        spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    Line::from(spans)
}

fn truncate_left(value: &str, max_width: usize) -> String {
    if Line::raw(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let suffix_width = max_width.saturating_sub(1);
    let mut suffix = Vec::new();
    let mut used = 0;
    for character in value.chars().rev() {
        let width = Line::raw(character.to_string()).width();
        if used + width > suffix_width {
            break;
        }
        suffix.push(character);
        used += width;
    }
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

fn truncate_right(value: &str, max_width: usize) -> String {
    if Line::raw(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let prefix_width = max_width.saturating_sub(1);
    let mut prefix = String::new();
    let mut used = 0;
    for character in value.chars() {
        let width = Line::raw(character.to_string()).width();
        if used + width > prefix_width {
            break;
        }
        prefix.push(character);
        used += width;
    }
    prefix.push('…');
    prefix
}

fn picker_entry_line(
    entry: &BrowserEntry,
    browser: &BrowserState,
    selected: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let background = if selected {
        colors.selection
    } else {
        colors.surface
    };
    let marker_style = Style::default().fg(colors.accent).bg(background);
    let icon = if entry.name == "../" {
        "↑ "
    } else if entry.is_dir {
        "› "
    } else {
        "  "
    };
    let icon_color = if entry.name == "../" {
        colors.text_dim
    } else if entry.is_dir {
        colors.directory
    } else if entry.is_recent {
        colors.recent
    } else {
        colors.text
    };
    let mut spans = vec![
        Span::styled(if selected { "▌ " } else { "  " }, marker_style),
        Span::styled(icon, Style::default().fg(icon_color).bg(background)),
    ];

    let display_name = if browser.filter.is_empty() || entry.name == "../" {
        entry.name.clone()
    } else {
        let mut name = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        if entry.is_dir && !name.ends_with('/') {
            name.push('/');
        }
        name
    };
    let matches = browser.match_indices(&display_name);
    let basename_start = if entry.is_dir || !browser.filter.is_empty() {
        0
    } else {
        display_name
            .char_indices()
            .rev()
            .find(|(_, character)| *character == '/')
            .map_or(0, |(index, _)| display_name[..=index].chars().count())
    };
    for (index, character) in display_name.chars().enumerate() {
        let foreground = if matches.binary_search(&index).is_ok() {
            colors.matched
        } else if index < basename_start || entry.name == "../" {
            colors.text_dim
        } else if entry.is_recent {
            colors.recent
        } else if entry.is_dir {
            colors.directory
        } else {
            colors.text
        };
        let mut style = Style::default().fg(foreground).bg(background);
        if matches.binary_search(&index).is_ok() || (selected && index >= basename_start) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(character.to_string(), style));
    }

    let mut line = Line::from(spans);
    let mut used = line.width();
    let source = (!browser.filter.is_empty())
        .then(|| browser.entry_source(entry))
        .flatten();
    let show_parent = entry.is_recent || source == Some(BrowserEntrySource::Subdirectory);
    let parent = show_parent
        .then(|| entry.path.parent())
        .flatten()
        .map(|parent| shorten_path(&parent.to_string_lossy()));
    if entry.is_recent || source.is_some() {
        let available = width.saturating_sub(used + 2);
        let label = source.map(BrowserEntrySource::label);
        let label_width = label.map_or(0, |label| Line::raw(label).width());
        let separator_width = usize::from(label.is_some() && parent.is_some()) * 2;
        if available >= label_width + separator_width + usize::from(parent.is_some()) {
            let parent_width = available.saturating_sub(label_width + separator_width);
            let parent = parent.map(|parent| truncate_left(&parent, parent_width));
            let context_width = label_width
                + separator_width
                + parent
                    .as_deref()
                    .map_or(0, |parent| Line::raw(parent).width());
            let gap = width.saturating_sub(used + context_width);
            line.spans.push(Span::styled(
                " ".repeat(gap),
                Style::default().bg(background),
            ));
            if let Some(label) = label {
                let foreground = match source {
                    Some(BrowserEntrySource::Recent) => colors.recent,
                    Some(BrowserEntrySource::Here) => colors.loading,
                    Some(BrowserEntrySource::Subdirectory) => colors.directory,
                    None => colors.muted,
                };
                line.spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(foreground)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if label.is_some() && parent.is_some() {
                line.spans
                    .push(Span::styled("  ", Style::default().bg(background)));
            }
            if let Some(parent) = parent {
                let matches = browser.match_indices(&parent);
                for (index, character) in parent.chars().enumerate() {
                    let matched = matches.binary_search(&index).is_ok();
                    let mut style = Style::default()
                        .fg(if matched {
                            colors.matched
                        } else {
                            colors.text_dim
                        })
                        .bg(background);
                    if matched {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    line.spans.push(Span::styled(character.to_string(), style));
                }
            }
            used = width;
        }
    }
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(background),
        ));
    }
    line
}

fn picker_hint_line(
    bindings: &[(&str, &str)],
    status: Option<(String, RatatuiColor)>,
    colors: PickerTheme,
) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default().bg(colors.chrome))];
    for (key, action) in bindings {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(colors.accent)
                .bg(colors.selection)
                .add_modifier(Modifier::BOLD),
        ));
        if !action.is_empty() {
            spans.push(Span::styled(
                format!(" {action}  "),
                Style::default().fg(colors.muted).bg(colors.chrome),
            ));
        }
    }
    if let Some((status, color)) = status {
        spans.push(Span::styled(
            status,
            Style::default().fg(color).bg(colors.chrome),
        ));
    }
    Line::from(spans)
}

fn picker_rect(area: Rect) -> Rect {
    let width = if area.width > 4 {
        (area.width * 3 / 4).max(50).min(area.width - 4)
    } else {
        area.width.max(1)
    };
    let height = if area.height > 4 {
        (area.height * 3 / 4).max(6).min(area.height - 2)
    } else {
        area.height.max(1)
    };
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn link_picker_detail_height(height: u16) -> u16 {
    if height >= 24 {
        8
    } else if height >= 14 {
        6
    } else if height >= 9 {
        5
    } else {
        0
    }
}

fn link_picker_visible_height(area: Rect, geometry: LinkPickerGeometry) -> usize {
    let (_, pane) = link_picker_panes(area, geometry);
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let border_height = match layout {
        LinkPickerLayout::Vertical => 0,
        LinkPickerLayout::Horizontal => 1,
        LinkPickerLayout::Floating => 2,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let chrome_height = if layout == LinkPickerLayout::Floating {
        3
    } else {
        2
    };
    let content_height = pane.height.saturating_sub(border_height);
    usize::from(
        content_height
            .saturating_sub(chrome_height + link_picker_detail_height(content_height))
            .max(1),
    )
}

fn shorten_path(path: &str) -> String {
    std::env::var_os("HOME")
        .and_then(|home| {
            path.strip_prefix(home.to_string_lossy().as_ref())
                .map(|suffix| format!("~{suffix}"))
        })
        .unwrap_or_else(|| path.to_string())
}

fn picker_color(color: crossterm::style::Color) -> RatatuiColor {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => RatatuiColor::Rgb(r, g, b),
        _ => RatatuiColor::Reset,
    }
}

#[derive(Clone, Copy)]
struct PickerTheme {
    backdrop: RatatuiColor,
    surface: RatatuiColor,
    chrome: RatatuiColor,
    selection: RatatuiColor,
    border: RatatuiColor,
    accent: RatatuiColor,
    directory: RatatuiColor,
    recent: RatatuiColor,
    matched: RatatuiColor,
    loading: RatatuiColor,
    text: RatatuiColor,
    text_dim: RatatuiColor,
    muted: RatatuiColor,
}

impl From<Palette> for PickerTheme {
    fn from(theme: Palette) -> Self {
        Self {
            backdrop: picker_color(theme.bg_dark1),
            surface: picker_color(theme.bg),
            chrome: picker_color(theme.bg_dark),
            selection: picker_color(theme.bg_highlight),
            border: picker_color(theme.blue7),
            accent: picker_color(theme.blue),
            directory: picker_color(theme.blue1),
            recent: picker_color(theme.yellow),
            matched: picker_color(theme.magenta),
            loading: picker_color(theme.cyan),
            text: picker_color(theme.fg),
            text_dim: picker_color(theme.fg_dark),
            muted: picker_color(theme.comment),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn horizontal_navigation_only_scrolls_to_reveal_hidden_targets() {
        let scroll = |current, left, right| {
            super::horizontal_scroll_to_reveal(current, 2000, 800, left, right)
        };
        assert_eq!(scroll(300, 400.0, 500.0), 300);
        assert_eq!(scroll(300, 300.0, 1100.0), 300);
        assert_eq!(scroll(300, 250.0, 400.0), 250);
        assert_eq!(scroll(300, 1000.0, 1150.0), 350);
        assert_eq!(scroll(300, 1100.0, 1100.0), 301);
        assert_eq!(scroll(300, 400.0, 400.0), 300);
        assert_eq!(scroll(300, 100.0, 1400.0), 300);
        assert_eq!(scroll(0, 100.0, 1400.0), 100);
        assert_eq!(scroll(900, 100.0, 1400.0), 600);
        assert_eq!(scroll(300, 2000.0, 2000.0), 1200);
        // Shared continuous scroll can exceed this narrow page's own crop.
        // Its x=50 target is visible at the actual per-page offset 20.
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 120, 100, 50.0, 51.0),
            100,
        );
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 120, 100, 0.0, 10.0),
            0,
        );
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 60, 100, 30.0, 35.0),
            100,
        );
    }

    use super::{
        BrowserState, FILE_STABLE_FOR, FileFingerprint, FileWatcher, LinkIndexProgress,
        LinkPickerDocument, LinkPickerFocus, LinkPickerGeometry, LinkPickerImage, LinkPickerState,
        PerformanceSnapshot, PositionedImage, SearchPickerState, SearchState, ZOOM_DEFAULT,
        ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, apply_picker_navigation, centered_scaled_view,
        clear_image_canvas, clear_picker, clear_picker_filter, cycled_tab_index, draw_link_picker,
        draw_picker, draw_search_picker, draw_theme_picker, filter_document_links, filter_outline,
        filter_theme_indices, link_at_cell, link_picker_focus_for_key, link_picker_label,
        link_picker_link_at_position, link_picker_list_area, link_picker_navigation_index,
        link_picker_panes, link_picker_visible_height, next_link_picker_layout, numbered_tab_index,
        outline_start_index, picker_color, picker_rect, render_timing_status,
        restore_link_picker_split, search_target_page, show_link_picker_split, stale_status_row,
        stepped_zoom, synchronized_output, update_link_number_selection, write_clipboard_osc52,
    };
    use crate::config::LinkPickerLayout;
    use crate::kitty::Placement;
    use crate::pptx::{
        DocumentLink, LinkTarget, OutlineItem, PageLink, PageLinkRect, SearchPageMatch,
    };
    use crate::terminal::{ImagePlacement, Viewport};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::cell::Cell;
    use std::fs;
    use std::io::{self, Write};
    use std::time::Instant;

    fn continuous_frame(
        key: crate::pptx::RenderKey,
        revision: crate::pptx::DocumentRevision,
        _highlighted: bool,
    ) -> std::sync::Arc<crate::pptx::Frame> {
        std::sync::Arc::new(crate::pptx::Frame {
            key,
            revision,
            width: 80,
            height: 240,
            page_width_pt: 80.0,
            page_height_pt: 240.0,
            content_bounds: [0.0, 0.0, 1.0, 1.0],
            compressed_rgba: crate::kitty::compress_rgba(&[255; 80 * 240 * 4]).unwrap(),
            render_elapsed: std::time::Duration::ZERO,
            dark_mode_elapsed: None,
            highlight_elapsed: None,
            compression_elapsed: std::time::Duration::ZERO,
            generation: 0,
            links: Vec::new(),
        })
    }

    #[test]
    fn tab_basenames_cannot_emit_filename_controls() {
        use std::os::unix::ffi::OsStringExt;

        let (mut app, _, _file) = continuous_app();
        let (mut other, _, _other_file) = continuous_app();
        app.session.tabs.push(other.session.tabs.remove(0));
        let mut bytes = b"old\x1b]52;c;owned\x07\n\r\t\x7f".to_vec();
        bytes.extend_from_slice("\u{85}界".as_bytes());
        bytes.push(0xff);
        bytes.extend_from_slice(b".pptx");
        let name = std::ffi::OsString::from_vec(bytes);
        app.session.tabs[0].path = std::path::PathBuf::from(&name);
        app.session.tabs[1].path = std::path::PathBuf::from(&name);
        let safe: String = name
            .to_string_lossy()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        for columns in [0, 8, 36, 120] {
            let mut actual = Vec::new();
            app.draw_tab_bar_at_width(&mut actual, columns).unwrap();
            for tab in &mut app.session.tabs {
                tab.path = std::path::PathBuf::from(&safe);
            }
            let mut expected = Vec::new();
            app.draw_tab_bar_at_width(&mut expected, columns).unwrap();
            assert_eq!(actual, expected);
            for tab in &mut app.session.tabs {
                tab.path = std::path::PathBuf::from(&name);
            }
        }
    }

    #[test]
    fn search_replacement_completion_and_clear_evict_obsolete_frames_immediately() {
        let (mut app, viewport, _file) = continuous_app();
        let document_id = app.tab().document_id;
        let revision = app.tab().revision;
        let plain_neighbor = app.page_key(1, viewport);
        app.tab_mut().cache.insert(
            plain_neighbor,
            continuous_frame(plain_neighbor, revision, false),
        );
        for request_id in 1..=8 {
            app.tab_mut().replace_search(super::SearchState {
                query: format!("query {request_id}"),
                request_id,
                total_pages: 2,
                searching: true,
                ..Default::default()
            });
            assert!(app.tab().cache.keys().all(|key| key.search_request_id == 0));
            app.apply_search_results(
                document_id,
                request_id,
                vec![SearchPageMatch {
                    page: 0,
                    occurrences: 1,
                    context: "match".into(),
                }],
                1,
            )
            .unwrap();
            let highlighted = app.page_key(0, viewport);
            assert_eq!(highlighted.search_request_id, request_id);
            assert!(
                app.store_frame((*continuous_frame(highlighted, revision, true)).clone())
                    .is_some()
            );
            let wrong_neighbor = crate::pptx::RenderKey {
                page: 1,
                ..highlighted
            };
            assert!(
                app.store_frame((*continuous_frame(wrong_neighbor, revision, true)).clone())
                    .is_none()
            );
            assert_eq!(app.tab().cache.len(), 2);
        }
        let highlighted = app.page_key(0, viewport);
        app.tab_mut().replace_search(super::SearchState::default());
        assert_eq!(app.tab().cache.len(), 1);
        assert!(app.tab().cache.contains_key(&plain_neighbor));
        assert!(
            app.store_frame((*continuous_frame(highlighted, revision, true)).clone())
                .is_none()
        );
    }

    fn continuous_app() -> (super::App, Viewport, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let revision = crate::pptx::DocumentRevision::read(file.path()).unwrap();
        // These draw tests supply completed frames and an already-pending
        // replacement. A disconnected worker keeps them independent of background threads.
        let worker = crate::pptx::PptxWorker::disconnected();
        let config = toml::from_str("").unwrap();
        let defaults = super::AppDefaults::from_config(&config);
        let mut app = super::App::new(
            worker,
            2,
            0,
            file.path().to_owned(),
            FileWatcher::new(file.path()).unwrap(),
            (Vec::new(), revision),
            defaults,
        );
        let viewport = Viewport {
            columns: 8,
            rows: 20,
            pixel_width: 80,
            pixel_height: 200,
            top: 1,
            status_row: 21,
        };
        app.test_viewport = Some(viewport);
        app.tab_mut().scroll_y = 100;
        for page in 0..2 {
            let key = app.page_key(page, viewport);
            app.tab_mut()
                .cache
                .insert(key, continuous_frame(key, revision, page == 1));
        }
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        // Evict the second page while its replacement is pending, leaving its
        // terminal image visible.
        let target = app.page_key(1, viewport);
        app.tab_mut().cache.retain(|key, _| key.page != 1);
        app.pending.insert(target);
        (app, viewport, file)
    }

    fn continuous_gap_app() -> (super::App, Viewport, tempfile::NamedTempFile) {
        let (mut app, viewport, file) = continuous_app();
        let revision = crate::pptx::DocumentRevision::read(file.path()).unwrap();
        let key = app.page_key(1, viewport);
        let mut neighbor = continuous_frame(key, revision, false);
        let frame = std::sync::Arc::get_mut(&mut neighbor).unwrap();
        frame.width = 160;
        frame.height = 120;
        frame.compressed_rgba =
            crate::kitty::compress_rgba(&[220, 40, 80, 255].repeat(160 * 120)).unwrap();
        app.tab_mut().cache.insert(key, neighbor);
        app.tab_mut().scroll_y = 245;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        (app, viewport, file)
    }

    #[test]
    fn continuous_gap_screenshot_uses_submitted_neighbor_pixels() {
        let (app, viewport, _file) = continuous_gap_app();
        let pages = app.screenshot_pages(viewport).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gap.png");
        crate::screenshot::save(
            &path,
            viewport,
            &pages,
            &crate::screenshot::blank_overlay(viewport).unwrap(),
            [1, 2, 3],
        )
        .unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (80, 200));
        let at = |x: usize, y: usize| &pixels[(y * 80 + x) * 4..(y * 80 + x + 1) * 4];
        assert_eq!(at(0, 4), [1, 2, 3, 255]);
        assert_eq!(at(79, 5), [220, 40, 80, 255]);
        assert_eq!(at(0, 124), [220, 40, 80, 255]);
        assert_eq!(at(0, 125), [1, 2, 3, 255]);
    }

    #[test]
    fn continuous_gap_picker_maps_submitted_neighbor_and_preserves_gap_view() {
        let (mut app, viewport, _file) = continuous_gap_app();
        let image = app.picker_image(viewport).unwrap();
        assert_eq!((image.source_width, image.source_height), (80, 120));
        assert_eq!(image.original.placement.offset_y, 5);
        assert_eq!(
            (
                image.original.placement.columns,
                image.original.placement.rows
            ),
            (0, 0)
        );
        app.retain_primary_image(&mut Vec::new()).unwrap();
        app.link_picker = Some(LinkPickerState::new(1));
        app.link_picker_geometry.layout = LinkPickerLayout::Vertical;
        let (preview, _) =
            link_picker_panes(super::link_picker_area(viewport), app.link_picker_geometry);
        let positioned =
            super::position_link_picker_image(image, preview, LinkPickerLayout::Vertical);
        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: positioned.left + positioned.placement.columns - 1,
            row: positioned.top + positioned.placement.rows - 1,
            modifiers: KeyModifiers::NONE,
        };
        let (frame, placement, top) = app.page_at_mouse(mouse, viewport).unwrap();
        let hit = placement
            .source_cell(mouse.column, mouse.row - top, frame.width, frame.height)
            .unwrap();
        assert_eq!(frame.key.page, 1);
        assert_eq!((hit.x + hit.width, hit.y + hit.height), (80, 120));
        app.link_picker_geometry.layout = LinkPickerLayout::Floating;
        let mouse = crossterm::event::MouseEvent {
            column: 0,
            row: viewport.top,
            ..mouse
        };
        let (frame, placement, top) = app.page_at_mouse(mouse, viewport).unwrap();
        let hit = placement
            .source_cell(mouse.column, mouse.row - top, frame.width, frame.height)
            .unwrap();
        assert_eq!((hit.y, hit.height), (0, 5));
        assert_eq!((app.tab().page, app.tab().scroll_y), (0, 245));
    }

    #[test]
    fn screenshot_rejects_missing_or_stale_visible_neighbors() {
        let (mut app, viewport, file) = continuous_app();
        let target = app.page_key(1, viewport);
        app.missing_visible_page = Some(target);
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
        app.missing_visible_page = None;
        fs::write(file.path(), b"changed revision").unwrap();
        let revision = crate::pptx::DocumentRevision::read(file.path()).unwrap();
        app.visible_pages[1].frame = continuous_frame(target, revision, false);
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
    }

    #[test]
    fn screenshot_rejects_new_anchor_until_its_view_is_submitted() {
        let (mut app, viewport, _file) = continuous_gap_app();
        app.tab_mut().page = 1;
        app.tab_mut().scroll_y = 0;
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
        let frame = app.tab().cache[&app.page_key(1, viewport)].clone();
        app.draw_continuous(&frame, viewport, &mut Vec::new())
            .unwrap();
        let pages = app.screenshot_pages(viewport).unwrap();
        assert_eq!(pages[0].placement.offset_y, 0);
        assert_eq!(pages[0].frame.key.page, 1);
    }

    #[test]
    fn rendered_destination_without_vertical_coordinate_preserves_scroll() {
        let (mut app, viewport, _file) = continuous_app();
        app.viewer.continuous_scroll = false;
        app.tab_mut().scroll_y = 20;
        app.tab_mut().pending_destination = Some(super::LinkDestination {
            page: 0,
            // A 90-degree XYZ null 300 null destination specifies only rendered x.
            top_ratio: None,
            left_ratio: Some(0.75),
        });
        let frame = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_frame_unsynchronized(&frame, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_y, 20);
    }

    #[test]
    fn continuous_redraw_clamps_retained_offsets_to_smaller_page() {
        let (mut app, mut viewport, _file) = continuous_app();
        viewport.pixel_width = 60;
        app.tab_mut().page_count = 1;
        app.tab_mut().scroll_x = 37;
        app.tab_mut().scroll_y = 123;
        let revision = app.tab().watcher.accepted;
        let key = app.page_key(0, viewport);
        let frame = continuous_frame(key, revision, false);
        app.tab_mut().cache.insert(key, frame.clone());
        app.draw_continuous(&frame, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (20, 40));
    }

    #[test]
    fn continuous_zoom_retains_x_offset_for_wider_page_below_narrow_top_page() {
        let (mut app, mut viewport, _file) = continuous_app();
        viewport.pixel_width = 60;
        let revision = app.tab().watcher.accepted;
        let narrow_key = app.page_key(0, viewport);
        let wide_key = app.page_key(1, viewport);
        let mut narrow = continuous_frame(narrow_key, revision, false);
        let first = std::sync::Arc::get_mut(&mut narrow).unwrap();
        first.width = 40;
        first.compressed_rgba = crate::kitty::compress_rgba(&[255; 40 * 240 * 4]).unwrap();
        let wide = continuous_frame(wide_key, revision, false);
        app.tab_mut().cache.clear();
        app.tab_mut().cache.insert(narrow_key, narrow.clone());
        app.tab_mut().cache.insert(wide_key, wide);
        app.tab_mut().scroll_x = 20;
        app.tab_mut().scroll_y = 220;
        app.draw_continuous(&narrow, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 20);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 0);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 20);
    }

    #[test]
    fn continuous_redraw_preserves_offset_for_wider_visible_target() {
        let (mut app, viewport, _file) = continuous_app();
        let target = app.page_key(1, viewport);
        let mut wide = continuous_frame(target, app.tab().watcher.accepted, true);
        std::sync::Arc::get_mut(&mut wide).unwrap().width = 160;
        app.tab_mut().cache.insert(target, wide);
        app.tab_mut().scroll_x = 45;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 45);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 0);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 45);
        app.tab_mut().cache.remove(&target);
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 45);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 45);
        app.tab_mut().scroll_y = 0;
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 0);
    }

    #[test]
    fn retained_enter_picker_reindexes_after_reload_resets_links() {
        let (mut app, _, _file) = continuous_app();
        assert!(!app.link_mode);
        app.link_picker = Some(LinkPickerState::new(0));
        app.start_link_index();
        let old_request = app.tab().link_index.request_id;

        // Reload replaces the document's index while retaining its open picker.
        app.tab_mut().link_index = super::LinkIndexState::new(2);
        app.ensure_link_index();
        let request_id = app.tab().link_index.request_id;
        assert_ne!(request_id, old_request);
        assert!(app.tab().link_index.indexing);
        let document_id = app.tab().document_id;
        app.receive_link_index_progress(
            document_id,
            super::LinkIndexUpdate {
                request_id,
                links: vec![DocumentLink {
                    source_page: 0,
                    ordinal: 0,
                    source_top_ratio: 0.1,
                    label: "Reloaded link".to_string(),
                    source_context: None,
                    reference_context: None,
                    target: LinkTarget::Uri("https://example.com/reloaded".to_string()),
                }],
                scanned: 2,
                total: 2,
                complete: true,
            },
            &mut Vec::new(),
        )
        .expect("fresh links should update the retained picker");
        assert_eq!(app.tab().link_index.links[0].label, "Reloaded link");
        assert_eq!(app.link_picker.as_ref().unwrap().selection_key, Some((0, 0)));
    }

    #[test]
    fn link_mode_picker_accepts_q_in_filter_text() {
        let (mut app, _, _file) = continuous_app();
        app.link_mode = true;
        app.link_picker = Some(LinkPickerState::new(0));
        for character in "/abcq".chars() {
            app.handle_link_picker_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut Vec::new(),
            )
            .expect("filter input should not close the picker");
        }
        let picker = app.link_picker.as_ref().expect("picker remains open");
        assert!(app.link_mode);
        assert!(picker.filtering);
        assert_eq!(picker.filter, "abcq");
    }

    #[test]
    fn reload_preserves_view_and_clamps_removed_pages() {
        let (mut app, _, _file) = continuous_app();
        let (mut other, _, _other_file) = continuous_app();
        let mut other_tab = other.session.tabs.pop().unwrap();
        other_tab.document_id += 1;
        app.session.tabs.push(other_tab);
        app.session.active_tab = 1;
        for pages in [2, 1] {
            let tab = &mut app.session.tabs[0];
            tab.page = 1;
            tab.scroll_x = 37;
            tab.scroll_y = 123;
            tab.zoom = 150;
            let document_id = tab.document_id;
            let revision = tab.revision;
            app.session.pending_open = Some(super::session::PendingOpen::Reload {
                document_id,
                fingerprint: tab.watcher.accepted,
            });
            app.finish_open(document_id, pages, Vec::new(), revision, &mut Vec::new())
                .unwrap();
            let tab = &app.session.tabs[0];
            assert_eq!(tab.page, pages - 1);
            assert_eq!((tab.scroll_x, tab.scroll_y, tab.zoom), (37, 123, 150));
            assert!(tab.cache.is_empty());
        }
    }

    #[test]
    fn continuous_redraw_retains_expired_flash_until_replacement_upload() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let target = app.page_key(1, viewport);
        let highlighted = app.visible_pages[1].frame.clone();
        let old_id = app.visible_pages[1].image_id;
        let delete = format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\");

        app.generation += 1;
        app.tab_mut().scroll_y = 115;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(app.missing_visible_page, Some(target));
        assert!(app.pending.contains(&target));
        assert!(!app.tab().cache.contains_key(&target));
        assert_eq!(app.visible_pages.len(), 2);
        let retained = &app.visible_pages[1];
        assert!(std::sync::Arc::ptr_eq(&retained.frame, &highlighted));
        assert_eq!(retained.image_id, old_id);
        assert_eq!(retained.top, 14);
        assert_eq!(retained.placement.offset_y, 5);
        assert_eq!(retained.placement.crop.unwrap().height, 65);
        assert!(output.contains(&format!("\x1b_Ga=p,i={old_id},")));
        assert!(!output.contains("\x1b_Ga=T,"));
        assert!(!output.contains(&delete));

        let clean = continuous_frame(target, highlighted.revision, false);
        app.tab_mut().cache.insert(target, clean.clone());
        app.pending.remove(&target);
        let new_id = app.next_image_id;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        let upload = output
            .find(&format!("\x1b_Ga=T,f=32,s=80,v=240,i={new_id},"))
            .unwrap();
        let upload_end = upload + output[upload..].find("\x1b\\").unwrap() + 2;
        assert!(upload_end <= output.find(&delete).unwrap());
        assert_eq!(app.missing_visible_page, None);
        assert!(std::sync::Arc::ptr_eq(&app.visible_pages[1].frame, &clean));
        assert_eq!(app.visible_pages[1].image_id, new_id);
    }

    #[test]
    fn continuous_redraw_removes_offscreen_cache_miss() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let old_id = app.visible_pages[1].image_id;
        app.tab_mut().scroll_y = 0;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(app.visible_pages.len(), 1);
        assert_eq!(app.visible_pages[0].frame.key.page, 0);
        assert_eq!(app.missing_visible_page, None);
        assert!(output.contains(&format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\")));
        assert!(!output.contains(&format!("\x1b_Ga=p,i={old_id},")));
    }

    #[test]
    fn continuous_redraw_rejects_incompatible_visible_frames() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let target = app.page_key(1, viewport);
        let revision = app.visible_pages[1].frame.revision;
        let other_file = tempfile::NamedTempFile::new().unwrap();
        let other_revision = crate::pptx::DocumentRevision::read(other_file.path()).unwrap();
        let mut incompatible = Vec::new();
        let changes: [fn(&mut crate::pptx::RenderKey); 13] = [
            |key: &mut crate::pptx::RenderKey| key.document_id += 1,
            |key: &mut crate::pptx::RenderKey| key.page += 1,
            |key: &mut crate::pptx::RenderKey| key.width += 1,
            |key: &mut crate::pptx::RenderKey| key.height += 1,
            |key: &mut crate::pptx::RenderKey| key.zoom += 1,
            |key: &mut crate::pptx::RenderKey| key.fit = crate::pptx::FitMode::Height,
            |key: &mut crate::pptx::RenderKey| key.invert = !key.invert,
            |key: &mut crate::pptx::RenderKey| key.dark_mode_style.background[0] ^= 1,
            |key: &mut crate::pptx::RenderKey| key.search_request_id += 1,
            |key: &mut crate::pptx::RenderKey| key.search_highlight[0] ^= 1,
            |key: &mut crate::pptx::RenderKey| key.link_mode = !key.link_mode,
            |key: &mut crate::pptx::RenderKey| key.link_highlight[0] ^= 1,
            |key: &mut crate::pptx::RenderKey| key.selected_link_ordinal = Some(1),
        ];
        for change in changes {
            let mut key = target;
            change(&mut key);
            incompatible.push((key, revision));
        }
        incompatible.push((target, other_revision));
        for (key, incompatible_revision) in incompatible {
            app.tab_mut()
                .cache
                .insert(target, continuous_frame(target, revision, true));
            app.draw_continuous(&primary, viewport, &mut Vec::new())
                .unwrap();
            app.tab_mut().cache.remove(&target);
            let old_id = app.visible_pages[1].image_id;
            app.visible_pages[1].frame = continuous_frame(key, incompatible_revision, true);
            let mut output = Vec::new();
            app.draw_continuous(&primary, viewport, &mut output)
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(app.visible_pages.len(), 1, "{key:?}");
            assert_eq!(app.missing_visible_page, Some(target));
            assert!(output.contains(&format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\")));
            assert!(!output.contains(&format!("\x1b_Ga=p,i={old_id},")));
        }
    }

    #[test]
    fn synchronized_output_closes_successful_and_failed_updates() {
        let mut successful = Vec::new();
        let result: io::Result<()> = synchronized_output(&mut successful, |output| {
            output.write_all(b"complete frame")
        });
        result.expect("successful synchronized update");
        assert_eq!(successful, b"\x1b[?2026hcomplete frame\x1b[?2026l");

        let mut failed = Vec::new();
        let result: io::Result<()> = synchronized_output(&mut failed, |output| {
            output.write_all(b"partial frame")?;
            Err(io::Error::other("render failed"))
        });
        assert_eq!(
            result.expect_err("failed synchronized update").kind(),
            io::ErrorKind::Other
        );
        assert_eq!(failed, b"\x1b[?2026hpartial frame\x1b[?2026l");
    }

    #[test]
    fn synchronized_output_batches_flushes_and_reports_submission_errors() {
        #[derive(Default)]
        struct Sink {
            bytes: Vec<u8>,
            writes: usize,
            flushes: usize,
            fail_flush: bool,
        }

        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                if self.fail_flush {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed terminal"))
                } else {
                    Ok(())
                }
            }
        }

        for fail_flush in [false, true] {
            let mut output = io::BufWriter::new(Sink {
                fail_flush,
                ..Sink::default()
            });
            let result: io::Result<()> = synchronized_output(&mut output, |frame| {
                frame.write_all(b"first")?;
                frame.flush()?;
                frame.write_all(b"second")?;
                frame.flush()
            });
            if fail_flush {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            } else {
                result.unwrap();
            }
            let sink = output.get_ref();
            assert_eq!(sink.bytes, b"\x1b[?2026hfirstsecond\x1b[?2026l");
            assert_eq!(sink.writes, 1);
            assert_eq!(sink.flushes, 1);
        }
    }

    #[test]
    fn render_timings_are_compact_by_default_and_expand_on_demand() {
        assert_eq!(
            render_timing_status(15, Some(12), Some(0), 27, 17, false),
            "render 71ms"
        );
        assert_eq!(
            render_timing_status(15, Some(12), Some(0), 27, 17, true),
            "render 15ms  dark 12ms  highlight 0ms  compress 27ms  transfer 17ms"
        );

        let snapshot = PerformanceSnapshot {
            render_ms: 15,
            dark_mode_ms: Some(12),
            highlight_ms: Some(0),
            compression_ms: 27,
            transfer_ms: 17,
            link_count: 5,
        };
        assert_eq!(snapshot.status(false, true), "render 71ms  5 page links");
    }

    #[test]
    fn path_navigation_prefers_the_active_duplicate_then_any_matching_tab() {
        let (mut app, _, _file) = continuous_app();
        let (mut other, _, other_file) = continuous_app();
        let path = app.tab().path.clone();
        let mut duplicate = other.session.tabs.pop().unwrap();
        duplicate.document_id += 1;
        duplicate.path = path.clone();
        app.session.tabs.push(duplicate);

        app.session.active_tab = 1;
        assert_eq!(app.tab_index_for_path(&path), Some(1));
        app.session.active_tab = 0;
        assert_eq!(app.tab_index_for_path(&path), Some(0));

        app.session.tabs[0].path = other_file.path().to_owned();
        assert_eq!(app.tab_index_for_path(&path), Some(1));
    }

    #[test]
    fn tab_switching_wraps_in_both_directions() {
        assert_eq!(cycled_tab_index(0, 3, 1), 1);
        assert_eq!(cycled_tab_index(2, 3, 1), 0);
        assert_eq!(cycled_tab_index(2, 3, -1), 1);
        assert_eq!(cycled_tab_index(0, 3, -1), 2);
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
            Some(0)
        );
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT)),
            Some(8)
        );
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn search_navigation_wraps_between_matching_pages() {
        let matches = [
            SearchPageMatch {
                page: 2,
                occurrences: 1,
                context: String::new(),
            },
            SearchPageMatch {
                page: 5,
                occurrences: 2,
                context: String::new(),
            },
            SearchPageMatch {
                page: 9,
                occurrences: 1,
                context: String::new(),
            },
        ];

        assert_eq!(search_target_page(&matches, 2, true), Some(5));
        assert_eq!(search_target_page(&matches, 9, true), Some(2));
        assert_eq!(search_target_page(&matches, 5, false), Some(2));
        assert_eq!(search_target_page(&matches, 2, false), Some(9));
    }

    #[test]
    fn search_status_reports_progress_and_results() {
        let mut search = SearchState {
            query: "needle".into(),
            request_id: 1,
            matches: Vec::new(),
            total_occurrences: 0,
            scanned: 8,
            total_pages: 20,
            searching: true,
        };
        assert_eq!(
            search.status_label(0).as_deref(),
            Some("  search 8/20  /needle")
        );

        search.searching = false;
        search.matches = vec![SearchPageMatch {
            page: 4,
            occurrences: 3,
            context: String::new(),
        }];
        search.total_occurrences = 3;
        assert_eq!(search.highlight_request_id(4), 1);
        assert_eq!(search.highlight_request_id(5), 0);
        assert_eq!(
            search.status_label(4).as_deref(),
            Some("  search 1/1 · 3 hits  /needle")
        );
    }

    #[test]
    fn search_picker_groups_contextual_results_by_section_and_page() {
        let search = SearchState {
            query: "representation".into(),
            request_id: 1,
            matches: vec![
                SearchPageMatch {
                    page: 0,
                    occurrences: 2,
                    context: "…visual representation learning from video…".into(),
                },
                SearchPageMatch {
                    page: 2,
                    occurrences: 1,
                    context: "…a representation objective used in prior work…".into(),
                },
            ],
            total_occurrences: 3,
            scanned: 3,
            total_pages: 3,
            searching: false,
        };
        let outline = vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 0,
                depth: 0,
            },
            OutlineItem {
                title: "Methods".into(),
                page: 2,
                depth: 0,
            },
        ];
        let mut state = SearchPickerState::new(0);
        state.sync(0, &search.matches);
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Vertical);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_search_picker(
                    frame,
                    area,
                    &search,
                    &outline,
                    &state,
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw search picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Search representation"));
        assert!(rendered.contains("3 hits on 2 pages"));
        assert!(rendered.contains("Section  Introduction"));
        assert!(rendered.contains("Page 1 · 2 hits · current"));
        assert!(rendered.contains("visual representation learning"));
        assert!(rendered.contains("Section  Methods"));
        assert!(rendered.contains("Page 3 · 1 hit"));
    }

    #[test]
    fn status_text_flattens_errors_without_terminal_controls() {
        let columns = Cell::new(80);
        assert_eq!(
            format!(
                "{}",
                super::StatusText("reload failed: bad\r\nfile\t\u{1b}[2J", &columns)
            ),
            "reload failed: bad  file  [2J"
        );
    }

    #[test]
    fn status_text_shares_the_row_budget_across_fragments() {
        let columns = Cell::new(6);
        let text = format!(
            "{}{}{}{}",
            super::StatusText(1, &columns),
            super::StatusText("/1 ", &columns),
            super::StatusText("漢字", &columns),
            super::StatusText(" help", &columns),
        );
        assert_eq!(text, "1/1 漢");
        let columns = Cell::new(0);
        assert_eq!(
            format!("{}", super::StatusText("reload failed", &columns)),
            ""
        );
    }

    #[test]
    fn status_text_preserves_combining_and_emoji_graphemes() {
        let columns = Cell::new(4);
        assert_eq!(
            format!("{}", super::StatusText("e\u{301}👩‍🔬xy", &columns)),
            "e\u{301}👩‍🔬x"
        );
    }

    #[test]
    fn growing_viewport_clears_the_previous_status_row() {
        assert_eq!(stale_status_row(Some(20), 40), Some(20));
        assert_eq!(stale_status_row(Some(40), 20), None);
        assert_eq!(stale_status_row(Some(40), 40), None);
        assert_eq!(stale_status_row(None, 40), None);
    }

    #[test]
    fn file_changes_must_stabilize_before_reload() {
        let directory = tempfile::tempdir().unwrap();
        let pptx = directory.path().join("document.pptx");
        fs::write(&pptx, "initial deck").unwrap();
        let initial = FileFingerprint::read(&pptx).unwrap();
        fs::write(&pptx, "updated deck with different size").unwrap();
        let changed = FileFingerprint::read(&pptx).unwrap();
        assert_ne!(initial, changed);
        let started = Instant::now();
        let mut watcher = FileWatcher {
            accepted: initial,
            candidate: None,
            next_poll: started,
        };

        assert_eq!(watcher.observe(changed, started), None);
        assert_eq!(
            watcher.observe(changed, started + FILE_STABLE_FOR),
            Some(changed)
        );
        watcher.accept(changed);
        assert_eq!(watcher.observe(changed, started + FILE_STABLE_FOR), None);
    }

    fn outline_fixture() -> Vec<OutlineItem> {
        vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 0,
                depth: 0,
            },
            OutlineItem {
                title: "Background".into(),
                page: 4,
                depth: 1,
            },
            OutlineItem {
                title: "Results".into(),
                page: 9,
                depth: 0,
            },
        ]
    }

    #[test]
    fn clipboard_write_uses_base64_osc52_sequence() {
        let mut output = Vec::new();

        write_clipboard_osc52(&mut output, "hi").expect("clipboard write");

        assert_eq!(output, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn mouse_cell_intersection_selects_tiny_link_targets() {
        let target = LinkTarget::Internal {
            page: 7,
            top_ratio: Some(0.5),
            left_ratio: None,
        };
        let links = [PageLink {
            rect: PageLinkRect {
                left: 52,
                top: 22,
                right: 55,
                bottom: 25,
            },
            label: "[7]".into(),
            target: target.clone(),
        }];
        let placement = ImagePlacement {
            left: 10,
            columns: 20,
            rows: 10,
            crop: None,
            scroll_x: 0,
            scroll_y: 0,
            native_cell: None,
            offset_y: 0,
        };

        assert_eq!(
            link_at_cell(&links, placement, 200, 100, 1, 15, 3),
            Some(target)
        );
        assert_eq!(link_at_cell(&links, placement, 200, 100, 1, 14, 3), None);
        assert_eq!(link_at_cell(&links, placement, 200, 100, 1, 15, 0), None);
    }

    #[test]
    fn link_picker_supports_multi_digit_number_selection() {
        let mut input = String::new();

        assert_eq!(update_link_number_selection(&mut input, '1', 20), Some(0));
        assert_eq!(update_link_number_selection(&mut input, '0', 20), Some(9));
        assert_eq!(input, "10");
        assert_eq!(update_link_number_selection(&mut input, '9', 20), Some(8));
        assert_eq!(input, "9");
    }

    #[test]
    fn link_picker_supports_page_and_end_navigation_keys() {
        let control = KeyModifiers::CONTROL;
        assert_eq!(
            link_picker_navigation_index(3, 30, KeyEvent::new(KeyCode::Char('f'), control), 10,),
            Some(13)
        );
        assert_eq!(
            link_picker_navigation_index(13, 30, KeyEvent::new(KeyCode::Char('b'), control), 10,),
            Some(3)
        );
        assert_eq!(
            link_picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                10,
            ),
            Some(0)
        );
        assert_eq!(
            link_picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
                10,
            ),
            Some(29)
        );
        assert_eq!(
            link_picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                10,
            ),
            None
        );
    }

    #[test]
    fn split_link_picker_focuses_document_and_links_without_affecting_floating_layout() {
        let plain = KeyModifiers::NONE;
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Vertical,
                KeyEvent::new(KeyCode::Char('h'), plain),
            ),
            Some(LinkPickerFocus::Document)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Document,
                LinkPickerLayout::Horizontal,
                KeyEvent::new(KeyCode::Char('l'), plain),
            ),
            Some(LinkPickerFocus::Links)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Vertical,
                KeyEvent::new(KeyCode::Tab, plain),
            ),
            Some(LinkPickerFocus::Document)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Floating,
                KeyEvent::new(KeyCode::Char('h'), plain),
            ),
            None
        );
    }

    #[test]
    fn link_picker_filter_matches_labels_pages_and_urls() {
        let links = vec![
            DocumentLink {
                source_page: 1,
                source_top_ratio: 0.1,
                ordinal: 0,
                label: "(Grill et al., 2020)".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 10,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "project page".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Uri("https://example.invalid/paper".into()),
            },
        ];
        assert_eq!(filter_document_links(&links, "grill"), vec![0]);
        assert_eq!(filter_document_links(&links, "source page 3"), vec![1]);
        assert_eq!(filter_document_links(&links, "example paper"), vec![1]);
        assert!(filter_document_links(&links, "missing").is_empty());
    }

    #[test]
    fn link_picker_pointer_rows_resolve_only_link_entries() {
        let links = vec![
            DocumentLink {
                source_page: 1,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "first".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 4,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.4,
                ordinal: 0,
                label: "second".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 5,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
        ];
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let list = link_picker_list_area(area, geometry);
        let state = LinkPickerState::new(0);
        let document = LinkPickerDocument::new(&links, &[]);

        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y),
            None
        );
        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y + 1),
            Some(0)
        );
        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y + 3),
            Some(1)
        );
    }

    #[test]
    fn persistent_link_picker_tracks_the_current_page_in_the_document_index() {
        let links = vec![
            DocumentLink {
                source_page: 0,
                source_top_ratio: 0.1,
                ordinal: 0,
                label: "first".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 4,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 4,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "current".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 6,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 6,
                source_top_ratio: 0.3,
                ordinal: 0,
                label: "later".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
        ];
        let mut state = LinkPickerState::new(4);

        state.sync(4, &links, false);
        assert_eq!(state.selected, 1);

        state.sync(5, &links, false);
        assert_eq!(state.selected, 2);
        assert_eq!(state.selection_key, Some((6, 0)));
    }

    #[test]
    fn link_picker_shows_link_text_and_destinations() {
        let links = vec![
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "[12]".into(),
                source_context: Some(
                    "Prior work identifies the same limitation [12] in dense retrieval. Additional synthetic context continues onto a second line for validation."
                        .into(),
                ),
                reference_context: Some("[12] Example et al. A synthetic reference title.".into()),
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 3,
                source_top_ratio: 0.3,
                ordinal: 0,
                label: "project page".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Uri("https://example.invalid/paper".into()),
            },
        ];
        let outline = vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 2,
                depth: 0,
            },
            OutlineItem {
                title: "Project links".into(),
                page: 3,
                depth: 0,
            },
        ];
        let mut state = LinkPickerState::new(2);
        state.select(1, &links);
        state.number_input = "2".into();
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw link picker");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(40, 15)].bg, ratatui::style::Color::Reset);
        assert_eq!(
            buffer[(pane.x + pane.width / 2, pane.y + pane.height / 2)].bg,
            picker_color(crate::theme::TOKYO_NIGHT_MOON.bg_dark)
        );
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Links 2"));
        assert!(rendered.contains("auto/vertical"));
        assert!(rendered.contains("Section  Introduction"));
        assert!(rendered.contains("Section  Project links"));
        assert!(rendered.contains("Page 3 · current"));
        assert!(rendered.contains("citation [12]"));
        assert!(!rendered.contains("Slide 8"));
        assert!(rendered.contains("project page"));
        assert!(!rendered.contains("Selected"));
        assert!(rendered.contains("Page 4 → https://example.invalid/paper"));
        assert!(!rendered.contains("copy URL"));
        assert!(!rendered.contains("2/2"));

        state.focus = LinkPickerFocus::Document;
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw document-focused split");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Slide focused  · l: links"));

        state.focus = LinkPickerFocus::Links;

        state.select(0, &links);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw citation context");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Context  Prior work identifies"));
        assert!(rendered.contains("second line for validation."));
        assert!(rendered.contains("Reference  [12] Example et al."));

        state.filter = "project".into();
        state.filtering = true;
        state.select(1, &links);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw filtered links");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Links 1/2"));
        assert!(rendered.contains("/ project"));
        assert!(!rendered.contains("citation [12]"));
        assert!(!rendered.contains("1/1"));

        state.filter.clear();
        state.filtering = false;
        state.select(0, &links);

        let compact_area = Rect::new(0, 0, 80, 24);
        let compact_geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Horizontal);
        let (_, compact_pane) = link_picker_panes(compact_area, compact_geometry);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    compact_area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    compact_geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw compact horizontal citation context");
        let buffer = terminal.backend().buffer();
        let rendered: String = (compact_pane.y..compact_pane.y + compact_pane.height)
            .flat_map(|y| {
                (compact_pane.x..compact_pane.x + compact_pane.width)
                    .map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Context  Prior work identifies"));
        assert!(rendered.contains("Reference  [12] Example et al."));
    }

    #[test]
    fn persistent_link_picker_can_stay_open_on_a_page_without_links() {
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let state = LinkPickerState::new(4);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&[], &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 28,
                        total_pages: 28,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw empty link picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Links 0"));
        assert!(rendered.contains("No annotated links in this document"));
    }

    #[test]
    fn floating_link_picker_is_centered_opaque_and_bordered() {
        let links = vec![DocumentLink {
            source_page: 0,
            source_top_ratio: 0.1,
            ordinal: 0,
            label: "project page".into(),
            source_context: None,
            reference_context: None,
            target: LinkTarget::Uri("https://example.invalid/paper".into()),
        }];
        let state = LinkPickerState::new(0);
        let area = Rect::new(0, 0, 100, 40);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Floating);
        let (_, popup) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 1,
                        total_pages: 1,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw floating link picker");
        let buffer = terminal.backend().buffer();

        assert_eq!(popup, Rect::new(12, 5, 75, 30));
        assert_eq!(buffer[(0, 0)].bg, ratatui::style::Color::Reset);
        assert_eq!(buffer[(popup.x, popup.y)].symbol(), "┌");
        assert_eq!(
            buffer[(popup.x + popup.width / 2, popup.y + popup.height / 2)].bg,
            picker_color(crate::theme::TOKYO_NIGHT_MOON.bg_dark)
        );
        let rendered: String = (popup.y..popup.y + popup.height)
            .flat_map(|y| {
                (popup.x..popup.x + popup.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("j/k C-b/f g/G"));
        assert!(rendered.contains("esc"));
    }

    #[test]
    fn split_link_sidebar_has_no_navigation_footer() {
        let links = vec![DocumentLink {
            source_page: 0,
            source_top_ratio: 0.1,
            ordinal: 0,
            label: "project page".into(),
            source_context: None,
            reference_context: None,
            target: LinkTarget::Uri("https://example.invalid/paper".into()),
        }];
        let state = LinkPickerState::new(0);
        let area = Rect::new(0, 0, 80, 24);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 1,
                        total_pages: 1,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw compact link sidebar");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(!rendered.contains("j/k"));
        assert!(!rendered.contains("↵"));
        assert!(!rendered.contains("s/a"));
        assert!(!rendered.contains("esc"));
        assert!(!rendered.contains("1/1"));
    }

    #[test]
    fn link_picker_reserves_room_for_details_when_space_allows() {
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            20
        );
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 20, 8),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            6
        );
    }

    #[test]
    fn link_picker_normalizes_split_numeric_citations() {
        assert_eq!(link_picker_label("[23]"), "citation [23]");
        assert_eq!(link_picker_label("8,"), "citation [8]");
        assert_eq!(link_picker_label("34]"), "citation [34]");
        assert_eq!(link_picker_label("project page"), "project page");
    }

    #[test]
    fn link_picker_cycles_explicit_layouts_from_the_resolved_layout() {
        let wide = Rect::new(0, 0, 120, 30);
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Auto),
            LinkPickerLayout::Horizontal
        );
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Horizontal),
            LinkPickerLayout::Floating
        );
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Floating),
            LinkPickerLayout::Vertical
        );
    }

    #[test]
    fn outline_start_index_selects_nearest_preceding_entry() {
        let items = outline_fixture();

        assert_eq!(outline_start_index(&items, 0), 0);
        assert_eq!(outline_start_index(&items, 6), 1);
        assert_eq!(outline_start_index(&items, 20), 2);
    }

    #[test]
    fn filter_outline_matches_titles_and_passes_all_when_empty() {
        let items = outline_fixture();

        assert_eq!(filter_outline(&items, ""), vec![0, 1, 2]);
        assert_eq!(filter_outline(&items, "result"), vec![2]);
        assert!(filter_outline(&items, "zzz").is_empty());
    }

    #[test]
    fn picker_plain_arrow_keys_change_selection() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pptx"), b"synthetic").expect("first deck");
        let mut browser = BrowserState::new(directory.path().to_path_buf());

        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 0);
    }

    #[test]
    fn picker_supports_page_and_end_navigation_keys() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for index in 0..8 {
            fs::write(directory.path().join(format!("{index}.pptx")), b"synthetic").expect("deck");
        }
        let mut browser = BrowserState::new(directory.path().to_path_buf());

        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            3,
            false,
        ));
        assert_eq!(browser.selected, 3);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            3,
            false,
        ));
        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
            3,
            false,
        ));
        assert_eq!(browser.selected, browser.filtered_indices.len() - 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            3,
            false,
        ));
        assert_eq!(browser.selected, 0);

        browser.filter = "one".into();
        browser.rebuild_filter();
        assert!(!apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            3,
            true,
        ));
    }

    #[test]
    fn picker_escape_clears_the_filter_before_closing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pptx"), b"synthetic").expect("deck");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "one".into();
        browser.rebuild_filter();

        assert!(clear_picker_filter(&mut browser));
        assert!(browser.filter.is_empty());
        assert!(!clear_picker_filter(&mut browser));
    }

    #[test]
    fn picker_uses_mdr_three_quarter_layout() {
        let area = Rect::new(0, 0, 100, 40);

        assert_eq!(picker_rect(area), Rect::new(12, 5, 75, 30));
    }

    #[test]
    fn picker_preserves_all_four_border_corners_with_long_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let nested = directory.path().join("a".repeat(100));
        fs::create_dir(&nested).expect("nested directory");
        let browser = BrowserState::new(nested);
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_picker(frame, &browser, false, crate::theme::TOKYO_NIGHT_MOON))
            .expect("draw picker");
        let buffer = terminal.backend().buffer();
        let right = popup.x + popup.width - 1;
        let bottom = popup.y + popup.height - 1;

        assert_eq!(buffer[(popup.x, popup.y)].symbol(), "┌");
        assert_eq!(buffer[(right, popup.y)].symbol(), "┐");
        assert_eq!(buffer[(popup.x, bottom)].symbol(), "└");
        assert_eq!(buffer[(right, bottom)].symbol(), "┘");
    }

    #[test]
    fn picker_uses_layered_theme_and_selection_marker() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pptx"), b"synthetic").expect("deck");
        let browser = BrowserState::new(directory.path().to_path_buf());
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let theme = crate::theme::TOKYO_NIGHT_MOON;
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_picker(frame, &browser, false, theme))
            .expect("draw picker");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].bg, picker_color(theme.bg_dark1));
        assert_eq!(buffer[(popup.x, popup.y)].fg, picker_color(theme.blue7));
        assert_eq!(
            buffer[(popup.x + 1, popup.y + 1)].bg,
            picker_color(theme.bg_dark)
        );
        assert_eq!(buffer[(popup.x + 1, popup.y + 2)].symbol(), "▌");
        assert_eq!(
            buffer[(popup.x + 1, popup.y + 2)].bg,
            picker_color(theme.bg_highlight)
        );
    }

    #[test]
    fn theme_picker_previews_the_selected_palette() {
        let mut alternate = crate::theme::TOKYO_NIGHT_MOON;
        alternate.bg_dark1 = crossterm::style::Color::Rgb {
            r: 0x10,
            g: 0x20,
            b: 0x30,
        };
        let themes = vec![
            (
                "tokyo-night-moon".to_string(),
                crate::theme::TOKYO_NIGHT_MOON,
            ),
            ("synthetic-theme".to_string(), alternate),
        ];
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_theme_picker(frame, &themes, &[0, 1], 1, "", false))
            .expect("draw theme picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (popup.y..popup.y + popup.height)
            .flat_map(|y| {
                (popup.x..popup.x + popup.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert_eq!(buffer[(0, 0)].bg, picker_color(alternate.bg_dark1));
        assert!(rendered.contains("tokyo-night-moon"));
        assert!(rendered.contains("synthetic-theme"));
        assert!(rendered.contains("Themes"));
        assert_eq!(filter_theme_indices(&themes, "synthetic"), vec![1]);
    }

    #[test]
    fn stepped_zoom_snaps_and_clamps_within_range() {
        assert_eq!(stepped_zoom(ZOOM_DEFAULT, true), ZOOM_DEFAULT + ZOOM_STEP);
        assert_eq!(stepped_zoom(ZOOM_DEFAULT, false), ZOOM_MIN);
        assert_eq!(stepped_zoom(ZOOM_MIN, false), ZOOM_MIN);
        assert_eq!(stepped_zoom(ZOOM_MAX, true), ZOOM_MAX);
        assert_eq!(stepped_zoom(ZOOM_MAX - ZOOM_STEP, true), ZOOM_MAX);
    }

    fn zoom_size(old: u16, new: u16) -> impl Fn(u32, u32, u32) -> Option<(u32, u32, Option<u32>)> {
        move |_, width, height| {
            Some((
                super::scale_zoom(width, old, new),
                super::scale_zoom(height, old, new),
                None,
            ))
        }
    }

    #[test]
    fn zoom_keeps_viewport_center_when_page_first_fits_then_overflows() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                zoom_size(100, 300),
                false,
                |_| Some((60, 40, None))
            ),
            Ok((0, 40, 20))
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (30, 50), zoom_size(100, 150), false, |_| {
                Some((200, 160, None))
            }),
            Ok((0, 70, 95))
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (70, 95), zoom_size(150, 100), false, |_| {
                Some((300, 240, None))
            }),
            Ok((0, 30, 50))
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (200, 160),
                zoom_size(150, 100),
                false,
                |_| Some((300, 240, None))
            ),
            Ok((0, 100, 80))
        );
    }

    #[test]
    fn zoom_uses_actual_terminal_cell_placement_when_page_first_fits() {
        let viewport = Viewport {
            columns: 80,
            rows: 29,
            pixel_width: 960,
            pixel_height: 580,
            top: 0,
            status_row: 29,
        };
        // 870px occupies 73 columns, offset by three 12px cells: its midpoint
        // is at x=471, nine pixels left of the viewport center.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                zoom_size(100, 125),
                false,
                |_| Some((870, 580, None))
            ),
            Ok((0, 75, 73))
        );
    }

    #[test]
    fn zoom_keeps_center_on_next_page_and_can_reveal_previous_page() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(viewport, 0, 2, (0, 90), zoom_size(100, 200), true, |page| {
                Some(if page == 0 {
                    (200, 100, None)
                } else {
                    (100, 120, None)
                })
            }),
            Ok((1, 50, 0))
        );
        assert_eq!(
            centered_scaled_view(viewport, 1, 2, (50, 0), zoom_size(200, 100), true, |page| {
                Some(if page == 0 {
                    (400, 200, None)
                } else {
                    (200, 240, None)
                })
            }),
            Ok((0, 0, 90))
        );
    }

    #[test]
    fn zoom_anchors_the_narrow_center_page_at_its_actual_crop() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        // Page one begins within the viewport and permits shared scroll_x=100,
        // but the centered page zero is only 120px wide and crops at x=20.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (100, 0),
                zoom_size(100, 200),
                true,
                |page| {
                    Some(if page == 0 {
                        (120, 60, None)
                    } else {
                        (300, 180, None)
                    })
                }
            ),
            Ok((0, 90, 40)),
        );
    }

    #[test]
    fn zoom_identifies_uncached_predecessor_and_keeps_short_page_center() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(viewport, 1, 2, (0, 0), zoom_size(200, 100), true, |page| {
                (page == 1).then_some((200, 160, None))
            }),
            Err(0)
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (0, 0), zoom_size(100, 125), true, |_| Some(
                (100, 70, None)
            )),
            Ok((0, 13, 8))
        );
    }

    #[test]
    fn content_zoom_keeps_the_center_across_differently_indented_pages() {
        let viewport = Viewport {
            columns: 8,
            rows: 10,
            pixel_width: 80,
            pixel_height: 200,
            top: 0,
            status_row: 10,
        };
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (480, 650),
                |page, _, _| Some((800, 800, Some(if page == 0 { 610 } else { 110 }))),
                true,
                |page| Some((640, 640, Some(if page == 0 { 480 } else { 80 }))),
            ),
            Ok((1, 110, 13))
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (110, 13),
                |page, _, _| Some((640, 640, Some(if page == 0 { 480 } else { 80 }))),
                true,
                |page| Some((800, 800, Some(if page == 0 { 610 } else { 110 }))),
            ),
            Ok((0, 480, 650))
        );
    }

    #[test]
    fn content_zoom_preserves_pan_with_a_narrow_preceding_page() {
        let viewport = Viewport {
            columns: 8,
            rows: 10,
            pixel_width: 80,
            pixel_height: 200,
            top: 0,
            status_row: 10,
        };
        let position = centered_scaled_view(
            viewport,
            1,
            2,
            (320, 0),
            |page, _, _| {
                Some(if page == 0 {
                    (80, 80, Some(0))
                } else {
                    (640, 640, Some(280))
                })
            },
            true,
            |page| {
                Some(if page == 0 {
                    (100, 100, Some(0))
                } else {
                    (800, 800, Some(360))
                })
            },
        )
        .unwrap();
        assert_eq!(position, (0, -32, 80));
        let (mut app, _, _file) = continuous_app();
        app.tab_mut().fit = crate::pptx::FitMode::Content;
        app.tab_mut().center_content = false;
        let revision = app.tab().cache.values().next().unwrap().revision;
        for (page, size, bounds) in [
            (0, 80, [0.0, 0.0, 1.0, 1.0]),
            (1, 640, [0.4, 0.4, 0.6, 0.6]),
        ] {
            let key = app.page_key(page, viewport);
            let mut frame = continuous_frame(key, revision, false);
            let data = std::sync::Arc::get_mut(&mut frame).unwrap();
            data.width = size;
            data.height = size;
            data.content_bounds = bounds;
            app.tab_mut().cache.insert(key, frame);
        }
        app.tab_mut().page = position.0;
        app.tab_mut().scroll_x = position.1;
        app.tab_mut().scroll_y = position.2;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, -32);
        assert_eq!(app.visible_pages[0].frame.key.page, 1);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 248);
        app.pending_vertical_scroll = 20;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!((app.tab().page, app.tab().scroll_x), (1, 248));
        app.pending_vertical_scroll = -20;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!((app.tab().page, app.tab().scroll_x), (0, -32));
    }

    #[test]
    fn content_fit_centers_asymmetric_margins_only_once() {
        let (mut app, viewport, _file) = continuous_app();
        app.tab_mut().fit = crate::pptx::FitMode::Content;
        let key = app.render_key(viewport);
        let mut frame = continuous_frame(
            key,
            app.tab().cache.values().next().unwrap().revision,
            false,
        );
        let data = std::sync::Arc::get_mut(&mut frame).unwrap();
        data.width = 400;
        data.height = 800;
        data.content_bounds = [0.6, 0.5, 0.9, 0.75];
        app.tab_mut().center_content = true;
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (260, 400));
        app.tab_mut().scroll_x = 200;
        app.tab_mut().scroll_y = 350;
        app.reset_render_state();
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (200, 350));
    }

    #[test]
    fn content_scrolling_keeps_differently_indented_pages_visible() {
        let (mut app, viewport, _file) = continuous_app();
        app.tab_mut().fit = crate::pptx::FitMode::Content;
        app.tab_mut().center_content = false;
        let revision = app.tab().cache.values().next().unwrap().revision;
        for (page, bounds) in [(0, [0.7, 0.4, 0.9, 0.6]), (1, [0.1, 0.4, 0.3, 0.6])] {
            let key = app.page_key(page, viewport);
            let mut frame = continuous_frame(key, revision, false);
            let data = std::sync::Arc::get_mut(&mut frame).unwrap();
            data.width = 400;
            data.height = 400;
            data.content_bounds = bounds;
            app.tab_mut().cache.insert(key, frame);
        }
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.tab_mut().scroll_x = super::content_scroll(&primary, viewport).0 + 10;
        app.tab_mut().scroll_y = 350;
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.visible_pages.len(), 2);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 290);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 50);
        assert_eq!(app.tab().scroll_x, 290);

        app.pending_vertical_scroll = 70;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (1, 50, 10)
        );
        app.pending_vertical_scroll = -70;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (0, 290, 350)
        );
    }

    #[test]
    fn fit_cycle_preserves_zoomed_portrait_and_landscape_centers() {
        use crate::pptx::FitMode;
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        for (size, scroll, fit, expected) in [
            ((80, 160), (0, 40), FitMode::Width, (0, 50, 160)),
            ((200, 400), (50, 160), FitMode::Height, (0, 0, 40)),
            ((80, 160), (0, 40), FitMode::Page, (0, 0, 40)),
            ((200, 80), (50, 0), FitMode::Width, (0, 50, 0)),
            ((200, 80), (50, 0), FitMode::Height, (0, 150, 40)),
            ((400, 160), (150, 40), FitMode::Page, (0, 50, 0)),
        ] {
            assert_eq!(
                centered_scaled_view(
                    viewport,
                    0,
                    1,
                    scroll,
                    |_, width, height| {
                        let (width, height) = super::fitted_page_size(
                            viewport,
                            width as f32,
                            height as f32,
                            [0.0, 0.0, 1.0, 1.0],
                            fit,
                            200,
                        );
                        Some((width, height, None))
                    },
                    false,
                    |_| Some((size.0, size.1, None)),
                ),
                Ok(expected),
                "{fit:?}, {size:?}"
            );
        }
    }

    #[test]
    fn fit_cycle_keeps_center_on_next_page_with_different_aspect_ratio() {
        use crate::pptx::FitMode;
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        // The center is 25px down page one, not on the first visible landscape page.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (0, 35),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Width,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| Some(if page == 0 {
                    (100, 40, None)
                } else {
                    (40, 80, None)
                }),
            ),
            Ok((1, 0, 23))
        );
        // Shrinking that portrait page must fetch its predecessor before moving the view.
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (0, 23),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Height,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| (page == 1).then_some((100, 200, None)),
            ),
            Err(0)
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (0, 23),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Height,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| Some(if page == 0 {
                    (100, 40, None)
                } else {
                    (100, 200, None)
                }),
            ),
            Ok((0, 0, 75))
        );
    }

    #[test]
    fn fit_cycle_preserves_center_when_old_raster_rounds_the_aspect_ratio() {
        let viewport = Viewport {
            columns: 100,
            rows: 40,
            pixel_width: 1000,
            pixel_height: 800,
            top: 0,
            status_row: 40,
        };
        // Fit-page rounds a 210x10000pt page to 17x800px. Recovering its
        // aspect ratio from those pixels would shift the center by 280px.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                |_, _, _| Some(super::fitted_page_size(
                    viewport,
                    210.0,
                    10000.0,
                    [0.0, 0.0, 1.0, 1.0],
                    crate::pptx::FitMode::Width,
                    100,
                ))
                .map(|(width, height)| (width, height, None)),
                false,
                |_| Some((17, 800, None)),
            ),
            Ok((0, 0, 23410))
        );
    }

    #[test]
    fn picker_disambiguates_recent_files_with_long_parent_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let current = directory.path().join("working");
        fs::create_dir(&current).unwrap();
        let mut recents = Vec::new();
        for name in ["alpha", "beta"] {
            let parent = directory.path().join("long-prefix-".repeat(8)).join(name);
            fs::create_dir_all(&parent).unwrap();
            let file = parent.join("same.pptx");
            fs::write(&file, b"synthetic").unwrap();
            recents.push(file);
        }
        let mut browser = BrowserState::new(current);
        browser.set_recents(recents);
        let area = Rect::new(0, 0, 80, 30);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        for filter in ["", "same"] {
            browser.filter = filter.into();
            browser.rebuild_filter();
            terminal
                .draw(|frame| {
                    draw_picker(
                        frame,
                        &browser,
                        !filter.is_empty(),
                        crate::theme::TOKYO_NIGHT_MOON,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..area.height)
                .map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            for parent in ["alpha", "beta"] {
                assert!(
                    rows.iter()
                        .any(|row| row.contains("same.pptx") && row.contains(parent)),
                    "identical filenames must retain their distinct parent: {rows:?}"
                );
            }
        }
    }

    #[test]
    fn closing_picker_clears_its_terminal_buffer() {
        let mut output = Vec::new();

        clear_picker(&mut output, crate::theme::TOKYO_NIGHT_MOON).expect("clear picker");

        assert!(output.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(output.windows(6).any(|window| window == b"\x1b[?25l"));
    }

    #[test]
    fn image_canvas_uses_terminal_background_without_a_scaled_graphics_layer() {
        let mut output = Vec::new();
        let viewport = Viewport {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
            top: 0,
            status_row: 24,
        };

        clear_image_canvas(&mut output, viewport).expect("prepare canvas");

        assert!(!output.windows(3).any(|window| window == b"\x1b_"));
        assert!(output.windows(4).any(|window| window == b"\x1b[0m"));
        assert_eq!(
            output
                .windows(4)
                .filter(|window| *window == b"\x1b[2K")
                .count(),
            usize::from(viewport.rows)
        );
    }

    #[test]
    fn link_picker_uses_grimoire_auto_split() {
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 50, 30), Rect::new(50, 1, 50, 30))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 80, 50),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 80, 25), Rect::new(0, 26, 80, 25))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(30, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 70, 30), Rect::new(70, 1, 30, 30))
        );
    }

    #[test]
    fn link_picker_supports_forced_and_floating_layouts() {
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 80, 50),
                LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            ),
            (Rect::new(0, 1, 40, 50), Rect::new(40, 1, 40, 50))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Horizontal),
            ),
            (Rect::new(0, 1, 100, 15), Rect::new(0, 16, 100, 15))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Floating),
            ),
            (Rect::new(0, 1, 100, 30), Rect::new(12, 5, 75, 22))
        );
    }

    #[test]
    fn link_picker_repositions_the_retained_page_without_retransmitting_it() {
        let mut output = Vec::new();
        let area = Rect::new(0, 0, 100, 30);
        let theme = crate::theme::TOKYO_NIGHT_MOON;
        let image = LinkPickerImage {
            image_id: 12,
            source_width: 600,
            source_height: 800,
            crop: None,
            cell_width: 10,
            cell_height: 20,
            original: PositionedImage {
                left: 20,
                top: 0,
                placement: Placement {
                    image_id: 12,
                    columns: 60,
                    rows: 30,
                    offset_y: 0,
                    z_index: super::PAGE_IMAGE_Z_INDEX,
                    crop: None,
                },
            },
        };

        show_link_picker_split(
            &mut output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            theme,
        )
        .expect("show link picker split");
        restore_link_picker_split(
            &mut output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            theme,
        )
        .expect("restore link picker split");

        let output = String::from_utf8(output).expect("terminal output");
        assert!(output.contains("a=p,i=12,p=1,c=45,r=30"));
        assert!(output.contains("a=p,i=12,p=1,c=60,r=30"));
        assert_eq!(output.matches("a=p,i=12").count(), 2);
        assert!(!output.contains("a=T"));
        assert!(!output.contains("\x1b[2J"));

        let mut floating_output = Vec::new();
        show_link_picker_split(
            &mut floating_output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Floating),
            theme,
        )
        .expect("show floating link picker");
        let floating_output = String::from_utf8(floating_output).expect("terminal output");
        assert!(!floating_output.contains("a=p"));
        assert!(!floating_output.contains("a=T"));
    }

    #[test]
    fn modal_worker_messages_update_state_and_defer_clipboard_output() {
        let (mut app, viewport, _file) = continuous_app();
        let target_key = app.page_key(1, viewport);
        assert!(app.pending.contains(&target_key));
        assert!(!app.tab().cache.contains_key(&target_key));

        let frame = (*continuous_frame(target_key, app.tab().revision, false)).clone();
        app.handle_modal_worker_message(crate::pptx::WorkerMessage::Frame(frame))
            .expect("frame message should succeed");
        assert!(!app.pending.contains(&target_key));
        assert!(app.tab().cache.contains_key(&target_key));

        let document_id = app.tab().document_id;
        app.tab_mut().search = SearchState {
            query: "needle".to_string(),
            request_id: 7,
            matches: Vec::new(),
            total_occurrences: 0,
            scanned: 0,
            total_pages: 2,
            searching: true,
        };
        app.handle_modal_worker_message(crate::pptx::WorkerMessage::SearchProgress {
            document_id,
            request_id: 7,
            scanned: 1,
            total: 2,
            matches: vec![SearchPageMatch {
                page: 1,
                occurrences: 2,
                context: "needle".to_string(),
            }],
            total_occurrences: 2,
        })
        .expect("search progress should succeed");
        assert_eq!(app.tab().search.scanned, 1);
        assert!(app.tab().search.searching);

        app.handle_modal_worker_message(crate::pptx::WorkerMessage::SearchResults {
            document_id,
            request_id: 7,
            matches: vec![SearchPageMatch {
                page: 1,
                occurrences: 3,
                context: "needle".to_string(),
            }],
            total_occurrences: 3,
        })
        .expect("search results should succeed");
        assert!(!app.tab().search.searching);
        assert_eq!(app.tab().search.total_occurrences, 3);
        assert_eq!(app.tab().page, 1);

        app.tab_mut().link_index = super::LinkIndexState {
            request_id: 9,
            links: Vec::new(),
            scanned: 0,
            total_pages: 2,
            indexing: true,
        };
        app.handle_modal_worker_message(crate::pptx::WorkerMessage::LinkIndexProgress {
            document_id,
            request_id: 9,
            links: vec![DocumentLink {
                source_page: 0,
                ordinal: 0,
                source_top_ratio: 0.1,
                label: "Section".to_string(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Uri("https://example.com".to_string()),
            }],
            scanned: 2,
            total: 2,
            complete: true,
        })
        .expect("link index progress should succeed");
        assert!(!app.tab().link_index.indexing);
        assert_eq!(app.tab().link_index.links.len(), 1);

        app.handle_modal_worker_message(crate::pptx::WorkerMessage::Text {
            document_id,
            page: 0,
            content: "copied slide text".to_string(),
        })
        .expect("text message should succeed");
        assert_eq!(app.pending_clipboard.as_deref(), Some("copied slide text"));

        let mut output = Vec::new();
        app.flush_pending_clipboard(&mut output)
            .expect("flushing pending clipboard should succeed");
        assert!(app.pending_clipboard.is_none());
        let output = String::from_utf8(output).expect("utf8 output");
        assert!(output.contains("\x1b]52;c;"));

        assert!(
            app.handle_modal_worker_message(crate::pptx::WorkerMessage::Error("boom".to_string()))
                .is_err()
        );
    }
}
