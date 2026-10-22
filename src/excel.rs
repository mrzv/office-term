use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use gridcraft_engine::cf::{CfCache, CfLook};
use gridcraft_engine::cmd::print::{
    HfContext, PrintOptions, SheetLayout, expand_header_footer, hf_date_time, layout_sheet,
};
use gridcraft_engine::core::{CellRef, MAX_COLS, RangeRef, Value, col_to_letters};
use gridcraft_engine::links::{LinkKind, classify};
use gridcraft_engine::model::{
    Anchor, BorderLine, BorderStyle, Borders, Color, HAlign, HfPage, Orientation, PatternType,
    ShapeKind, ShapeTextAlign, ShapeTextAnchor, Sheet, Style, StyleId, StyleTable, Underline,
    VAlign, Visibility, Workbook,
};
use gridcraft_engine::numfmt::{FormatColor, format_general_fit};
use kurbo::Rect as KurboRect;
use vello_cpu::kurbo::{self, Affine, BezPath, Shape};
use vello_cpu::peniko;
use vello_cpu::{Pixmap, RenderContext, Resources};
use wordcraft_fonts::{FaceRef, FontDb};

use crate::pptx::{
    CachedSlideText, LinkTarget, OutlineItem, PageLink, bbox_to_page_link_rect, build_cached_text,
    default_link_label, normalize_context_text, raster_mask_from_rects,
};

const PX_TO_PT: f32 = 0.75;
const PAD_PX: f32 = 2.0;
const DENSE_BLOCK: u64 = 4_000;
const MAX_WRAP_LINES: usize = 2_000;

type Rgb = [u8; 3];
type CachedPixmap = (Arc<Pixmap>, u32, u32);
type ImageCache = HashMap<(u64, usize), Option<CachedPixmap>>;

fn layout_k(lay: &SheetLayout) -> f32 {
    PX_TO_PT * lay.scale
}

fn layout_avail(lay: &SheetLayout) -> (f32, f32) {
    let [l, r, t, b, ..] = lay.margins;
    (
        (lay.page_w - l - r).max(36.0),
        (lay.page_h - t - b).max(36.0),
    )
}

#[derive(Clone, Copy)]
struct ExcelPageSpec {
    layout_idx: usize,
    sheet_page_idx: usize,
    row_band: (u32, u32),
    col_band: (u32, u32),
}

struct ExcelPageMeta {
    content_bounds: [f32; 4],
    picture_rects: Vec<KurboRect>,
    links: Vec<(KurboRect, String, LinkTarget)>,
    cached_text: CachedSlideText,
}

pub(crate) struct ExcelDocument {
    workbook: Workbook,
    layouts: Vec<SheetLayout>,
    pages: Vec<ExcelPageSpec>,
    meta_cache: Vec<OnceCell<ExcelPageMeta>>,
    image_cache: RefCell<ImageCache>,
    file_name: String,
    file_path: String,
}

impl ExcelDocument {
    pub(crate) fn load(path: &Path, bytes: &[u8]) -> Result<(Self, u32, Vec<OutlineItem>), String> {
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workbook.xlsx")
            .to_string();
        let file_path = path.to_string_lossy().to_string();
        let (mut wb, _) =
            gridcraft_engine::io::open_bytes(&file_name, bytes).map_err(|e| e.to_string())?;
        let mut calc = gridcraft_engine::calc::Calc::new();
        calc.recalc_all(&mut wb);

        let opts = PrintOptions::default();
        let mut layouts = Vec::new();
        let mut pages = Vec::new();
        let mut outline = Vec::new();

        for si in 0..wb.sheets.len() {
            if wb.sheets[si].visibility != Visibility::Visible {
                continue;
            }
            configure_landscape_fit_width(Arc::make_mut(&mut wb.sheets[si]));
            let Some(content_range) = sheet_content_range(&wb.sheets[si]) else {
                continue;
            };
            let Some(mut lay) = layout_sheet(&wb, si, &opts, Some(content_range)) else {
                continue;
            };
            let sh = &wb.sheets[si];
            lay.settings.gridlines = sh.show_gridlines || sh.print.gridlines;
            let mut sheet_specs = Vec::new();
            for &(ri, ci) in &lay.pages {
                let Some(row_band) = lay.row_bands.get(ri).copied() else {
                    continue;
                };
                let Some(col_band) = lay.col_bands.get(ci).copied() else {
                    continue;
                };
                let Some(trimmed_rows) = trim_band_rows(sh, row_band, col_band) else {
                    continue;
                };
                let sheet_page_idx = sheet_specs.len();
                sheet_specs.push((sheet_page_idx, trimmed_rows, col_band));
            }
            if sheet_specs.is_empty() {
                continue;
            }
            let first_page_idx = pages.len();
            outline.push(OutlineItem {
                title: sh.name.clone(),
                page: first_page_idx as u32,
                depth: 0,
            });
            let layout_idx = layouts.len();
            let multi_page = sheet_specs.len() > 1;
            for (sheet_page_idx, row_band, col_band) in sheet_specs {
                let page_idx = pages.len();
                pages.push(ExcelPageSpec {
                    layout_idx,
                    sheet_page_idx,
                    row_band,
                    col_band,
                });
                if multi_page {
                    let range = RangeRef::new(
                        CellRef::new(row_band.0, col_band.0),
                        CellRef::new(row_band.1, col_band.1),
                    );
                    outline.push(OutlineItem {
                        title: format!("{} ({})", sh.name, range),
                        page: page_idx as u32,
                        depth: 1,
                    });
                }
            }
            layouts.push(lay);
        }

        if pages.is_empty() && !wb.sheets.is_empty() {
            let fallback_si = wb
                .sheets
                .iter()
                .position(|s| s.visibility == Visibility::Visible)
                .unwrap_or(0);
            configure_landscape_fit_width(Arc::make_mut(&mut wb.sheets[fallback_si]));
            let default_area = RangeRef::new(CellRef::new(0, 0), CellRef::new(9, 6));
            if let Some(mut lay) = layout_sheet(&wb, fallback_si, &opts, Some(default_area)) {
                let sh = &wb.sheets[fallback_si];
                lay.settings.gridlines = sh.show_gridlines || sh.print.gridlines;
                lay.area = default_area;
                lay.row_bands = vec![(0, 9)];
                lay.col_bands = vec![(0, 6)];
                lay.pages = vec![(0, 0)];
                outline.push(OutlineItem {
                    title: sh.name.clone(),
                    page: 0,
                    depth: 0,
                });
                pages.push(ExcelPageSpec {
                    layout_idx: 0,
                    sheet_page_idx: 0,
                    row_band: (0, 9),
                    col_band: (0, 6),
                });
                layouts.push(lay);
            }
        }

        if pages.is_empty() {
            return Err(format!("{} has no pages", path.display()));
        }
        let page_count =
            u32::try_from(pages.len()).map_err(|_| "workbook has too many pages".to_string())?;
        let meta_cache = (0..pages.len()).map(|_| OnceCell::new()).collect();
        Ok((
            Self {
                workbook: wb,
                layouts,
                pages,
                meta_cache,
                image_cache: RefCell::new(HashMap::new()),
                file_name,
                file_path,
            },
            page_count,
            outline,
        ))
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn page_size(&self, page: usize) -> (f64, f64) {
        self.pages
            .get(page)
            .and_then(|spec| self.layouts.get(spec.layout_idx))
            .map(|lay| {
                (
                    f64::from(lay.page_w.max(1.0)),
                    f64::from(lay.page_h.max(1.0)),
                )
            })
            .unwrap_or((792.0, 612.0))
    }

    fn page_meta(&self, page: usize) -> Option<&ExcelPageMeta> {
        let cell = self.meta_cache.get(page)?;
        Some(cell.get_or_init(|| self.compute_page_meta(page)))
    }

    pub(crate) fn content_bounds(&self, page: usize) -> [f32; 4] {
        self.page_meta(page)
            .map(|m| m.content_bounds)
            .unwrap_or([0.0, 0.0, 1.0, 1.0])
    }

    pub(crate) fn picture_mask(
        &self,
        page: usize,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Option<Vec<u8>> {
        let meta = self.page_meta(page)?;
        raster_mask_from_rects(&meta.picture_rects, width, height, scale)
    }

    pub(crate) fn extract_page_links(
        &self,
        page: usize,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Vec<PageLink> {
        let Some(meta) = self.page_meta(page) else {
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
        self.page_meta(page)
            .map(|m| m.cached_text.clone())
            .unwrap_or_else(|| build_cached_text(String::new(), Vec::new()))
    }

    pub(crate) fn render_page_rgba(
        &self,
        page: usize,
        width: u32,
        height: u32,
    ) -> (u32, u32, f64, Vec<u8>) {
        let (pw, ph) = self.page_size(page);
        let scale = (f64::from(width) / pw)
            .min(f64::from(height) / ph)
            .max(0.01);
        let rw = ((pw * scale).round() as u32).clamp(1, 16_000);
        let rh = ((ph * scale).round() as u32).clamp(1, 16_000);
        let render_scale = f64::from(rw) / pw;

        let Some(&spec) = self.pages.get(page) else {
            return (
                rw,
                rh,
                render_scale,
                vec![255u8; rw as usize * rh as usize * 4],
            );
        };
        let Some(lay) = self.layouts.get(spec.layout_idx) else {
            return (
                rw,
                rh,
                render_scale,
                vec![255u8; rw as usize * rh as usize * 4],
            );
        };
        let Some(sh) = self.workbook.sheet(lay.sheet) else {
            return (
                rw,
                rh,
                render_scale,
                vec![255u8; rw as usize * rh as usize * 4],
            );
        };

        let w16 = rw.min(u16::MAX as u32) as u16;
        let h16 = rh.min(u16::MAX as u32) as u16;
        let view = Affine::scale_non_uniform(f64::from(w16) / pw, f64::from(h16) / ph);
        let mut pg = PageCanvas::new(w16, h16, view);
        pg.fill_rect(0.0, 0.0, lay.page_w, lay.page_h, [255, 255, 255]);

        let mut cf = if sh.cond_formats.is_empty() {
            None
        } else {
            Some(CfCache::new())
        };
        let k = layout_k(lay);
        let (aw, ah) = layout_avail(lay);
        let rsegs = segments(lay.title_rows, spec.row_band);
        let csegs = segments(lay.title_cols, spec.col_band);
        let content_w =
            (lay.heading_w + csegs.iter().map(|s| seg_size(sh, *s, false)).sum::<f32>()) * k;
        let content_h =
            (lay.heading_h + rsegs.iter().map(|s| seg_size(sh, *s, true)).sum::<f32>()) * k;
        let ox = lay.margins[0]
            + if lay.settings.center_h {
                ((aw - content_w) / 2.0).max(0.0)
            } else {
                0.0
            };
        let oy = lay.margins[2]
            + if lay.settings.center_v {
                ((ah - content_h) / 2.0).max(0.0)
            } else {
                0.0
            };

        let mut image_cache = self.image_cache.borrow_mut();
        let mut y = oy + lay.heading_h * k;
        let mut body: Option<Block> = None;
        for (rsi, rs) in rsegs.iter().enumerate() {
            let mut x = ox + lay.heading_w * k;
            for (csi, cs) in csegs.iter().enumerate() {
                let b = make_block(&self.workbook, lay.sheet, sh, *rs, *cs, x, y, lay);
                draw_block(
                    &mut pg,
                    &b,
                    lay.settings.gridlines,
                    &mut cf,
                    &mut image_cache,
                );
                if lay.settings.headings {
                    draw_headings(
                        &mut pg,
                        &b,
                        rsi == 0,
                        csi == 0,
                        (ox, oy),
                        (lay.heading_w * k, lay.heading_h * k),
                    );
                }
                x += seg_size(sh, *cs, false) * k;
                if rsi + 1 == rsegs.len() && csi + 1 == csegs.len() {
                    body = Some(b);
                }
            }
            y += seg_size(sh, *rs, true) * k;
        }
        if let Some(b) = &body {
            draw_objects(&mut pg, &self.workbook, lay.sheet, sh, b, &mut image_cache);
        }
        if lay.settings.headings {
            let fill = [0xF2, 0xF2, 0xF2];
            pg.fill_rect(ox, oy, lay.heading_w * k, lay.heading_h * k, fill);
            pg.rect_stroke(
                ox,
                oy,
                lay.heading_w * k,
                lay.heading_h * k,
                0.3,
                [0xA0, 0xA0, 0xA0],
            );
        }
        let (date, time) = hf_date_time(&self.workbook);
        let cx = HfContext {
            page: page + 1,
            pages: self.pages.len(),
            sheet: sh.name.clone(),
            file: self.file_name.clone(),
            path: self.file_path.clone(),
            date,
            time,
        };
        draw_header_footer(
            &mut pg,
            lay,
            lay.settings.hf_page(spec.sheet_page_idx == 0, page + 1),
            &cx,
        );

        let rgba = pg.finish();
        (u32::from(w16), u32::from(h16), render_scale, rgba)
    }

    fn compute_page_meta(&self, page: usize) -> ExcelPageMeta {
        let default_bounds = [0.0, 0.0, 1.0, 1.0];
        let Some(&spec) = self.pages.get(page) else {
            return ExcelPageMeta {
                content_bounds: default_bounds,
                picture_rects: Vec::new(),
                links: Vec::new(),
                cached_text: build_cached_text(String::new(), Vec::new()),
            };
        };
        let Some(lay) = self.layouts.get(spec.layout_idx) else {
            return ExcelPageMeta {
                content_bounds: default_bounds,
                picture_rects: Vec::new(),
                links: Vec::new(),
                cached_text: build_cached_text(String::new(), Vec::new()),
            };
        };
        let Some(sh) = self.workbook.sheet(lay.sheet) else {
            return ExcelPageMeta {
                content_bounds: default_bounds,
                picture_rects: Vec::new(),
                links: Vec::new(),
                cached_text: build_cached_text(String::new(), Vec::new()),
            };
        };

        let pw = lay.page_w.max(1.0);
        let ph = lay.page_h.max(1.0);
        let k = layout_k(lay);
        let (aw, ah) = layout_avail(lay);
        let rsegs = segments(lay.title_rows, spec.row_band);
        let csegs = segments(lay.title_cols, spec.col_band);
        let content_w =
            (lay.heading_w + csegs.iter().map(|s| seg_size(sh, *s, false)).sum::<f32>()) * k;
        let content_h =
            (lay.heading_h + rsegs.iter().map(|s| seg_size(sh, *s, true)).sum::<f32>()) * k;
        let ox = lay.margins[0]
            + if lay.settings.center_h {
                ((aw - content_w) / 2.0).max(0.0)
            } else {
                0.0
            };
        let oy = lay.margins[2]
            + if lay.settings.center_v {
                ((ah - content_h) / 2.0).max(0.0)
            } else {
                0.0
            };

        let mut min_x = ox;
        let mut min_y = oy;
        let mut max_x = (ox + content_w).min(pw);
        let mut max_y = (oy + content_h).min(ph);

        let mut picture_rects = Vec::new();
        let mut links = Vec::new();
        let mut raw = String::new();
        let mut char_rects: Vec<Option<KurboRect>> = Vec::new();

        let push_str = |raw: &mut String,
                        rects: &mut Vec<Option<KurboRect>>,
                        s: &str,
                        r: Option<KurboRect>| {
            for ch in s.chars() {
                raw.push(ch);
                rects.push(r);
            }
        };

        if !sh.name.trim().is_empty() {
            push_str(&mut raw, &mut char_rects, sh.name.trim(), None);
            raw.push('\n');
            char_rects.push(None);
        }

        let mut object_texts: Vec<(&str, KurboRect)> = Vec::new();
        let mut y = oy + lay.heading_h * k;
        let mut blocks: Vec<(usize, usize, Block)> = Vec::new();
        for (rsi, rs) in rsegs.iter().enumerate() {
            let mut x = ox + lay.heading_w * k;
            for (csi, cs) in csegs.iter().enumerate() {
                let b = make_block(&self.workbook, lay.sheet, sh, *rs, *cs, x, y, lay);
                let block_range = b.range();

                for (&cell, pic) in &sh.cell_pictures {
                    if !block_range.contains(cell) || pic.data.is_empty() {
                        continue;
                    }
                    let (cx, cy, cw, ch) = b.rect(RangeRef::cell(cell));
                    if cw > 0.0 && ch > 0.0 {
                        picture_rects.push(KurboRect::new(
                            f64::from(cx),
                            f64::from(cy),
                            f64::from(cx + cw),
                            f64::from(cy + ch),
                        ));
                    }
                }

                for (&cell, hl) in &sh.hyperlinks {
                    if !block_range.contains(cell) {
                        continue;
                    }
                    let rect = sh
                        .merges
                        .iter()
                        .find(|m| m.start == cell)
                        .copied()
                        .unwrap_or_else(|| RangeRef::cell(cell));
                    let (lx, ly, lw, lh) = b.rect(rect);
                    if lw <= 0.0 || lh <= 0.0 {
                        continue;
                    }
                    if let Some(target) = self.resolve_hyperlink(lay.sheet, &hl.target, hl.external)
                    {
                        let cell_label = normalize_context_text(
                            &gridcraft_engine::display::cell_text(&self.workbook, sh, cell),
                        );
                        let label = if !cell_label.is_empty() {
                            cell_label
                        } else if let Some(tip) = hl.tooltip.as_deref()
                            && !tip.trim().is_empty()
                        {
                            normalize_context_text(tip)
                        } else {
                            default_link_label(&target)
                        };
                        let bbox = KurboRect::new(
                            f64::from(lx),
                            f64::from(ly),
                            f64::from(lx + lw),
                            f64::from(ly + lh),
                        );
                        links.push((bbox, label, target));
                    }
                }

                if rsi + 1 == rsegs.len() && csi + 1 == csegs.len() {
                    let (sx0, sy0) = (
                        sh.col_left(block_range.start.col),
                        sh.row_top(block_range.start.row),
                    );
                    let (sx1, sy1) = (
                        sh.col_left(block_range.end.col.saturating_add(1)),
                        sh.row_top(block_range.end.row.saturating_add(1)),
                    );
                    let (bx, by) = (
                        b.xs.first().copied().unwrap_or(0.0),
                        b.ys.first().copied().unwrap_or(0.0),
                    );
                    let visible = |r: (f64, f64, f64, f64)| {
                        r.2 > 0.0
                            && r.3 > 0.0
                            && r.0 < sx1
                            && r.0 + r.2 > sx0
                            && r.1 < sy1
                            && r.1 + r.3 > sy0
                    };
                    let place = |r: (f64, f64, f64, f64)| {
                        (
                            bx + ((r.0 - sx0) as f32) * b.k,
                            by + ((r.1 - sy0) as f32) * b.k,
                            r.2 as f32 * b.k,
                            r.3 as f32 * b.k,
                        )
                    };
                    for im in &sh.images {
                        let r = anchor_rect(sh, &im.anchor);
                        if visible(r) {
                            let (ix, iy, iw, ih) = place(r);
                            picture_rects.push(KurboRect::new(
                                f64::from(ix),
                                f64::from(iy),
                                f64::from(ix + iw),
                                f64::from(iy + ih),
                            ));
                            min_x = min_x.min(ix);
                            min_y = min_y.min(iy);
                            max_x = max_x.max(ix + iw);
                            max_y = max_y.max(iy + ih);
                        }
                    }
                    for shp in &sh.shapes {
                        let r = anchor_rect(sh, &shp.anchor);
                        if visible(r) {
                            let (sx, sy, sw, sh_h) = place(r);
                            min_x = min_x.min(sx);
                            min_y = min_y.min(sy);
                            max_x = max_x.max(sx + sw);
                            max_y = max_y.max(sy + sh_h);
                            let t = shp.text.trim();
                            if !t.is_empty() {
                                object_texts.push((
                                    t,
                                    KurboRect::new(
                                        f64::from(sx),
                                        f64::from(sy),
                                        f64::from(sx + sw),
                                        f64::from(sy + sh_h),
                                    ),
                                ));
                            }
                        }
                    }
                    for ch in &sh.charts {
                        let r = anchor_rect(sh, &ch.anchor);
                        if visible(r) {
                            let (cx, cy, cw, ch_h) = place(r);
                            min_x = min_x.min(cx);
                            min_y = min_y.min(cy);
                            max_x = max_x.max(cx + cw);
                            max_y = max_y.max(cy + ch_h);
                            if let Some(title) = ch.title.as_deref() {
                                let t = title.trim();
                                if !t.is_empty() {
                                    object_texts.push((
                                        t,
                                        KurboRect::new(
                                            f64::from(cx),
                                            f64::from(cy),
                                            f64::from(cx + cw),
                                            f64::from(cy + ch_h),
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }

                blocks.push((rsi, csi, b));
                x += seg_size(sh, *cs, false) * k;
            }
            y += seg_size(sh, *rs, true) * k;
        }

        let mut placed_cells: Vec<(usize, u32, usize, u32, String, Option<KurboRect>)> = Vec::new();
        for (rsi, csi, b) in &blocks {
            let block_range = b.range();
            let cells: Vec<CellRef> = if block_range.count() <= DENSE_BLOCK {
                block_range.iter().collect()
            } else {
                let mut set: Vec<CellRef> =
                    sh.cells.iter_range(block_range).map(|(c, _)| c).collect();
                set.extend(
                    sh.spill
                        .range(block_range.start..=block_range.end)
                        .map(|(c, _)| *c)
                        .filter(|c| block_range.contains(*c)),
                );
                set.sort_unstable();
                set.dedup();
                set
            };
            for cell in cells {
                let text = gridcraft_engine::display::cell_text(&self.workbook, sh, cell);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let range = sh
                    .merges
                    .iter()
                    .find(|m| m.start == cell)
                    .copied()
                    .unwrap_or_else(|| RangeRef::cell(cell));
                let (cx, cy, cw, ch) = b.rect(range);
                let cell_rect = (cw > 0.0 && ch > 0.0).then(|| {
                    KurboRect::new(
                        f64::from(cx),
                        f64::from(cy),
                        f64::from(cx + cw),
                        f64::from(cy + ch),
                    )
                });
                placed_cells.push((
                    *rsi,
                    cell.row,
                    *csi,
                    cell.col,
                    trimmed.to_string(),
                    cell_rect,
                ));
            }
        }
        placed_cells.sort_by_key(|&(rsi, row, csi, col, _, _)| (rsi, row, csi, col));
        let mut current_row: Option<(usize, u32)> = None;
        for (rsi, row, _, _, trimmed, cell_rect) in placed_cells {
            match current_row {
                Some(r) if r == (rsi, row) => {
                    raw.push('\t');
                    char_rects.push(None);
                }
                Some(_) => {
                    raw.push('\n');
                    char_rects.push(None);
                }
                None => {}
            }
            current_row = Some((rsi, row));
            push_str(&mut raw, &mut char_rects, &trimmed, cell_rect);
        }
        if current_row.is_some() {
            raw.push('\n');
            char_rects.push(None);
        }
        for (t, bbox) in object_texts {
            push_str(&mut raw, &mut char_rects, t, Some(bbox));
            raw.push('\n');
            char_rects.push(None);
        }

        let content_bounds = if max_x > min_x + 1.0 && max_y > min_y + 1.0 {
            let pad_x = (pw * 0.02).max(4.0);
            let pad_y = (ph * 0.02).max(4.0);
            [
                ((min_x - pad_x).clamp(0.0, pw) / pw).clamp(0.0, 1.0),
                ((min_y - pad_y).clamp(0.0, ph) / ph).clamp(0.0, 1.0),
                ((max_x + pad_x).clamp(0.0, pw) / pw).clamp(0.0, 1.0),
                ((max_y + pad_y).clamp(0.0, ph) / ph).clamp(0.0, 1.0),
            ]
        } else {
            default_bounds
        };

        ExcelPageMeta {
            content_bounds,
            picture_rects,
            links,
            cached_text: build_cached_text(raw, char_rects),
        }
    }

    fn resolve_hyperlink(
        &self,
        current_sheet: usize,
        raw: &str,
        external: bool,
    ) -> Option<LinkTarget> {
        match classify(raw) {
            LinkKind::External(uri) => Some(LinkTarget::Uri(uri)),
            LinkKind::Internal(target) => {
                if external {
                    return Some(LinkTarget::Uri(target));
                }
                let (sheet_idx, cell) = self.resolve_internal_ref(current_sheet, &target)?;
                let page_idx = self.find_page_for_cell(sheet_idx, cell)?;
                let page = u32::try_from(page_idx).ok()?;
                Some(LinkTarget::Internal {
                    page,
                    top_ratio: None,
                    left_ratio: None,
                })
            }
            LinkKind::Refused(_) => None,
        }
    }

    fn resolve_internal_ref(&self, current_sheet: usize, target: &str) -> Option<(usize, CellRef)> {
        let t = target.trim().trim_start_matches('#');
        if let Some((sheet_part, ref_part)) = t.rsplit_once('!') {
            let sheet_name = sheet_part.trim().trim_matches('\'').replace("''", "'");
            let sheet_idx = self.workbook.sheet_index(&sheet_name)?;
            let cell = parse_first_cell(ref_part)?;
            return Some((sheet_idx, cell));
        }
        if let Some(cell) = parse_first_cell(t) {
            return Some((current_sheet, cell));
        }
        let dn = self.workbook.name(t, current_sheet)?;
        let formula = dn.formula.trim().trim_start_matches('=');
        if let Some((sheet_part, ref_part)) = formula.rsplit_once('!') {
            let sheet_name = sheet_part.trim().trim_matches('\'').replace("''", "'");
            let sheet_idx = self.workbook.sheet_index(&sheet_name)?;
            let cell = parse_first_cell(ref_part)?;
            return Some((sheet_idx, cell));
        }
        let cell = parse_first_cell(formula)?;
        Some((dn.scope.unwrap_or(current_sheet), cell))
    }

    fn find_page_for_cell(&self, sheet_idx: usize, cell: CellRef) -> Option<usize> {
        let mut first_sheet_page = None;
        for (idx, spec) in self.pages.iter().enumerate() {
            let Some(lay) = self.layouts.get(spec.layout_idx) else {
                continue;
            };
            if lay.sheet != sheet_idx {
                continue;
            }
            if first_sheet_page.is_none() {
                first_sheet_page = Some(idx);
            }
            if cell.row >= spec.row_band.0
                && cell.row <= spec.row_band.1
                && cell.col >= spec.col_band.0
                && cell.col <= spec.col_band.1
            {
                return Some(idx);
            }
        }
        first_sheet_page
    }
}

fn parse_first_cell(s: &str) -> Option<CellRef> {
    let first = s.split(':').next()?.trim().replace('$', "");
    CellRef::parse(&first)
}

fn configure_landscape_fit_width(sh: &mut Sheet) {
    if sh.print.paper.trim().eq_ignore_ascii_case("ledger") {
        sh.print.paper = "Tabloid".to_string();
    }
    sh.print.orientation = Orientation::Landscape;
    sh.print.fit_to = Some((1, 0));
    sh.print.print_area = None;
}

fn value_has_content(v: &Value) -> bool {
    match v {
        Value::Empty => false,
        Value::Text(s) => !s.trim().is_empty(),
        Value::Number(_) | Value::Bool(_) | Value::Error(_) => true,
        Value::Array(items) => items.iter().any(value_has_content),
    }
}

fn cell_has_direct_content(sh: &Sheet, c: CellRef) -> bool {
    if let Some(cell) = sh.cells.get(c)
        && (value_has_content(&cell.value) || (sh.show_formulas && cell.formula.is_some()))
    {
        return true;
    }
    if sh.spill.get(&c).is_some_and(value_has_content) {
        return true;
    }
    if sh.cell_pictures.get(&c).is_some_and(|p| !p.data.is_empty()) {
        return true;
    }
    if sh
        .comments
        .get(&c)
        .is_some_and(|cm| !cm.text.trim().is_empty())
    {
        return true;
    }
    if sh.hyperlinks.contains_key(&c) {
        return true;
    }
    sh.sparklines.iter().any(|sp| sp.cell == c)
}

fn anchor_cell_range(sh: &Sheet, a: &Anchor) -> Option<RangeRef> {
    if a.width <= 0.0 || a.height <= 0.0 {
        return None;
    }
    let (x, y, w, h) = anchor_rect(sh, a);
    let start = CellRef::new(sh.row_at(y), sh.col_at(x));
    let end = CellRef::new(
        sh.row_at((y + h - 1e-3).max(y)),
        sh.col_at((x + w - 1e-3).max(x)),
    );
    Some(RangeRef::new(start, end))
}

fn extend_bounds(bounds: &mut Option<RangeRef>, r: RangeRef) {
    *bounds = Some(bounds.map_or(r, |b| b.union(&r)));
}

fn sheet_content_range(sh: &Sheet) -> Option<RangeRef> {
    let mut bounds: Option<RangeRef> = None;

    for (c, cell) in sh.cells.iter() {
        if value_has_content(&cell.value) || (sh.show_formulas && cell.formula.is_some()) {
            extend_bounds(&mut bounds, RangeRef::cell(c));
        }
    }
    for (&c, val) in &sh.spill {
        if value_has_content(val) {
            extend_bounds(&mut bounds, RangeRef::cell(c));
        }
    }
    for (&c, pic) in &sh.cell_pictures {
        if !pic.data.is_empty() {
            extend_bounds(&mut bounds, RangeRef::cell(c));
        }
    }
    for sp in &sh.sparklines {
        extend_bounds(&mut bounds, RangeRef::cell(sp.cell));
    }
    for (&c, cm) in &sh.comments {
        if !cm.text.trim().is_empty() {
            extend_bounds(&mut bounds, RangeRef::cell(c));
        }
    }
    for &c in sh.hyperlinks.keys() {
        extend_bounds(&mut bounds, RangeRef::cell(c));
    }
    for t in &sh.tables {
        if let Some(hr) = t.header_range() {
            extend_bounds(&mut bounds, hr);
        }
        if let Some(tr) = t.totals_range() {
            extend_bounds(&mut bounds, tr);
        }
    }
    for p in &sh.pivots {
        if let Some(pr) = p.last_range {
            extend_bounds(&mut bounds, pr);
        }
    }
    for ch in &sh.charts {
        if let Some(r) = anchor_cell_range(sh, &ch.anchor) {
            extend_bounds(&mut bounds, r);
        }
    }
    for im in &sh.images {
        if !im.data.is_empty()
            && let Some(r) = anchor_cell_range(sh, &im.anchor)
        {
            extend_bounds(&mut bounds, r);
        }
    }
    for shp in &sh.shapes {
        if let Some(r) = anchor_cell_range(sh, &shp.anchor) {
            extend_bounds(&mut bounds, r);
        }
    }
    for sl in &sh.slicers {
        if let Some(r) = anchor_cell_range(sh, &sl.anchor) {
            extend_bounds(&mut bounds, r);
        }
    }
    for m in &sh.merges {
        if m.is_full_rows() || m.is_full_cols() {
            continue;
        }
        if cell_has_direct_content(sh, m.start) || bounds.is_some_and(|b| b.contains(m.start)) {
            extend_bounds(&mut bounds, *m);
        }
    }

    bounds
}

fn mark_band_rows(
    min_r: &mut Option<u32>,
    max_r: &mut Option<u32>,
    row_band: (u32, u32),
    r0: u32,
    r1: u32,
) {
    let a = r0.min(r1).max(row_band.0);
    let b = r0.max(r1).min(row_band.1);
    if a <= b {
        *min_r = Some(min_r.map_or(a, |m| m.min(a)));
        *max_r = Some(max_r.map_or(b, |m| m.max(b)));
    }
}

fn trim_band_rows(sh: &Sheet, row_band: (u32, u32), col_band: (u32, u32)) -> Option<(u32, u32)> {
    let band = RangeRef::new(
        CellRef::new(row_band.0, col_band.0),
        CellRef::new(row_band.1, col_band.1),
    );
    let mut min_r: Option<u32> = None;
    let mut max_r: Option<u32> = None;

    for (c, cell) in sh.cells.iter_range(band) {
        if value_has_content(&cell.value) || (sh.show_formulas && cell.formula.is_some()) {
            mark_band_rows(&mut min_r, &mut max_r, row_band, c.row, c.row);
        }
    }
    for (&c, val) in sh.spill.range(band.start..=band.end) {
        if band.contains(c) && value_has_content(val) {
            mark_band_rows(&mut min_r, &mut max_r, row_band, c.row, c.row);
        }
    }
    for (&c, pic) in &sh.cell_pictures {
        if band.contains(c) && !pic.data.is_empty() {
            mark_band_rows(&mut min_r, &mut max_r, row_band, c.row, c.row);
        }
    }
    for sp in &sh.sparklines {
        if band.contains(sp.cell) {
            mark_band_rows(&mut min_r, &mut max_r, row_band, sp.cell.row, sp.cell.row);
        }
    }
    for (&c, cm) in &sh.comments {
        if band.contains(c) && !cm.text.trim().is_empty() {
            mark_band_rows(&mut min_r, &mut max_r, row_band, c.row, c.row);
        }
    }
    for &c in sh.hyperlinks.keys() {
        if band.contains(c) {
            mark_band_rows(&mut min_r, &mut max_r, row_band, c.row, c.row);
        }
    }
    for t in &sh.tables {
        for r in [t.header_range(), t.totals_range()].into_iter().flatten() {
            if r.intersects(&band) {
                mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
            }
        }
    }
    for p in &sh.pivots {
        if let Some(r) = p.last_range
            && r.intersects(&band)
        {
            mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
        }
    }
    for ch in &sh.charts {
        if let Some(r) = anchor_cell_range(sh, &ch.anchor)
            && r.intersects(&band)
        {
            mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
        }
    }
    for im in &sh.images {
        if !im.data.is_empty()
            && let Some(r) = anchor_cell_range(sh, &im.anchor)
            && r.intersects(&band)
        {
            mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
        }
    }
    for shp in &sh.shapes {
        if let Some(r) = anchor_cell_range(sh, &shp.anchor)
            && r.intersects(&band)
        {
            mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
        }
    }
    for sl in &sh.slicers {
        if let Some(r) = anchor_cell_range(sh, &sl.anchor)
            && r.intersects(&band)
        {
            mark_band_rows(&mut min_r, &mut max_r, row_band, r.start.row, r.end.row);
        }
    }
    for m in &sh.merges {
        if m.is_full_rows() || m.is_full_cols() || !m.intersects(&band) {
            continue;
        }
        let in_active_rows = min_r
            .zip(max_r)
            .is_some_and(|(lo, hi)| m.start.row >= lo && m.start.row <= hi);
        if cell_has_direct_content(sh, m.start) || in_active_rows {
            mark_band_rows(&mut min_r, &mut max_r, row_band, m.start.row, m.end.row);
        }
    }

    min_r.zip(max_r)
}

#[derive(Clone, Copy)]
struct FontSpec<'a> {
    family: &'a str,
    bold: bool,
    italic: bool,
}

impl<'a> FontSpec<'a> {
    const SANS: Self = Self {
        family: "Calibri",
        bold: false,
        italic: false,
    };
    const SANS_BOLD: Self = Self {
        family: "Calibri",
        bold: true,
        italic: false,
    };
}

fn resolve_face(font: FontSpec<'_>) -> wordcraft_fonts::word::Resolved {
    let family = if font.family.trim().is_empty() {
        "Calibri"
    } else {
        font.family
    };
    wordcraft_fonts::word::resolve(family, font.bold, font.italic)
}

fn measure_text_width(font: FontSpec<'_>, size: f32, text: &str) -> f32 {
    if text.is_empty() || !size.is_finite() || size <= 0.0 {
        return 0.0;
    }
    let resolved = resolve_face(font);
    let base_face = resolved.face;
    let db = FontDb::global();
    let mut width = 0.0f64;
    for ch in text.chars() {
        width += char_advance(base_face, db, size, ch);
    }
    width as f32
}

fn char_advance(base_face: FaceRef, db: &FontDb, size: f32, ch: char) -> f64 {
    let mut face = base_face;
    let mut gid = face.glyph_for(ch);
    if gid == 0
        && let Some(fb) = db.fallback_for(ch, face.id())
    {
        face = FaceRef::of(&fb);
        gid = face.glyph_for(ch);
    }
    face.advance(gid) * (f64::from(size) / face.upem.max(1.0))
}

struct ChartMeasure;

impl gridcraft_chart::Measure for ChartMeasure {
    fn text_width(&self, text: &str, size: f32, bold: bool) -> f32 {
        measure_text_width(
            if bold {
                FontSpec::SANS_BOLD
            } else {
                FontSpec::SANS
            },
            size,
            text,
        )
    }
}

struct PageCanvas {
    width: u16,
    height: u16,
    view: Affine,
    ctx: RenderContext,
    clips: usize,
}

impl PageCanvas {
    fn new(width: u16, height: u16, view: Affine) -> Self {
        let ctx = RenderContext::new_with(
            width,
            height,
            vello_cpu::RenderSettings {
                num_threads: 0,
                ..Default::default()
            },
        );
        Self {
            width,
            height,
            view,
            ctx,
            clips: 0,
        }
    }

    fn push_clip(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let r = kurbo::Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + w.max(0.0)),
            f64::from(y + h.max(0.0)),
        );
        self.ctx.set_transform(self.view);
        self.ctx.push_clip_layer(&r.to_path(0.1));
        self.clips += 1;
    }

    fn pop_clip(&mut self) {
        if self.clips > 0 {
            self.ctx.pop_layer();
            self.clips -= 1;
        }
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: Rgb) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = kurbo::Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + w),
            f64::from(y + h),
        );
        self.ctx.set_transform(self.view);
        self.ctx
            .set_paint(peniko::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
        self.ctx.fill_rect(&r);
    }

    fn rect_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, lw: f32, rgb: Rgb) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = kurbo::Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + w),
            f64::from(y + h),
        );
        self.ctx.set_transform(self.view);
        self.ctx
            .set_paint(peniko::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
        self.ctx
            .set_stroke(kurbo::Stroke::new(f64::from(lw.max(0.1))));
        self.ctx.stroke_rect(&r);
    }

    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, lw: f32, rgb: Rgb) {
        self.stroke_line_dashed(x0, y0, x1, y1, lw, rgb, &[]);
    }

    #[allow(clippy::too_many_arguments)]
    fn stroke_line_dashed(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        lw: f32,
        rgb: Rgb,
        dash: &[f32],
    ) {
        let mut p = BezPath::new();
        p.move_to((f64::from(x0), f64::from(y0)));
        p.line_to((f64::from(x1), f64::from(y1)));
        self.ctx.set_transform(self.view);
        self.ctx
            .set_paint(peniko::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
        let stroke = if dash.is_empty() {
            kurbo::Stroke::new(f64::from(lw.max(0.1)))
        } else {
            kurbo::Stroke::new(f64::from(lw.max(0.1)))
                .with_dashes(0.0, dash.iter().map(|v| f64::from(*v)))
        };
        self.ctx.set_stroke(stroke);
        self.ctx.stroke_path(&p);
    }

    fn polyline(&mut self, pts: &[[f32; 2]], lw: f32, rgb: Rgb, dash: &[f32]) {
        if pts.len() < 2 {
            return;
        }
        let mut p = BezPath::new();
        for (i, pt) in pts.iter().enumerate() {
            if i == 0 {
                p.move_to((f64::from(pt[0]), f64::from(pt[1])));
            } else {
                p.line_to((f64::from(pt[0]), f64::from(pt[1])));
            }
        }
        self.ctx.set_transform(self.view);
        self.ctx
            .set_paint(peniko::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
        let stroke = if dash.is_empty() {
            kurbo::Stroke::new(f64::from(lw.max(0.1)))
        } else {
            kurbo::Stroke::new(f64::from(lw.max(0.1)))
                .with_dashes(0.0, dash.iter().map(|v| f64::from(*v)))
        };
        self.ctx.set_stroke(stroke);
        self.ctx.stroke_path(&p);
    }

    fn polygon(&mut self, pts: &[[f32; 2]], fill: Option<Rgb>, stroke: Option<(Rgb, f32)>) {
        if pts.len() < 3 {
            return;
        }
        let mut p = BezPath::new();
        for (i, pt) in pts.iter().enumerate() {
            if i == 0 {
                p.move_to((f64::from(pt[0]), f64::from(pt[1])));
            } else {
                p.line_to((f64::from(pt[0]), f64::from(pt[1])));
            }
        }
        p.close_path();
        self.ctx.set_transform(self.view);
        if let Some(c) = fill {
            self.ctx
                .set_paint(peniko::Color::from_rgba8(c[0], c[1], c[2], 255));
            self.ctx.fill_path(&p);
        }
        if let Some((c, lw)) = stroke {
            self.ctx
                .set_paint(peniko::Color::from_rgba8(c[0], c[1], c[2], 255));
            self.ctx
                .set_stroke(kurbo::Stroke::new(f64::from(lw.max(0.1))));
            self.ctx.stroke_path(&p);
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, fill: Option<Rgb>, stroke: Option<(Rgb, f32)>) {
        if r <= 0.0 {
            return;
        }
        let circle = kurbo::Circle::new((f64::from(cx), f64::from(cy)), f64::from(r)).to_path(0.1);
        self.ctx.set_transform(self.view);
        if let Some(c) = fill {
            self.ctx
                .set_paint(peniko::Color::from_rgba8(c[0], c[1], c[2], 255));
            self.ctx.fill_path(&circle);
        }
        if let Some((c, lw)) = stroke {
            self.ctx
                .set_paint(peniko::Color::from_rgba8(c[0], c[1], c[2], 255));
            self.ctx
                .set_stroke(kurbo::Stroke::new(f64::from(lw.max(0.1))));
            self.ctx.stroke_path(&circle);
        }
    }

    fn image_pixmap(&mut self, x: f32, y: f32, w: f32, h: f32, pm: Arc<Pixmap>) {
        let (iw, ih) = (pm.width(), pm.height());
        if w <= 0.0 || h <= 0.0 || iw == 0 || ih == 0 {
            return;
        }
        let r = kurbo::Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + w),
            f64::from(y + h),
        );
        let sx = r.width() / f64::from(iw.max(1));
        let sy = r.height() / f64::from(ih.max(1));
        self.ctx.set_transform(self.view);
        self.ctx.set_paint(vello_cpu::Image {
            image: vello_cpu::ImageSource::Pixmap(pm),
            sampler: peniko::ImageSampler::default().with_quality(peniko::ImageQuality::Medium),
        });
        self.ctx.set_paint_transform(
            Affine::translate((r.x0, r.y0)) * Affine::scale_non_uniform(sx, sy),
        );
        self.ctx.fill_rect(&r);
        self.ctx.reset_paint_transform();
    }

    fn text(&mut self, x: f32, y: f32, size: f32, font: FontSpec<'_>, color: Rgb, text: &str) {
        self.text_rotated(x, y, size, font, color, text, 0.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_rotated(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        font: FontSpec<'_>,
        color: Rgb,
        text: &str,
        angle: f32,
    ) {
        if text.is_empty() || !size.is_finite() || size <= 0.0 {
            return;
        }
        let resolved = resolve_face(font);
        let base_face = resolved.face;
        let db = FontDb::global();
        self.ctx
            .set_paint(peniko::Color::from_rgba8(color[0], color[1], color[2], 255));
        let skew = if resolved.synth_italic {
            Affine::new([1.0, 0.0, -0.21, 1.0, 0.0, 0.0])
        } else {
            Affine::IDENTITY
        };
        let rot = if angle.is_finite() && angle.abs() > 1e-4 {
            Affine::rotate(f64::from(angle))
        } else {
            Affine::IDENTITY
        };
        let origin = Affine::translate((f64::from(x), f64::from(y))) * rot;
        let mut gx = 0.0f64;
        for ch in text.chars() {
            let mut face = base_face;
            let mut gid = face.glyph_for(ch);
            if gid == 0
                && let Some(fb) = db.fallback_for(ch, face.id())
            {
                face = FaceRef::of(&fb);
                gid = face.glyph_for(ch);
            }
            let k = f64::from(size) / face.upem.max(1.0);
            let outline = db.outline(&face, gid);
            if !outline.elements().is_empty() {
                self.ctx.set_transform(
                    self.view * origin * Affine::translate((gx, 0.0)) * skew * Affine::scale(k),
                );
                self.ctx.fill_path(&outline);
                if resolved.synth_bold {
                    self.ctx.set_stroke(kurbo::Stroke::new(face.upem * 0.03));
                    self.ctx.stroke_path(&outline);
                }
            }
            gx += face.advance(gid) * k;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        while self.clips > 0 {
            self.pop_clip();
        }
        self.ctx.flush();
        let mut pixels = vec![0u8; self.width as usize * self.height as usize * 4];
        let mut res = Resources::new();
        if let Some(pm) = vello_cpu::PixmapMut::new(self.width, self.height, &mut pixels) {
            self.ctx.render(pm, &mut res);
        }
        for px in pixels.as_chunks_mut::<4>().0 {
            let a = u32::from(px[3]);
            if a != 0 && a != 255 {
                for c in px.iter_mut().take(3) {
                    *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        pixels
    }
}

fn premul_pixmap(w: u32, h: u32, rgba: &[u8]) -> Option<Pixmap> {
    if w == 0 || h == 0 || w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
        return None;
    }
    if rgba.len() != (w as usize * h as usize).saturating_mul(4) {
        return None;
    }
    let data: Vec<vello_cpu::color::PremulRgba8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let a = u16::from(p[3]);
            let m = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
            vello_cpu::color::PremulRgba8 {
                r: m(p[0]),
                g: m(p[1]),
                b: m(p[2]),
                a: p[3],
            }
        })
        .collect();
    Some(Pixmap::from_parts(data, w as u16, h as u16))
}

fn rgb_of(c: Color, wb: &Workbook) -> Option<Rgb> {
    c.resolve(&wb.theme)
}

fn numfmt_rgb(c: FormatColor) -> Rgb {
    match c {
        FormatColor::Black | FormatColor::Indexed(_) => [0, 0, 0],
        FormatColor::Blue => [0, 0, 255],
        FormatColor::Cyan => [0, 255, 255],
        FormatColor::Green => [0, 128, 0],
        FormatColor::Magenta => [255, 0, 255],
        FormatColor::Red => [255, 0, 0],
        FormatColor::White => [255, 255, 255],
        FormatColor::Yellow => [255, 255, 0],
    }
}

fn blend(fg: [u8; 4]) -> Rgb {
    let a = u16::from(fg[3]);
    let mix = |c: u8| ((u16::from(c) * a + 255 * (255 - a)) / 255) as u8;
    [mix(fg[0]), mix(fg[1]), mix(fg[2])]
}

struct Look {
    style: Style,
    fill: Option<Rgb>,
    cf: Option<CfLook>,
}

fn cell_look(wb: &Workbook, si: usize, sh: &Sheet, c: CellRef, cf: &mut Option<CfCache>) -> Look {
    let mut style = match sh.table_at(c) {
        Some(t) => gridcraft_engine::tables::table_cell_style(wb, si, t, c),
        None => wb.styles.get(sh.style_id(c)).clone(),
    };
    let cfl = cf.as_mut().and_then(|cache| cache.look(wb, si, c));
    if let Some(st) = cfl.as_ref().and_then(|l| l.style.as_ref()) {
        if st.fill.pattern != PatternType::None {
            style.fill = st.fill;
        }
        if st.font.color != Color::Auto {
            style.font.color = st.font.color;
        }
        style.font.bold |= st.font.bold;
        style.font.italic |= st.font.italic;
        style.font.strike |= st.font.strike;
        if st.font.underline != Underline::None {
            style.font.underline = st.font.underline;
        }
        if st.num_fmt.as_str() != "General" {
            style.num_fmt = st.num_fmt.clone();
        }
        for (dst, src) in [
            (&mut style.borders.left, st.borders.left),
            (&mut style.borders.right, st.borders.right),
            (&mut style.borders.top, st.borders.top),
            (&mut style.borders.bottom, st.borders.bottom),
        ] {
            if !src.is_none() {
                *dst = src;
            }
        }
    }
    let mut fill = match style.fill.pattern {
        PatternType::None => None,
        PatternType::Solid => rgb_of(style.fill.fg, wb),
        _ => {
            let fg = rgb_of(style.fill.fg, wb).unwrap_or([0, 0, 0]);
            let bg = rgb_of(style.fill.bg, wb).unwrap_or([255, 255, 255]);
            Some([0, 1, 2].map(|i| ((u16::from(fg[i]) + u16::from(bg[i]) * 3) / 4) as u8))
        }
    };
    if let Some(rgb) = cfl.as_ref().and_then(|l| l.scale_fill) {
        fill = Some(rgb);
    }
    Look {
        style,
        fill,
        cf: cfl,
    }
}

#[derive(Clone, Copy)]
struct Xf {
    ox: f32,
    oy: f32,
    k: f32,
}

impl Xf {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        (self.ox + x * self.k, self.oy + y * self.k)
    }

    fn pts(&self, pts: &[[f32; 2]]) -> Vec<[f32; 2]> {
        pts.iter()
            .map(|q| [self.ox + q[0] * self.k, self.oy + q[1] * self.k])
            .collect()
    }
}

fn draw_prims(pg: &mut PageCanvas, prims: &[gridcraft_chart::Prim], xf: Xf) {
    use gridcraft_chart::{HAlign as H, Prim, VAlign as V};
    let k = xf.k;
    for p in prims {
        match p {
            Prim::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                ..
            } => {
                let (px, py) = xf.p(*x, *y);
                if let Some(f) = fill {
                    pg.fill_rect(px, py, w * k, h * k, blend(*f));
                }
                if let Some((c, lw)) = stroke {
                    pg.rect_stroke(px, py, w * k, h * k, (lw * k).max(0.1), blend(*c));
                }
            }
            Prim::Line {
                pts,
                color,
                width,
                dash,
            } => {
                let d = if *dash {
                    vec![3.0 * k, 2.0 * k]
                } else {
                    vec![]
                };
                pg.polyline(&xf.pts(pts), (width * k).max(0.1), blend(*color), &d);
            }
            Prim::Polygon { pts, fill, stroke } => {
                pg.polygon(
                    &xf.pts(pts),
                    Some(blend(*fill)),
                    stroke.map(|(c, w)| (blend(c), (w * k).max(0.1))),
                );
            }
            Prim::Wedge {
                cx,
                cy,
                r_outer,
                r_inner,
                a0,
                a1,
                fill,
                stroke,
            } => {
                let span = (a1 - a0).abs().min(std::f32::consts::TAU);
                let n = ((span / (std::f32::consts::PI / 36.0)).ceil() as usize).clamp(2, 400);
                let at = |a: f32, r: f32| [cx + r * a.sin(), cy - r * a.cos()];
                let mut pts: Vec<[f32; 2]> = (0..=n)
                    .map(|i| at(a0 + (a1 - a0) * i as f32 / n as f32, *r_outer))
                    .collect();
                if *r_inner > 0.0 {
                    pts.extend(
                        (0..=n)
                            .rev()
                            .map(|i| at(a0 + (a1 - a0) * i as f32 / n as f32, *r_inner)),
                    );
                } else {
                    pts.push([*cx, *cy]);
                }
                pg.polygon(
                    &xf.pts(&pts),
                    Some(blend(*fill)),
                    stroke.map(|(c, w)| (blend(c), (w * k).max(0.1))),
                );
            }
            Prim::Circle {
                cx,
                cy,
                r,
                fill,
                stroke,
            } => {
                let (px, py) = xf.p(*cx, *cy);
                pg.circle(
                    px,
                    py,
                    r * k,
                    fill.map(blend),
                    stroke.map(|(c, w)| (blend(c), (w * k).max(0.1))),
                );
            }
            Prim::Text {
                x,
                y,
                text,
                size,
                color,
                bold,
                align,
                valign,
                rotation,
            } => {
                let font = if *bold {
                    FontSpec::SANS_BOLD
                } else {
                    FontSpec::SANS
                };
                let size = size * k;
                let w = measure_text_width(font, size, text);
                let dx = match align {
                    H::Left => 0.0,
                    H::Center => -w / 2.0,
                    H::Right => -w,
                };
                let dy = match valign {
                    V::Top => size * 0.78,
                    V::Middle => size * 0.35,
                    V::Bottom => -size * 0.21,
                };
                let (sn, cs) = if rotation.is_finite() {
                    rotation.sin_cos()
                } else {
                    (0.0, 1.0)
                };
                let (ax, ay) = xf.p(*x, *y);
                pg.text_rotated(
                    ax + dx * cs - dy * sn,
                    ay + dx * sn + dy * cs,
                    size,
                    font,
                    blend(*color),
                    text,
                    *rotation,
                );
            }
        }
    }
}

fn wrap_lines(text: &str, font: FontSpec<'_>, size: f32, width: f32) -> Vec<String> {
    let mut out = Vec::new();
    let face = resolve_face(font).face;
    let db = FontDb::global();
    let advance = |ch| {
        if size.is_finite() && size > 0.0 {
            char_advance(face, db, size, ch)
        } else {
            0.0
        }
    };
    let space_width = advance(' ');
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0.0;
        for word in para.split(' ') {
            let word_width: f64 = word.chars().map(&advance).sum();
            if !line.is_empty() {
                if (line_width + space_width + word_width) as f32 <= width {
                    line.push(' ');
                    line.push_str(word);
                    line_width += space_width + word_width;
                    continue;
                }
                out.push(std::mem::take(&mut line));
                if out.len() == MAX_WRAP_LINES {
                    return out;
                }
                line_width = 0.0;
            }
            for ch in word.chars() {
                let char_width = advance(ch);
                if !line.is_empty() && (line_width + char_width) as f32 > width {
                    out.push(std::mem::take(&mut line));
                    if out.len() == MAX_WRAP_LINES {
                        return out;
                    }
                    line_width = 0.0;
                }
                line.push(ch);
                line_width += char_width;
            }
        }
        out.push(line);
        if out.len() == MAX_WRAP_LINES {
            return out;
        }
    }
    out
}

struct Block<'a> {
    wb: &'a Workbook,
    si: usize,
    sh: &'a Sheet,
    rows: (u32, u32),
    cols: (u32, u32),
    xs: Vec<f32>,
    ys: Vec<f32>,
    k: f32,
    scale: f32,
}

impl Block<'_> {
    fn x(&self, col: u32) -> f32 {
        if col >= self.cols.0 && col <= self.cols.1 + 1 {
            return self
                .xs
                .get((col - self.cols.0) as usize)
                .copied()
                .unwrap_or(0.0);
        }
        let x0 = self.xs.first().copied().unwrap_or(0.0);
        x0 + ((self.sh.col_left(col) - self.sh.col_left(self.cols.0)) as f32) * self.k
    }

    fn y(&self, row: u32) -> f32 {
        if row >= self.rows.0 && row <= self.rows.1 + 1 {
            return self
                .ys
                .get((row - self.rows.0) as usize)
                .copied()
                .unwrap_or(0.0);
        }
        let y0 = self.ys.first().copied().unwrap_or(0.0);
        y0 + ((self.sh.row_top(row) - self.sh.row_top(self.rows.0)) as f32) * self.k
    }

    fn rect(&self, r: RangeRef) -> (f32, f32, f32, f32) {
        let (x0, y0) = (self.x(r.start.col), self.y(r.start.row));
        (
            x0,
            y0,
            self.x(r.end.col.saturating_add(1)) - x0,
            self.y(r.end.row.saturating_add(1)) - y0,
        )
    }

    fn range(&self) -> RangeRef {
        RangeRef::new(
            CellRef::new(self.rows.0, self.cols.0),
            CellRef::new(self.rows.1, self.cols.1),
        )
    }
}

fn border_dash(style: BorderStyle, k: f32) -> Vec<f32> {
    let u = k.max(0.2);
    match style {
        BorderStyle::Dashed | BorderStyle::MediumDashed => vec![4.0 * u, 2.0 * u],
        BorderStyle::Dotted => vec![1.0 * u, 1.0 * u],
        BorderStyle::Hair => vec![0.75 * u, 0.75 * u],
        BorderStyle::DashDot | BorderStyle::MediumDashDot | BorderStyle::SlantDashDot => {
            vec![4.0 * u, 1.5 * u, 1.0 * u, 1.5 * u]
        }
        BorderStyle::DashDotDot | BorderStyle::MediumDashDotDot => {
            vec![4.0 * u, 1.5 * u, 1.0 * u, 1.5 * u, 1.0 * u, 1.5 * u]
        }
        _ => vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_border(
    pg: &mut PageCanvas,
    b: &BorderLine,
    wb: &Workbook,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    k: f32,
) {
    if b.is_none() {
        return;
    }
    let color = rgb_of(b.color, wb).unwrap_or([0, 0, 0]);
    if b.style == BorderStyle::Double {
        let off = 1.0 * k;
        let lw = (0.75 * k).max(0.2);
        let (dx, dy) = if (y0 - y1).abs() < 0.001 {
            (0.0, off)
        } else {
            (off, 0.0)
        };
        pg.stroke_line(x0 - dx, y0 - dy, x1 - dx, y1 - dy, lw, color);
        pg.stroke_line(x0 + dx, y0 + dy, x1 + dx, y1 + dy, lw, color);
        return;
    }
    let lw = (b.style.width() * k).clamp(0.25, 6.0);
    pg.stroke_line_dashed(x0, y0, x1, y1, lw, color, &border_dash(b.style, k));
}

fn font_for(style: &Style) -> FontSpec<'_> {
    FontSpec {
        family: &style.font.name,
        bold: style.font.bold,
        italic: style.font.italic,
    }
}

fn block_cells(wb: &Workbook, sh: &Sheet, block: RangeRef) -> Vec<CellRef> {
    if block.count() <= DENSE_BLOCK {
        return block.iter().collect();
    }
    let mut cells: Vec<CellRef> = sh.cells.iter_range(block).map(|(c, _)| c).collect();
    cells.extend(
        sh.spill
            .range(block.start..=block.end)
            .map(|(c, _)| *c)
            .filter(|c| block.contains(*c)),
    );
    cells.extend(
        sh.cell_pictures
            .keys()
            .copied()
            .filter(|c| block.contains(*c)),
    );
    let paints_blank = |style: &Style| {
        style.fill.pattern != PatternType::None || style.borders != Borders::default()
    };
    let visible_style = |id| paints_blank(wb.styles.get(id));
    // Only expand formatting sources intersecting this block, not its unstyled grid.
    let mut add_range = |range: RangeRef| {
        if let Some(range) = range.intersection(&block) {
            cells.extend(range.iter());
        }
    };
    for (&row, info) in sh.rows.range(block.start.row..=block.end.row) {
        if info.style.is_some_and(&visible_style) {
            add_range(RangeRef::new(
                CellRef::new(row, block.start.col),
                CellRef::new(row, block.end.col),
            ));
        }
    }
    for (&col, info) in sh.cols.range(block.start.col..=block.end.col) {
        if info.style.is_some_and(&visible_style) {
            add_range(RangeRef::new(
                CellRef::new(block.start.row, col),
                CellRef::new(block.end.row, col),
            ));
        }
    }
    for format in &sh.cond_formats {
        use gridcraft_engine::model::CfRule;
        let style = match &format.rule {
            CfRule::CellIs { style, .. }
            | CfRule::Expression { style, .. }
            | CfRule::ContainsText { style, .. }
            | CfRule::NotContainsText { style, .. }
            | CfRule::BeginsWith { style, .. }
            | CfRule::EndsWith { style, .. }
            | CfRule::Blanks { style }
            | CfRule::NoErrors { style } => Some(style),
            // The remaining rules cannot paint an unstored blank.
            _ => None,
        };
        if !style.is_some_and(|style| paints_blank(style)) {
            continue;
        }
        for &range in &format.ranges {
            add_range(range);
        }
    }
    for table in &sh.tables {
        add_range(table.range);
    }
    cells.sort_unstable();
    cells.dedup();
    cells
}

fn draw_block(
    pg: &mut PageCanvas,
    b: &Block<'_>,
    gridlines: bool,
    cf: &mut Option<CfCache>,
    image_cache: &mut ImageCache,
) {
    let (bx, by) = (
        b.xs.first().copied().unwrap_or(0.0),
        b.ys.first().copied().unwrap_or(0.0),
    );
    let (bw, bh) = (
        b.xs.last().copied().unwrap_or(bx) - bx,
        b.ys.last().copied().unwrap_or(by) - by,
    );
    if bw <= 0.0 || bh <= 0.0 {
        return;
    }
    let block = b.range();
    let sh = b.sh;
    let merges: Vec<RangeRef> = sh
        .merges
        .iter()
        .filter(|m| m.intersects(&block))
        .copied()
        .collect();
    let covered = |c: CellRef| merges.iter().any(|m| m.contains(c));
    let mut cells = block_cells(b.wb, sh, block);
    cells.retain(|c| !covered(*c));
    pg.push_clip(bx, by, bw, bh);
    if gridlines {
        let grey = [0xB0, 0xB0, 0xB0];
        let lw = (0.35 * b.scale).clamp(0.1, 0.5);
        for x in &b.xs {
            pg.stroke_line(*x, by, *x, by + bh, lw, grey);
        }
        for y in &b.ys {
            pg.stroke_line(bx, *y, bx + bw, *y, lw, grey);
        }
    }
    let skip = |c: &CellRef| {
        sh.cond_formats.is_empty()
            && sh.value_ref(*c).is_none_or(|v| v.is_empty())
            && !sh.cell_pictures.contains_key(c)
            && sh.style_id(*c) == StyleId::DEFAULT
            && (sh.tables.is_empty() || sh.table_at(*c).is_none())
    };
    let looks: Vec<(CellRef, Look)> = cells
        .iter()
        .filter(|c| !skip(c))
        .chain(merges.iter().map(|m| &m.start))
        .map(|c| (*c, cell_look(b.wb, b.si, sh, *c, cf)))
        .filter(|(c, l)| {
            l.fill.is_some()
                || l.cf.is_some()
                || merges.iter().any(|m| m.start == *c)
                || !sh.value(*c).is_empty()
                || sh.cell_pictures.contains_key(c)
                || l.style.borders != Borders::default()
        })
        .collect();
    let rect_of = |c: CellRef| -> (f32, f32, f32, f32, bool) {
        match merges.iter().find(|m| m.start == c) {
            Some(m) => {
                let (x, y, w, h) = b.rect(*m);
                (x, y, w, h, true)
            }
            None => {
                let (x, y, w, h) = b.rect(RangeRef::cell(c));
                (x, y, w, h, false)
            }
        }
    };
    for (c, l) in &looks {
        let (x, y, w, h, merged) = rect_of(*c);
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        if let Some(f) = l.fill {
            pg.fill_rect(x, y, w, h, f);
        } else if merged && gridlines {
            pg.fill_rect(x, y, w, h, [255, 255, 255]);
        }
        if merged && gridlines {
            pg.rect_stroke(
                x,
                y,
                w,
                h,
                (0.35 * b.scale).clamp(0.1, 0.5),
                [0xB0, 0xB0, 0xB0],
            );
        }
        if let Some((frac, rgb, _)) = l.cf.as_ref().and_then(|cf| cf.bar) {
            let pad = 1.5 * b.k;
            let bwid = ((w - 2.0 * pad) * frac.clamp(0.0, 1.0)).max(0.0);
            pg.fill_rect(x + pad, y + pad, bwid, (h - 2.0 * pad).max(0.0), rgb);
        }
    }
    for (c, l) in &looks {
        let (x, y, w, h, merged) = rect_of(*c);
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        let bd = &l.style.borders;
        let (right, bottom) = match merges.iter().find(|m| m.start == *c).filter(|_| merged) {
            Some(m) => (
                b.wb.styles
                    .get(sh.style_id(CellRef::new(m.start.row, m.end.col)))
                    .borders
                    .right,
                b.wb.styles
                    .get(sh.style_id(CellRef::new(m.end.row, m.start.col)))
                    .borders
                    .bottom,
            ),
            None => (bd.right, bd.bottom),
        };
        draw_border(pg, &bd.top, b.wb, x, y, x + w, y, b.k);
        draw_border(pg, &bottom, b.wb, x, y + h, x + w, y + h, b.k);
        draw_border(pg, &bd.left, b.wb, x, y, x, y + h, b.k);
        draw_border(pg, &right, b.wb, x + w, y, x + w, y + h, b.k);
        draw_border(pg, &bd.diag_down, b.wb, x, y, x + w, y + h, b.k);
        draw_border(pg, &bd.diag_up, b.wb, x, y + h, x + w, y, b.k);
    }
    for (c, l) in &looks {
        let (x, y, w, h, merged) = rect_of(*c);
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        if let Some(picture) = sh.cell_pictures.get(c) {
            draw_cell_picture(pg, &picture.data, (x, y, w, h), b.k, image_cache);
        } else {
            draw_text(pg, b, *c, l, (x, y, w, h), merged, &merges, (bx, bx + bw));
        }
    }
    pg.pop_clip();
}

fn draw_cell_picture(
    pg: &mut PageCanvas,
    data: &[u8],
    rect: (f32, f32, f32, f32),
    k: f32,
    image_cache: &mut ImageCache,
) {
    let (x, y, w, h) = rect;
    let pad = 2.0 * k;
    let (w, h) = ((w - 2.0 * pad).max(0.0), (h - 2.0 * pad).max(0.0));
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some((pm, iw, ih)) = decode_cached_pixmap(data, image_cache) else {
        return;
    };
    let fit = (w / iw as f32).min(h / ih as f32);
    let (pw, ph) = (iw as f32 * fit, ih as f32 * fit);
    pg.image_pixmap(
        x + pad + (w - pw) / 2.0,
        y + pad + (h - ph) / 2.0,
        pw,
        ph,
        pm,
    );
}

fn empty_for_overflow(sh: &Sheet, c: CellRef, merges: &[RangeRef]) -> bool {
    sh.value_ref(c).is_none_or(|v| v.is_empty()) && !merges.iter().any(|m| m.contains(c))
}

fn center_across_width(styles: &StyleTable, sh: &Sheet, c: CellRef, merges: &[RangeRef]) -> f32 {
    let mut ext = 0.0;
    let mut col = c.col + 1;
    while col < MAX_COLS && ext < 20_000.0 {
        let nc = CellRef::new(c.row, col);
        if !empty_for_overflow(sh, nc, merges)
            || styles.get(sh.style_id(nc)).align.h != HAlign::CenterAcross
        {
            break;
        }
        ext += sh.col_width(col);
        col += 1;
    }
    ext
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    pg: &mut PageCanvas,
    b: &Block<'_>,
    c: CellRef,
    l: &Look,
    rect: (f32, f32, f32, f32),
    merged: bool,
    merges: &[RangeRef],
    limits: (f32, f32),
) {
    let (x, y, w, h) = rect;
    let sh = b.sh;
    let wb = b.wb;
    let v = sh.value(c);
    if v.is_empty() || l.cf.as_ref().is_some_and(|cf| cf.hide_value) {
        return;
    }
    if !sh.show_zeros && v.as_f64() == Some(0.0) {
        return;
    }
    let st = &l.style;
    let mut size = st.font.size.clamp(1.0, 409.0) * b.scale;
    let font = font_for(st);
    let pad = PAD_PX * b.k;
    let indent = st.align.indent as f32 * 9.0 * b.k;
    let avail = (w - 2.0 * pad - indent).max(0.0);
    let code = st.num_fmt.as_str();
    let formatted = gridcraft_engine::display::format(&v, code, wb);
    let mut color = formatted
        .color
        .map(numfmt_rgb)
        .or_else(|| rgb_of(st.font.color, wb))
        .unwrap_or([0, 0, 0]);
    if l.cf
        .as_ref()
        .and_then(|x| x.style.as_ref())
        .is_some_and(|s| s.font.color != Color::Auto)
    {
        color = rgb_of(st.font.color, wb).unwrap_or(color);
    }
    let is_num = matches!(v, Value::Number(_));
    let wrap = st.align.wrap;
    let mut text = if sh.show_formulas {
        gridcraft_engine::display::cell_text(wb, sh, c)
    } else if code == "General"
        && let Value::Number(n) = v
    {
        let digit = measure_text_width(font, size, "0").max(0.1);
        let max_chars = (avail / digit).floor().max(1.0) as usize;
        format_general_fit(n, max_chars.min(11)).unwrap_or_else(|| "#".repeat(max_chars.min(255)))
    } else {
        gridcraft_engine::display::value_text_as(wb, &v, code)
    };
    if !wrap {
        text = text.replace(['\n', '\r'], " ");
    }
    if text.is_empty() {
        return;
    }
    let mut tw = measure_text_width(font, size, &text);
    if st.align.shrink && !wrap && tw > avail && tw > 0.0 {
        size = (size * avail / tw).max(1.0);
        tw = measure_text_width(font, size, &text);
    }
    if is_num && !wrap && tw > avail + 0.01 && !sh.show_formulas {
        let hash = measure_text_width(font, size, "#").max(0.1);
        text = "#".repeat(((avail / hash).floor().max(1.0) as usize).min(255));
        tw = measure_text_width(font, size, &text);
    }
    let h_align = match st.align.h {
        HAlign::General => {
            if is_num {
                HAlign::Right
            } else if matches!(v, Value::Bool(_) | Value::Error(_)) {
                HAlign::Center
            } else {
                HAlign::Left
            }
        }
        HAlign::CenterAcross | HAlign::Distributed => HAlign::Center,
        HAlign::Justify | HAlign::Fill => HAlign::Left,
        other => other,
    };
    let rot = st.align.rotation;
    let rotated = rot != 0 && rot != 255;
    let lines: Vec<String> = match (wrap, rotated) {
        (true, false) => wrap_lines(&text, font, size, avail.max(size)),
        (true, true) if rot.abs() == 90 => wrap_lines(&text, font, size, (h - 2.0 * pad).max(size)),
        (true, true) => vec![text.lines().next().unwrap_or_default().to_string()],
        (false, _) => vec![text],
    };
    let line_h = size * 1.2;
    let n = lines.len().max(1) as f32;
    let first_baseline = match st.align.v {
        VAlign::Top => y + pad + size * 0.8,
        VAlign::Center | VAlign::Justify | VAlign::Distributed => {
            y + (h - n * line_h) / 2.0 + line_h * 0.5 + size * 0.33
        }
        VAlign::Bottom => y + h - pad - size * 0.22 - (n - 1.0) * line_h,
    };
    let spans = st.align.h == HAlign::CenterAcross && !wrap && !merged && rot == 0;
    let across = if spans {
        center_across_width(&wb.styles, sh, c, merges) * b.k
    } else {
        0.0
    };
    let (mut clip_l, mut clip_r) = (x, x + w + across);
    if !wrap
        && !merged
        && !is_num
        && rot.abs() != 90
        && across <= 0.0
        && tw + 2.0 * pad + indent > w
    {
        let need = tw + 2.0 * pad + indent - w;
        let grow = |dir: i64, mut need: f32| -> f32 {
            let mut ext = 0.0;
            let mut col = i64::from(c.col) + dir;
            while need > 0.0 && col >= 0 && col < i64::from(MAX_COLS) {
                let nc = CellRef::new(c.row, col as u32);
                if !empty_for_overflow(sh, nc, merges) {
                    break;
                }
                let cw = sh.col_width(nc.col) * b.k;
                ext += cw;
                need -= cw;
                col += dir;
                if ext > 20_000.0 {
                    break;
                }
            }
            ext
        };
        match h_align {
            HAlign::Right => clip_l -= grow(-1, need),
            HAlign::Center => {
                clip_l -= grow(-1, need / 2.0);
                clip_r += grow(1, need / 2.0);
            }
            _ => clip_r += grow(1, need),
        }
    }
    let clip_l = clip_l.max(limits.0);
    let clip_r = clip_r.min(limits.1);
    pg.push_clip(clip_l, y, (clip_r - clip_l).max(0.0), h);
    if rotated {
        let deg = f32::from(rot).clamp(-90.0, 90.0);
        let angle = -deg.to_radians();
        let last = lines.len().saturating_sub(1) as f32;
        for (i, t) in lines.iter().enumerate() {
            let i = i as f32;
            let (ox, oy) = if deg > 0.0 {
                (
                    x + pad + size * 0.8 * deg.to_radians().sin() + i * line_h,
                    y + h - pad,
                )
            } else {
                (x + pad + (last - i) * line_h, y + pad + size * 0.8)
            };
            pg.text_rotated(ox, oy, size, font, color, t, angle);
        }
        pg.pop_clip();
        return;
    }
    if rot == 255 {
        let chars: Vec<char> = lines.concat().chars().take(500).collect();
        let mut yy = y + pad + size * 0.8;
        for ch in chars {
            let s = ch.to_string();
            let cw = measure_text_width(font, size, &s);
            pg.text(x + (w - cw) / 2.0, yy, size, font, color, &s);
            yy += line_h;
            if yy > y + h + line_h {
                break;
            }
        }
        pg.pop_clip();
        return;
    }
    for (i, line) in lines.iter().enumerate() {
        let lw = measure_text_width(font, size, line);
        let lx = match h_align {
            HAlign::Right => x + w - pad - lw - indent,
            HAlign::Center => x + (w + across - lw) / 2.0,
            _ => x + pad + indent,
        };
        let ly = first_baseline + i as f32 * line_h;
        if ly - size > y + h || ly < y - line_h {
            continue;
        }
        pg.text(lx, ly, size, font, color, line);
        if st.font.underline != Underline::None {
            let uw = (size * 0.06).max(0.2);
            pg.stroke_line(lx, ly + size * 0.12, lx + lw, ly + size * 0.12, uw, color);
            if matches!(
                st.font.underline,
                Underline::Double | Underline::DoubleAccounting
            ) {
                pg.stroke_line(lx, ly + size * 0.24, lx + lw, ly + size * 0.24, uw, color);
            }
        }
        if st.font.strike {
            pg.stroke_line(
                lx,
                ly - size * 0.3,
                lx + lw,
                ly - size * 0.3,
                (size * 0.06).max(0.2),
                color,
            );
        }
    }
    pg.pop_clip();
}

fn anchor_rect(sh: &Sheet, a: &Anchor) -> (f64, f64, f64, f64) {
    (
        sh.col_left(a.cell.col) + f64::from(a.dx),
        sh.row_top(a.cell.row) + f64::from(a.dy),
        f64::from(a.width.max(0.0)),
        f64::from(a.height.max(0.0)),
    )
}

const MAX_CACHED_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_IMAGE_DIM: u32 = 2048;
const MAX_DECODE_IMAGE_DIM: u32 = 16_384;
const MAX_DECODE_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_DECODE_IMAGE_ALLOC: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_CACHE_ENTRIES: usize = 256;

fn decode_cached_pixmap(data: &[u8], image_cache: &mut ImageCache) -> Option<CachedPixmap> {
    if data.is_empty() || data.len() > MAX_CACHED_IMAGE_BYTES {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let key = (hasher.finish(), data.len());
    if let Some(cached) = image_cache.get(&key) {
        return cached.clone();
    }

    let decoded = (|| {
        let (w, h) = gridcraft_engine::io::image_size(data)?;
        if w == 0
            || h == 0
            || w > MAX_DECODE_IMAGE_DIM
            || h > MAX_DECODE_IMAGE_DIM
            || u64::from(w) * u64::from(h) > MAX_DECODE_IMAGE_PIXELS
        {
            return None;
        }
        let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .ok()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DECODE_IMAGE_DIM);
        limits.max_image_height = Some(MAX_DECODE_IMAGE_DIM);
        limits.max_alloc = Some(MAX_DECODE_IMAGE_ALLOC);
        reader.limits(limits);
        let img = reader.decode().ok()?;
        let (iw, ih) = (img.width(), img.height());
        if iw == 0 || ih == 0 {
            return None;
        }
        let img = if iw > MAX_CACHED_IMAGE_DIM || ih > MAX_CACHED_IMAGE_DIM {
            img.thumbnail(MAX_CACHED_IMAGE_DIM, MAX_CACHED_IMAGE_DIM)
        } else {
            img
        };
        let rgba = img.to_rgba8();
        let pm = premul_pixmap(rgba.width(), rgba.height(), rgba.as_raw())?;
        Some((Arc::new(pm), iw, ih))
    })();

    if image_cache.len() >= MAX_IMAGE_CACHE_ENTRIES {
        image_cache.clear();
    }
    image_cache.insert(key, decoded.clone());
    decoded
}

fn draw_objects(
    pg: &mut PageCanvas,
    wb: &Workbook,
    si: usize,
    sh: &Sheet,
    b: &Block<'_>,
    image_cache: &mut ImageCache,
) {
    let block = b.range();
    let (sx0, sy0) = (sh.col_left(block.start.col), sh.row_top(block.start.row));
    let (sx1, sy1) = (
        sh.col_left(block.end.col.saturating_add(1)),
        sh.row_top(block.end.row.saturating_add(1)),
    );
    let (bx, by) = (
        b.xs.first().copied().unwrap_or(0.0),
        b.ys.first().copied().unwrap_or(0.0),
    );
    let (bw, bh) = (
        b.xs.last().copied().unwrap_or(bx) - bx,
        b.ys.last().copied().unwrap_or(by) - by,
    );
    let visible = |r: (f64, f64, f64, f64)| {
        r.2 > 0.0 && r.3 > 0.0 && r.0 < sx1 && r.0 + r.2 > sx0 && r.1 < sy1 && r.1 + r.3 > sy0
    };
    let place = |r: (f64, f64, f64, f64)| {
        (
            bx + ((r.0 - sx0) as f32) * b.k,
            by + ((r.1 - sy0) as f32) * b.k,
            r.2 as f32 * b.k,
            r.3 as f32 * b.k,
        )
    };
    pg.push_clip(bx, by, bw, bh);
    for sp in &sh.sparklines {
        if !block.contains(sp.cell) {
            continue;
        }
        let vals: Vec<Option<f64>> =
            match gridcraft_engine::calc::evaluate(wb, si, sp.cell, &sp.source) {
                Value::Array(a) => a.iter().take(10_000).map(Value::as_f64).collect(),
                v => vec![v.as_f64()],
            };
        let color = rgb_of(sp.color, wb).unwrap_or([0x15, 0x60, 0x82]);
        let (x, y, w, h) = b.rect(RangeRef::cell(sp.cell));
        let (wpx, hpx) = (w / b.k, h / b.k);
        let prims = gridcraft_chart::render_sparkline(
            sp.kind,
            &vals,
            [color[0], color[1], color[2], 255],
            sp.markers,
            wpx,
            hpx,
        );
        draw_prims(
            pg,
            &prims,
            Xf {
                ox: x,
                oy: y,
                k: b.k,
            },
        );
    }
    for im in &sh.images {
        let r = anchor_rect(sh, &im.anchor);
        if !visible(r) {
            continue;
        }
        let (x, y, w, h) = place(r);
        if let Some((pm, _, _)) = decode_cached_pixmap(&im.data, image_cache) {
            pg.image_pixmap(x, y, w, h, pm);
        }
    }
    for shp in &sh.shapes {
        let r = anchor_rect(sh, &shp.anchor);
        if !visible(r) {
            continue;
        }
        let (x, y, w, h) = place(r);
        let fill = rgb_of(shp.fill, wb);
        let line = rgb_of(shp.line, wb)
            .filter(|_| !shp.format.no_line)
            .map(|c| (c, (0.75 * b.k).max(0.25)));
        match shp.kind {
            ShapeKind::Ellipse => {
                let pts: Vec<[f32; 2]> = (0..72)
                    .map(|i| {
                        let a = i as f32 / 72.0 * std::f32::consts::TAU;
                        [
                            x + w / 2.0 + w / 2.0 * a.cos(),
                            y + h / 2.0 + h / 2.0 * a.sin(),
                        ]
                    })
                    .collect();
                pg.polygon(&pts, fill, line);
            }
            ShapeKind::Triangle => {
                pg.polygon(&[[x + w / 2.0, y], [x + w, y + h], [x, y + h]], fill, line)
            }
            ShapeKind::Line | ShapeKind::Arrow => {
                let c = line.map(|l| l.0).or(fill).unwrap_or([0, 0, 0]);
                pg.stroke_line(x, y, x + w, y + h, (1.0 * b.k).max(0.3), c);
                if shp.kind == ShapeKind::Arrow {
                    let ang = h.atan2(w);
                    let al = 8.0 * b.k;
                    let p =
                        |da: f32| [x + w - al * (ang + da).cos(), y + h - al * (ang + da).sin()];
                    pg.polygon(&[[x + w, y + h], p(0.4), p(-0.4)], Some(c), None);
                }
            }
            _ => {
                pg.polygon(
                    &[[x, y], [x + w, y], [x + w, y + h], [x, y + h]],
                    fill,
                    line,
                );
            }
        }
        if !shp.text.is_empty() {
            let fm = &shp.format;
            let text_box = shp.kind == ShapeKind::TextBox;
            let size = fm.font_size.unwrap_or(11.0).clamp(1.0, 4000.0) * b.scale;
            let font = FontSpec {
                family: fm.font.as_deref().unwrap_or("Calibri"),
                bold: fm.bold,
                italic: fm.italic,
            };
            let color = fm
                .text_color
                .and_then(|c| rgb_of(c, wb))
                .unwrap_or([0, 0, 0]);
            let align = fm.align.unwrap_or(if text_box {
                ShapeTextAlign::Left
            } else {
                ShapeTextAlign::Center
            });
            let anchor = fm.anchor.unwrap_or(if text_box {
                ShapeTextAnchor::Top
            } else {
                ShapeTextAnchor::Middle
            });
            let lines = wrap_lines(&shp.text, font, size, (w - 4.0 * b.k).max(size));
            let block_h = lines.len().min(500) as f32 * size * 1.2;
            let mut yy = match anchor {
                ShapeTextAnchor::Top => y + 2.0 * b.k,
                ShapeTextAnchor::Middle => y + (h - block_h) / 2.0,
                ShapeTextAnchor::Bottom => y + h - 2.0 * b.k - block_h,
            } + size * 0.8;
            for l in lines.iter().take(500) {
                let lw = measure_text_width(font, size, l);
                let lx = match align {
                    ShapeTextAlign::Left => x + 2.0 * b.k,
                    ShapeTextAlign::Center => x + (w - lw) / 2.0,
                    ShapeTextAlign::Right => x + w - 2.0 * b.k - lw,
                };
                pg.text(lx, yy, size, font, color, l);
                yy += size * 1.2;
            }
        }
    }
    for ch in &sh.charts {
        let r = anchor_rect(sh, &ch.anchor);
        if !visible(r) {
            continue;
        }
        let (x, y, _, _) = place(r);
        let data = gridcraft_chart::resolve(wb, si, ch);
        let prims = gridcraft_chart::render(ch, &data, r.2 as f32, r.3 as f32, &ChartMeasure);
        draw_prims(
            pg,
            &prims,
            Xf {
                ox: x,
                oy: y,
                k: b.k,
            },
        );
    }
    pg.pop_clip();
}

fn draw_headings(
    pg: &mut PageCanvas,
    b: &Block<'_>,
    col_heads: bool,
    row_heads: bool,
    at: (f32, f32),
    size: (f32, f32),
) {
    let fill = [0xF2, 0xF2, 0xF2];
    let line = [0xA0, 0xA0, 0xA0];
    let fsize = 9.0 * b.scale.max(0.1);
    let lw = (0.35 * b.scale).clamp(0.1, 0.5);
    if col_heads {
        let y = at.1;
        for col in b.cols.0..=b.cols.1 {
            let (x0, x1) = (b.x(col), b.x(col + 1));
            if x1 - x0 <= 0.0 {
                continue;
            }
            pg.fill_rect(x0, y, x1 - x0, size.1, fill);
            pg.rect_stroke(x0, y, x1 - x0, size.1, lw, line);
            let t = col_to_letters(col);
            let tw = measure_text_width(FontSpec::SANS, fsize, &t);
            pg.text(
                x0 + (x1 - x0 - tw) / 2.0,
                y + size.1 / 2.0 + fsize * 0.35,
                fsize,
                FontSpec::SANS,
                [0x40, 0x40, 0x40],
                &t,
            );
        }
    }
    if row_heads {
        let x = at.0;
        for row in b.rows.0..=b.rows.1 {
            let (y0, y1) = (b.y(row), b.y(row + 1));
            if y1 - y0 <= 0.0 {
                continue;
            }
            pg.fill_rect(x, y0, size.0, y1 - y0, fill);
            pg.rect_stroke(x, y0, size.0, y1 - y0, lw, line);
            let t = (row + 1).to_string();
            let tw = measure_text_width(FontSpec::SANS, fsize, &t);
            pg.text(
                x + (size.0 - tw) / 2.0,
                y0 + (y1 - y0) / 2.0 + fsize * 0.35,
                fsize,
                FontSpec::SANS,
                [0x40, 0x40, 0x40],
                &t,
            );
        }
    }
}

fn draw_header_footer(pg: &mut PageCanvas, lay: &SheetLayout, which: HfPage, cx: &HfContext) {
    let [l, r, _, _, hm, fm] = lay.margins;
    let pw = lay.page_w;
    let ph = lay.page_h;
    for (spec, header) in [
        (lay.settings.hf(which, false), true),
        (lay.settings.hf(which, true), false),
    ] {
        if spec.is_empty() {
            continue;
        }
        let secs = expand_header_footer(spec, cx);
        for (i, sec) in secs.iter().enumerate() {
            if sec.text.trim().is_empty() {
                continue;
            }
            let size = sec.size.unwrap_or(10.0).clamp(1.0, 72.0);
            let font = FontSpec {
                family: "Calibri",
                bold: sec.bold,
                italic: sec.italic,
            };
            let lines: Vec<&str> = sec.text.split('\n').take(20).collect();
            let n = lines.len() as f32;
            for (j, line) in lines.iter().enumerate() {
                let lw = measure_text_width(font, size, line);
                let x = match i {
                    0 => l,
                    1 => (pw - lw) / 2.0,
                    _ => pw - r - lw,
                };
                let y = if header {
                    hm + size * 0.8 + j as f32 * size * 1.2
                } else {
                    ph - fm - size * 0.22 - (n - 1.0 - j as f32) * size * 1.2
                };
                pg.text(x, y, size, font, [0, 0, 0], line);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn make_block<'a>(
    wb: &'a Workbook,
    si: usize,
    sh: &'a Sheet,
    rows: (u32, u32),
    cols: (u32, u32),
    x: f32,
    y: f32,
    lay: &SheetLayout,
) -> Block<'a> {
    let k = PX_TO_PT * lay.scale;
    let mut xs = Vec::with_capacity((cols.1 - cols.0 + 2) as usize);
    let mut acc = x;
    xs.push(acc);
    for c in cols.0..=cols.1 {
        acc += sh.col_width(c) * k;
        xs.push(acc);
    }
    let mut ys = Vec::with_capacity((rows.1 - rows.0 + 2) as usize);
    let mut acc = y;
    ys.push(acc);
    for r in rows.0..=rows.1 {
        acc += sh.row_height(r) * k;
        ys.push(acc);
    }
    Block {
        wb,
        si,
        sh,
        rows,
        cols,
        xs,
        ys,
        k,
        scale: lay.scale,
    }
}

fn segments(titles: Option<(u32, u32)>, band: (u32, u32)) -> Vec<(u32, u32)> {
    let mut v = Vec::new();
    if let Some((a, b)) = titles
        && a < band.0
    {
        v.push((a, b.min(band.0 - 1)));
    }
    v.push(band);
    v
}

fn seg_size(sh: &Sheet, seg: (u32, u32), rows: bool) -> f32 {
    if rows {
        (seg.0..=seg.1).map(|r| sh.row_height(r)).sum()
    } else {
        (seg.0..=seg.1).map(|c| sh.col_width(c)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_blank_formatting_matches_dense_rendering() {
        use gridcraft_engine::model::{CfRule, CondFormat, Fill, LineInfo, Table};

        let mut wb = Workbook::new();
        let mut row_style = Style::default();
        row_style.fill = Fill::solid(Color::rgb(20, 40, 200));
        row_style.borders.bottom = BorderLine::thin();
        let row_style = wb.styles.intern(row_style);
        let mut col_style = Style::default();
        col_style.fill = Fill::solid(Color::rgb(20, 180, 40));
        let col_style = wb.styles.intern(col_style);
        let mut border_style = Style::default();
        border_style.borders.left = BorderLine::thin();
        let border_style = wb.styles.intern(border_style);
        let mut cf_style = Style::default();
        cf_style.fill = Fill::solid(Color::rgb(200, 40, 20));
        {
            let sh = Arc::make_mut(&mut wb.sheets[0]);
            sh.default_col_width = 4.0;
            sh.default_row_height = 4.0;
            sh.rows.insert(
                10,
                LineInfo {
                    style: Some(row_style),
                    ..Default::default()
                },
            );
            sh.cols.insert(
                20,
                LineInfo {
                    style: Some(border_style),
                    ..Default::default()
                },
            );
            sh.cols.insert(
                21,
                LineInfo {
                    style: Some(col_style),
                    ..Default::default()
                },
            );
            sh.cond_formats.push(CondFormat {
                ranges: vec![RangeRef::new(CellRef::new(30, 30), CellRef::new(34, 34))],
                rule: CfRule::Blanks {
                    style: Box::new(cf_style),
                },
                priority: 1,
                stop_if_true: false,
            });
            sh.tables.push(Table {
                id: 1,
                name: "BlankTable".into(),
                range: RangeRef::new(CellRef::new(35, 5), CellRef::new(40, 8)),
                header_row: true,
                totals_row: false,
                columns: Vec::new(),
                style: "TableStyleMedium2".into(),
                banded_rows: true,
                banded_cols: false,
                first_col: false,
                last_col: false,
                filter_button: false,
                filter: Vec::new(),
                table_type: None,
                connection_id: None,
            });
        }
        let area = RangeRef::new(CellRef::new(0, 0), CellRef::new(99, 99));
        let mut lay = layout_sheet(&wb, 0, &PrintOptions::default(), Some(area)).unwrap();
        lay.scale = 1.0;
        let sh = &wb.sheets[0];
        // Compare an interior viewport: border end caps at the different block edges
        // otherwise legitimately rasterize differently.
        const VIEW_SIZE: usize = 147;
        assert!(sh.cells.is_empty());
        let render = |end, gridlines| {
            let b = make_block(&wb, 0, sh, (0, end), (0, end), 0.0, 0.0, &lay);
            let mut pg = PageCanvas::new(VIEW_SIZE as u16, VIEW_SIZE as u16, Affine::IDENTITY);
            draw_block(
                &mut pg,
                &b,
                gridlines,
                &mut Some(CfCache::new()),
                &mut ImageCache::new(),
            );
            pg.finish()
        };
        for gridlines in [false, true] {
            let dense = render(49, gridlines);
            let sparse = render(99, gridlines);
            assert!(
                sparse == dense,
                "dense and sparse interior pixels differ, gridlines: {gridlines}"
            );
            for (x, y, rgb) in [
                (4, 31, [20, 40, 200]),
                (64, 4, [20, 180, 40]),
                (91, 91, [200, 40, 20]),
            ] {
                let offset = (y * VIEW_SIZE + x) * 4;
                assert_eq!(&sparse[offset..offset + 3], &rgb);
            }
            assert_eq!(
                sparse[(106 * VIEW_SIZE + 16) * 4 + 3],
                255,
                "blank table header"
            );
        }
    }

    #[test]
    fn huge_unstyled_blocks_remain_sparse() {
        use gridcraft_engine::core::MAX_ROWS;
        use gridcraft_engine::model::{CfRule, CondFormat, LineInfo};

        let mut wb = Workbook::new();
        let font_only = wb
            .styles
            .derive(StyleId::DEFAULT, |style| style.font.bold = true);
        let font_style = wb.styles.get(font_only).clone();
        let sh = Arc::make_mut(&mut wb.sheets[0]);
        sh.rows.insert(
            0,
            LineInfo {
                style: Some(font_only),
                ..Default::default()
            },
        );
        let cell = CellRef::new(5, 7);
        sh.set_value(cell, Value::from("stored"));
        let area = RangeRef::new(CellRef::new(0, 0), CellRef::new(MAX_ROWS - 1, MAX_COLS - 1));
        sh.cond_formats.push(CondFormat {
            ranges: vec![area],
            rule: CfRule::Expression {
                formula: "TRUE".into(),
                style: Box::new(font_style),
            },
            priority: 1,
            stop_if_true: false,
        });
        assert_eq!(block_cells(&wb, &wb.sheets[0], area), vec![cell]);
    }

    #[test]
    fn internal_names_prefer_current_sheet_and_reject_other_local_scopes() {
        use gridcraft_engine::model::DefinedName;

        let mut wb = Workbook::new();
        wb.sheets.push(Arc::new(Sheet::new("Other")));
        let name = |name: &str, scope, formula: &str| DefinedName {
            name: name.into(),
            scope,
            formula: formula.into(),
            comment: String::new(),
            hidden: false,
        };
        wb.names = vec![
            name("Jump", None, "$A$1"),
            name("Jump", Some(0), "$C$3"),
            name("Jump", Some(1), "$D$4"),
            name("OtherOnly", Some(1), "$E$5"),
            name("GlobalOnly", None, "'Other'!$B$2"),
        ];
        let mut doc = ExcelDocument {
            workbook: wb,
            layouts: Vec::new(),
            pages: Vec::new(),
            meta_cache: Vec::new(),
            image_cache: RefCell::new(HashMap::new()),
            file_name: "names.xlsx".into(),
            file_path: "names.xlsx".into(),
        };
        for _ in 0..2 {
            assert_eq!(
                doc.resolve_internal_ref(0, "#jUmP"),
                Some((0, CellRef::new(2, 2)))
            );
            assert_eq!(
                doc.resolve_internal_ref(1, "Jump"),
                Some((1, CellRef::new(3, 3)))
            );
            assert_eq!(doc.resolve_internal_ref(0, "OtherOnly"), None);
            assert_eq!(
                doc.resolve_internal_ref(1, "OtherOnly"),
                Some((1, CellRef::new(4, 4)))
            );
            assert_eq!(
                doc.resolve_internal_ref(0, "GlobalOnly"),
                Some((1, CellRef::new(1, 1)))
            );
            doc.workbook.names.reverse();
        }
        doc.workbook.names.retain(|name| name.scope != Some(0));
        assert_eq!(
            doc.resolve_internal_ref(0, "Jump"),
            Some((0, CellRef::new(0, 0)))
        );
    }

    #[test]
    fn long_word_wrapping_caps_lines_and_preserves_character_boundaries() {
        let font = FontSpec::SANS;
        let size = 11.0;
        let one = measure_text_width(font, size, "W");
        let lines = wrap_lines(&"W".repeat(32_767), font, size, one * 0.5);
        assert_eq!(lines.len(), MAX_WRAP_LINES);
        assert!(lines.iter().all(|line| line == "W"));
        let width = measure_text_width(font, size, "éé");
        assert_eq!(wrap_lines("ééééé", font, size, width), ["éé", "éé", "é"]);
        let width = measure_text_width(font, size, "one two");
        assert_eq!(
            wrap_lines("one two three", font, size, width),
            ["one two", "three"]
        );
        assert_eq!(
            wrap_lines("one  two\n\nthree", font, size, 1_000.0),
            ["one  two", "", "three"]
        );
        assert_eq!(
            wrap_lines(&"\n".repeat(32_767), font, size, width).len(),
            MAX_WRAP_LINES
        );
    }

    #[test]
    fn repeated_title_rows_and_cols_appear_in_subsequent_page_text_and_bboxes() {
        let mut wb = Workbook::new();
        {
            let sh = Arc::make_mut(&mut wb.sheets[0]);
            sh.name = "Titles".to_string();
            sh.set_value(CellRef::new(0, 0), Value::from("CornerTitle"));
            sh.set_value(CellRef::new(0, 2), Value::from("Col2Header"));
            sh.set_value(CellRef::new(5, 0), Value::from("Row5Label"));
            sh.set_value(CellRef::new(5, 2), Value::from("PageBodyValue"));
            sh.print.title_rows = Some((0, 0));
            sh.print.title_cols = Some((0, 0));
        }

        let opts = PrintOptions::default();
        let mut lay = layout_sheet(
            &wb,
            0,
            &opts,
            Some(RangeRef::new(CellRef::new(0, 0), CellRef::new(5, 2))),
        )
        .expect("sheet layout should build");
        lay.row_bands = vec![(1, 4), (5, 5)];
        lay.col_bands = vec![(1, 1), (2, 2)];
        lay.pages = vec![(0, 0), (1, 1)];

        let doc = ExcelDocument {
            workbook: wb,
            layouts: vec![lay],
            pages: vec![
                ExcelPageSpec {
                    layout_idx: 0,
                    sheet_page_idx: 0,
                    row_band: (1, 4),
                    col_band: (1, 1),
                },
                ExcelPageSpec {
                    layout_idx: 0,
                    sheet_page_idx: 1,
                    row_band: (5, 5),
                    col_band: (2, 2),
                },
            ],
            meta_cache: vec![OnceCell::new(), OnceCell::new()],
            image_cache: RefCell::new(HashMap::new()),
            file_name: "titles.xlsx".to_string(),
            file_path: "titles.xlsx".to_string(),
        };

        let cached = doc.extract_cached_text(1);
        let text = &cached.raw;
        assert!(text.contains("CornerTitle"), "text was: {text:?}");
        assert!(text.contains("Col2Header"), "text was: {text:?}");
        assert!(text.contains("Row5Label"), "text was: {text:?}");
        assert!(text.contains("PageBodyValue"), "text was: {text:?}");

        let corner_pos = text.find("CornerTitle").unwrap();
        let body_pos = text.find("PageBodyValue").unwrap();
        let corner_char_idx = text[..corner_pos].chars().count();
        let body_char_idx = text[..body_pos].chars().count();
        let corner_rect = cached.char_rects[corner_char_idx].expect("corner char rect");
        let body_rect = cached.char_rects[body_char_idx].expect("body char rect");
        assert!(
            corner_rect.x1 <= body_rect.x0 + 0.5,
            "repeated title column rect {:?} should sit left of body rect {:?}",
            corner_rect,
            body_rect
        );
        assert!(
            corner_rect.y1 <= body_rect.y0 + 0.5,
            "repeated title row rect {:?} should sit above body rect {:?}",
            corner_rect,
            body_rect
        );
    }

    #[test]
    fn decode_cached_pixmap_reuses_arc_pixmap_and_rejects_oversized_png_headers() {
        let mut png_bytes = Vec::new();
        {
            let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]));
            img.write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .expect("encode tiny png");
        }

        let mut cache = ImageCache::new();
        let (pm1, w1, h1) =
            decode_cached_pixmap(&png_bytes, &mut cache).expect("first decode should succeed");
        let (pm2, w2, h2) =
            decode_cached_pixmap(&png_bytes, &mut cache).expect("cached decode should succeed");
        assert_eq!((w1, h1), (4, 3));
        assert_eq!((w2, h2), (4, 3));
        assert!(
            Arc::ptr_eq(&pm1, &pm2),
            "second decode should return cached Arc<Pixmap>"
        );
        assert_eq!(cache.len(), 1);

        // Craft a PNG header claiming 20000x20000 dimensions; it must be rejected before decode.
        let mut oversized_png = png_bytes.clone();
        oversized_png[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        oversized_png[20..24].copy_from_slice(&20_000u32.to_be_bytes());
        assert!(decode_cached_pixmap(&oversized_png, &mut cache).is_none());
    }
}
