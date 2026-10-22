use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, unbounded};
use deckcraft_fonts::FontDb;
use deckcraft_model::{
    PhType, Presentation, Rect as ModelRect, Shape, ShapeKind, resolve,
    text::{Action, Hyperlink},
};
use deckcraft_render::{Placed, RenderOpts, place_slide, render_slide};
use kurbo::{Point as KurboPoint, Rect as KurboRect};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::kitty;

pub type DocumentId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentRevision {
    len: u64,
    modified_ns: Option<u128>,
    inode: Option<(u64, u64)>,
}

impl DocumentRevision {
    pub fn read(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            Some((metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let inode = None;
        Ok(Self {
            len: metadata.len(),
            modified_ns,
            inode,
        })
    }

    pub fn check(&self, path: &Path) -> io::Result<()> {
        if Self::read(path)? == *self {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "presentation changed on disk",
            ))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub page: u32,
    pub depth: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPageMatch {
    pub page: u32,
    pub occurrences: u32,
    pub context: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FitMode {
    #[default]
    Page,
    Width,
    Height,
    Content,
}

impl FitMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Page => Self::Width,
            Self::Width => Self::Height,
            Self::Height => Self::Content,
            Self::Content => Self::Page,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Page => "fit-page",
            Self::Width => "fit-width",
            Self::Height => "fit-height",
            Self::Content => "fit-content",
        }
    }

    pub(crate) fn raster_size(
        self,
        width: f32,
        height: f32,
        content: [f32; 4],
        target_width: u32,
        target_height: u32,
    ) -> (u32, u32) {
        let mut scale = match self {
            Self::Page | Self::Width => target_width as f32 / width,
            Self::Height => target_height as f32 / height,
            Self::Content => {
                // Reserve a little breathing room without changing page geometry.
                const INNER_FRACTION: f32 = 0.96;
                (target_width as f32 * INNER_FRACTION / (width * (content[2] - content[0]))).min(
                    target_height as f32 * INNER_FRACTION / (height * (content[3] - content[1])),
                )
            }
        };
        if self == Self::Page && height * scale > target_height as f32 {
            scale = target_height as f32 / height;
        }
        if self == Self::Content {
            // Content can be a single tiny mark. Bound the full-page bitmap,
            // not just the fitted object, before converting dimensions to i32.
            const MAX_PIXELS: f32 = 16_777_216.0;
            const MAX_DIMENSION: f32 = 32_768.0;
            scale = scale
                .min((MAX_PIXELS / (width * height)).sqrt())
                .min(MAX_DIMENSION / width.max(height));
        }
        let pixels = |size: f32| {
            (if self == Self::Content {
                size.floor()
            } else {
                size.round()
            })
            .max(1.0) as u32
        };
        (pixels(width * scale), pixels(height * scale))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DarkModeStyle {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
}

impl DarkModeStyle {
    pub const fn new(background: [u8; 3], foreground: [u8; 3]) -> Self {
        Self {
            background,
            foreground,
        }
    }
}

impl Default for DarkModeStyle {
    fn default() -> Self {
        Self::new([0x15, 0x21, 0x2a], [0xff, 0xff, 0xff])
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderKey {
    pub document_id: DocumentId,
    pub page: u32,
    pub width: u16,
    pub height: u16,
    pub zoom: u16,
    pub fit: FitMode,
    pub invert: bool,
    pub dark_mode_style: DarkModeStyle,
    pub search_request_id: u64,
    pub search_highlight: [u8; 3],
    pub link_mode: bool,
    pub link_highlight: [u8; 3],
    pub selected_link_ordinal: Option<u32>,
}

#[derive(Debug)]
pub struct RenderRequest {
    pub key: RenderKey,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub key: RenderKey,
    pub revision: DocumentRevision,
    pub width: u32,
    pub height: u32,
    pub page_width_pt: f32,
    pub page_height_pt: f32,
    pub content_bounds: [f32; 4],
    pub compressed_rgba: Vec<u8>,
    pub render_elapsed: Duration,
    pub dark_mode_elapsed: Option<Duration>,
    pub highlight_elapsed: Option<Duration>,
    pub compression_elapsed: Duration,
    pub generation: u64,
    pub links: Vec<PageLink>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Internal {
        page: u32,
        top_ratio: Option<f32>,
        left_ratio: Option<f32>,
    },
    Uri(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageLinkRect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageLink {
    pub rect: PageLinkRect,
    pub label: String,
    pub target: LinkTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocumentLink {
    pub source_page: u32,
    pub source_top_ratio: f32,
    pub ordinal: u32,
    pub label: String,
    pub source_context: Option<String>,
    pub reference_context: Option<String>,
    pub target: LinkTarget,
}

#[derive(Debug)]
pub enum WorkerMessage {
    Ready {
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: DocumentRevision,
    },
    Opened {
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: DocumentRevision,
    },
    OpenError {
        document_id: DocumentId,
        error: String,
    },
    Text {
        document_id: DocumentId,
        page: u32,
        content: String,
    },
    SearchProgress {
        document_id: DocumentId,
        request_id: u64,
        scanned: u32,
        total: u32,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    },
    SearchResults {
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    },
    LinkIndexProgress {
        document_id: DocumentId,
        request_id: u64,
        links: Vec<DocumentLink>,
        scanned: u32,
        total: u32,
        complete: bool,
    },
    Frame(Frame),
    Error(String),
}

enum WorkerCommand {
    Render(RenderRequest),
    Prefetch(RenderRequest),
    Open {
        document_id: DocumentId,
        path: PathBuf,
    },
    Close {
        document_id: DocumentId,
    },
    ExtractText {
        document_id: DocumentId,
        page: u32,
    },
    Search {
        document_id: DocumentId,
        request_id: u64,
        query: String,
    },
    IndexLinks {
        document_id: DocumentId,
        request_id: u64,
    },
    Shutdown,
}

pub struct PptxWorker {
    commands: Sender<WorkerCommand>,
    messages: Receiver<WorkerMessage>,
    generation: Arc<AtomicU64>,
    link_index_epoch: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

/// Initializes the global `deckcraft-fonts` and `wordcraft-fonts` databases before any background
/// threads are spawned.
///
/// On macOS, `system_font_dirs()` includes
/// `$HOME/Library/Group Containers/UBF8T346G9.Office/FontCache/4/CloudFonts`. Reading inside
/// `~/Library/Group Containers` from a terminal triggers a macOS TCC prompt (`"kitty" would like
/// to access data from other apps`). To avoid touching Microsoft Office's group container while
/// still keeping user-installed fonts (`~/Library/Fonts`) and giving `officeterm` its own font
/// cache (`~/.cache/officeterm/fonts`), we point `HOME` at a synthetic `<cache>/officeterm/font-home`
/// directory while `FontDb::global()` records its search paths, then immediately restore `HOME`.
pub fn init_font_db() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        #[cfg(target_os = "macos")]
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && !home.as_os_str().is_empty()
        {
            let cache_base = std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| home.join(".cache"));
            if let Ok(font_home) = prepare_macos_font_home(&home, &cache_base) {
                struct HomeGuard(PathBuf);
                impl Drop for HomeGuard {
                    fn drop(&mut self) {
                        // SAFETY: `init_font_db` runs on the main thread during startup before
                        // any worker or file-browser threads are spawned.
                        unsafe {
                            std::env::set_var("HOME", &self.0);
                        }
                    }
                }
                let _restore = HomeGuard(home);
                // SAFETY: `init_font_db` runs on the main thread during startup before any
                // worker or file-browser threads are spawned.
                unsafe {
                    std::env::set_var("HOME", &font_home);
                }
                let _ = FontDb::global();
                let _ = wordcraft_fonts::FontDb::global();
                return;
            }
        }
        let _ = FontDb::global();
        let _ = wordcraft_fonts::FontDb::global();
    });
}

#[cfg(any(target_os = "macos", test))]
fn ensure_symlink(target: &Path, link: &Path) -> io::Result<()> {
    if let Ok(existing) = std::fs::read_link(link) {
        if existing == target {
            return Ok(());
        }
        let _ = std::fs::remove_file(link);
    } else if link.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(link).or_else(|_| std::fs::remove_dir_all(link));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
fn prepare_macos_font_home(home: &Path, cache_base: &Path) -> io::Result<PathBuf> {
    let app_cache = cache_base.join("officeterm");
    let font_cache = app_cache.join("fonts");
    std::fs::create_dir_all(&font_cache)?;

    let font_home = app_cache.join("font-home");
    let library_dir = font_home.join("Library");
    std::fs::create_dir_all(&library_dir)?;
    ensure_symlink(&home.join("Library/Fonts"), &library_dir.join("Fonts"))?;

    let office_cache_parent =
        library_dir.join("Group Containers/UBF8T346G9.Office/FontCache/4");
    std::fs::create_dir_all(&office_cache_parent)?;
    ensure_symlink(&font_cache, &office_cache_parent.join("CloudFonts"))?;

    Ok(font_home)
}

impl PptxWorker {
    pub fn spawn(
        path: PathBuf,
    ) -> Result<(Self, u32, Vec<OutlineItem>, DocumentRevision), String> {
        let (command_tx, command_rx) = unbounded();
        let (message_tx, message_rx) = unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        let link_index_epoch = Arc::new(AtomicU64::new(0));
        let worker_link_epoch = Arc::clone(&link_index_epoch);

        let handle = thread::spawn(move || {
            worker_loop(
                path,
                command_rx,
                message_tx,
                worker_generation,
                worker_link_epoch,
            );
        });

        let (pages, outline, revision) = match message_rx.recv() {
            Ok(WorkerMessage::Ready {
                pages,
                outline,
                revision,
            }) => (pages, outline, revision),
            Ok(WorkerMessage::Error(error)) => return Err(error),
            Ok(_) => return Err("unexpected presentation worker startup message".into()),
            Err(_) => return Err("presentation worker exited during startup".into()),
        };

        Ok((
            Self {
                commands: command_tx,
                messages: message_rx,
                generation,
                link_index_epoch,
                handle: Some(handle),
            },
            pages,
            outline,
            revision,
        ))
    }

    pub fn render(&self, request: RenderRequest) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::Render(request))
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn prefetch(&self, request: RenderRequest) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::Prefetch(request))
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn open(&self, document_id: DocumentId, path: PathBuf) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::Open { document_id, path })
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn close(&self, document_id: DocumentId) {
        let _ = self.commands.send(WorkerCommand::Close { document_id });
    }

    pub fn extract_text(&self, document_id: DocumentId, page: u32) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::ExtractText { document_id, page })
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn search(
        &self,
        document_id: DocumentId,
        request_id: u64,
        query: String,
    ) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::Search {
                document_id,
                request_id,
                query,
            })
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn cancel_search(&self, document_id: DocumentId, request_id: u64) {
        let _ = self.search(document_id, request_id, String::new());
    }

    pub fn index_links(&self, document_id: DocumentId, request_id: u64) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::IndexLinks {
                document_id,
                request_id,
            })
            .map_err(|_| "presentation worker stopped".into())
    }

    pub fn cancel_link_index(&self) {
        self.link_index_epoch.fetch_add(1, Ordering::Release);
    }

    pub fn begin_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    pub fn try_recv(&self) -> Option<WorkerMessage> {
        self.messages.try_recv().ok()
    }

    #[cfg(test)]
    pub(crate) fn disconnected() -> Self {
        let (commands, _) = unbounded();
        let (_, messages) = unbounded();
        Self {
            commands,
            messages,
            generation: Arc::new(AtomicU64::new(0)),
            link_index_epoch: Arc::new(AtomicU64::new(0)),
            handle: None,
        }
    }
}

impl Drop for PptxWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum DocumentBackend {
    Pptx(Box<Presentation>),
    Word(Box<crate::word::WordDocument>),
    Excel(Box<crate::excel::ExcelDocument>),
}

struct OpenPresentation {
    backend: DocumentBackend,
    revision: DocumentRevision,
    pages: u32,
    text_cache: Vec<Option<CachedSlideText>>,
    content_bounds_cache: HashMap<u32, [f32; 4]>,
    active_search: Option<ActiveSearch>,
}

#[derive(Clone)]
pub(crate) struct CachedSlideText {
    pub(crate) raw: String,
    pub(crate) normalized: String,
    pub(crate) source_index_by_byte: Vec<usize>,
    pub(crate) char_rects: Vec<Option<ModelRect>>,
}

pub(crate) fn build_cached_text(
    raw: String,
    char_rects: Vec<Option<ModelRect>>,
) -> CachedSlideText {
    let (normalized, source_index_by_byte) =
        normalize_search_characters(raw.chars().enumerate());
    CachedSlideText {
        raw,
        normalized,
        source_index_by_byte,
        char_rects,
    }
}

#[derive(Clone)]
struct ActiveSearch {
    request_id: u64,
    query: String,
}

#[derive(Clone, Copy)]
struct ActiveLinkIndex {
    document_id: DocumentId,
    request_id: u64,
    epoch: u64,
    next_page: u32,
    total_pages: u32,
}

struct PendingCommands {
    renders: VecDeque<RenderRequest>,
    prefetch: VecDeque<RenderRequest>,
    opens: VecDeque<(DocumentId, PathBuf)>,
    closes: VecDeque<DocumentId>,
    text_requests: VecDeque<(DocumentId, u32)>,
    searches: VecDeque<(DocumentId, u64, String)>,
    link_indices: VecDeque<(DocumentId, u64)>,
    shutdown: bool,
}

impl PendingCommands {
    fn new() -> Self {
        Self {
            renders: VecDeque::new(),
            prefetch: VecDeque::new(),
            opens: VecDeque::new(),
            closes: VecDeque::new(),
            text_requests: VecDeque::new(),
            searches: VecDeque::new(),
            link_indices: VecDeque::new(),
            shutdown: false,
        }
    }

    fn ingest(&mut self, command: WorkerCommand) {
        match command {
            WorkerCommand::Render(request) => {
                self.renders
                    .retain(|queued| queued.generation >= request.generation);
                self.prefetch.retain(|queued| {
                    queued.generation >= request.generation
                        && !(queued.key == request.key && queued.generation == request.generation)
                });
                if !self
                    .renders
                    .iter()
                    .any(|queued| queued.key == request.key && queued.generation == request.generation)
                {
                    self.renders.push_back(request);
                }
            }
            WorkerCommand::Prefetch(request) => {
                let already_queued = self
                    .renders
                    .iter()
                    .chain(self.prefetch.iter())
                    .any(|queued| queued.key == request.key && queued.generation == request.generation);
                if !already_queued {
                    self.prefetch.push_back(request);
                }
            }
            WorkerCommand::Open { document_id, path } => {
                self.opens.retain(|(queued_id, _)| *queued_id != document_id);
                self.opens.push_back((document_id, path));
            }
            WorkerCommand::Close { document_id } => {
                self.renders.retain(|queued| queued.key.document_id != document_id);
                self.prefetch.retain(|queued| queued.key.document_id != document_id);
                self.opens.retain(|(queued_id, _)| *queued_id != document_id);
                self.text_requests.retain(|(queued_id, _)| *queued_id != document_id);
                self.searches.retain(|(queued_id, _, _)| *queued_id != document_id);
                self.link_indices.retain(|(queued_id, _)| *queued_id != document_id);
                self.closes.push_back(document_id);
            }
            WorkerCommand::ExtractText { document_id, page } => {
                self.text_requests.push_back((document_id, page));
            }
            WorkerCommand::Search {
                document_id,
                request_id,
                query,
            } => {
                self.searches.retain(|(queued_id, _, _)| *queued_id != document_id);
                self.searches.push_back((document_id, request_id, query));
            }
            WorkerCommand::IndexLinks {
                document_id,
                request_id,
            } => {
                self.link_indices.retain(|(queued_id, _)| *queued_id != document_id);
                self.link_indices.push_back((document_id, request_id));
            }
            WorkerCommand::Shutdown => {
                self.shutdown = true;
            }
        }
    }

    fn drain(&mut self, rx: &Receiver<WorkerCommand>) {
        while let Ok(command) = rx.try_recv() {
            self.ingest(command);
            if self.shutdown {
                break;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.renders.is_empty()
            && self.prefetch.is_empty()
            && self.opens.is_empty()
            && self.closes.is_empty()
            && self.text_requests.is_empty()
            && self.searches.is_empty()
            && self.link_indices.is_empty()
            && !self.shutdown
    }
}

const MAX_DOCUMENT_FILE_BYTES: u64 = 256 * 1024 * 1024;

fn load_presentation_from_bytes(path: &Path, bytes: &[u8]) -> Result<Presentation, String> {
    if deckcraft_pptx::is_encrypted(bytes) {
        return Err(format!(
            "{} is password-protected / encrypted",
            path.display()
        ));
    }
    let mut presentation = deckcraft_pptx::import(bytes)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    deckcraft_format::repair(&mut presentation);
    install_embedded_fonts(&presentation);
    if presentation.slides.is_empty() {
        return Err(format!("{} has no slides", path.display()));
    }
    Ok(presentation)
}

fn install_embedded_fonts(presentation: &Presentation) {
    static INSTALLED_FONTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<(u64, usize)>>,
    > = std::sync::OnceLock::new();

    if presentation.embedded_fonts.is_empty() {
        return;
    }
    let installed =
        INSTALLED_FONTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let font_db = FontDb::global();
    for embedded in &presentation.embedded_fonts {
        for media_id in embedded.faces().into_iter().flatten() {
            if let Some(item) = presentation.media(media_id)
                && let Some(sfnt) = deckcraft_fonts::embedded_sfnt(&item.data)
            {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&sfnt, &mut hasher);
                let hash = std::hash::Hasher::finish(&hasher);
                let is_new = installed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((hash, sfnt.len()));
                if is_new {
                    font_db.add_font(sfnt.to_owned());
                }
            }
        }
    }
}

fn open_presentation(
    path: &Path,
) -> Result<(OpenPresentation, u32, Vec<OutlineItem>, DocumentRevision), String> {
    let revision = DocumentRevision::read(path)
        .map_err(|error| format!("failed to stat {}: {error}", path.display()))?;
    if revision.len > MAX_DOCUMENT_FILE_BYTES {
        return Err(format!(
            "{} exceeds maximum supported file size (256 MiB)",
            path.display()
        ));
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let is_supported = matches!(
        ext.as_str(),
        "docx"
            | "doc"
            | "docm"
            | "dotx"
            | "dotm"
            | "dot"
            | "rtf"
            | "odt"
            | "xlsx"
            | "xls"
            | "xlsm"
            | "xlsb"
            | "xltx"
            | "xltm"
            | "xlt"
            | "ods"
            | "csv"
            | "tsv"
            | "pptx"
            | "ppsx"
            | "potx"
            | "pptm"
            | "ppsm"
            | "potm"
    );
    if !is_supported {
        return Err(format!("unsupported document format: {}", path.display()));
    }

    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;

    let (backend, pages, outline) = match ext.as_str() {
        "docx" | "doc" | "docm" | "dotx" | "dotm" | "dot" | "rtf" | "odt" => {
            let (word, pages, outline) = crate::word::WordDocument::load(path, &bytes, &ext)?;
            (DocumentBackend::Word(Box::new(word)), pages, outline)
        }
        "xlsx" | "xls" | "xlsm" | "xlsb" | "xltx" | "xltm" | "xlt" | "ods" | "csv" | "tsv" => {
            let (excel, pages, outline) = crate::excel::ExcelDocument::load(path, &bytes)?;
            (DocumentBackend::Excel(Box::new(excel)), pages, outline)
        }
        "pptx" | "ppsx" | "potx" | "pptm" | "ppsm" | "potm" => {
            let presentation = load_presentation_from_bytes(path, &bytes)?;
            let pages = u32::try_from(presentation.slides.len())
                .map_err(|_| "presentation has too many slides".to_string())?;
            let outline = collect_outline(&presentation);
            (DocumentBackend::Pptx(Box::new(presentation)), pages, outline)
        }
        _ => unreachable!(),
    };

    let doc = OpenPresentation {
        backend,
        revision,
        pages,
        text_cache: (0..pages).map(|_| None).collect(),
        content_bounds_cache: HashMap::new(),
        active_search: None,
    };
    Ok((doc, pages, outline, revision))
}

fn collect_outline(presentation: &Presentation) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    let has_sections = !presentation.sections.is_empty();
    let mut section_by_first_slide = HashMap::new();
    for section in &presentation.sections {
        if let Some(&first_id) = section.slides.first() {
            section_by_first_slide.insert(first_id, section.name.trim().to_string());
        }
    }

    for (index, slide) in presentation.slides.iter().enumerate() {
        let Ok(page) = u32::try_from(index) else {
            break;
        };
        if let Some(section_name) = section_by_first_slide.get(&slide.id)
            && !section_name.is_empty()
        {
            items.push(OutlineItem {
                title: section_name.clone(),
                page,
                depth: 0,
            });
        }
        let title = slide_title(slide, index);
        let depth = u16::from(has_sections);
        items.push(OutlineItem { title, page, depth });
    }
    items
}

fn slide_title(slide: &deckcraft_model::Slide, index: usize) -> String {
    for shape in &slide.shapes {
        if let Some(ph) = &shape.ph
            && matches!(ph.kind, PhType::Title | PhType::CtrTitle)
            && let Some(text) = &shape.text
        {
            let title = normalize_context_text(&text.text());
            if !title.is_empty() {
                return title;
            }
        }
    }
    for shape in &slide.shapes {
        if let Some(text) = &shape.text {
            let full_text = text.text();
            let first_line = full_text
                .lines()
                .map(normalize_context_text)
                .find(|line| !line.is_empty());
            if let Some(line) = first_line {
                return truncate_chars(&line, 80);
            }
        }
    }
    if !slide.name.trim().is_empty() {
        return slide.name.trim().to_string();
    }
    format!("Slide {}", index + 1)
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn worker_loop(
    path: PathBuf,
    commands: Receiver<WorkerCommand>,
    messages: Sender<WorkerMessage>,
    latest_generation: Arc<AtomicU64>,
    link_index_epoch: Arc<AtomicU64>,
) {
    let (initial_doc, pages, outline, revision) = match open_presentation(&path) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = messages.send(WorkerMessage::Error(error));
            return;
        }
    };

    if messages
        .send(WorkerMessage::Ready {
            pages,
            outline,
            revision,
        })
        .is_err()
    {
        return;
    }

    let mut documents = HashMap::from([(1, initial_doc)]);
    let mut pending = PendingCommands::new();
    let mut active_link_index: Option<ActiveLinkIndex> = None;

    loop {
        if pending.is_empty() && active_link_index.is_none() {
            match commands.recv() {
                Ok(command) => pending.ingest(command),
                Err(_) => break,
            }
        }
        pending.drain(&commands);
        if pending.shutdown {
            break;
        }

        while let Some(document_id) = pending.closes.pop_front() {
            documents.remove(&document_id);
            if active_link_index.is_some_and(|active| active.document_id == document_id) {
                active_link_index = None;
            }
        }

        if let Some((document_id, path)) = pending.opens.pop_front() {
            match open_presentation(&path) {
                Ok((doc, pages, outline, revision)) => {
                    documents.insert(document_id, doc);
                    if active_link_index.is_some_and(|active| active.document_id == document_id) {
                        active_link_index = None;
                    }
                    if messages
                        .send(WorkerMessage::Opened {
                            document_id,
                            pages,
                            outline,
                            revision,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    if messages
                        .send(WorkerMessage::OpenError { document_id, error })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            continue;
        }

        if let Some((document_id, page)) = pending.text_requests.pop_front() {
            if let Some(doc) = documents.get_mut(&document_id) {
                let content = extract_slide_text(doc, page);
                if messages
                    .send(WorkerMessage::Text {
                        document_id,
                        page,
                        content,
                    })
                    .is_err()
                {
                    break;
                }
            }
            continue;
        }

        if let Some((document_id, request_id, query)) = pending.searches.pop_front() {
            let Some(doc) = documents.get_mut(&document_id) else {
                continue;
            };
            let normalized_query = normalize_search_query(&query);
            doc.active_search = (!normalized_query.is_empty()).then(|| ActiveSearch {
                request_id,
                query: normalized_query.clone(),
            });
            if !search_document(
                document_id,
                doc,
                request_id,
                &normalized_query,
                &commands,
                &mut pending,
                &messages,
            ) {
                break;
            }
            continue;
        }

        if let Some(request) = pending.renders.pop_front() {
            if request.generation != latest_generation.load(Ordering::Acquire) {
                continue;
            }
            let Some(doc) = documents.get_mut(&request.key.document_id) else {
                continue;
            };
            match render_page(doc, request) {
                Ok(Some(frame)) => {
                    if messages.send(WorkerMessage::Frame(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if messages.send(WorkerMessage::Error(error)).is_err() {
                        break;
                    }
                }
            }
            continue;
        }

        if let Some(request) = pending.prefetch.pop_front() {
            if request.generation != latest_generation.load(Ordering::Acquire) {
                continue;
            }
            let Some(doc) = documents.get_mut(&request.key.document_id) else {
                continue;
            };
            if let Ok(Some(frame)) = render_page(doc, request)
                && messages.send(WorkerMessage::Frame(frame)).is_err()
            {
                break;
            }
            continue;
        }

        if active_link_index.is_some_and(|active| {
            pending
                .link_indices
                .iter()
                .any(|(queued_id, _)| *queued_id == active.document_id)
        }) {
            active_link_index = None;
        }

        if active_link_index.is_none() {
            while let Some((document_id, request_id)) = pending.link_indices.pop_front() {
                if let Some(doc) = documents.get(&document_id) {
                    active_link_index = Some(ActiveLinkIndex {
                        document_id,
                        request_id,
                        epoch: link_index_epoch.load(Ordering::Acquire),
                        next_page: 0,
                        total_pages: doc.pages,
                    });
                    break;
                }
            }
        }

        if let Some(mut active) = active_link_index.take() {
            if active.epoch != link_index_epoch.load(Ordering::Acquire) {
                continue;
            }
            let Some(doc) = documents.get_mut(&active.document_id) else {
                continue;
            };
            let links = index_slide_links(doc, active.next_page);
            active.next_page += 1;
            let complete = active.next_page >= active.total_pages;
            if messages
                .send(WorkerMessage::LinkIndexProgress {
                    document_id: active.document_id,
                    request_id: active.request_id,
                    links,
                    scanned: active.next_page,
                    total: active.total_pages,
                    complete,
                })
                .is_err()
            {
                break;
            }
            if !complete {
                active_link_index = Some(active);
            }
        }
    }
}

fn render_page(doc: &mut OpenPresentation, request: RenderRequest) -> Result<Option<Frame>, String> {
    let key = request.key;
    let Ok(slide_index) = usize::try_from(key.page) else {
        return Ok(None);
    };
    let page_count = match &doc.backend {
        DocumentBackend::Pptx(pres) => pres.slides.len(),
        DocumentBackend::Word(word) => word.page_count(),
        DocumentBackend::Excel(excel) => excel.page_count(),
    };
    if slide_index >= page_count {
        return Ok(None);
    }

    let (page_w_f64, page_h_f64) = match &doc.backend {
        DocumentBackend::Pptx(pres) => (
            pres.slide_size.width.max(1.0),
            pres.slide_size.height.max(1.0),
        ),
        DocumentBackend::Word(word) => word.page_size(slide_index),
        DocumentBackend::Excel(excel) => excel.page_size(slide_index),
    };
    let page_width_pt = page_w_f64 as f32;
    let page_height_pt = page_h_f64 as f32;
    let content_bounds = slide_content_bounds(doc, key.page);

    let zoom = u32::from(key.zoom.max(1));
    let target_width = (u32::from(key.width) * zoom / 100).max(1);
    let target_height = (u32::from(key.height) * zoom / 100).max(1);
    let (width, height) = key.fit.raster_size(
        page_width_pt,
        page_height_pt,
        content_bounds,
        target_width,
        target_height,
    );
    let (width, height) = clamp_to_max_side(width, height);

    let render_started = Instant::now();
    let (width, height, scale, mut rgba, mask, links) = match &doc.backend {
        DocumentBackend::Pptx(pres) => {
            let image = render_slide(
                pres,
                slide_index,
                &RenderOpts {
                    scale: (f64::from(width) / page_w_f64).min(f64::from(height) / page_h_f64),
                    size: Some((width, height)),
                    ..Default::default()
                },
            );
            let width = image.width.max(1);
            let height = image.height.max(1);
            let scale = (f64::from(width) / page_w_f64).min(f64::from(height) / page_h_f64);
            let rgba = image.to_straight();
            let placed = place_slide(pres, slide_index);
            let mask = if key.invert {
                raster_picture_mask(pres, slide_index, width, height, scale)
            } else {
                None
            };
            let links = extract_slide_links(pres, slide_index, &placed, width, height, scale);
            (width, height, scale, rgba, mask, links)
        }
        DocumentBackend::Word(word) => {
            let (w, h, s, rgba) = word.render_page_rgba(slide_index, width, height);
            let mask = if key.invert {
                word.picture_mask(slide_index, w, h, s)
            } else {
                None
            };
            let links = word.extract_page_links(slide_index, w, h, s);
            (w, h, s, rgba, mask, links)
        }
        DocumentBackend::Excel(excel) => {
            let (w, h, s, rgba) = excel.render_page_rgba(slide_index, width, height);
            let mask = if key.invert {
                excel.picture_mask(slide_index, w, h, s)
            } else {
                None
            };
            let links = excel.extract_page_links(slide_index, w, h, s);
            (w, h, s, rgba, mask, links)
        }
    };
    let render_elapsed = render_started.elapsed();

    let dark_mode_elapsed = if key.invert {
        let dark_started = Instant::now();
        darken_rgba(&mut rgba, mask.as_deref(), key.dark_mode_style);
        Some(dark_started.elapsed())
    } else {
        None
    };

    let highlight_started = Instant::now();
    let mut highlighted = false;
    if key.search_request_id != 0
        && let Some(active) = doc.active_search.clone()
        && active.request_id == key.search_request_id
    {
        let rects = search_match_pixel_rects(doc, key.page, &active.query, width, height, scale);
        if !rects.is_empty() {
            highlight_pixel_rects(&mut rgba, width, height, &rects, key.search_highlight);
            highlighted = true;
        }
    }
    if key.link_mode && !links.is_empty() {
        let selected_rects = selected_page_link_rectangles(&links, key.selected_link_ordinal);
        highlight_link_rects(
            &mut rgba,
            width,
            height,
            &links,
            &selected_rects,
            key.link_highlight,
        );
        highlighted = true;
    }
    let highlight_elapsed = highlighted.then(|| highlight_started.elapsed());

    let compression_started = Instant::now();
    let compressed_rgba = kitty::compress_rgba(&rgba).map_err(|error| error.to_string())?;
    let compression_elapsed = compression_started.elapsed();

    Ok(Some(Frame {
        key,
        revision: doc.revision,
        width,
        height,
        page_width_pt,
        page_height_pt,
        content_bounds,
        compressed_rgba,
        render_elapsed,
        dark_mode_elapsed,
        highlight_elapsed,
        compression_elapsed,
        generation: request.generation,
        links,
    }))
}

fn clamp_to_max_side(width: u32, height: u32) -> (u32, u32) {
    let max_side = deckcraft_render::MAX_SIDE;
    let width = width.max(1);
    let height = height.max(1);
    if width <= max_side && height <= max_side {
        return (width, height);
    }
    let clamp_scale =
        (f64::from(max_side) / f64::from(width)).min(f64::from(max_side) / f64::from(height));
    (
        ((f64::from(width) * clamp_scale).round() as u32).clamp(1, max_side),
        ((f64::from(height) * clamp_scale).round() as u32).clamp(1, max_side),
    )
}

fn slide_content_bounds(doc: &mut OpenPresentation, page: u32) -> [f32; 4] {
    if let Some(&bounds) = doc.content_bounds_cache.get(&page) {
        return bounds;
    }
    let full = [0.0, 0.0, 1.0, 1.0];
    let Ok(slide_index) = usize::try_from(page) else {
        return full;
    };
    let bounds = match &doc.backend {
        DocumentBackend::Pptx(pres) => {
            if slide_index >= pres.slides.len() {
                return full;
            }
            let slide_w = pres.slide_size.width.max(1.0) as f32;
            let slide_h = pres.slide_size.height.max(1.0) as f32;
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;

            visit_slide_shapes(pres, slide_index, |shape, rect| {
                if !matches!(shape.kind, ShapeKind::Group { .. }) {
                    include_rect_bounds(
                        rect,
                        &mut min_x,
                        &mut min_y,
                        &mut max_x,
                        &mut max_y,
                        slide_w,
                        slide_h,
                    );
                }
            });

            if min_x.is_finite()
                && min_y.is_finite()
                && max_x > min_x + 1.0
                && max_y > min_y + 1.0
            {
                let pad_x = (slide_w * 0.02).max(4.0);
                let pad_y = (slide_h * 0.02).max(4.0);
                [
                    ((min_x - pad_x).clamp(0.0, slide_w) / slide_w).clamp(0.0, 1.0),
                    ((min_y - pad_y).clamp(0.0, slide_h) / slide_h).clamp(0.0, 1.0),
                    ((max_x + pad_x).clamp(0.0, slide_w) / slide_w).clamp(0.0, 1.0),
                    ((max_y + pad_y).clamp(0.0, slide_h) / slide_h).clamp(0.0, 1.0),
                ]
            } else {
                full
            }
        }
        DocumentBackend::Word(word) => word.content_bounds(slide_index),
        DocumentBackend::Excel(excel) => excel.content_bounds(slide_index),
    };
    doc.content_bounds_cache.insert(page, bounds);
    bounds
}

pub(crate) fn include_rect_bounds(
    r: KurboRect,
    min_x: &mut f32,
    min_y: &mut f32,
    max_x: &mut f32,
    max_y: &mut f32,
    slide_w: f32,
    slide_h: f32,
) {
    if r.width() <= 0.0 || r.height() <= 0.0 {
        return;
    }
    let x0 = (r.x0 as f32).clamp(0.0, slide_w);
    let y0 = (r.y0 as f32).clamp(0.0, slide_h);
    let x1 = (r.x1 as f32).clamp(0.0, slide_w);
    let y1 = (r.y1 as f32).clamp(0.0, slide_h);
    // Ignore full-bleed background shapes when computing content bounds.
    if x0 <= 1.0 && y0 <= 1.0 && x1 >= slide_w - 1.0 && y1 >= slide_h - 1.0 {
        return;
    }
    *min_x = min_x.min(x0);
    *min_y = min_y.min(y0);
    *max_x = max_x.max(x1);
    *max_y = max_y.max(y1);
}

fn visit_slide_shapes(
    presentation: &Presentation,
    slide_index: usize,
    mut f: impl FnMut(&Shape, KurboRect),
) {
    let Some(slide) = presentation.slides.get(slide_index) else {
        return;
    };
    if let Some(rctx) = resolve::Ctx::for_slide(presentation, slide) {
        let (show_layout, show_master) = resolve::show_master_shapes(slide, rctx.layout);
        if show_master {
            let mctx = resolve::Ctx::for_master(presentation, rctx.master);
            for shape in rctx.master.shapes.iter().filter(|s| s.ph.is_none()) {
                visit_shape(Some(&mctx), shape, kurbo::Affine::IDENTITY, 0, &mut f);
            }
        }
        if show_layout
            && let Some(layout) = rctx.layout
        {
            let lctx = resolve::Ctx::for_layout(presentation, rctx.master, layout);
            for shape in layout.shapes.iter().filter(|s| s.ph.is_none()) {
                visit_shape(Some(&lctx), shape, kurbo::Affine::IDENTITY, 0, &mut f);
            }
        }
        for shape in &slide.shapes {
            visit_shape(Some(&rctx), shape, kurbo::Affine::IDENTITY, 0, &mut f);
        }
    } else {
        for shape in &slide.shapes {
            visit_shape(None, shape, kurbo::Affine::IDENTITY, 0, &mut f);
        }
    }
}

fn visit_shape(
    rctx: Option<&resolve::Ctx<'_>>,
    shape: &Shape,
    parent: kurbo::Affine,
    depth: usize,
    f: &mut impl FnMut(&Shape, KurboRect),
) {
    if depth > 32 || shape.hidden {
        return;
    }
    let Some(x) = rctx
        .map(|ctx| resolve::xfrm(ctx, shape))
        .or(shape.xfrm)
        .filter(|x| x.is_finite())
    else {
        return;
    };
    let m = parent * x.affine();
    let rect = transform_rect_bbox(&m, KurboRect::new(0.0, 0.0, x.w, x.h));
    f(shape, rect);
    if let ShapeKind::Group { children, child } = &shape.kind {
        let sx = if child.w.abs() > 1e-9 { x.w / child.w } else { 1.0 };
        let sy = if child.h.abs() > 1e-9 { x.h / child.h } else { 1.0 };
        let gm = m
            * kurbo::Affine::scale_non_uniform(sx, sy)
            * kurbo::Affine::translate(kurbo::Vec2::new(-child.x, -child.y));
        for child_shape in children {
            visit_shape(rctx, child_shape, gm, depth + 1, f);
        }
    }
}

fn raster_picture_mask(
    presentation: &Presentation,
    slide_index: usize,
    width: u32,
    height: u32,
    scale: f64,
) -> Option<Vec<u8>> {
    let mut rects = Vec::new();
    visit_slide_shapes(presentation, slide_index, |shape, rect| {
        if matches!(shape.kind, ShapeKind::Picture { .. }) && rect.width() > 0.0 && rect.height() > 0.0 {
            rects.push(rect);
        }
    });
    raster_mask_from_rects(&rects, width, height, scale)
}

pub(crate) fn raster_mask_from_rects(
    rects: &[KurboRect],
    width: u32,
    height: u32,
    scale: f64,
) -> Option<Vec<u8>> {
    if rects.is_empty() {
        return None;
    }
    let w = width as usize;
    let h = height as usize;
    let mut mask = vec![0_u8; w.saturating_mul(h)];
    for rect in rects {
        let left = ((rect.x0 * scale).floor() as i64).clamp(0, width as i64) as usize;
        let top = ((rect.y0 * scale).floor() as i64).clamp(0, height as i64) as usize;
        let right = ((rect.x1 * scale).ceil() as i64).clamp(0, width as i64) as usize;
        let bottom = ((rect.y1 * scale).ceil() as i64).clamp(0, height as i64) as usize;
        for y in top..bottom.min(h) {
            mask[y * w + left..y * w + right.min(w)].fill(255);
        }
    }
    Some(mask)
}

pub(crate) fn darken_rgba(rgba: &mut [u8], image_mask: Option<&[u8]>, style: DarkModeStyle) {
    let transform = DarkModeTransform::new(style);
    match image_mask {
        Some(mask) => {
            for (pixel, &masked) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(mask) {
                if masked == 255 {
                    continue;
                }
                let [r, g, b] = transform.apply(pixel[0], pixel[1], pixel[2]);
                if masked == 0 {
                    pixel[0] = r;
                    pixel[1] = g;
                    pixel[2] = b;
                } else {
                    let keep = u16::from(masked);
                    let flip = 255 - keep;
                    pixel[0] = ((u16::from(pixel[0]) * keep + u16::from(r) * flip + 127) / 255) as u8;
                    pixel[1] = ((u16::from(pixel[1]) * keep + u16::from(g) * flip + 127) / 255) as u8;
                    pixel[2] = ((u16::from(pixel[2]) * keep + u16::from(b) * flip + 127) / 255) as u8;
                }
            }
        }
        None => {
            for pixel in rgba.as_chunks_mut::<4>().0 {
                let [r, g, b] = transform.apply(pixel[0], pixel[1], pixel[2]);
                pixel[0] = r;
                pixel[1] = g;
                pixel[2] = b;
            }
        }
    }
}

struct DarkModeTransform {
    background: [f32; 3],
    foreground: [f32; 3],
}

impl DarkModeTransform {
    fn new(style: DarkModeStyle) -> Self {
        Self {
            background: style.background.map(|channel| f32::from(channel) / 255.0),
            foreground: style.foreground.map(|channel| f32::from(channel) / 255.0),
        }
    }

    fn apply(&self, r: u8, g: u8, b: u8) -> [u8; 3] {
        let rf = f32::from(r) / 255.0;
        let gf = f32::from(g) / 255.0;
        let bf = f32::from(b) / 255.0;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let lightness = 0.5 * (max + min);
        let target_lightness = 1.0 - lightness;
        let delta = target_lightness - lightness;
        if max - min < 0.05 {
            let t = target_lightness.clamp(0.0, 1.0);
            return [0, 1, 2].map(|i| {
                let value = self.background[i] + (self.foreground[i] - self.background[i]) * t;
                (value * 255.0).round().clamp(0.0, 255.0) as u8
            });
        }
        [rf, gf, bf].map(|channel| {
            let shifted = (channel + delta).clamp(0.0, 1.0);
            (shifted * 255.0).round().clamp(0.0, 255.0) as u8
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

fn highlight_pixel_rects(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rects: &[PixelRect],
    tint: [u8; 3],
) {
    let w = width as usize;
    let h = height as usize;
    for rect in rects {
        let left = (rect.left as usize).min(w);
        let right = (rect.right as usize).min(w);
        let top = (rect.top as usize).min(h);
        let bottom = (rect.bottom as usize).min(h);
        for y in top..bottom {
            for x in left..right {
                let idx = (y * w + x) * 4;
                for c in 0..3 {
                    rgba[idx + c] =
                        ((u16::from(rgba[idx + c]) * 3 + u16::from(tint[c]) * 2) / 5) as u8;
                }
            }
        }
    }
}

fn highlight_link_rects(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    links: &[PageLink],
    selected_rects: &[PageLinkRect],
    tint: [u8; 3],
) {
    let w = width as usize;
    let h = height as usize;
    for link in links {
        let is_selected = selected_rects.contains(&link.rect);
        let left = (link.rect.left as usize).min(w);
        let right = (link.rect.right as usize).min(w);
        let top = (link.rect.top as usize).min(h);
        let bottom = (link.rect.bottom as usize).min(h);
        if left >= right || top >= bottom {
            continue;
        }
        for y in top..bottom {
            for x in left..right {
                let border = x - left < 2 || right - 1 - x < 2 || y - top < 2 || bottom - 1 - y < 2;
                let idx = (y * w + x) * 4;
                if border {
                    for c in 0..3 {
                        rgba[idx + c] = if is_selected {
                            tint[c]
                        } else {
                            ((u16::from(rgba[idx + c]) + u16::from(tint[c]) * 2) / 3) as u8
                        };
                    }
                } else if is_selected {
                    for c in 0..3 {
                        rgba[idx + c] =
                            ((u16::from(rgba[idx + c]) * 3 + u16::from(tint[c])) / 4) as u8;
                    }
                }
            }
        }
    }
}

fn extract_slide_links(
    presentation: &Presentation,
    slide_index: usize,
    placed: &Placed,
    width: u32,
    height: u32,
    scale: f64,
) -> Vec<PageLink> {
    let total_slides = presentation.slides.len();
    let mut links = Vec::new();

    visit_slide_shapes(presentation, slide_index, |shape, bbox| {
        if let Some(hlink) = &shape.click
            && let Some(target) = resolve_hyperlink(presentation, slide_index, total_slides, hlink)
            && let Some(rect) = bbox_to_page_link_rect(bbox, width, height, scale)
        {
            let label = shape
                .text
                .as_ref()
                .map(|tb| normalize_context_text(&tb.text()))
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    (!hlink.tooltip.trim().is_empty()).then(|| hlink.tooltip.trim().to_string())
                })
                .unwrap_or_else(|| default_link_label(&target));
            links.push(PageLink {
                rect,
                label,
                target,
            });
        }
    });

    for pt in &placed.texts {
        let mut char_offset = 0usize;
        for para in &pt.body.paragraphs {
            for run in &para.runs {
                let run_len = run.text.chars().count();
                if let Some(hlink) = &run.props.link
                    && let Some(target) =
                        resolve_hyperlink(presentation, slide_index, total_slides, hlink)
                    && let Some(bbox) = placed_text_span_bbox(pt, char_offset, char_offset + run_len)
                    && let Some(rect) = bbox_to_page_link_rect(bbox, width, height, scale)
                {
                    let label = normalize_context_text(&run.text);
                    let label = if label.is_empty() {
                        default_link_label(&target)
                    } else {
                        label
                    };
                    links.push(PageLink {
                        rect,
                        label,
                        target,
                    });
                }
                char_offset += run_len;
            }
            char_offset += 1; // paragraph newline
        }
    }

    links
}

fn resolve_hyperlink(
    presentation: &Presentation,
    slide_index: usize,
    total_slides: usize,
    hlink: &Hyperlink,
) -> Option<LinkTarget> {
    match &hlink.action {
        Action::Url { url } => {
            let url = url.trim();
            (!url.is_empty()).then(|| LinkTarget::Uri(url.to_string()))
        }
        Action::Slide { slide } => {
            let page = presentation.slides.iter().position(|s| s.id == *slide)?;
            Some(LinkTarget::Internal {
                page: u32::try_from(page).ok()?,
                top_ratio: None,
                left_ratio: None,
            })
        }
        Action::NextSlide => {
            let next = slide_index + 1;
            (next < total_slides).then_some(LinkTarget::Internal {
                page: next as u32,
                top_ratio: None,
                left_ratio: None,
            })
        }
        Action::PreviousSlide => slide_index.checked_sub(1).map(|prev| LinkTarget::Internal {
            page: prev as u32,
            top_ratio: None,
            left_ratio: None,
        }),
        Action::FirstSlide => (total_slides > 0).then_some(LinkTarget::Internal {
            page: 0,
            top_ratio: None,
            left_ratio: None,
        }),
        Action::LastSlide => total_slides.checked_sub(1).map(|last| LinkTarget::Internal {
            page: last as u32,
            top_ratio: None,
            left_ratio: None,
        }),
        _ => None,
    }
}

pub(crate) fn default_link_label(target: &LinkTarget) -> String {
    match target {
        LinkTarget::Internal { page, .. } => format!("Page {}", page + 1),
        LinkTarget::Uri(uri) => uri.clone(),
    }
}

fn placed_text_span_bbox(
    pt: &deckcraft_render::PlacedText,
    char_start: usize,
    char_end: usize,
) -> Option<KurboRect> {
    let mut union: Option<KurboRect> = None;
    for line in &pt.layout.lines {
        if let Some((x0, x1)) = line.span(char_start, char_end) {
            let local = KurboRect::new(x0, line.top, x1, line.bottom);
            let rect = transform_rect_bbox(&pt.transform, local);
            union = Some(match union {
                Some(prev) => prev.union(rect),
                None => rect,
            });
        }
    }
    union
}

pub(crate) fn transform_rect_bbox(affine: &kurbo::Affine, rect: KurboRect) -> KurboRect {
    let p0 = *affine * KurboPoint::new(rect.x0, rect.y0);
    let p1 = *affine * KurboPoint::new(rect.x1, rect.y0);
    let p2 = *affine * KurboPoint::new(rect.x1, rect.y1);
    let p3 = *affine * KurboPoint::new(rect.x0, rect.y1);
    let min_x = p0.x.min(p1.x).min(p2.x).min(p3.x);
    let min_y = p0.y.min(p1.y).min(p2.y).min(p3.y);
    let max_x = p0.x.max(p1.x).max(p2.x).max(p3.x);
    let max_y = p0.y.max(p1.y).max(p2.y).max(p3.y);
    KurboRect::new(min_x, min_y, max_x, max_y)
}

pub(crate) fn bbox_to_page_link_rect(
    bbox: KurboRect,
    width: u32,
    height: u32,
    scale: f64,
) -> Option<PageLinkRect> {
    let left = ((bbox.x0 * scale).floor() as i64).clamp(0, i64::from(width)) as u32;
    let top = ((bbox.y0 * scale).floor() as i64).clamp(0, i64::from(height)) as u32;
    let right = ((bbox.x1 * scale).ceil() as i64).clamp(0, i64::from(width)) as u32;
    let bottom = ((bbox.y1 * scale).ceil() as i64).clamp(0, i64::from(height)) as u32;
    (right > left && bottom > top).then_some(PageLinkRect {
        left,
        top,
        right,
        bottom,
    })
}

fn selected_page_link_rectangles(
    links: &[PageLink],
    selected_ordinal: Option<u32>,
) -> Vec<PageLinkRect> {
    let Some(selected) = selected_ordinal.and_then(|ordinal| usize::try_from(ordinal).ok()) else {
        return Vec::new();
    };
    links
        .get(selected)
        .map(|link| vec![link.rect])
        .unwrap_or_default()
}

fn index_slide_links(doc: &mut OpenPresentation, page: u32) -> Vec<DocumentLink> {
    let Ok(slide_index) = usize::try_from(page) else {
        return Vec::new();
    };
    let index_width = 1000u32;
    let (index_height, scale, links) = match &doc.backend {
        DocumentBackend::Pptx(pres) => {
            if slide_index >= pres.slides.len() {
                return Vec::new();
            }
            let slide_w = pres.slide_size.width.max(1.0);
            let slide_h = pres.slide_size.height.max(1.0);
            let index_height = ((f64::from(index_width) * slide_h / slide_w).round() as u32).max(1);
            let scale = f64::from(index_width) / slide_w;
            let placed = place_slide(pres, slide_index);
            let links = extract_slide_links(
                pres,
                slide_index,
                &placed,
                index_width,
                index_height,
                scale,
            );
            (index_height, scale, links)
        }
        DocumentBackend::Word(word) => {
            if slide_index >= word.page_count() {
                return Vec::new();
            }
            let (page_w, page_h) = word.page_size(slide_index);
            let index_height = ((f64::from(index_width) * page_h / page_w).round() as u32).max(1);
            let scale = f64::from(index_width) / page_w;
            let links = word.extract_page_links(slide_index, index_width, index_height, scale);
            (index_height, scale, links)
        }
        DocumentBackend::Excel(excel) => {
            if slide_index >= excel.page_count() {
                return Vec::new();
            }
            let (page_w, page_h) = excel.page_size(slide_index);
            let index_height = ((f64::from(index_width) * page_h / page_w).round() as u32).max(1);
            let scale = f64::from(index_width) / page_w;
            let links = excel.extract_page_links(slide_index, index_width, index_height, scale);
            (index_height, scale, links)
        }
    };
    let cached = cached_slide_text(doc, page).cloned();
    let indexed_context = cached
        .as_ref()
        .map(|text| normalize_context_with_indices(&text.raw));
    let mut label_occurrences = HashMap::<String, usize>::new();
    let mut used_byte_starts = std::collections::HashSet::<usize>::new();
    links
        .into_iter()
        .enumerate()
        .map(|(ordinal, link)| {
            let occurrence = label_occurrences.entry(link.label.clone()).or_default();
            let source_context = match (&cached, &indexed_context) {
                (Some(cached_text), Some((norm_text, byte_to_char))) => context_around_link(
                    norm_text,
                    byte_to_char,
                    &cached_text.char_rects,
                    &link.rect,
                    index_width,
                    index_height,
                    scale,
                    &link.label,
                    *occurrence,
                    &mut used_byte_starts,
                    90,
                    110,
                ),
                _ => None,
            };
            *occurrence += 1;
            DocumentLink {
                source_page: page,
                source_top_ratio: (link.rect.top as f32 / index_height as f32).clamp(0.0, 1.0),
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                label: link.label,
                source_context,
                reference_context: None,
                target: link.target,
            }
        })
        .collect()
}

fn extract_slide_text(doc: &mut OpenPresentation, page: u32) -> String {
    cached_slide_text(doc, page)
        .map(|cached| cached.raw.clone())
        .unwrap_or_default()
}

fn cached_slide_text(doc: &mut OpenPresentation, page: u32) -> Option<&CachedSlideText> {
    let index = usize::try_from(page).ok()?;
    if index >= doc.text_cache.len() {
        return None;
    }
    if doc.text_cache[index].is_none() {
        doc.text_cache[index] = Some(match &doc.backend {
            DocumentBackend::Pptx(pres) => {
                let placed = place_slide(pres, index);
                let mut raw = String::new();
                let mut char_rects = Vec::new();
                for pt in &placed.texts {
                    let body_text = pt.body.text();
                    if body_text.trim().is_empty() {
                        continue;
                    }
                    if !raw.is_empty() {
                        raw.push('\n');
                        char_rects.push(None);
                    }
                    let lines = &pt.layout.lines;
                    let mut line_idx = 0usize;
                    for (char_idx, ch) in body_text.chars().enumerate() {
                        raw.push(ch);
                        while line_idx < lines.len()
                            && lines[line_idx].span(char_idx, usize::MAX).is_none()
                        {
                            line_idx += 1;
                        }
                        let rect = lines
                            .get(line_idx)
                            .and_then(|line| {
                                let (x0, x1) = line.span(char_idx, char_idx + 1)?;
                                let local = KurboRect::new(x0, line.top, x1, line.bottom);
                                Some(transform_rect_bbox(&pt.transform, local))
                            });
                        char_rects.push(rect);
                    }
                }
                let notes_text = pres.slides[index].notes.text();
                if !notes_text.trim().is_empty() {
                    if !raw.is_empty() {
                        raw.push_str("\n\n");
                        char_rects.push(None);
                        char_rects.push(None);
                    }
                    for ch in notes_text.chars() {
                        raw.push(ch);
                        char_rects.push(None);
                    }
                }
                build_cached_text(raw, char_rects)
            }
            DocumentBackend::Word(word) => word.extract_cached_text(index),
            DocumentBackend::Excel(excel) => excel.extract_cached_text(index),
        });
    }
    doc.text_cache[index].as_ref()
}

fn search_match_pixel_rects(
    doc: &mut OpenPresentation,
    page: u32,
    normalized_query: &str,
    width: u32,
    height: u32,
    scale: f64,
) -> Vec<PixelRect> {
    if normalized_query.is_empty() {
        return Vec::new();
    }
    let Some(cached) = cached_slide_text(doc, page) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (start, end) in normalized_match_ranges(&cached.normalized, normalized_query) {
        let Some(&first_char) = cached.source_index_by_byte.get(start) else {
            continue;
        };
        let Some(&last_char) = cached
            .source_index_by_byte
            .get(end.saturating_sub(1))
        else {
            continue;
        };
        let mut union: Option<ModelRect> = None;
        for idx in first_char..=last_char {
            if let Some(Some(r)) = cached.char_rects.get(idx)
                && r.width() > 0.0
                && r.height() > 0.0
            {
                union = Some(match union {
                    Some(prev) => prev.union(*r),
                    None => *r,
                });
            }
        }
        if let Some(r) = union {
            let left = ((r.x0 * scale).floor() as i64).clamp(0, i64::from(width)) as u32;
            let top = ((r.y0 * scale).floor() as i64).clamp(0, i64::from(height)) as u32;
            let right = ((r.x1 * scale).ceil() as i64).clamp(0, i64::from(width)) as u32;
            let bottom = ((r.y1 * scale).ceil() as i64).clamp(0, i64::from(height)) as u32;
            if right > left && bottom > top {
                out.push(PixelRect {
                    left,
                    top,
                    right,
                    bottom,
                });
            }
        }
    }
    out
}

fn search_document(
    document_id: DocumentId,
    doc: &mut OpenPresentation,
    request_id: u64,
    query: &str,
    commands: &Receiver<WorkerCommand>,
    pending: &mut PendingCommands,
    messages: &Sender<WorkerMessage>,
) -> bool {
    if query.is_empty() {
        return messages
            .send(WorkerMessage::SearchResults {
                document_id,
                request_id,
                matches: Vec::new(),
                total_occurrences: 0,
            })
            .is_ok();
    }

    let total = doc.pages;
    let mut matches = Vec::new();
    let mut total_occurrences = 0u32;

    for page in 0..total {
        pending.drain(commands);
        if pending.shutdown
            || pending.closes.contains(&document_id)
            || pending
                .searches
                .iter()
                .any(|(queued_id, _, _)| *queued_id == document_id)
        {
            return !pending.shutdown;
        }
        if let Some(cached) = cached_slide_text(doc, page) {
            let ranges = normalized_match_ranges(&cached.normalized, query);
            if let Some(&(first_start, first_end)) = ranges.first() {
                let occurrences = u32::try_from(ranges.len()).unwrap_or(u32::MAX);
                total_occurrences = total_occurrences.saturating_add(occurrences);
                let context = search_context(&cached.normalized, first_start, first_end);
                matches.push(SearchPageMatch {
                    page,
                    occurrences,
                    context,
                });
            }
        }
        if (page + 1) % 8 == 0
            && page + 1 < total
            && messages
                .send(WorkerMessage::SearchProgress {
                    document_id,
                    request_id,
                    scanned: page + 1,
                    total,
                    matches: matches.clone(),
                    total_occurrences,
                })
                .is_err()
        {
            return false;
        }
    }

    messages
        .send(WorkerMessage::SearchResults {
            document_id,
            request_id,
            matches,
            total_occurrences,
        })
        .is_ok()
}

pub(crate) fn normalize_search_query(query: &str) -> String {
    normalize_search_characters(query.chars().enumerate()).0
}

fn normalize_search_characters(
    chars: impl Iterator<Item = (usize, char)>,
) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut source_index_by_byte = Vec::new();
    let mut pending_space_index = None;

    for (source_index, character) in chars {
        if matches!(character, '\u{00ad}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}') {
            continue;
        }
        if character.is_whitespace() {
            if !normalized.is_empty() {
                pending_space_index.get_or_insert(source_index);
            }
            continue;
        }
        let mut emitted = false;
        for decomposed in character.nfkd() {
            if is_combining_mark(decomposed) {
                continue;
            }
            for lower in decomposed.to_lowercase() {
                if lower.is_whitespace() {
                    if !normalized.is_empty() {
                        pending_space_index.get_or_insert(source_index);
                    }
                    continue;
                }
                if let Some(space_index) = pending_space_index.take() {
                    normalized.push(' ');
                    source_index_by_byte.push(space_index);
                }
                let start = normalized.len();
                normalized.push(lower);
                source_index_by_byte.resize(normalized.len(), source_index);
                debug_assert!(normalized.len() > start);
                emitted = true;
            }
        }
        let _ = emitted;
    }

    (normalized, source_index_by_byte)
}

fn normalized_match_ranges(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    haystack
        .match_indices(needle)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect()
}

fn search_context(text: &str, match_start: usize, match_end: usize) -> String {
    let before = text[..match_start]
        .chars()
        .rev()
        .take(36)
        .collect::<Vec<_>>();
    let start = match_start
        - before
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
    let after_len: usize = text[match_end..]
        .chars()
        .take(64)
        .map(char::len_utf8)
        .sum();
    let end = match_end + after_len;
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.push_str(text[start..end].trim());
    if end < text.len() {
        snippet.push('…');
    }
    snippet
}

pub(crate) fn normalize_context_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_context_with_indices(raw: &str) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut source_index_by_byte = Vec::new();
    let mut pending_space = None;

    for (char_idx, ch) in raw.chars().enumerate() {
        if ch.is_whitespace() {
            if !normalized.is_empty() {
                pending_space.get_or_insert(char_idx);
            }
            continue;
        }
        if let Some(space_idx) = pending_space.take() {
            normalized.push(' ');
            source_index_by_byte.push(space_idx);
        }
        normalized.push(ch);
        source_index_by_byte.resize(normalized.len(), char_idx);
    }

    (normalized, source_index_by_byte)
}

#[allow(clippy::too_many_arguments)]
fn context_around_link(
    text: &str,
    source_index_by_byte: &[usize],
    char_rects: &[Option<ModelRect>],
    link_rect: &PageLinkRect,
    width: u32,
    height: u32,
    scale: f64,
    label: &str,
    fallback_occurrence: usize,
    used_byte_starts: &mut std::collections::HashSet<usize>,
    before_chars: usize,
    after_chars: usize,
) -> Option<String> {
    let label = normalize_context_text(label);
    if label.is_empty() {
        return None;
    }
    let candidates: Vec<(usize, &str)> = text.match_indices(&label).collect();
    if candidates.is_empty() {
        return None;
    }

    let matched_span = candidates
        .iter()
        .copied()
        .find(|&(start, matched)| {
            if used_byte_starts.contains(&start) {
                return false;
            }
            let Some(&first_char) = source_index_by_byte.get(start) else {
                return false;
            };
            let Some(&last_char) = source_index_by_byte.get(start + matched.len().saturating_sub(1))
            else {
                return false;
            };
            let mut union: Option<ModelRect> = None;
            for idx in first_char..=last_char {
                if let Some(Some(r)) = char_rects.get(idx)
                    && r.width() > 0.0
                    && r.height() > 0.0
                {
                    union = Some(match union {
                        Some(prev) => prev.union(*r),
                        None => *r,
                    });
                }
            }
            let Some(bbox) = union else {
                return false;
            };
            let Some(candidate_rect) = bbox_to_page_link_rect(bbox, width, height, scale) else {
                return false;
            };
            candidate_rect.left < link_rect.right
                && candidate_rect.right > link_rect.left
                && candidate_rect.top < link_rect.bottom
                && candidate_rect.bottom > link_rect.top
        })
        .or_else(|| candidates.get(fallback_occurrence).copied())?;

    let (start, matched) = matched_span;
    used_byte_starts.insert(start);
    let end = start + matched.len();
    let prefix_bytes: usize = text[..start]
        .chars()
        .rev()
        .take(before_chars)
        .map(char::len_utf8)
        .sum();
    let suffix_bytes: usize = text[end..]
        .chars()
        .take(after_chars)
        .map(char::len_utf8)
        .sum();
    let slice_start = start - prefix_bytes;
    let slice_end = end + suffix_bytes;
    let mut out = String::new();
    if slice_start > 0 {
        out.push('…');
    }
    out.push_str(text[slice_start..slice_end].trim());
    if slice_end < text.len() {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deckcraft_model::{
        Paragraph, PhType, Placeholder, Run, ShapeId, Slide, SlideId, TextBody, Xfrm,
    };

    fn sample_pptx_bytes() -> Vec<u8> {
        let mut pres = Presentation::default();
        pres.slides.clear();
        let mut slide = Slide {
            id: SlideId(1),
            ..Default::default()
        };
        let title = Shape {
            id: ShapeId(2),
            xfrm: Some(Xfrm {
                x: 72.0,
                y: 54.0,
                w: 600.0,
                h: 80.0,
                ..Default::default()
            }),
            kind: ShapeKind::Shape,
            text_box: true,
            text: Some(TextBody {
                paragraphs: vec![Paragraph {
                    runs: vec![Run::new("Quarterly Overview")],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ph: Some(Placeholder {
                kind: PhType::Title,
                idx: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        slide.shapes.push(title);
        pres.slides.push(slide.into());
        deckcraft_pptx::export(&pres).expect("export pptx")
    }

    #[test]
    fn opens_renders_and_searches_pptx_slides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.pptx");
        std::fs::write(&path, sample_pptx_bytes()).unwrap();

        let (mut doc, pages, outline, _) = open_presentation(&path).unwrap();
        assert_eq!(pages, 1);
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].title, "Quarterly Overview");

        let text = extract_slide_text(&mut doc, 0);
        assert!(text.contains("Quarterly Overview"));

        let key = RenderKey {
            document_id: 0,
            page: 0,
            width: 320,
            height: 180,
            zoom: 100,
            fit: FitMode::Page,
            invert: true,
            dark_mode_style: DarkModeStyle::default(),
            search_request_id: 0,
            search_highlight: [0xff, 0xd7, 0x00],
            link_mode: false,
            link_highlight: [0x00, 0xd7, 0xff],
            selected_link_ordinal: None,
        };
        let frame = render_page(&mut doc, RenderRequest { key, generation: 1 })
            .unwrap()
            .unwrap();
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 180);
        assert!(!frame.compressed_rgba.is_empty());
        assert!(frame.content_bounds[0] >= 0.0 && frame.content_bounds[0] < frame.content_bounds[2]);
        assert!(frame.content_bounds[2] <= 1.0);
        assert!(frame.content_bounds[1] >= 0.0 && frame.content_bounds[1] < frame.content_bounds[3]);
        assert!(frame.content_bounds[3] <= 1.0);

        let content_key = RenderKey {
            fit: FitMode::Content,
            ..key
        };
        let content_frame = render_page(
            &mut doc,
            RenderRequest {
                key: content_key,
                generation: 2,
            },
        )
        .unwrap()
        .unwrap();
        assert!(content_frame.width > frame.width);
        assert!(content_frame.height > frame.height);
    }

    #[test]
    fn group_child_transforms_apply_to_links_and_picture_mask() {
        let mut pres = Presentation::default();
        pres.slides.clear();
        let mut slide = Slide {
            id: SlideId(1),
            show_master_shapes: false,
            ..Default::default()
        };
        let linked_child = Shape {
            id: ShapeId(10),
            xfrm: Some(Xfrm::new(10.0, 20.0, 50.0, 25.0)),
            kind: ShapeKind::Picture {
                fill: Default::default(),
            },
            click: Some(Hyperlink {
                action: Action::Url {
                    url: "https://example.com".into(),
                },
                tooltip: "Grouped link".into(),
                highlight_click: false,
                sound: None,
                stop_sound: false,
                zoom_return: None,
            }),
            ..Default::default()
        };
        let group = Shape {
            id: ShapeId(9),
            xfrm: Some(Xfrm::new(200.0, 100.0, 200.0, 100.0)),
            kind: ShapeKind::Group {
                child: Xfrm::new(0.0, 0.0, 100.0, 50.0),
                children: vec![linked_child],
            },
            ..Default::default()
        };
        slide.shapes.push(group);
        pres.slides.push(slide.into());

        // Child at (10, 20, 50x25) in child space (0, 0, 100x50) mapped onto group (200, 100, 200x100)
        // lands at slide coordinates x = 200 + 10*2 = 220..320, y = 100 + 20*2 = 140..190.
        let placed = place_slide(&pres, 0);
        let links = extract_slide_links(&pres, 0, &placed, 960, 540, 1.0);
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].rect,
            PageLinkRect {
                left: 220,
                top: 140,
                right: 320,
                bottom: 190,
            }
        );

        let mask = raster_picture_mask(&pres, 0, 960, 540, 1.0).expect("picture mask");
        assert_eq!(mask[150 * 960 + 250], 255);
        assert_eq!(mask[30 * 960 + 20], 0);
    }

    #[test]
    fn pending_commands_preserve_queued_renders_and_prefetches_in_same_generation() {
        let base_key = RenderKey {
            document_id: 1,
            page: 0,
            width: 800,
            height: 600,
            zoom: 100,
            fit: FitMode::Page,
            invert: false,
            dark_mode_style: DarkModeStyle::default(),
            search_request_id: 0,
            search_highlight: [0xff, 0xd7, 0x00],
            link_mode: false,
            link_highlight: [0x00, 0xd7, 0xff],
            selected_link_ordinal: None,
        };
        let key_for_page = |page: u32, zoom: u16| RenderKey {
            page,
            zoom,
            ..base_key
        };

        let mut pending = PendingCommands::new();
        pending.ingest(WorkerCommand::Render(RenderRequest {
            key: key_for_page(0, 100),
            generation: 1,
        }));
        pending.ingest(WorkerCommand::Prefetch(RenderRequest {
            key: key_for_page(1, 100),
            generation: 1,
        }));
        pending.ingest(WorkerCommand::Prefetch(RenderRequest {
            key: key_for_page(2, 100),
            generation: 1,
        }));
        // Visible render for page 1 promotes page 1 from prefetch to renders without dropping
        // page 0 (already in renders) or page 2 (still in prefetch).
        pending.ingest(WorkerCommand::Render(RenderRequest {
            key: key_for_page(1, 100),
            generation: 1,
        }));

        let render_pages: Vec<u32> = pending.renders.iter().map(|r| r.key.page).collect();
        let prefetch_pages: Vec<u32> = pending.prefetch.iter().map(|r| r.key.page).collect();
        assert_eq!(render_pages, vec![0, 1]);
        assert_eq!(prefetch_pages, vec![2]);

        // Starting generation 2 (e.g. zooming in to 110%) prunes generation 1 requests while
        // keeping all generation 2 requests queued in the same generation.
        pending.ingest(WorkerCommand::Render(RenderRequest {
            key: key_for_page(0, 110),
            generation: 2,
        }));
        pending.ingest(WorkerCommand::Render(RenderRequest {
            key: key_for_page(1, 110),
            generation: 2,
        }));
        let gen2_pages: Vec<(u32, u16, u64)> = pending
            .renders
            .iter()
            .map(|r| (r.key.page, r.key.zoom, r.generation))
            .collect();
        assert_eq!(gen2_pages, vec![(0, 110, 2), (1, 110, 2)]);
        assert!(pending.prefetch.is_empty());
    }

    #[test]
    fn pending_commands_coalesce_searches_and_link_indices_per_document() {
        let mut pending = PendingCommands::new();
        pending.ingest(WorkerCommand::Search {
            document_id: 1,
            request_id: 10,
            query: "first".into(),
        });
        pending.ingest(WorkerCommand::Search {
            document_id: 2,
            request_id: 20,
            query: "other".into(),
        });
        pending.ingest(WorkerCommand::Search {
            document_id: 1,
            request_id: 11,
            query: "updated".into(),
        });
        assert_eq!(
            pending.searches.iter().cloned().collect::<Vec<_>>(),
            vec![
                (2, 20, "other".to_string()),
                (1, 11, "updated".to_string()),
            ]
        );

        pending.ingest(WorkerCommand::IndexLinks {
            document_id: 1,
            request_id: 100,
        });
        pending.ingest(WorkerCommand::IndexLinks {
            document_id: 2,
            request_id: 200,
        });
        assert_eq!(
            pending.link_indices.iter().cloned().collect::<Vec<_>>(),
            vec![(1, 100), (2, 200)]
        );

        // Closing document 1 removes its pending search and link-index without dropping document 2.
        pending.ingest(WorkerCommand::Close { document_id: 1 });
        assert_eq!(
            pending.searches.iter().cloned().collect::<Vec<_>>(),
            vec![(2, 20, "other".to_string())]
        );
        assert_eq!(
            pending.link_indices.iter().cloned().collect::<Vec<_>>(),
            vec![(2, 200)]
        );
    }

    #[test]
    fn open_presentation_rejects_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an office file").unwrap();
        let err = match open_presentation(&path) {
            Ok(_) => panic!("expected unsupported format error"),
            Err(err) => err,
        };
        assert!(err.contains("unsupported document format"), "got: {err}");
    }

    #[test]
    fn link_indexing_matches_context_around_the_linked_occurrence_by_bbox() {
        let mut pres = Presentation::default();
        pres.slides.clear();
        let mut slide = Slide {
            id: SlideId(1),
            show_master_shapes: false,
            ..Default::default()
        };
        // First shape contains unlinked "here" in an intro sentence.
        slide.shapes.push(Shape {
            id: ShapeId(2),
            xfrm: Some(Xfrm::new(40.0, 40.0, 600.0, 50.0)),
            kind: ShapeKind::Shape,
            text_box: true,
            text: Some(TextBody {
                paragraphs: vec![Paragraph {
                    runs: vec![Run::new("Unlinked intro text appears here in the header.")],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        // Second shape further down has a linked run with label "here".
        let mut linked_run = Run::new("here");
        linked_run.props.link = Some(Hyperlink {
            action: Action::Url {
                url: "https://example.com/target".into(),
            },
            tooltip: String::new(),
            highlight_click: false,
            sound: None,
            stop_sound: false,
            zoom_return: None,
        });
        slide.shapes.push(Shape {
            id: ShapeId(3),
            xfrm: Some(Xfrm::new(40.0, 300.0, 600.0, 50.0)),
            kind: ShapeKind::Shape,
            text_box: true,
            text: Some(TextBody {
                paragraphs: vec![Paragraph {
                    runs: vec![
                        Run::new("Follow the actual portal link "),
                        linked_run,
                        Run::new(" for details."),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        pres.slides.push(slide.into());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("links.pptx");
        std::fs::write(&path, deckcraft_pptx::export(&pres).unwrap()).unwrap();

        let (mut doc, _, _, _) = open_presentation(&path).unwrap();
        let indexed = index_slide_links(&mut doc, 0);
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].label, "here");
        let ctx = indexed[0].source_context.as_deref().unwrap_or_default();
        assert!(
            ctx.contains("Follow the actual portal link here for details."),
            "unexpected link context: {ctx}"
        );
    }

    #[test]
    fn worker_delivers_all_visible_and_prefetched_slides_across_zoom_levels() {
        let mut pres = Presentation::default();
        pres.slides.clear();
        for idx in 0..3u32 {
            let mut slide = Slide {
                id: SlideId(idx + 1),
                ..Default::default()
            };
            slide.shapes.push(Shape {
                id: ShapeId(10 + idx),
                xfrm: Some(Xfrm::new(72.0, 54.0, 400.0, 80.0)),
                kind: ShapeKind::Shape,
                text_box: true,
                text: Some(TextBody {
                    paragraphs: vec![Paragraph {
                        runs: vec![Run::new(format!("Slide {}", idx + 1))],
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            });
            pres.slides.push(slide.into());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.pptx");
        std::fs::write(&path, deckcraft_pptx::export(&pres).unwrap()).unwrap();

        let (worker, pages, _, _) = PptxWorker::spawn(path).unwrap();
        assert_eq!(pages, 3);

        let key_for = |page: u32, zoom: u16| RenderKey {
            document_id: 1,
            page,
            width: 320,
            height: 240,
            zoom,
            fit: FitMode::Page,
            invert: false,
            dark_mode_style: DarkModeStyle::default(),
            search_request_id: 0,
            search_highlight: [0xff, 0xd7, 0x00],
            link_mode: false,
            link_highlight: [0x00, 0xd7, 0xff],
            selected_link_ordinal: None,
        };

        worker.begin_generation(1);
        worker
            .render(RenderRequest {
                key: key_for(0, 100),
                generation: 1,
            })
            .unwrap();
        worker
            .render(RenderRequest {
                key: key_for(1, 100),
                generation: 1,
            })
            .unwrap();
        worker
            .prefetch(RenderRequest {
                key: key_for(2, 100),
                generation: 1,
            })
            .unwrap();

        let mut received_gen1 = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while received_gen1.len() < 3 && Instant::now() < deadline {
            if let Some(WorkerMessage::Frame(frame)) = worker.try_recv()
                && frame.generation == 1
            {
                received_gen1.push(frame.key.page);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert_eq!(received_gen1, vec![0, 1, 2]);

        // Now zoom in to 125% (generation 2) with two visible slides requested back-to-back.
        worker.begin_generation(2);
        worker
            .render(RenderRequest {
                key: key_for(0, 125),
                generation: 2,
            })
            .unwrap();
        worker
            .render(RenderRequest {
                key: key_for(1, 125),
                generation: 2,
            })
            .unwrap();

        let mut received_gen2 = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while received_gen2.len() < 2 && Instant::now() < deadline {
            if let Some(WorkerMessage::Frame(frame)) = worker.try_recv()
                && frame.generation == 2
            {
                received_gen2.push((frame.key.page, frame.width, frame.height));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert_eq!(received_gen2, vec![(0, 400, 225), (1, 400, 225)]);
    }

    #[test]
    fn zoom_and_large_raster_sizes_stay_within_renderer_bounds_and_match_rgba_payload() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zoom.pptx");
        std::fs::write(&path, sample_pptx_bytes()).unwrap();
        let (mut doc, _, _, _) = open_presentation(&path).unwrap();

        for zoom in [100u16, 110, 125, 200, 400] {
            let key = RenderKey {
                document_id: 1,
                page: 0,
                width: 320,
                height: 180,
                zoom,
                fit: FitMode::Page,
                invert: true,
                dark_mode_style: DarkModeStyle::default(),
                search_request_id: 0,
                search_highlight: [0xff, 0xd7, 0x00],
                link_mode: false,
                link_highlight: [0x00, 0xd7, 0xff],
                selected_link_ordinal: None,
            };
            let frame = render_page(&mut doc, RenderRequest { key, generation: 1 })
                .unwrap()
                .unwrap();
            let target_width = (u32::from(key.width) * u32::from(key.zoom) / 100).max(1);
            let target_height = (u32::from(key.height) * u32::from(key.zoom) / 100).max(1);
            let expected = key.fit.raster_size(
                frame.page_width_pt,
                frame.page_height_pt,
                frame.content_bounds,
                target_width,
                target_height,
            );
            assert_eq!((frame.width, frame.height), expected);

            let mut decompressed = Vec::new();
            flate2::read::ZlibDecoder::new(frame.compressed_rgba.as_slice())
                .read_to_end(&mut decompressed)
                .unwrap();
            assert_eq!(
                decompressed.len(),
                (frame.width as usize) * (frame.height as usize) * 4
            );
        }

        // Verify that oversized dimensions exceeding deckcraft_render::MAX_SIDE are clamped
        // proportionally while preserving aspect ratio.
        assert_eq!(
            clamp_to_max_side(20_000, 11_250),
            (deckcraft_render::MAX_SIDE, 9_000)
        );
        assert_eq!(
            clamp_to_max_side(9_000, 20_000),
            (7_200, deckcraft_render::MAX_SIDE)
        );
    }

    #[test]
    fn opens_renders_and_searches_docx_and_xlsx_documents() {
        let dir = tempfile::tempdir().unwrap();

        // Word document (.docx)
        let docx_path = dir.path().join("report.docx");
        let mut word_doc = wordcraft_doc::Document::new();
        let mut heading =
            wordcraft_doc::Paragraph::with_text("Executive Summary", Default::default());
        heading.props.style = Some("Heading1".to_string());
        heading.props.outline_level = Some(0);
        let body = wordcraft_doc::Paragraph::with_text(
            "Revenue increased by 42% in Q3.",
            Default::default(),
        );
        word_doc.body = vec![
            wordcraft_doc::para_block(heading),
            wordcraft_doc::para_block(body),
        ];
        std::fs::write(&docx_path, wordcraft_docx::write(&word_doc).unwrap()).unwrap();

        let (mut wdoc, wpages, woutline, _) = open_presentation(&docx_path).unwrap();
        assert!(wpages >= 1);
        assert_eq!(woutline.len(), 1);
        assert_eq!(woutline[0].title, "Executive Summary");
        let wtext = extract_slide_text(&mut wdoc, 0);
        assert!(wtext.contains("Executive Summary"));
        assert!(wtext.contains("Revenue increased by 42%"));

        let key = RenderKey {
            document_id: 0,
            page: 0,
            width: 320,
            height: 400,
            zoom: 100,
            fit: FitMode::Page,
            invert: true,
            dark_mode_style: DarkModeStyle::default(),
            search_request_id: 0,
            search_highlight: [0xff, 0xd7, 0x00],
            link_mode: false,
            link_highlight: [0x00, 0xd7, 0xff],
            selected_link_ordinal: None,
        };
        let wframe = render_page(&mut wdoc, RenderRequest { key, generation: 1 })
            .unwrap()
            .unwrap();
        assert!(wframe.width > 0 && wframe.height > 0);
        assert!(!wframe.compressed_rgba.is_empty());

        // Excel workbook (.xlsx)
        let xlsx_path = dir.path().join("metrics.xlsx");
        let mut wb = gridcraft_engine::model::Workbook::new();
        {
            use gridcraft_engine::core::{CellRef, Value};
            let ws = Arc::make_mut(&mut wb.sheets[0]);
            ws.name = "Financials".to_string();
            ws.set_value(CellRef::new(0, 0), Value::from("Quarter"));
            ws.set_value(CellRef::new(0, 1), Value::from("Revenue"));
            ws.set_value(CellRef::new(1, 0), Value::from("Q3 2026"));
            ws.set_value(CellRef::new(1, 1), Value::Number(125000.0));
        }
        std::fs::write(
            &xlsx_path,
            gridcraft_engine::io::save_bytes(&wb, "metrics.xlsx").unwrap(),
        )
        .unwrap();

        let (mut edoc, epages, eoutline, _) = open_presentation(&xlsx_path).unwrap();
        assert!(epages >= 1);
        assert_eq!(eoutline.len(), 1);
        assert_eq!(eoutline[0].title, "Financials");
        let etext = extract_slide_text(&mut edoc, 0);
        assert!(etext.contains("Quarter"));
        assert!(etext.contains("Q3 2026"));

        let eframe = render_page(&mut edoc, RenderRequest { key, generation: 1 })
            .unwrap()
            .unwrap();
        assert!(eframe.width > 0 && eframe.height > 0);
        assert!(eframe.page_width_pt > eframe.page_height_pt);
        assert!(!eframe.compressed_rgba.is_empty());
    }

    #[test]
    fn excel_landscape_fit_width_trims_empty_cells_and_skips_blank_pages() {
        use gridcraft_engine::core::{CellRef, RangeRef, Value};
        use gridcraft_engine::model::{Cell, Color, Fill, Formula, PatternType, Style, StyleId};

        let dir = tempfile::tempdir().unwrap();
        let xlsx_path = dir.path().join("wide_and_sparse.xlsx");
        let mut wb = gridcraft_engine::model::Workbook::new();
        let style_id = wb.styles.intern(Style {
            fill: Fill {
                pattern: PatternType::Solid,
                fg: Color::Rgb(0xEEEEEE),
                bg: Color::Auto,
            },
            ..Style::default()
        });
        {
            let ws = Arc::make_mut(&mut wb.sheets[0]);
            ws.name = "WideSheet".to_string();
            // 14 columns wide (A..N), which would split across multiple pages in portrait mode.
            for col in 0..14 {
                ws.set_value(CellRef::new(0, col), Value::from(format!("Header {col}")));
                ws.set_value(CellRef::new(1, col), Value::Number(f64::from(col) * 10.0));
            }
            // Add styled-only empty cells and an empty formula far away; these should be ignored.
            ws.set_style(CellRef::new(400, 20), style_id);
            ws.set_cell(
                CellRef::new(450, 15),
                Cell {
                    value: Value::from(""),
                    formula: Some(Arc::new(Formula::new("\"\""))),
                    style: StyleId::DEFAULT,
                },
            );
            // Set an oversized print area; it should be ignored in favor of tight content bounds.
            ws.print.print_area =
                Some(RangeRef::new(CellRef::new(0, 0), CellRef::new(500, 25)));
        }
        std::fs::write(
            &xlsx_path,
            gridcraft_engine::io::save_bytes(&wb, "wide_and_sparse.xlsx").unwrap(),
        )
        .unwrap();

        let (mut edoc, epages, eoutline, _) = open_presentation(&xlsx_path).unwrap();
        assert_eq!(epages, 1);
        assert_eq!(eoutline.len(), 1);
        let etext = extract_slide_text(&mut edoc, 0);
        assert!(etext.contains("Header 0"));
        assert!(etext.contains("Header 13"));

        // Verify sparse vertical data skips intermediate blank page bands.
        let sparse_path = dir.path().join("sparse_rows.xlsx");
        let mut wb2 = gridcraft_engine::model::Workbook::new();
        {
            let ws = Arc::make_mut(&mut wb2.sheets[0]);
            ws.name = "Sparse".to_string();
            ws.set_value(CellRef::new(0, 0), Value::from("Top block"));
            ws.set_value(CellRef::new(200, 0), Value::from("Bottom block"));
        }
        std::fs::write(
            &sparse_path,
            gridcraft_engine::io::save_bytes(&wb2, "sparse_rows.xlsx").unwrap(),
        )
        .unwrap();

        let (mut sdoc, spages, _, _) = open_presentation(&sparse_path).unwrap();
        assert_eq!(spages, 2);
        assert!(extract_slide_text(&mut sdoc, 0).contains("Top block"));
        assert!(extract_slide_text(&mut sdoc, 1).contains("Bottom block"));
    }

    #[cfg(unix)]
    #[test]
    fn macos_font_home_redirects_office_cloud_fonts_to_officeterm_cache() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let user_fonts = home.join("Library/Fonts");
        std::fs::create_dir_all(&user_fonts).unwrap();
        let cache_base = dir.path().join("cache");

        let font_home = prepare_macos_font_home(&home, &cache_base).unwrap();
        // Calling a second time is idempotent.
        let font_home_again = prepare_macos_font_home(&home, &cache_base).unwrap();
        assert_eq!(font_home, font_home_again);

        let fonts_link = font_home.join("Library/Fonts");
        assert_eq!(std::fs::read_link(&fonts_link).unwrap(), user_fonts);

        let cloud_fonts_link =
            font_home.join("Library/Group Containers/UBF8T346G9.Office/FontCache/4/CloudFonts");
        let expected_cache = cache_base.join("officeterm/fonts");
        assert_eq!(
            std::fs::read_link(&cloud_fonts_link).unwrap(),
            expected_cache
        );
        assert!(expected_cache.is_dir());
        assert!(!home.join("Library/Group Containers").exists());
    }
}
