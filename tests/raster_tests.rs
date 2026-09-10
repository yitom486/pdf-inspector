//! Buffer-level tests for the bounded page raster API: validation without a
//! native library, real rendering against synthetic PDFs when PDFium is
//! available, and fail-closed resource limits. All fixtures are synthetic
//! (built with lopdf in-memory); no private documents.

#![cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]

use lopdf::{dictionary, Document, Object};
use pdf_inspector::vision::RenderError as PdfiumRenderError;
use pdf_inspector::vision::{
    render_pages_png, PdfiumRenderer, RasterError, RASTER_DEFAULT_DPI, RASTER_MAX_DPI,
    RASTER_MAX_PAGES, RASTER_MAX_PIXELS_PER_PAGE, RASTER_MAX_TOTAL_BYTES,
};

const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

fn load_renderer() -> Option<PdfiumRenderer> {
    match PdfiumRenderer::load() {
        Ok(renderer) => Some(renderer),
        Err(PdfiumRenderError::PdfiumLoad { .. }) => {
            eprintln!("skipping PDFium raster test because no native library is installed");
            None
        }
        Err(error) => panic!("failed to load PDFium: {error}"),
    }
}

/// Synthetic blank document: `pages` empty pages with US Letter MediaBox.
fn blank_pdf(pages: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    for _ in 0..pages {
        let page_id = doc.new_object_id();
        doc.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }
            .into(),
        );
        kids.push(Object::Reference(page_id));
    }
    doc.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => Object::Integer(pages as i64),
        }
        .into(),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn ihdr_dimensions(png: &[u8]) -> (u32, u32) {
    assert!(png.len() >= 24, "PNG too short for an IHDR chunk");
    assert_eq!(&png[0..8], &PNG_SIGNATURE, "missing PNG signature");
    assert_eq!(&png[12..16], b"IHDR", "first chunk must be IHDR");
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    (width, height)
}

#[test]
fn resource_limits_are_pinned() {
    assert_eq!(RASTER_MAX_PAGES, 8);
    assert_eq!(RASTER_DEFAULT_DPI, 150.0);
    assert_eq!(RASTER_MAX_DPI, 300.0);
    assert!(RASTER_MAX_PIXELS_PER_PAGE > 0);
    assert!(RASTER_MAX_TOTAL_BYTES > 0);
}

#[test]
fn validation_fails_closed_without_touching_pdfium() {
    // Empty request.
    assert!(matches!(
        render_pages_png(b"%PDF-1.4 fake", &[], None),
        Err(RasterError::EmptyPages)
    ));
    // Over the page cap (9 > 8).
    let nine: Vec<u32> = (1..=9).collect();
    assert!(matches!(
        render_pages_png(b"%PDF-1.4 fake", &nine, None),
        Err(RasterError::TooManyPages { requested: 9 })
    ));
    // Bad DPI values never reach the renderer.
    for dpi in [0.0, -72.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            matches!(
                render_pages_png(b"%PDF-1.4 fake", &[1], Some(dpi)),
                Err(RasterError::InvalidDpi { .. })
            ),
            "dpi {dpi} must be rejected"
        );
    }
    assert!(matches!(
        render_pages_png(b"%PDF-1.4 fake", &[1], Some(301.0)),
        Err(RasterError::DpiTooHigh { .. })
    ));
}

#[test]
fn renders_single_blank_page_as_png() {
    let Some(_renderer) = load_renderer() else {
        return;
    };
    let pdf = blank_pdf(1);
    let pages = render_pages_png(&pdf, &[1], None).unwrap();
    assert_eq!(pages.len(), 1);
    let page = &pages[0];
    assert_eq!(page.page_number, 1);
    assert!(page.width > 0 && page.height > 0);
    // US Letter (612x792pt) at 150 DPI scales proportionally.
    let scale = f64::from(page.width) / 612.0;
    assert!(
        (scale - 150.0 / 72.0).abs() < 0.02,
        "unexpected scale {scale}"
    );
    assert!(((f64::from(page.height) / 792.0) - scale).abs() < 0.02);
    // PNG signature plus IHDR dimensions matching the reported size.
    assert_eq!(ihdr_dimensions(&page.png), (page.width, page.height));
}

#[test]
fn two_pages_keep_request_order_and_dedupe() {
    let Some(_renderer) = load_renderer() else {
        return;
    };
    let pdf = blank_pdf(2);
    let pages = render_pages_png(&pdf, &[2, 1], None).unwrap();
    assert_eq!(
        pages.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        vec![2, 1]
    );
    let deduped = render_pages_png(&pdf, &[2, 1, 2, 1], None).unwrap();
    assert_eq!(
        deduped.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        vec![2, 1]
    );
}

#[test]
fn lower_dpi_produces_smaller_images() {
    let Some(_renderer) = load_renderer() else {
        return;
    };
    let pdf = blank_pdf(1);
    let low = render_pages_png(&pdf, &[1], Some(72.0)).unwrap();
    let high = render_pages_png(&pdf, &[1], None).unwrap();
    assert!(low[0].width < high[0].width);
    assert!(low[0].height < high[0].height);
    assert!(low[0].png.len() < high[0].png.len());
}

#[test]
fn rejects_zero_out_of_range_and_corrupt_input() {
    let Some(_renderer) = load_renderer() else {
        return;
    };
    let pdf = blank_pdf(1);
    assert!(render_pages_png(&pdf, &[0], None).is_err());
    assert!(render_pages_png(&pdf, &[2], None).is_err());
    assert!(render_pages_png(&pdf, &[u32::MAX], None).is_err());
    assert!(render_pages_png(b"not a pdf", &[1], None).is_err());
    assert!(render_pages_png(b"", &[1], None).is_err());
}
