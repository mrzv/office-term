use crate::pptx::{FitMode, Frame, LinkTarget, PageLink};
use crate::terminal::{ImagePlacement, Viewport};

pub(super) const ZOOM_DEFAULT: u16 = 100;
pub(super) const ZOOM_MIN: u16 = 100;
pub(super) const ZOOM_MAX: u16 = 400;
pub(super) const ZOOM_STEP: u16 = 25;

pub(super) fn link_at_cell(
    links: &[PageLink],
    placement: ImagePlacement,
    image_width: u32,
    image_height: u32,
    viewport_top: u16,
    column: u16,
    row: u16,
) -> Option<LinkTarget> {
    let cell = placement.source_cell(
        column,
        row.checked_sub(viewport_top)?,
        image_width,
        image_height,
    )?;
    let (x0, x1, y0, y1) = (cell.x, cell.x + cell.width, cell.y, cell.y + cell.height);
    let cell_center_x = u64::from(x0) + u64::from(x1);
    let cell_center_y = u64::from(y0) + u64::from(y1);

    links
        .iter()
        .filter(|link| {
            link.rect.left < x1
                && link.rect.right > x0
                && link.rect.top < y1
                && link.rect.bottom > y0
        })
        .min_by_key(|link| {
            let link_center_x = u64::from(link.rect.left) + u64::from(link.rect.right);
            let link_center_y = u64::from(link.rect.top) + u64::from(link.rect.bottom);
            cell_center_x
                .abs_diff(link_center_x)
                .saturating_pow(2)
                .saturating_add(cell_center_y.abs_diff(link_center_y).saturating_pow(2))
        })
        .map(|link| link.target.clone())
}

/// Keep a visible horizontal target stationary; otherwise reveal it with the least movement.
pub(super) fn horizontal_scroll_to_reveal(
    current: u32,
    page_width: u32,
    viewport_width: u32,
    target_left: f32,
    target_right: f32,
) -> u32 {
    let left = (target_left.min(target_right).floor() as u32).min(page_width.saturating_sub(1));
    let right = (target_left.max(target_right).ceil() as u32)
        .max(left.saturating_add(1))
        .min(page_width);
    let right_aligned = right.saturating_sub(viewport_width);
    let min_scroll = left.min(right_aligned);
    let max_scroll = left.max(right_aligned);
    let page_max = page_width.saturating_sub(viewport_width);
    // Continuous view can share an offset larger than this page can crop.
    // Judge visibility using this page's actual crop, without moving its wider neighbor.
    if (min_scroll..=max_scroll).contains(&current.min(page_max)) {
        current
    } else {
        current.clamp(min_scroll, max_scroll).min(page_max)
    }
}

/// Steps the zoom percentage by one increment, snapping to the `ZOOM_STEP`
/// grid and clamping to the supported range.
pub(super) fn stepped_zoom(current: u16, zoom_in: bool) -> u16 {
    let next = if zoom_in {
        current.saturating_add(ZOOM_STEP)
    } else {
        current.saturating_sub(ZOOM_STEP)
    };
    next.clamp(ZOOM_MIN, ZOOM_MAX)
}

pub(super) fn scale_zoom(value: u32, old_zoom: u16, new_zoom: u16) -> u32 {
    ((u64::from(value) * u64::from(new_zoom) + u64::from(old_zoom) / 2) / u64::from(old_zoom))
        .min(u64::from(u32::MAX)) as u32
}

pub(super) fn scale_pixels(value: u32, old_size: u32, new_size: u32) -> u32 {
    let old_size = u64::from(old_size.max(1));
    ((u64::from(value) * u64::from(new_size) + old_size / 2) / old_size).min(u64::from(u32::MAX))
        as u32
}

pub(super) fn content_axis_scroll(start: f32, end: f32, size: u32, visible: u16) -> u32 {
    (((start + end) * 0.5 * size as f32 - f32::from(visible) * 0.5)
        .round()
        .max(0.0) as u32)
        .min(size.saturating_sub(u32::from(visible)))
}

pub(super) fn content_scroll(frame: &Frame, viewport: Viewport) -> (i64, u32) {
    let [left, top, right, bottom] = frame.content_bounds;
    (
        i64::from(content_axis_scroll(
            left,
            right,
            frame.width,
            viewport.pixel_width,
        )),
        content_axis_scroll(top, bottom, frame.height, viewport.pixel_height),
    )
}

// Keep content-relative pan signed until placement: a narrower first visible
// page must not clamp the horizontal anchor of a wider following page.
pub(super) fn content_horizontal_scroll(
    scroll_x: i64,
    from: &Frame,
    to: &Frame,
    viewport: Viewport,
) -> i64 {
    if from.key.fit == FitMode::Content {
        scroll_x - content_scroll(from, viewport).0 + content_scroll(to, viewport).0
    } else {
        scroll_x
    }
}

pub(super) fn fitted_page_size(
    viewport: Viewport,
    width: f32,
    height: f32,
    content: [f32; 4],
    fit: FitMode,
    zoom: u16,
) -> (u32, u32) {
    let target_width = (u32::from(viewport.pixel_width) * u32::from(zoom) / 100).max(1);
    let target_height = (u32::from(viewport.pixel_height) * u32::from(zoom) / 100).max(1);
    // Compute f32 scaling and final rounding from the original page dimensions
    // in points: reconstructing aspect ratio from a raster amplifies rounding.
    fit.raster_size(width, height, content, target_width, target_height)
}

/// Keep the document point under the viewport center fixed, even if it belongs to
/// the next visible page. Page gaps stay one terminal row rather than scaling.
/// Geometry includes an optional content-centered horizontal origin. Rebase
/// between these origins when the center page and first visible page differ.
pub(super) fn centered_scaled_view(
    viewport: Viewport,
    mut page: u32,
    page_count: u32,
    (scroll_x, scroll_y): (i64, u32),
    scaled_size: impl Fn(u32, u32, u32) -> Option<(u32, u32, Option<u32>)>,
    continuous: bool,
    page_size: impl Fn(u32) -> Option<(u32, u32, Option<u32>)>,
) -> Result<(u32, i64, u32), u32> {
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    let half_width = i64::from(width / 2);
    let half_height = i64::from(height / 2);
    let gap = (height / u32::from(viewport.rows).max(1)).max(1);
    let mut center_y = i64::from(scroll_y) + half_height;
    let mut geometry = page_size(page).ok_or(page)?;
    let initial_origin = geometry.2.unwrap_or(0);
    if continuous {
        while page + 1 < page_count {
            let span = i64::from(geometry.1) + i64::from(gap);
            if center_y < span {
                break;
            }
            center_y -= span;
            page += 1;
            geometry = page_size(page).ok_or(page)?;
        }
    }
    let (page_width, page_height, origin) = geometry;
    let scroll_x =
        (scroll_x - i64::from(initial_origin) + i64::from(origin.unwrap_or(0))).max(0) as u32;
    let (new_width, new_height, new_origin) =
        scaled_size(page, page_width, page_height).ok_or(page)?;
    let center_x = if page_width < width {
        // Narrow images are centered by whole terminal cells; leftover pixels
        // stay on the right, not symmetrically around the image midpoint.
        let cell_width = (width / u32::from(viewport.columns).max(1)).max(1);
        let left = u32::from(viewport.place(page_width, page_height, 0, 0).left) * cell_width;
        (width / 2).saturating_sub(left).min(page_width)
    } else {
        scroll_x
            .min(page_width.saturating_sub(width))
            .saturating_add(width / 2)
            .min(page_width)
    };
    let x = (i64::from(scale_pixels(center_x, page_width, new_width)) - half_width)
        .clamp(0, i64::from(new_width.saturating_sub(width))) as u32;
    if center_y >= i64::from(page_height) && (!continuous || page + 1 == page_count) {
        center_y = i64::from(page_height / 2);
    }
    let mut y = i64::from(scale_pixels(
        center_y.min(i64::from(page_height)) as u32,
        page_height,
        new_height,
    )) - half_height;
    if continuous {
        while y < 0 && page > 0 {
            page -= 1;
            let (previous_width, previous_height, _) = page_size(page).ok_or(page)?;
            y += i64::from(
                scaled_size(page, previous_width, previous_height)
                    .ok_or(page)?
                    .1,
            ) + i64::from(gap);
        }
        while page + 1 < page_count {
            let (current_width, current_height, _) = page_size(page).ok_or(page)?;
            let span = i64::from(
                scaled_size(page, current_width, current_height)
                    .ok_or(page)?
                    .1,
            ) + i64::from(gap);
            if y < span {
                break;
            }
            y -= span;
            page += 1;
        }
    }
    let (top_page_width, top_page_height, _) = page_size(page).ok_or(page)?;
    let (_, scaled_height, top_origin) =
        scaled_size(page, top_page_width, top_page_height).ok_or(page)?;
    let x = if let (Some(new_origin), Some(top_origin)) = (new_origin, top_origin) {
        i64::from(x) - i64::from(new_origin) + i64::from(top_origin)
    } else {
        i64::from(x)
    };
    let max_y = if continuous && page + 1 < page_count {
        scaled_height.saturating_add(gap - 1)
    } else {
        scaled_height.saturating_sub(height)
    };
    Ok((page, x, y.clamp(0, i64::from(max_y)) as u32))
}
