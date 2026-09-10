//! Bounded in-memory PDF page rasterization for AI review material.
//!
//! Small, auditable wrapper over [`PdfiumRenderer`]: at most
//! [`RASTER_MAX_PAGES`] pages per call, capped DPI, capped pixels per page,
//! capped total PNG bytes (checked after encoding), memory-only output.
//! No OCR, no model downloads, no filesystem writes.
//!
//! Available only with the `render-pdfium` feature (plus `image` PNG support,
//! which that feature pulls in). The default library build is unaffected.

use std::collections::HashSet;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use thiserror::Error;

use super::pdfium::{PdfiumRenderer, RenderError};
use super::{RenderOptions, RenderPixelFormat};

/// Maximum number of pages per call.
pub const RASTER_MAX_PAGES: usize = 8;
/// Default output resolution in DPI (matches [`RenderOptions`]).
pub const RASTER_DEFAULT_DPI: f32 = 150.0;
/// Hard ceiling for caller-supplied DPI.
pub const RASTER_MAX_DPI: f32 = 300.0;
/// Maximum pixels of a single rendered page (checked before encoding).
pub const RASTER_MAX_PIXELS_PER_PAGE: u32 = 8_000_000;
/// Maximum total PNG bytes across all returned pages (checked after encoding).
pub const RASTER_MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// One rasterized page: PNG bytes plus true dimensions.
#[derive(Debug, Clone)]
pub struct RasteredPage {
    /// 1-based PDF page number, echoing the (deduplicated) request order.
    pub page_number: u32,
    /// PNG-encoded image bytes.
    pub png: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Fail-closed errors for bounded rasterization.
///
/// Input validation runs before PDFium is touched, so malformed requests
/// fail identically with or without a native library installed. Rendering
/// failures (including out-of-bounds pages) surface through [`RasterError::Render`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RasterError {
    /// No pages requested.
    #[error("no pages requested: pageNumbers must contain at least one 1-based page")]
    EmptyPages,
    /// More pages than [`RASTER_MAX_PAGES`].
    #[error("too many pages requested ({requested})")]
    TooManyPages {
        /// Number of (deduplicated) pages requested.
        requested: usize,
    },
    /// DPI is NaN, infinite, or not positive.
    #[error("invalid DPI ({dpi}): must be a finite number greater than zero")]
    InvalidDpi {
        /// Rejected DPI value.
        dpi: f32,
    },
    /// DPI above [`RASTER_MAX_DPI`].
    #[error("DPI ({dpi}) exceeds the maximum allowed resolution")]
    DpiTooHigh {
        /// Rejected DPI value.
        dpi: f32,
    },
    /// Rendered bitmap exceeds [`RASTER_MAX_PIXELS_PER_PAGE`].
    #[error("rendered page {page} is {width}x{height}px, exceeding the per-page pixel limit")]
    PageTooLarge {
        /// 1-based page number.
        page: u32,
        /// Rendered width in pixels.
        width: u32,
        /// Rendered height in pixels.
        height: u32,
    },
    /// Renderer returned a pixel buffer inconsistent with its dimensions.
    #[error("pixel buffer from the renderer is inconsistent with the reported dimensions")]
    InvalidBuffer,
    /// PNG encoding failed.
    #[error("PNG encoding failed")]
    PngEncode,
    /// Total encoded output exceeds [`RASTER_MAX_TOTAL_BYTES`].
    #[error("total PNG output ({bytes} bytes) exceeds the output limit")]
    TotalBytesExceeded {
        /// Encoded bytes accumulated so far.
        bytes: usize,
    },
    /// Renderer failure (bad PDF, missing library, out-of-bounds page, ...).
    #[error(transparent)]
    Render(#[from] RenderError),
}

/// Renders selected 1-indexed pages to PNG images, in memory.
///
/// Duplicate page numbers are removed deterministically (first-occurrence
/// order kept); the returned pages follow that order one-to-one. Pixel
/// format is fixed to 8-bit RGB; annotations and form fields follow the
/// [`RenderOptions`] defaults. No files are written and no model is loaded.
pub fn render_pages_png(
    pdf_bytes: &[u8],
    page_numbers: &[u32],
    dpi: Option<f32>,
) -> Result<Vec<RasteredPage>, RasterError> {
    if page_numbers.is_empty() {
        return Err(RasterError::EmptyPages);
    }
    let mut seen = HashSet::new();
    let mut pages = Vec::with_capacity(page_numbers.len());
    for &page in page_numbers {
        if seen.insert(page) {
            pages.push(page);
        }
    }
    if pages.len() > RASTER_MAX_PAGES {
        return Err(RasterError::TooManyPages {
            requested: pages.len(),
        });
    }
    let dpi = dpi.unwrap_or(RASTER_DEFAULT_DPI);
    if !dpi.is_finite() || dpi <= 0.0 {
        return Err(RasterError::InvalidDpi { dpi });
    }
    if dpi > RASTER_MAX_DPI {
        return Err(RasterError::DpiTooHigh { dpi });
    }

    let renderer = PdfiumRenderer::load()?;
    let options = RenderOptions::new()
        .dpi(dpi)
        .pixel_format(RenderPixelFormat::Rgb8)
        .max_output_bytes_per_page(u64::from(RASTER_MAX_PIXELS_PER_PAGE) * 3);
    let rendered = renderer.render_pages(pdf_bytes, &pages, None, &options)?;

    let mut out = Vec::with_capacity(rendered.len());
    let mut total_bytes: usize = 0;
    for page in rendered {
        if page.format() != RenderPixelFormat::Rgb8 {
            return Err(RasterError::InvalidBuffer);
        }
        let (number, width, height, stride, pixels_len) = (
            page.page(),
            page.width(),
            page.height(),
            page.stride(),
            page.pixels().len(),
        );
        width
            .checked_mul(height)
            .filter(|&count| count > 0 && count <= RASTER_MAX_PIXELS_PER_PAGE)
            .ok_or(RasterError::PageTooLarge {
                page: number,
                width,
                height,
            })?;
        // Rows may carry native padding: repack the active row bytes, never trust stride.
        let row_bytes = (width as usize)
            .checked_mul(3)
            .filter(|&row| row > 0 && stride >= row)
            .ok_or(RasterError::InvalidBuffer)?;
        stride
            .checked_mul(height as usize)
            .filter(|&total| total == pixels_len)
            .ok_or(RasterError::InvalidBuffer)?;
        let mut packed = Vec::with_capacity(row_bytes * height as usize);
        for row in page.pixels().chunks_exact(stride).take(height as usize) {
            packed.extend_from_slice(&row[..row_bytes]);
        }
        let image = RgbImage::from_raw(width, height, packed).ok_or(RasterError::InvalidBuffer)?;
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
            .map_err(|_| RasterError::PngEncode)?;
        total_bytes = total_bytes.saturating_add(png.len());
        if total_bytes > RASTER_MAX_TOTAL_BYTES {
            return Err(RasterError::TotalBytesExceeded { bytes: total_bytes });
        }
        out.push(RasteredPage {
            page_number: number,
            png,
            width,
            height,
        });
    }
    Ok(out)
}
