use crate::pptx::Frame;
use crate::terminal::{ImagePlacement, Viewport};
use flate2::read::ZlibDecoder;
use png::{BitDepth, ColorType, Encoder};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Page<'a> {
    pub frame: &'a Frame,
    pub placement: ImagePlacement,
    /// Page's first terminal row relative to the content viewport.
    pub row: u16,
}

fn pixel_count(viewport: Viewport) -> io::Result<usize> {
    if viewport.pixel_width == 0
        || viewport.pixel_height == 0
        || viewport.columns == 0
        || viewport.rows == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "viewport dimensions must be non-zero",
        ));
    }
    usize::from(viewport.pixel_width)
        .checked_mul(usize::from(viewport.pixel_height))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "viewport is too large"))
}

pub fn blank_overlay(viewport: Viewport) -> io::Result<Vec<u8>> {
    Ok(vec![0; pixel_count(viewport)?])
}

fn blend(dst: &mut [u8], src: &[u8]) {
    let alpha = u32::from(src[3]);
    if alpha == 0 {
        return;
    }
    let inverse = 255 - alpha;
    for i in 0..3 {
        dst[i] = ((u32::from(src[i]) * alpha + u32::from(dst[i]) * inverse + 127) / 255) as u8;
    }
    dst[3] = 255;
}

fn decompress(frame: &Frame) -> io::Result<Vec<u8>> {
    let expected = usize::try_from(frame.width)
        .ok()
        .and_then(|w| {
            usize::try_from(frame.height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame dimensions overflow"))?;
    let decoder = ZlibDecoder::new(frame.compressed_rgba.as_slice());
    let mut rgba = Vec::with_capacity(expected);
    decoder.take(expected as u64 + 1).read_to_end(&mut rgba)?;
    if rgba.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cached frame has invalid RGBA length",
        ));
    }
    Ok(rgba)
}

fn compose(
    viewport: Viewport,
    pages: &[Page<'_>],
    overlay: &[u8],
    background: [u8; 3],
) -> io::Result<Vec<u8>> {
    let expected = pixel_count(viewport)?;
    if overlay.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "overlay dimensions do not match viewport",
        ));
    }
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    let mut output = vec![0; expected];
    for pixel in output.as_chunks_mut::<4>().0 {
        pixel[..3].copy_from_slice(&background);
        pixel[3] = 255;
    }
    let cell_w = (width / u32::from(viewport.columns.max(1))).max(1);
    let cell_h = (height / u32::from(viewport.rows.max(1))).max(1);
    for page in pages {
        let frame = page.frame;
        let rgba = decompress(frame)?;
        let crop = page.placement.crop.unwrap_or(crate::kitty::Crop {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        });
        let sx0 = crop.x.min(frame.width);
        let sy0 = crop.y.min(frame.height);
        let sx1 = crop.x.saturating_add(crop.width).min(frame.width);
        let sy1 = crop.y.saturating_add(crop.height).min(frame.height);
        if sx0 >= sx1 || sy0 >= sy1 {
            continue;
        }
        let native = page.placement.native_cell.is_some();
        let dst_x = u32::from(page.placement.left).saturating_mul(cell_w);
        let dst_y = u32::from(page.row).saturating_mul(cell_h);
        let dst_w = if native {
            sx1 - sx0
        } else {
            u32::from(page.placement.columns).saturating_mul(cell_w)
        };
        let dst_h = if native {
            sy1 - sy0
        } else {
            u32::from(page.placement.rows).saturating_mul(cell_h)
        };
        let dst_y = dst_y.saturating_add(if native { page.placement.offset_y } else { 0 });
        for dy in 0..dst_h {
            let y = dst_y.saturating_add(dy);
            if y >= height {
                break;
            }
            let sy = sy0 + (u64::from(dy) * u64::from(sy1 - sy0) / u64::from(dst_h.max(1))) as u32;
            for dx in 0..dst_w {
                let x = dst_x.saturating_add(dx);
                if x >= width {
                    break;
                }
                let sx =
                    sx0 + (u64::from(dx) * u64::from(sx1 - sx0) / u64::from(dst_w.max(1))) as u32;
                let source = ((sy * frame.width + sx) * 4) as usize;
                let target = ((y * width + x) * 4) as usize;
                output[target..target + 4].copy_from_slice(&rgba[source..source + 4]);
            }
        }
    }
    for (dst, src) in output
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(overlay.as_chunks::<4>().0)
    {
        blend(dst, src);
    }
    Ok(output)
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn save(
    path: &Path,
    viewport: Viewport,
    pages: &[Page<'_>],
    overlay: &[u8],
    background: [u8; 3],
) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output path must be absolute",
        ));
    }
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("screenshot output already exists: {}", path.display()),
        ));
    }
    let rgba = compose(viewport, pages, overlay, background)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output has no parent",
        )
    })?;
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output has no filename",
        )
    })?;
    let temp = loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".pptxterm-shot-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let (temp_path, file) = temp;
    let encoded = (|| -> io::Result<()> {
        let mut encoder = Encoder::new(
            file,
            u32::from(viewport.pixel_width),
            u32::from(viewport.pixel_height),
        );
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;
        Ok(())
    })();
    if let Err(error) = encoded {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::hard_link(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    let _ = fs::remove_file(temp_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pptx::{DarkModeStyle, DocumentRevision, FitMode, RenderKey};
    use std::time::Duration;

    #[test]
    fn screenshot_uses_page_crop_and_viewport_origin() {
        let mut source = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                source.extend_from_slice(&[x * 40, y * 40, 0, 255]);
            }
        }
        let compressed_rgba = crate::kitty::compress_rgba(&source).unwrap();
        let revision_file = tempfile::NamedTempFile::new().unwrap();
        let frame = Frame {
            key: RenderKey {
                document_id: 0,
                page: 0,
                width: 4,
                height: 4,
                zoom: 100,
                fit: FitMode::Page,
                invert: false,
                dark_mode_style: DarkModeStyle::new([0, 0, 0], [255, 255, 255]),
                search_request_id: 0,
                search_highlight: [255, 255, 0],
                link_mode: false,
                link_highlight: [0, 255, 255],
                selected_link_ordinal: None,
            },
            revision: DocumentRevision::read(revision_file.path()).unwrap(),
            width: 4,
            height: 4,
            page_width_pt: 4.0,
            page_height_pt: 4.0,
            content_bounds: [0.0, 0.0, 1.0, 1.0],
            compressed_rgba,
            render_elapsed: Duration::ZERO,
            dark_mode_elapsed: None,
            highlight_elapsed: None,
            compression_elapsed: Duration::ZERO,
            generation: 0,
            links: Vec::new(),
        };
        let viewport = Viewport {
            columns: 2,
            rows: 2,
            pixel_width: 4,
            pixel_height: 4,
            top: 0,
            status_row: 2,
        };
        let placement = ImagePlacement {
            left: 0,
            columns: 2,
            rows: 2,
            crop: Some(crate::kitty::Crop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }),
            scroll_x: 1,
            scroll_y: 1,
            native_cell: Some((1, 1)),
            offset_y: 1,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("viewport.png");
        save(
            &path,
            viewport,
            &[Page {
                frame: &frame,
                placement,
                row: 0,
            }],
            &[0; 4 * 4 * 4],
            [0, 0, 0],
        )
        .unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().expect("bounded PNG dimensions")];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (4, 4));
        let pixels = &pixels[..info.buffer_size()];
        assert_eq!(&pixels[..4], &[0, 0, 0, 255]);
        assert_eq!(&pixels[4..8], &[0, 0, 0, 255]);
        assert_eq!(&pixels[4 * 4..4 * 4 + 4], &[40, 40, 0, 255]);
        assert_eq!(&pixels[4 * 4 + 4..4 * 4 + 8], &[80, 40, 0, 255]);
        assert_eq!(&pixels[2 * 4 * 4..2 * 4 * 4 + 4], &[40, 80, 0, 255]);
        assert_eq!(&pixels[3 * 4 * 4..3 * 4 * 4 + 4], &[0, 0, 0, 255]);
    }
}
