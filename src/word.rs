use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::Path;

use kurbo::{Affine as KurboAffine, Rect as KurboRect};
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
        install_embedded_fonts(&doc);

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
        let pw = f64::from(p.w.max(1.0));
        let ph = f64::from(p.h.max(1.0));
        let scale = (f64::from(width) / pw).min(f64::from(height) / ph);
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
        let mut prev_y: Option<f64> = None;
        let mut prev_right: Option<f64> = None;
        collect_draws_text(
            &draws,
            KurboAffine::IDENTITY,
            &mut raw,
            &mut char_rects,
            &mut prev_y,
            &mut prev_right,
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
            let top_ratio =
                Some((caret.top / target_page.h.max(1.0)).clamp(0.0, 1.0));
            let left_ratio =
                Some((caret.x / target_page.w.max(1.0)).clamp(0.0, 1.0));
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

fn install_embedded_fonts(doc: &Document) {
    let embedded = wordcraft_docx::embedded_fonts(doc);
    if embedded.is_empty() {
        return;
    }
    let files: Vec<(Vec<u8>, String)> = embedded
        .into_iter()
        .filter_map(|f| {
            wordcraft_fonts::decode_font_file(&f.data)
                .ok()
                .map(|bytes| (bytes, f.name))
        })
        .collect();
    if !files.is_empty() {
        wordcraft_fonts::FontDb::global().add_document_fonts(files);
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
                if let Some(bbox) = glyphs_bbox(face, *size, glyphs, &transform) {
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
                include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
            }
            Draw::Image { rect, .. } => {
                let bbox = transform_rect_bbox(&transform, wc_rect_to_kurbo(*rect));
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
                include_rect_bounds(bbox, min_x, min_y, max_x, max_y, page_w, page_h);
            }
            Draw::Figure { draws: inner, .. } | Draw::Clip { items: inner, .. } => {
                collect_draws_meta(
                    inner,
                    transform,
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

fn glyphs_bbox(
    face: &wordcraft_fonts::FaceRef,
    size: f32,
    glyphs: &[(u32, f32, f32)],
    transform: &KurboAffine,
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
        union = Some(match union {
            Some(prev) => prev.union(r),
            None => r,
        });
    }
    union
}

fn collect_draws_text(
    draws: &[Draw],
    transform: KurboAffine,
    raw: &mut String,
    char_rects: &mut Vec<Option<KurboRect>>,
    prev_y: &mut Option<f64>,
    prev_right: &mut Option<f64>,
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
                if let Some(&(_, first_x, first_y)) = glyphs.first() {
                    let p = transform * kurbo::Point::new(f64::from(first_x), f64::from(first_y));
                    if let Some(py) = *prev_y {
                        if (p.y - py).abs() > sz * 0.45 {
                            if !raw.ends_with('\n') && !raw.is_empty() {
                                raw.push('\n');
                                char_rects.push(None);
                            }
                        } else if let Some(pr) = *prev_right
                            && p.x - pr > sz * 0.22
                            && !raw.ends_with(char::is_whitespace)
                            && !text.starts_with(char::is_whitespace)
                        {
                            raw.push(' ');
                            char_rects.push(None);
                        }
                    }
                    *prev_y = Some(p.y);
                }

                // Compute a rect per glyph, then map characters via `ranges`.
                let mut glyph_rects = Vec::with_capacity(glyphs.len());
                let mut last_right = prev_right.unwrap_or(0.0);
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
                    last_right = r.x1;
                    glyph_rects.push(r);
                }
                if !glyph_rects.is_empty() {
                    *prev_right = Some(last_right);
                }

                let run_bbox = glyph_rects.iter().copied().reduce(|a, b| a.union(b));
                for (byte_offset, ch) in text.char_indices() {
                    raw.push(ch);
                    let rect = if ranges.len() == glyph_rects.len() && !ranges.is_empty() {
                        ranges
                            .iter()
                            .position(|range| range.contains(&byte_offset))
                            .and_then(|g_idx| glyph_rects.get(g_idx).copied())
                            .or(run_bbox)
                    } else {
                        run_bbox
                    };
                    char_rects.push(rect);
                }
            }
            Draw::Figure { draws: inner, .. } | Draw::Clip { items: inner, .. } => {
                collect_draws_text(inner, transform, raw, char_rects, prev_y, prev_right);
            }
            Draw::Turned {
                x,
                y,
                turn,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(Draw::turn_matrix(*turn, *x, *y));
                collect_draws_text(inner, m, raw, char_rects, prev_y, prev_right);
            }
            Draw::Rotated {
                cx,
                cy,
                spin,
                items: inner,
            } => {
                let m = transform * affine_from_matrix(spin.matrix(*cx, *cy));
                collect_draws_text(inner, m, raw, char_rects, prev_y, prev_right);
            }
            _ => {}
        }
    }
}
