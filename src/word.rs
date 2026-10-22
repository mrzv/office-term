use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::Path;

use kurbo::{Affine as KurboAffine, Point as KurboPoint, Rect as KurboRect, Shape, Vec2};
use wordcraft_doc::{Document, Pos, StoryRef};
use wordcraft_layout::{
    DocLayout, LayoutCache, LayoutOptions, Placed, ViewMode,
    display::{DisplayOptions, Draw, page_display},
    outline::para_level,
};
use wordcraft_render::{RenderOptions, render_region};

use crate::pptx::{
    CachedSlideText, LinkTarget, OutlineItem, PageLink, bbox_to_page_link_rect, build_cached_text,
    default_link_label, include_rect_bounds, normalize_context_text, raster_mask_from_rects,
    transform_rect_bbox, truncate_chars,
};

pub(crate) struct WordDocument {
    doc: Document,
    layout: DocLayout,
    bookmarks: HashMap<String, Pos>,
    page_meta: Vec<OnceCell<WordPageMeta>>,
}

struct WordPageMeta {
    content_bounds: [f32; 4],
    image_rects: Vec<KurboRect>,
    links: Vec<(KurboRect, String, LinkTarget)>,
}

impl WordDocument {
    pub(crate) fn load(
        path: &Path,
        bytes: &[u8],
        ext: &str,
    ) -> Result<(Self, u32, Vec<OutlineItem>), String> {
        if wordcraft_docx::is_encrypted(bytes) || wordcraft_docbin::is_encrypted(bytes) {
            return Err(format!(
                "{} is password-protected / encrypted",
                path.display()
            ));
        }
        let mut doc = match ext {
            "docx" | "docm" | "dotx" | "dotm" => wordcraft_docx::read(bytes)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?,
            "doc" | "dot" => {
                if bytes.starts_with(b"{\\rtf") {
                    wordcraft_formats::import("rtf", bytes)
                        .ok_or_else(|| format!("unsupported format for {}", path.display()))?
                        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
                } else if bytes.starts_with(b"PK\x03\x04") {
                    wordcraft_docx::read(bytes)
                        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
                } else {
                    wordcraft_docbin::read(bytes)
                        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
                }
            }
            "rtf" | "odt" => wordcraft_formats::import(ext, bytes)
                .ok_or_else(|| format!("unsupported format: {ext}"))?
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?,
            _ => {
                return Err(format!("unsupported Word format: {}", path.display()));
            }
        };
        doc.ensure_nonempty();
        install_embedded_fonts(&mut doc);

        let mut cache = LayoutCache::new();
        let opts = LayoutOptions {
            view: ViewMode::Print,
            hide_deleted: true,
            proofing: false,
            ..Default::default()
        };
        let layout = wordcraft_layout::layout(&doc, &mut cache, &opts);
        if layout.pages.is_empty() {
            return Err(format!("{} has no pages", path.display()));
        }
        let pages = u32::try_from(layout.pages.len())
            .map_err(|_| "document has too many pages".to_string())?;
        let outline = collect_outline(&doc, &layout);
        let bookmarks = doc.bookmarks().into_iter().collect();

        Ok((
            Self {
                doc,
                layout,
                bookmarks,
                page_meta: (0..pages).map(|_| OnceCell::new()).collect(),
            },
            pages,
            outline,
        ))
    }

    pub(crate) fn page_count(&self) -> usize {
        self.layout.pages.len()
    }

    pub(crate) fn page_size(&self, page: usize) -> (f64, f64) {
        self.layout
            .pages
            .get(page)
            .map(|p| (f64::from(p.w.max(1.0)), f64::from(p.h.max(1.0))))
            .unwrap_or((612.0, 792.0))
    }

    pub(crate) fn content_bounds(&self, page: usize) -> [f32; 4] {
        self.ensure_page_meta(page)
            .map(|meta| meta.content_bounds)
            .unwrap_or([0.0, 0.0, 1.0, 1.0])
    }

    pub(crate) fn picture_mask(
        &self,
        page: usize,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Option<Vec<u8>> {
        let meta = self.ensure_page_meta(page)?;
        raster_mask_from_rects(&meta.image_rects, width, height, scale)
    }

    pub(crate) fn render_page_rgba(
        &self,
        page: usize,
        width: u32,
        height: u32,
    ) -> (u32, u32, f64, Vec<u8>) {
        let Some(p) = self.layout.pages.get(page) else {
            return (1, 1, 1.0, vec![255, 255, 255, 255]);
        };
        let (width, height, scale) = raster_geometry(
            f64::from(p.w.max(1.0)),
            f64::from(p.h.max(1.0)),
            width,
            height,
        );
        let pixmap = render_region(
            &self.doc,
            p,
            width,
            height,
            vello_cpu::kurbo::Affine::scale(scale),
            &RenderOptions::default(),
        );
        let w = pixmap.width.max(1);
        let h = pixmap.height.max(1);
        (w, h, scale, pixmap.to_straight())
    }

    pub(crate) fn extract_page_links(
        &self,
        page: usize,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Vec<PageLink> {
        let Some(meta) = self.ensure_page_meta(page) else {
            return Vec::new();
        };
        meta.links
            .iter()
            .filter_map(|(bbox, label, target)| {
                let rect = bbox_to_page_link_rect(*bbox, width, height, scale)?;
                Some(PageLink {
                    rect,
                    label: label.clone(),
                    target: target.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn extract_cached_text(&self, page: usize) -> CachedSlideText {
        let Some(p) = self.layout.pages.get(page) else {
            return build_cached_text(String::new(), Vec::new());
        };
        let draws = page_display(&self.doc, p, &DisplayOptions::default());
        let mut raw = String::new();
        let mut char_rects = Vec::new();
        let mut previous = None;
        collect_draws_text(
            &draws,
            KurboAffine::IDENTITY,
            None,
            &mut raw,
            &mut char_rects,
            &mut previous,
        );
        build_cached_text(raw, char_rects)
    }

    fn ensure_page_meta(&self, page: usize) -> Option<&WordPageMeta> {
        let cell = self.page_meta.get(page)?;
        let p = self.layout.pages.get(page)?;
        Some(cell.get_or_init(|| {
            let pw = p.w.max(1.0);
            let ph = p.h.max(1.0);
            let draws = page_display(&self.doc, p, &DisplayOptions::default());
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut image_rects = Vec::new();
            let mut raw_links: Vec<(KurboRect, String, String)> = Vec::new();

            collect_draws_meta(
                &draws,
                KurboAffine::IDENTITY,
                None,
                pw,
                ph,
                &mut min_x,
                &mut min_y,
                &mut max_x,
                &mut max_y,
                &mut image_rects,
                &mut raw_links,
            );

            let content_bounds = if min_x.is_finite()
                && min_y.is_finite()
                && max_x > min_x + 1.0
                && max_y > min_y + 1.0
            {
                let pad_x = (pw * 0.02).max(4.0);
                let pad_y = (ph * 0.02).max(4.0);
                [
                    ((min_x - pad_x).clamp(0.0, pw) / pw).clamp(0.0, 1.0),
                    ((min_y - pad_y).clamp(0.0, ph) / ph).clamp(0.0, 1.0),
                    ((max_x + pad_x).clamp(0.0, pw) / pw).clamp(0.0, 1.0),
                    ((max_y + pad_y).clamp(0.0, ph) / ph).clamp(0.0, 1.0),
                ]
            } else {
                [0.0, 0.0, 1.0, 1.0]
            };

            let links = raw_links
                .into_iter()
                .filter_map(|(bbox, label, url)| {
                    let target = self.resolve_link(&url)?;
                    let label = normalize_context_text(&label);
                    let label = if label.is_empty() {
                        default_link_label(&target)
                    } else {
                        label
                    };
                    Some((bbox, label, target))
                })
                .collect();

            WordPageMeta {
                content_bounds,
                image_rects,
                links,
            }
        }))
    }

    fn resolve_link(&self, url: &str) -> Option<LinkTarget> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(anchor) = trimmed.strip_prefix('#') {
            let anchor = anchor.trim();
            if anchor.is_empty() {
                return None;
            }
            let pos = self.bookmarks.get(anchor).or_else(|| {
                self.bookmarks
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(anchor))
                    .map(|(_, v)| v)
            })?;
            let caret = self.layout.caret(pos)?;
            let page = u32::try_from(caret.page).ok()?;
            let target_page = self.layout.pages.get(caret.page)?;
            let top_ratio = Some((caret.top / target_page.h.max(1.0)).clamp(0.0, 1.0));
            let left_ratio = Some((caret.x / target_page.w.max(1.0)).clamp(0.0, 1.0));
            Some(LinkTarget::Internal {
                page,
                top_ratio,
                left_ratio,
            })
        } else {
            Some(LinkTarget::Uri(trimmed.to_string()))
        }
    }
}

fn raster_geometry(page_w: f64, page_h: f64, width: u32, height: u32) -> (u32, u32, f64) {
    // Fit before rendering so upstream never applies its independently rounded x/y rescale.
    // A single uniform scale then describes both the raster and every overlay exactly.
    let (width, height) = wordcraft_geom::fit_pixels(
        width.clamp(1, wordcraft_render::MAX_SIDE),
        height.clamp(1, wordcraft_render::MAX_SIDE),
        wordcraft_geom::MAX_RASTER_PIXELS,
    );
    let scale = (f64::from(width) / page_w).min(f64::from(height) / page_h);
    (width, height, scale)
}

fn install_embedded_fonts(doc: &mut Document) {
    let files: Vec<(String, Vec<u8>)> = wordcraft_docx::embedded_fonts(doc)
        .into_iter()
        .filter_map(|font| {
            wordcraft_fonts::decode_font_file(&font.data)
                .ok()
                .map(|data| (font.name, data))
        })
        .collect();
    if !files.is_empty() {
        remap_document_fonts(doc, &register_document_fonts(files));
    }
}

type FontAliases = HashMap<String, String>;

fn register_document_fonts(files: Vec<(String, Vec<u8>)>) -> FontAliases {
    // Reopening reuses faces. Equality includes actual subset bytes, not just the family or a
    // digest: disjoint subsets must never share a registration.
    type Registrations = HashMap<Vec<(String, Vec<u8>)>, FontAliases>;
    static FONTS: std::sync::LazyLock<std::sync::Mutex<Registrations>> =
        std::sync::LazyLock::new(Default::default);
    let mut fonts = FONTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(aliases) = fonts.get(&files) {
        return aliases.clone();
    }
    let mut aliases = FontAliases::new();
    for (name, _) in &files {
        let next = aliases.len();
        aliases
            .entry(name.trim().to_lowercase())
            .or_insert_with(|| {
                let mut alias = format!("office-term-doc-{}-font-{next}", fonts.len());
                if wordcraft_doc::symbol_fonts::is_symbol_font(name) {
                    alias.push('@');
                    alias.push_str(name.trim());
                }
                alias
            });
    }
    // The supported document alias replaces typographic and legacy families. Never register
    // the original family globally: that would let the first document's subset win.
    wordcraft_fonts::FontDb::global().add_document_fonts(
        files
            .iter()
            .map(|(name, bytes)| (bytes.clone(), aliases[&name.trim().to_lowercase()].clone()))
            .collect(),
    );
    fonts.insert(files, aliases.clone());
    aliases
}

fn remap_font(font: &mut String, aliases: &FontAliases) {
    if let Some(alias) = aliases.get(&font.trim().to_lowercase()) {
        font.clone_from(alias);
    }
}

fn remap_optional_font(font: &mut Option<String>, aliases: &FontAliases) {
    if let Some(font) = font {
        remap_font(font, aliases);
    }
}

fn remap_char_fonts(props: &mut wordcraft_doc::CharProps, aliases: &FontAliases) {
    for font in [
        &mut props.font,
        &mut props.sym_font,
        &mut props.font_cs,
        &mut props.font_ea,
    ] {
        remap_optional_font(font, aliases);
    }
    if let Some(change) = &mut props.fmt_change {
        remap_char_fonts(&mut change.old, aliases);
    }
}

fn remap_para_fonts(props: &mut wordcraft_doc::ParaProps, aliases: &FontAliases) {
    remap_optional_font(&mut props.drop_cap_font, aliases);
    if let Some(change) = &mut props.fmt_change {
        remap_para_fonts(&mut change.old, aliases);
    }
}

fn remap_object_fonts(object: &mut wordcraft_doc::InlineObject, aliases: &FontAliases) {
    use wordcraft_doc::InlineObject;
    match object {
        InlineObject::Equation { math, .. } => {
            wordcraft_doc::math_edit::each_run_mut(&mut math.nodes, &mut |run| {
                remap_optional_font(&mut run.font, aliases);
            });
        }
        InlineObject::Ruby { props, .. } => remap_char_fonts(props, aliases),
        InlineObject::Graphic { graphic, .. } => {
            for item in &mut std::sync::Arc::make_mut(graphic).items {
                if let wordcraft_doc::graphic::GraphicItem::Text { font, .. } = item {
                    remap_optional_font(font, aliases);
                }
            }
        }
        InlineObject::Group { children, .. } => {
            for child in children {
                remap_object_fonts(&mut child.obj, aliases);
            }
        }
        _ => {}
    }
}

fn remap_blocks_fonts(blocks: &mut wordcraft_doc::Blocks, aliases: &FontAliases) {
    for block in blocks {
        match std::sync::Arc::make_mut(block) {
            wordcraft_doc::Block::Para(para) => {
                remap_para_fonts(&mut para.props, aliases);
                remap_char_fonts(&mut para.mark, aliases);
                for run in &mut para.runs {
                    remap_char_fonts(&mut run.props, aliases);
                }
                for object in &mut para.objects {
                    remap_object_fonts(object, aliases);
                }
            }
            wordcraft_doc::Block::Table(table) => {
                for row in &mut table.rows {
                    for cell in &mut row.cells {
                        remap_blocks_fonts(&mut cell.blocks, aliases);
                    }
                }
            }
        }
    }
}

fn remap_document_fonts(doc: &mut Document, aliases: &FontAliases) {
    remap_blocks_fonts(&mut doc.body, aliases);
    for part in doc.parts.values_mut() {
        remap_blocks_fonts(&mut part.blocks, aliases);
    }
    for separators in [&mut doc.footnote_separators, &mut doc.endnote_separators] {
        for blocks in [
            &mut separators.separator,
            &mut separators.continuation_separator,
            &mut separators.continuation_notice,
        ]
        .into_iter()
        .flatten()
        {
            remap_blocks_fonts(blocks, aliases);
        }
    }
    for block in &mut doc.glossary {
        remap_blocks_fonts(&mut block.blocks, aliases);
    }
    remap_char_fonts(&mut doc.styles.default_chr, aliases);
    remap_para_fonts(&mut doc.styles.default_para, aliases);
    for style in &mut doc.styles.styles {
        remap_char_fonts(&mut style.chr, aliases);
        remap_para_fonts(&mut style.para, aliases);
        if let Some(table) = &mut style.table {
            for props in [
                &mut table.header_chr,
                &mut table.band_chr,
                &mut table.first_col_chr,
                &mut table.total_chr,
                &mut table.last_col_chr,
                &mut table.col_band_chr,
            ] {
                remap_char_fonts(props, aliases);
            }
            for region in table.regions.values_mut() {
                remap_char_fonts(&mut region.chr, aliases);
            }
        }
    }
    for level in doc
        .numbering
        .abstracts
        .iter_mut()
        .flat_map(|n| &mut n.levels)
        .chain(
            doc.numbering
                .nums
                .iter_mut()
                .flat_map(|n| n.level_overrides.iter_mut().map(|(_, l)| l)),
        )
    {
        remap_char_fonts(&mut level.chr, aliases);
    }
    remap_font(&mut doc.settings.major_font, aliases);
    remap_font(&mut doc.settings.minor_font, aliases);
    for font in &mut doc.settings.roman_fonts {
        remap_font(font, aliases);
    }
    if let Some(watermark) = &mut doc.settings.watermark {
        remap_font(&mut watermark.font, aliases);
    }
}

fn collect_outline(doc: &Document, layout: &DocLayout) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    for (page_idx, page) in layout.pages.iter().enumerate() {
        let Ok(page_num) = u32::try_from(page_idx) else {
            break;
        };
        for item in &page.items {
            if let Placed::Lines {
                story: StoryRef::Body,
                path,
                l0,
                ..
            } = item
                && *l0 == 0
                && let Some(para) = doc.para(StoryRef::Body, path)
            {
                let level = para_level(doc, para);
                if (1..=9).contains(&level) {
                    let title = normalize_context_text(&para.shown_text());
                    if !title.is_empty() {
                        items.push(OutlineItem {
                            title: truncate_chars(&title, 80),
                            page: page_num,
                            depth: u16::from(level - 1),
                        });
                    }
                }
            }
        }
    }
    if items.is_empty() {
        for page_idx in 0..layout.pages.len() {
            let Ok(page_num) = u32::try_from(page_idx) else {
                break;
            };
            items.push(OutlineItem {
                title: format!("Page {}", page_idx + 1),
                page: page_num,
                depth: 0,
            });
        }
    }
    items
}

fn affine_from_matrix([a, b, c, d, e, f]: [f32; 6]) -> KurboAffine {
    KurboAffine::new([
        f64::from(a),
        f64::from(b),
        f64::from(c),
        f64::from(d),
        f64::from(e),
        f64::from(f),
    ])
}

fn wc_rect_to_kurbo(r: wordcraft_geom::Rect) -> KurboRect {
    KurboRect::new(
        f64::from(r.x),
        f64::from(r.y),
        f64::from(r.right()),
        f64::from(r.bottom()),
    )
}

fn glyph_advance_pt(face: &wordcraft_fonts::FaceRef, gid: u32, size_pt: f64) -> f64 {
    let upem = face.units_per_em().max(1.0);
    ((face.advance(gid) / upem) * size_pt).max(size_pt * 0.25)
}

#[allow(clippy::too_many_arguments)]
fn collect_draws_meta(
    draws: &[Draw],
    transform: KurboAffine,
    clip: Option<&DrawClip<'_>>,
    page_w: f32,
    page_h: f32,
    min_x: &mut f32,
    min_y: &mut f32,
    max_x: &mut f32,
    max_y: &mut f32,
    image_rects: &mut Vec<KurboRect>,
    links: &mut Vec<(KurboRect, String, String)>,
) {
    for draw in draws {
        match draw {
            Draw::Glyphs {
                face,
                size,
                glyphs,
                text,
                link,
                ..
            } => {
                if let Some(bbox) = glyphs_bbox(face, *size, glyphs, &transform, clip) {
                    include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
                    if let Some(url) = link.as_deref()
                        && !url.trim().is_empty()
                    {
                        if let Some((last_bbox, last_text, last_url)) = links.last_mut()
                            && last_url == url
                            && (last_bbox.y0 - bbox.y0).abs() <= f64::from((*size).max(4.0))
                            && (bbox.x0 - last_bbox.x1).abs() <= f64::from((*size).max(8.0))
                        {
                            *last_bbox = last_bbox.union(bbox);
                            last_text.push_str(text);
                        } else {
                            links.push((bbox, text.clone(), url.to_string()));
                        }
                    }
                }
            }
            Draw::Fill { rect, .. } | Draw::Shape { rect, .. } => {
                let bbox = transform_rect_bbox(&transform, wc_rect_to_kurbo(*rect));
                let Some(bbox) = clipped_rect(bbox, clip) else {
                    continue;
                };
                include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
            }
            Draw::Image { rect, .. } => {
                let bbox = transform_rect_bbox(&transform, wc_rect_to_kurbo(*rect));
                let Some(bbox) = clipped_rect(bbox, clip) else {
                    continue;
                };
                include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
                if bbox.width() > 0.0 && bbox.height() > 0.0 {
                    image_rects.push(bbox);
                }
            }
            Draw::Line {
                x0,
                y0,
                x1,
                y1,
                width,
                ..
            } => {
                let half = f64::from((*width).max(1.0)) * 0.5;
                let rx0 = f64::from(x0.min(*x1)) - half;
                let ry0 = f64::from(y0.min(*y1)) - half;
                let rx1 = f64::from(x0.max(*x1)) + half;
                let ry1 = f64::from(y0.max(*y1)) + half;
                let bbox = transform_rect_bbox(&transform, KurboRect::new(rx0, ry0, rx1, ry1));
                let Some(bbox) = clipped_rect(bbox, clip) else {
                    continue;
                };
                include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
            }
            Draw::Path {
                segs,
                fill,
                stroke,
                stroke_width,
                ..
            } => {
                if fill.is_none() && stroke.is_none() {
                    continue;
                }
                let path = wordcraft_layout::display::seg_path(segs);
                if path.elements().is_empty() {
                    continue;
                }
                let mut bbox = (transform * path).bounding_box();
                if stroke.is_some() {
                    // Match the renderer's round caps/joins and hairline width.
                    let width = if stroke_width.is_finite() && *stroke_width > 0.0 {
                        stroke_width.clamp(0.25, 200.0)
                    } else {
                        0.75
                    };
                    bbox = inflate_stroke_bounds(bbox, transform, width);
                }
                if let Some(bbox) = clipped_rect(bbox, clip) {
                    include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
                }
            }
            Draw::Ink {
                pts, width, alpha, ..
            } => {
                if *alpha <= 0.0 {
                    continue;
                }
                // Keep exactly the usable points accepted by render::ink_path, including taps.
                let mut points = pts
                    .iter()
                    .filter(|(x, y)| {
                        x.is_finite() && y.is_finite() && x.abs() < 1e6 && y.abs() < 1e6
                    })
                    .map(|&(x, y)| transform * KurboPoint::new(f64::from(x), f64::from(y)));
                let Some(first) = points.next() else {
                    continue;
                };
                let bbox = points.fold(
                    KurboRect::new(first.x, first.y, first.x, first.y),
                    |bbox, point| bbox.union_pt(point),
                );
                let width = if width.is_finite() {
                    width.clamp(0.25, 200.0)
                } else {
                    1.0
                };
                let bbox = inflate_stroke_bounds(bbox, transform, width);
                if let Some(bbox) = clipped_rect(bbox, clip) {
                    include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
                }
            }
            Draw::Clip { rect, items } => {
                let clip = DrawClip::new(wc_rect_to_kurbo(*rect), transform, clip);
                collect_draws_meta(
                    items,
                    transform,
                    Some(&clip),
                    page_w,
                    page_h,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                    image_rects,
                    links,
                );
            }
            Draw::Figure { draws: inner, .. } => {
                collect_draws_meta(
                    inner,
                    transform,
                    clip,
                    page_w,
                    page_h,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                    image_rects,
                    links,
                );
            }
            Draw::Turned {
                x,
                y,
                turn,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(Draw::turn_matrix(*turn, *x, *y));
                collect_draws_meta(
                    inner,
                    m,
                    clip,
                    page_w,
                    page_h,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                    image_rects,
                    links,
                );
            }
            Draw::Rotated {
                cx,
                cy,
                spin,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(spin.matrix(*cx, *cy));
                collect_draws_meta(
                    inner,
                    m,
                    clip,
                    page_w,
                    page_h,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                    image_rects,
                    links,
                );
            }
            _ => {}
        }
    }
}

fn inflate_stroke_bounds(rect: KurboRect, transform: KurboAffine, width: f32) -> KurboRect {
    let [a, b, c, d, _, _] = transform.as_coeffs();
    let half = f64::from(width) * 0.5;
    rect.inflate(half * a.hypot(c), half * b.hypot(d))
}

fn glyphs_bbox(
    face: &wordcraft_fonts::FaceRef,
    size: f32,
    glyphs: &[(u32, f32, f32)],
    transform: &KurboAffine,
    clip: Option<&DrawClip<'_>>,
) -> Option<KurboRect> {
    if glyphs.is_empty() {
        return None;
    }
    let sz = f64::from(size.max(1.0));
    let mut union: Option<KurboRect> = None;
    for (idx, &(gid, gx, gy)) in glyphs.iter().enumerate() {
        let adv = glyphs
            .get(idx + 1)
            .map(|&(_, nx, ny)| {
                let dx = f64::from(nx - gx);
                let dy = f64::from(ny - gy);
                if dy.abs() < sz * 0.5 && dx > 0.0 && dx < sz * 4.0 {
                    dx
                } else {
                    glyph_advance_pt(face, gid, sz)
                }
            })
            .unwrap_or_else(|| glyph_advance_pt(face, gid, sz));
        let x0 = f64::from(gx);
        let y0 = f64::from(gy) - sz * 0.85;
        let x1 = x0 + adv.max(1.0);
        let y1 = f64::from(gy) + sz * 0.25;
        let r = transform_rect_bbox(transform, KurboRect::new(x0, y0, x1, y1));
        let Some(r) = clipped_rect(r, clip) else {
            continue;
        };
        union = Some(match union {
            Some(prev) => prev.union(r),
            None => r,
        });
    }
    union
}

struct DrawClip<'a> {
    corners: [KurboPoint; 4],
    bounds: KurboRect,
    axis_aligned: bool,
    parent: Option<&'a DrawClip<'a>>,
}

impl<'a> DrawClip<'a> {
    fn new(rect: KurboRect, transform: KurboAffine, parent: Option<&'a Self>) -> Self {
        let corners = [
            (rect.x0, rect.y0),
            (rect.x1, rect.y0),
            (rect.x1, rect.y1),
            (rect.x0, rect.y1),
        ]
        .map(|point| transform * KurboPoint::from(point));
        let [a, b, c, d, _, _] = transform.as_coeffs();
        Self {
            corners,
            bounds: transform_rect_bbox(&transform, rect),
            axis_aligned: (b == 0.0 && c == 0.0) || (a == 0.0 && d == 0.0),
            parent,
        }
    }
}

fn clipped_rect(mut rect: KurboRect, clip: Option<&DrawClip<'_>>) -> Option<KurboRect> {
    let mut current = clip;
    let mut angled = false;
    while let Some(clip) = current {
        rect = rect.intersect(clip.bounds);
        angled |= !clip.axis_aligned;
        current = clip.parent;
    }
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    if !angled {
        return Some(rect);
    }
    // Keep rotated clip edges, not just their bounding boxes: otherwise invisible corner
    // content still becomes a link, search highlight, or image mask.
    let mut polygon = vec![
        KurboPoint::new(rect.x0, rect.y0),
        KurboPoint::new(rect.x1, rect.y0),
        KurboPoint::new(rect.x1, rect.y1),
        KurboPoint::new(rect.x0, rect.y1),
    ];
    let mut output = Vec::with_capacity(8);
    current = clip;
    while let Some(clip) = current {
        let corners = clip.corners;
        let orientation = (corners[1] - corners[0])
            .cross(corners[2] - corners[1])
            .signum();
        for edge in 0..4 {
            let a = corners[edge];
            let direction = corners[(edge + 1) % 4] - a;
            let mut previous = *polygon.last()?;
            let mut previous_distance = direction.cross(previous - a) * orientation;
            output.clear();
            for &point in &polygon {
                let distance = direction.cross(point - a) * orientation;
                if (distance >= 0.0) != (previous_distance >= 0.0) {
                    let t = previous_distance / (previous_distance - distance);
                    output.push(previous + (point - previous) * t);
                }
                if distance >= 0.0 {
                    output.push(point);
                }
                previous = point;
                previous_distance = distance;
            }
            std::mem::swap(&mut polygon, &mut output);
        }
        current = clip.parent;
    }
    let area = polygon
        .iter()
        .zip(polygon.iter().cycle().skip(1))
        .map(|(a, b)| a.x * b.y - a.y * b.x)
        .sum::<f64>()
        .abs();
    if area <= f64::EPSILON {
        return None;
    }
    let first = *polygon.first()?;
    let bounds = polygon.iter().skip(1).fold(
        KurboRect::new(first.x, first.y, first.x, first.y),
        |bounds, point| bounds.union_pt(*point),
    );
    (bounds.width() > 0.0 && bounds.height() > 0.0).then_some(bounds)
}

struct TextRunGeometry {
    start: KurboPoint,
    min_advance: f64,
    max_advance: f64,
    direction: Vec2,
}

fn collect_draws_text(
    draws: &[Draw],
    transform: KurboAffine,
    clip: Option<&DrawClip<'_>>,
    raw: &mut String,
    char_rects: &mut Vec<Option<KurboRect>>,
    previous: &mut Option<TextRunGeometry>,
) {
    for draw in draws {
        match draw {
            Draw::Glyphs {
                face,
                size,
                glyphs,
                text,
                ranges,
                ..
            } => {
                if text.is_empty() {
                    continue;
                }
                let sz = f64::from((*size).max(1.0));
                // Compute a rect per glyph, then map characters via `ranges`.
                let mut glyph_rects = Vec::with_capacity(glyphs.len());
                let mut geometry: Option<TextRunGeometry> = None;
                let [a, b, c, d, _, _] = transform.as_coeffs();
                let direction = Vec2::new(a, b).normalize();
                let text_size = sz * c.hypot(d);
                for (idx, &(gid, gx, gy)) in glyphs.iter().enumerate() {
                    let adv = glyphs
                        .get(idx + 1)
                        .map(|&(_, nx, ny)| {
                            let dx = f64::from(nx - gx);
                            let dy = f64::from(ny - gy);
                            if dy.abs() < sz * 0.5 && dx > 0.0 && dx < sz * 4.0 {
                                dx
                            } else {
                                glyph_advance_pt(face, gid, sz)
                            }
                        })
                        .unwrap_or_else(|| glyph_advance_pt(face, gid, sz));
                    let x0 = f64::from(gx);
                    let y0 = f64::from(gy) - sz * 0.85;
                    let x1 = x0 + adv.max(1.0);
                    let y1 = f64::from(gy) + sz * 0.25;
                    let r = transform_rect_bbox(&transform, KurboRect::new(x0, y0, x1, y1));
                    let rect = clipped_rect(r, clip);
                    if rect.is_some() {
                        let start = transform * KurboPoint::new(x0, f64::from(gy));
                        let end = transform * KurboPoint::new(x1, f64::from(gy));
                        let left = start.to_vec2().dot(direction);
                        let right = end.to_vec2().dot(direction);
                        let geometry = geometry.get_or_insert(TextRunGeometry {
                            start,
                            min_advance: left,
                            max_advance: right,
                            direction,
                        });
                        // Logical glyph order can run backwards inside a bidi run.
                        geometry.min_advance = geometry.min_advance.min(left).min(right);
                        geometry.max_advance = geometry.max_advance.max(left).max(right);
                    }
                    glyph_rects.push(rect);
                }
                let Some(run_bbox) = glyph_rects
                    .iter()
                    .flatten()
                    .copied()
                    .reduce(|a, b| a.union(b))
                else {
                    continue;
                };
                let Some(geometry) = geometry else {
                    continue;
                };
                if let Some(prev) = previous.as_ref() {
                    // Font size and clipping change a glyph's top, not its baseline.
                    if (geometry.start - prev.start).cross(prev.direction).abs() > text_size * 0.45
                        || geometry.direction.dot(prev.direction) < 0.99
                    {
                        if !raw.ends_with('\n') && !raw.is_empty() {
                            raw.push('\n');
                            char_rects.push(None);
                        }
                    } else if geometry.min_advance - prev.max_advance > text_size * 0.22
                        && !raw.ends_with(char::is_whitespace)
                        && !text.starts_with(char::is_whitespace)
                    {
                        raw.push(' ');
                        char_rects.push(None);
                    }
                }
                *previous = Some(geometry);
                for (byte_offset, ch) in text.char_indices() {
                    // Keep hidden characters as separators, not searchable/highlightable text.
                    let rect = if ranges.len() == glyph_rects.len() && !ranges.is_empty() {
                        ranges
                            .iter()
                            .position(|range| range.contains(&byte_offset))
                            .and_then(|g_idx| glyph_rects.get(g_idx).copied().flatten())
                    } else {
                        Some(run_bbox)
                    };
                    raw.push(if rect.is_some() { ch } else { ' ' });
                    char_rects.push(rect);
                }
            }
            Draw::Clip { rect, items } => {
                let clip = DrawClip::new(wc_rect_to_kurbo(*rect), transform, clip);
                collect_draws_text(
                    items,
                    transform,
                    Some(&clip),
                    raw,
                    char_rects,
                    previous,
                );
            }
            Draw::Figure { draws: inner, .. } => {
                collect_draws_text(inner, transform, clip, raw, char_rects, previous);
            }
            Draw::Turned {
                x,
                y,
                turn,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(Draw::turn_matrix(*turn, *x, *y));
                collect_draws_text(inner, m, clip, raw, char_rects, previous);
            }
            Draw::Rotated {
                cx,
                cy,
                spin,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(spin.matrix(*cx, *cy));
                collect_draws_text(inner, m, clip, raw, char_rects, previous);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{Block, CharProps, Paragraph, props::Rgb};
    use wordcraft_geom::Rect;

    fn document(doc: Document) -> WordDocument {
        let layout = wordcraft_layout::layout(
            &doc,
            &mut LayoutCache::new(),
            &LayoutOptions {
                view: ViewMode::Print,
                hide_deleted: true,
                proofing: false,
                ..Default::default()
            },
        );
        let page_meta = (0..layout.pages.len()).map(|_| OnceCell::new()).collect();
        WordDocument {
            doc,
            layout,
            bookmarks: HashMap::new(),
            page_meta,
        }
    }

    #[test]
    fn capped_raster_geometry_keeps_one_overlay_scale() {
        for (pw, ph, width, height) in [
            (612.0, 792.0, 16_000, 16_000),
            (997.0, 991.0, 15_999, 15_997),
            (612.0, 792.0, u32::MAX, 12_345),
        ] {
            let (w, h, scale) = raster_geometry(pw, ph, width, height);
            assert!(u64::from(w) * u64::from(h) <= wordcraft_geom::MAX_RASTER_PIXELS);
            assert_eq!(
                wordcraft_geom::fit_pixels(w, h, wordcraft_geom::MAX_RASTER_PIXELS),
                (w, h),
            );
            assert_eq!(scale, (f64::from(w) / pw).min(f64::from(h) / ph));
            assert!(pw * scale <= f64::from(w) + 1e-9);
            assert!(ph * scale <= f64::from(h) + 1e-9);
            assert!(scale < (f64::from(width) / pw).min(f64::from(height) / ph));
        }
    }

    fn glyphs(text: &str, positions: &[(f32, f32)]) -> Draw {
        let face = wordcraft_fonts::FontDb::global().face("Source Sans 3", "Regular");
        Draw::Glyphs {
            face: wordcraft_fonts::FaceRef::of(&face),
            size: 12.0,
            glyphs: text
                .chars()
                .zip(positions)
                .map(|(ch, &(x, y))| (face.glyph_for(ch), x, y))
                .collect(),
            color: Rgb(0, 0, 0),
            alpha: 1.0,
            synth_bold: false,
            synth_italic: false,
            text: text.to_string(),
            link: Some("https://example.com".to_string()),
            ranges: text
                .char_indices()
                .map(|(i, ch)| i..i + ch.len_utf8())
                .collect(),
            text_bg: None,
        }
    }

    fn image(rect: Rect) -> Draw {
        Draw::Image {
            rect,
            media: "unused".into(),
            crop: [0.0; 4],
            alpha: 1.0,
            alt: String::new(),
        }
    }

    type Metadata = ([f32; 4], Vec<KurboRect>, Vec<(KurboRect, String, String)>);

    fn metadata(draws: &[Draw]) -> Metadata {
        let mut bounds = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        let [min_x, min_y, max_x, max_y] = &mut bounds;
        let mut images = Vec::new();
        let mut links = Vec::new();
        collect_draws_meta(
            draws,
            KurboAffine::IDENTITY,
            None,
            200.0,
            200.0,
            min_x,
            min_y,
            max_x,
            max_y,
            &mut images,
            &mut links,
        );
        (bounds, images, links)
    }

    fn text(draws: &[Draw]) -> (String, Vec<Option<KurboRect>>) {
        let mut raw = String::new();
        let mut rects = Vec::new();
        collect_draws_text(
            draws,
            KurboAffine::IDENTITY,
            None,
            &mut raw,
            &mut rects,
            &mut None,
        );
        (raw, rects)
    }
    #[test]
    fn mixed_size_rtf_keeps_one_searchable_line_and_real_paragraph_breaks() {
        let bytes = br"{\rtf1\ansi\deff0{\fonttbl{\f0 Arial;}}\f0\fs24 A{\fs48 B}{\fs24 C}D\par}";
        let (doc, _, _) = WordDocument::load(Path::new("mixed.rtf"), bytes, "rtf").unwrap();
        let cached = doc.extract_cached_text(0);
        assert_eq!(cached.raw.trim(), "ABCD");
        assert_eq!(cached.normalized.matches("abcd").count(), 1);
        assert_eq!(cached.char_rects.len(), cached.raw.chars().count());
        assert!(cached.char_rects.iter().all(Option::is_some));

        let bytes = br"{\rtf1\ansi\deff0{\fonttbl{\f0 Arial;}}\f0\fs24 AB\par CD\par}";
        let (doc, _, _) = WordDocument::load(Path::new("lines.rtf"), bytes, "rtf").unwrap();
        let cached = doc.extract_cached_text(0);
        assert_eq!(cached.raw.trim(), "AB\nCD");
        assert!(!cached.normalized.contains("abcd"));
        assert!(cached.normalized.contains("ab cd"));
    }

    #[test]
    fn mixed_bidi_docx_run_edges_preserve_contiguous_search() {
        for logical in ["Xאבגדה", "X אבגדה"] {
            let mut source = Document::from_text(logical);
            let Block::Para(para) = std::sync::Arc::make_mut(&mut source.body[0]) else {
                panic!("paragraph")
            };
            para.runs = vec![
                wordcraft_doc::Run {
                    len: 1,
                    props: CharProps {
                        bold: Some(true),
                        ..Default::default()
                    },
                },
                wordcraft_doc::Run {
                    len: logical.len() - 1,
                    props: CharProps {
                        bold: Some(false),
                        ..Default::default()
                    },
                },
            ];
            let bytes = wordcraft_docx::write(&source).unwrap();
            let (doc, _, _) =
                WordDocument::load(Path::new("bidi.docx"), &bytes, "docx").unwrap();
            let cached = doc.extract_cached_text(0);
            assert_eq!(cached.raw.trim(), logical);
            let query = logical.to_lowercase();
            assert_eq!(cached.normalized.matches(query.as_str()).count(), 1);
            assert_eq!(cached.char_rects.len(), cached.raw.chars().count());
        }
    }

    #[test]
    fn clipped_turned_mixed_size_text_uses_baselines_and_reading_direction() {
        let mut large = glyphs("B", &[(5.0, 20.0)]);
        let Draw::Glyphs { size, .. } = &mut large else {
            unreachable!()
        };
        *size = 24.0;
        let draws = [Draw::Turned {
            x: 100.0,
            y: 20.0,
            turn: wordcraft_doc::props::TextDirection::Down,
            items: vec![Draw::Clip {
                rect: Rect::new(2.0, 10.0, 40.0, 50.0),
                items: vec![
                    glyphs("A", &[(0.0, 20.0)]),
                    large,
                    glyphs("C", &[(10.0, 20.0)]),
                    glyphs("D", &[(0.0, 50.0)]),
                ],
            }],
        }];
        let (raw, rects) = text(&draws);
        assert_eq!(raw, "ABC\nD");
        assert_eq!(rects.len(), 5);
        assert_eq!(rects[3], None);
        assert!(rects.iter().flatten().all(|rect| {
            rect.x0 >= 40.0 && rect.x1 <= 90.0 && rect.y0 >= 22.0 && rect.y1 <= 62.0
        }));
    }

    #[test]
    fn paths_and_ink_include_strokes_after_turning_and_clipping() {
        use wordcraft_doc::graphic::PathSeg;
        let path = Draw::Path {
            segs: vec![
                PathSeg::Move(10.0, 20.0),
                PathSeg::Line(30.0, 20.0),
                PathSeg::Line(30.0, 40.0),
                PathSeg::Line(10.0, 40.0),
                PathSeg::Close,
            ],
            fill: Some(Rgb(255, 0, 0)),
            stroke: Some(Rgb(255, 0, 0)),
            stroke_width: 4.0,
            dash: Vec::new(),
        };
        let turned = |draw, clip| {
            vec![Draw::Turned {
                x: 100.0,
                y: 50.0,
                turn: wordcraft_doc::props::TextDirection::Down,
                items: vec![Draw::Clip {
                    rect: clip,
                    items: vec![draw],
                }],
            }]
        };
        assert_eq!(
            metadata(&turned(path, Rect::new(8.0, 18.0, 20.0, 20.0))).0,
            [62.0, 58.0, 82.0, 78.0],
        );
        let ink = Draw::Ink {
            pts: vec![(10.0, 30.0), (40.0, 30.0)],
            color: Rgb(255, 0, 0),
            width: 4.0,
            alpha: 1.0,
        };
        assert_eq!(
            metadata(&turned(ink, Rect::new(15.0, 20.0, 20.0, 20.0))).0,
            [68.0, 65.0, 72.0, 85.0],
        );
        let tap = Draw::Ink {
            pts: vec![(f32::NAN, 0.0), (20.0, 30.0), (1e6, 1e6)],
            color: Rgb(255, 0, 0),
            width: 10.0,
            alpha: 1.0,
        };
        assert_eq!(metadata(&[tap.clone()]).0, [15.0, 25.0, 25.0, 35.0]);
        assert!(
            metadata(&turned(tap, Rect::new(50.0, 50.0, 10.0, 10.0)))
                .0
                .iter()
                .all(|v| !v.is_finite())
        );
    }

    #[test]
    fn freeform_docx_bounds_enclose_rendered_pixels() {
        use std::sync::Arc;
        use wordcraft_doc::{
            InlineObject,
            freeform::{FreePath, Freeform},
            para::{Anchor, Float, ShapeKind, Wrap},
        };
        for closed in [true, false] {
            let mut source = Document::from_text("Caption");
            let shape = InlineObject::Shape {
                kind: ShapeKind::Freeform,
                w: 200.0,
                h: 100.0,
                fill: closed.then_some(Rgb(255, 0, 0)),
                stroke: Some(Rgb(255, 0, 0)),
                stroke_width: 4.0,
                float: Float {
                    wrap: Wrap::InFrontOfText,
                    h_rel: Anchor::Page,
                    v_rel: Anchor::Page,
                    x: 300.0,
                    y: 400.0,
                    ..Default::default()
                },
                story: None,
                freeform: Some(Arc::new(Freeform {
                    w: 200.0,
                    h: 100.0,
                    paths: vec![FreePath {
                        pts: vec![[0.0, 0.0], [200.0, 0.0], [200.0, 100.0], [0.0, 100.0]],
                        closed,
                    }],
                    ..Default::default()
                })),
                effects: Default::default(),
                extra: Default::default(),
            };
            source
                .insert_object(&Pos::body(0, 0), shape, &Default::default())
                .unwrap();
            let bytes = wordcraft_docx::write(&source).unwrap();
            let (doc, _, _) =
                WordDocument::load(Path::new("freeform.docx"), &bytes, "docx").unwrap();
            let bounds = doc.content_bounds(0);
            let (width, height, scale, pixels) = doc.render_page_rgba(0, 612, 792);
            let (page_w, page_h) = doc.page_size(0);
            let mut red_pixels = 0;
            for (i, rgba) in pixels.chunks_exact(4).enumerate() {
                if rgba[0] > 200 && rgba[1] < 50 && rgba[2] < 50 {
                    red_pixels += 1;
                    let x = (i % width as usize) as f64 / scale / page_w;
                    let y = (i / width as usize) as f64 / scale / page_h;
                    assert!(
                        x >= f64::from(bounds[0])
                            && x <= f64::from(bounds[2])
                            && y >= f64::from(bounds[1])
                            && y <= f64::from(bounds[3]),
                        "red pixel ({x}, {y}) outside {bounds:?}, closed={closed}",
                    );
                }
            }
            assert_eq!(pixels.len(), width as usize * height as usize * 4);
            assert!(red_pixels > 100, "the freeform must actually render");
            assert!(bounds[2] > 0.8 && bounds[3] > 0.6);
        }
    }


    #[test]
    fn nested_clips_trim_links_images_bounds_and_text() {
        let draws = [Draw::Clip {
            rect: Rect {
                x: 12.0,
                y: 10.0,
                w: 10.0,
                h: 20.0,
            },
            items: vec![Draw::Clip {
                rect: Rect {
                    x: 15.0,
                    y: 12.0,
                    w: 5.0,
                    h: 10.0,
                },
                items: vec![
                    glyphs("AB", &[(10.0, 20.0), (22.0, 20.0)]),
                    image(Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 100.0,
                        h: 100.0,
                    }),
                ],
            }],
        }];
        let expected = KurboRect::new(15.0, 12.0, 20.0, 22.0);
        let (bounds, images, links) = metadata(&draws);
        assert_eq!(bounds, [15.0, 12.0, 20.0, 22.0]);
        assert_eq!(images, vec![expected]);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, expected);
        let (raw, rects) = text(&draws);
        assert_eq!(raw, "A ");
        assert_eq!(rects, vec![Some(expected), None]);
        let mask = raster_mask_from_rects(&images, 40, 40, 1.0).unwrap();
        assert_eq!(mask[0], 0);
        assert!(mask.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn clipped_glyph_gap_does_not_create_a_link_or_text() {
        let draws = [Draw::Clip {
            rect: Rect {
                x: 45.0,
                y: 10.0,
                w: 5.0,
                h: 20.0,
            },
            items: vec![glyphs("AB", &[(10.0, 20.0), (90.0, 20.0)])],
        }];
        let (bounds, images, links) = metadata(&draws);
        assert!(bounds.iter().all(|v| !v.is_finite()));
        assert!(images.is_empty());
        assert!(links.is_empty());
        assert_eq!(text(&draws), (String::new(), Vec::new()));
    }

    #[test]
    fn rotated_clips_reject_hidden_bounding_box_corners() {
        // The clip is a diamond centred at (50, 50). The inner inverse rotation puts
        // content in a corner of its bounding box, wholly outside the actual diamond.
        let draws = [Draw::Rotated {
            cx: 50.0,
            cy: 50.0,
            spin: wordcraft_geom::Spin::new(45.0, false, false),
            items: vec![Draw::Clip {
                rect: Rect {
                    x: 40.0,
                    y: 40.0,
                    w: 20.0,
                    h: 20.0,
                },
                items: vec![Draw::Rotated {
                    cx: 50.0,
                    cy: 50.0,
                    spin: wordcraft_geom::Spin::new(-45.0, false, false),
                    items: vec![
                        image(Rect {
                            x: 36.5,
                            y: 36.5,
                            w: 2.0,
                            h: 2.0,
                        }),
                        glyphs("A", &[(36.5, 38.5)]),
                        Draw::Ink {
                            pts: vec![(37.0, 37.0)],
                            color: Rgb(255, 0, 0),
                            width: 1.0,
                            alpha: 1.0,
                        },
                        Draw::Path {
                            segs: vec![
                                wordcraft_doc::graphic::PathSeg::Move(36.5, 36.5),
                                wordcraft_doc::graphic::PathSeg::Line(38.5, 36.5),
                                wordcraft_doc::graphic::PathSeg::Line(38.5, 38.5),
                                wordcraft_doc::graphic::PathSeg::Close,
                            ],
                            fill: Some(Rgb(255, 0, 0)),
                            stroke: Some(Rgb(255, 0, 0)),
                            stroke_width: 1.0,
                            dash: Vec::new(),
                        },
                    ],
                }],
            }],
        }];
        let (bounds, images, links) = metadata(&draws);
        assert!(bounds.iter().all(|v| !v.is_finite()));
        assert!(images.is_empty());
        assert!(links.is_empty());
        assert_eq!(text(&draws), (String::new(), Vec::new()));
    }

    fn subset_font(ch: char) -> Vec<u8> {
        subset_font_mapping(ch, ch)
    }

    fn subset_font_mapping(ch: char, source_glyph: char) -> Vec<u8> {
        // Replace only cmap with one format-12 mapping; outlines/metrics/names are the
        // real bundled fixture. Two files now have the same family but disjoint coverage.
        let mut bytes = wordcraft_fonts::bundled()[0].to_vec();
        let gid = wordcraft_fonts::FontDb::global()
            .face("Source Sans 3", "Regular")
            .glyph_for(source_glyph);
        assert_ne!(gid, 0);
        let tables = usize::from(u16::from_be_bytes(bytes[4..6].try_into().unwrap()));
        let record = (0..tables)
            .map(|i| 12 + 16 * i)
            .find(|&i| &bytes[i..i + 4] == b"cmap")
            .unwrap();
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        let offset = bytes.len() as u32;
        let mut cmap = Vec::new();
        for v in [0u16, 1, 3, 10] {
            cmap.extend_from_slice(&v.to_be_bytes());
        }
        cmap.extend_from_slice(&12u32.to_be_bytes());
        cmap.extend_from_slice(&12u16.to_be_bytes());
        cmap.extend_from_slice(&0u16.to_be_bytes());
        for v in [28u32, 0, 1, u32::from(ch), u32::from(ch), gid] {
            cmap.extend_from_slice(&v.to_be_bytes());
        }
        bytes[record + 8..record + 12].copy_from_slice(&offset.to_be_bytes());
        bytes[record + 12..record + 16].copy_from_slice(&(cmap.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&cmap);
        bytes
    }

    fn subset_document(family: &str, ch: char, bytes: Vec<u8>) -> WordDocument {
        let mut doc = Document::new();
        doc.body = vec![std::sync::Arc::new(Block::Para(Paragraph::with_text(
            &ch.to_string(),
            CharProps {
                font: Some(family.to_string()),
                ..Default::default()
            },
        )))];
        let aliases = register_document_fonts(vec![(family.to_string(), bytes)]);
        remap_document_fonts(&mut doc, &aliases);
        document(doc)
    }

    fn assert_subset_face(doc: &WordDocument, ch: char, excluded: char) {
        fn find(draws: &[Draw], ch: char) -> Option<wordcraft_fonts::FaceRef> {
            draws.iter().find_map(|draw| match draw {
                Draw::Glyphs { face, text, .. } if text.contains(ch) => Some(*face),
                Draw::Figure { draws, .. } => find(draws, ch),
                Draw::Clip { items, .. }
                | Draw::Turned { items, .. }
                | Draw::Rotated { items, .. } => find(items, ch),
                _ => None,
            })
        }
        let draws = page_display(&doc.doc, &doc.layout.pages[0], &DisplayOptions::default());
        let face = find(&draws, ch).expect("the paragraph or drawing emits its text");
        assert!(face.covers(ch));
        assert!(
            !face.covers(excluded),
            "must use this document's actual subset, not a fallback"
        );
    }

    #[test]
    fn same_family_subsets_are_isolated_in_both_open_orders_and_on_reload() {
        let a = subset_font('A');
        let b = subset_font('B');
        for (family, first, second, first_bytes, second_bytes) in [
            ("Subset order AB", 'A', 'B', &a, &b),
            ("Subset order BA", 'B', 'A', &b, &a),
        ] {
            let one = subset_document(family, first, first_bytes.clone());
            assert_subset_face(&one, first, second);
            let before = one.render_page_rgba(0, 160, 160);
            let two = subset_document(family, second, second_bytes.clone());
            assert_subset_face(&one, first, second);
            assert_subset_face(&two, second, first);
            assert_eq!(one.render_page_rgba(0, 160, 160), before);
            let reopened = subset_document(family, first, first_bytes.clone());
            assert_subset_face(&reopened, first, second);
            assert_eq!(reopened.render_page_rgba(0, 160, 160), before);
        }
    }

    #[test]
    fn document_font_remapping_preserves_inheritance_and_unrelated_families() {
        let mut doc = Document::from_text("A");
        doc.styles.default_chr.font = Some("Subset inherited".into());
        doc.styles.default_chr.font_cs = Some("Subset inherited".into());
        doc.styles.default_chr.font_ea = Some("Subset inherited".into());
        doc.styles.default_chr.sym_font = Some("Subset inherited".into());
        doc.styles.default_para.drop_cap_font = Some("Subset inherited".into());
        doc.settings.major_font = "Subset inherited".into();
        doc.settings.minor_font = "Source Serif 4".into();
        let aliases = register_document_fonts(vec![("Subset inherited".into(), subset_font('A'))]);
        let alias = &aliases["subset inherited"];
        remap_document_fonts(&mut doc, &aliases);
        for font in [
            &doc.styles.default_chr.font,
            &doc.styles.default_chr.font_cs,
            &doc.styles.default_chr.font_ea,
            &doc.styles.default_chr.sym_font,
            &doc.styles.default_para.drop_cap_font,
        ] {
            assert_eq!(font.as_deref(), Some(alias.as_str()));
        }
        assert_eq!(doc.settings.major_font, *alias);
        assert_eq!(doc.settings.minor_font, "Source Serif 4");
        assert_subset_face(&document(doc), 'A', 'B');
    }

    #[test]
    fn isolated_symbol_fonts_preserve_physical_codes_and_logical_text() {
        let aliases = register_document_fonts(vec![(
            "Symbol".into(),
            subset_font_mapping('\u{f061}', 'A'),
        )]);

        let mut doc = Document::from_text("a");
        let mut paragraph_style = doc
            .styles
            .styles
            .iter()
            .find(|style| style.id == "Normal")
            .unwrap()
            .clone();
        paragraph_style.id = "Symbol paragraph".into();
        paragraph_style.chr.font = Some("Symbol".into());
        doc.styles.styles.push(paragraph_style);
        let Block::Para(para) = std::sync::Arc::make_mut(&mut doc.body[0]) else {
            panic!("paragraph")
        };
        para.props.style = Some("Symbol paragraph".into());
        // Neither paragraph has a direct run font: aliases must preserve inherited selection.
        let mut table = wordcraft_doc::Table::new(1, 1, 100.0);
        table.props.style = Some("Symbol table".into());
        table.rows[0].cells[0].blocks = vec![std::sync::Arc::new(Block::Para(
            Paragraph::with_text("a", CharProps::default()),
        ))];
        let mut table_style = doc
            .styles
            .styles
            .iter()
            .find(|style| style.kind == wordcraft_doc::StyleKind::Table)
            .unwrap()
            .clone();
        table_style.id = "Symbol table".into();
        table_style.based_on = None;
        table_style.chr.font = Some("Symbol".into());
        doc.styles.styles.push(table_style);
        doc.body.push(std::sync::Arc::new(Block::Table(table)));
        doc.numbering
            .abstracts
            .push(wordcraft_doc::numbering::AbstractNum {
                id: 913,
                levels: vec![wordcraft_doc::numbering::Level {
                    text: "a".into(),
                    chr: CharProps {
                        font: Some("Symbol".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                ..Default::default()
            });
        remap_document_fonts(&mut doc, &aliases);
        let Block::Para(para) = doc.body[0].as_ref() else {
            panic!("paragraph")
        };
        assert_eq!(para.text, "a");
        assert_eq!(para.runs[0].len, para.text.len());
        let Block::Table(table) = doc.body[1].as_ref() else {
            panic!("table")
        };
        let Block::Para(para) = table.rows[0].cells[0].blocks[0].as_ref() else {
            panic!("cell paragraph")
        };
        assert_eq!(para.text, "a");
        assert_eq!(para.runs[0].len, para.text.len());
        assert_eq!(doc.numbering.abstracts.last().unwrap().levels[0].text, "a");
        let one = document(doc);
        let before = one.render_page_rgba(0, 160, 160);
        let _other = subset_document("Symbol", 'b', subset_font_mapping('\u{f062}', 'B'));
        assert_eq!(one.render_page_rgba(0, 160, 160), before);
        let draws = page_display(&one.doc, &one.layout.pages[0], &DisplayOptions::default());
        assert!(text(&draws).0.contains('α'));
        assert!(!text(&draws).0.contains('\u{f061}'));
    }

    #[test]
    fn symbol_unicode_fallback_is_not_mapped_twice() {
        let aliases =
            register_document_fonts(vec![("Symbol".into(), subset_font_mapping('µ', 'A'))]);
        let mut doc = Document::new();
        doc.body = vec![std::sync::Arc::new(Block::Para(Paragraph::with_text(
            "mµm",
            CharProps {
                font: Some("Symbol".into()),
                ..Default::default()
            },
        )))];
        remap_document_fonts(&mut doc, &aliases);
        let Block::Para(para) = doc.body[0].as_ref() else {
            panic!("paragraph")
        };
        assert_eq!(para.text, "mµm");
        assert_eq!(para.runs.len(), 1);
        assert_eq!(para.runs[0].len, para.text.len());
        let doc = document(doc);
        let draws = page_display(&doc.doc, &doc.layout.pages[0], &DisplayOptions::default());
        assert_eq!(
            text(&draws).0,
            wordcraft_doc::symbol_fonts::map_text("Symbol", "mµm")
        );
    }

    #[test]
    fn scoped_symbol_aliases_keep_every_supported_encoding() {
        for family in [
            "Symbol",
            "Symbol (PCL6)",
            "Wingdings",
            "Wingdings 2",
            "Wingdings 3",
            "Webdings",
        ] {
            let alias = format!("office-term-doc-encoding-font-0@{family}");
            assert!(wordcraft_doc::symbol_fonts::is_symbol_font(&alias));
            for code in 0x20..=0xff {
                for value in [code, 0xf000 + code] {
                    let ch = char::from_u32(value).unwrap();
                    assert_eq!(
                        wordcraft_doc::symbol_fonts::symbol_char(&alias, ch),
                        wordcraft_doc::symbol_fonts::symbol_char(family, ch),
                    );
                }
            }
            assert_eq!(
                wordcraft_doc::symbol_fonts::map_text(&alias, "a a\nmµm"),
                wordcraft_doc::symbol_fonts::map_text(family, "a a\nmµm"),
            );
        }
        assert!(!wordcraft_doc::symbol_fonts::is_symbol_font(
            "other-prefix@Symbol"
        ));
        assert!(!wordcraft_doc::symbol_fonts::is_symbol_font(
            "office-term-doc-0@Arial"
        ));
    }

    #[test]
    fn symbol_aliasing_keeps_whitespace_fields_ruby_and_outline_logical() {
        use wordcraft_doc::InlineObject;
        let aliases = register_document_fonts(vec![(
            "Symbol".into(),
            subset_font_mapping('\u{f061}', 'A'),
        )]);
        let props = CharProps {
            font: Some("Symbol".into()),
            ..Default::default()
        };
        let mut para = Paragraph::with_text("a a\n", props.clone());
        para.props.outline_level = Some(0);
        para.insert_object(
            para.text.len(),
            InlineObject::Field {
                instr: "UNKNOWN".into(),
                result: "mµm".into(),
                locked: true,
                dirty: false,
                code: false,
                form: String::new(),
            },
            &props,
        )
        .unwrap();
        para.insert_object(
            para.text.len(),
            InlineObject::Ruby {
                base: "a".into(),
                ruby: "a".into(),
                align: Default::default(),
                size: 6.0,
                raise: 10.0,
                base_size: 12.0,
                lang: String::new(),
                props: Box::new(props.clone()),
            },
            &props,
        )
        .unwrap();
        let original = para.text.clone();
        let original_shown = para.shown_text();
        let expected_title = normalize_context_text(&original_shown);
        let mut doc = Document::new();
        doc.body = vec![std::sync::Arc::new(Block::Para(para))];
        remap_document_fonts(&mut doc, &aliases);
        let Block::Para(para) = doc.body[0].as_ref() else {
            panic!("paragraph")
        };
        assert_eq!(para.text, original);
        assert_eq!(para.shown_text(), original_shown);
        assert!(matches!(&para.objects[0], InlineObject::Field { result, .. } if result == "mµm"));
        assert!(
            matches!(&para.objects[1], InlineObject::Ruby { base, ruby, props, .. }
            if base == "a" && ruby == "a" && props.font.as_deref() == Some(aliases["symbol"].as_str()))
        );
        let doc = document(doc);
        assert_eq!(
            collect_outline(&doc.doc, &doc.layout)[0].title,
            expected_title
        );
        let draws = page_display(&doc.doc, &doc.layout.pages[0], &DisplayOptions::default());
        let extracted = text(&draws).0;
        assert!(extracted.contains('α'));
        assert!(
            extracted.contains("mµm"),
            "cached atomic field text keeps upstream's logical payload"
        );
        assert!(!extracted.contains('\u{f061}'));
    }

    #[test]
    fn drawing_text_uses_its_documents_embedded_subset() {
        use wordcraft_doc::{
            InlineObject,
            graphic::{Graphic, GraphicItem},
        };
        let family = "Graphic subset";
        let aliases = register_document_fonts(vec![(family.into(), subset_font('A'))]);
        let mut para = Paragraph::new();
        para.insert_object(
            0,
            InlineObject::Graphic {
                w: 100.0,
                h: 40.0,
                alt: String::new(),
                float: Default::default(),
                graphic: std::sync::Arc::new(Graphic {
                    w: 100.0,
                    h: 40.0,
                    items: vec![GraphicItem::Text {
                        rect: [0.0, 0.0, 100.0, 40.0],
                        text: "A".into(),
                        size: 12.0,
                        color: Rgb(0, 0, 0),
                        bold: false,
                        align: Default::default(),
                        font: Some(family.into()),
                    }],
                    ..Default::default()
                }),
            },
            &CharProps::default(),
        )
        .unwrap();
        let mut doc = Document::new();
        doc.body = vec![std::sync::Arc::new(Block::Para(para))];
        remap_document_fonts(&mut doc, &aliases);
        let one = document(doc);
        assert_subset_face(&one, 'A', 'B');
        let before = one.render_page_rgba(0, 160, 160);
        let _other = subset_document(family, 'B', subset_font('B'));
        assert_subset_face(&one, 'A', 'B');
        assert_eq!(one.render_page_rgba(0, 160, 160), before);
    }
}
