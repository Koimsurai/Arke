//! PDF pages for the preview pane and Quick Look, rendered by Windows itself (`Windows.Data.Pdf`,
//! built into Windows 10 and 11). No third-party PDF library: the page is rendered to PNG in an
//! in-memory stream and decoded with the `image` crate. Background threads only.

use std::path::Path;

use arke_core::thumbs::Thumbnail;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::HSTRING;

use crate::shell::{PdfRender, ShellError};

/// Largest side rendered, whatever the caller asks for (a 4K Quick Look at 200 % stays sane).
const MAX_SIDE: u32 = 4096;

/// Pixel size that fits a `page_w` × `page_h` page (any unit) inside `max`, keeping its aspect
/// ratio. Never 0, never past [`MAX_SIDE`].
pub(crate) fn fit_page(page_w: f32, page_h: f32, max: (u32, u32)) -> (u32, u32) {
    #[allow(clippy::cast_precision_loss)]
    let (mw, mh) = (
        max.0.clamp(1, MAX_SIDE) as f32,
        max.1.clamp(1, MAX_SIDE) as f32,
    );
    let (pw, ph) = (page_w.max(1.), page_h.max(1.));
    let scale = (mw / pw).min(mh / ph);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let side = |v: f32| ((v * scale).round() as u32).clamp(1, MAX_SIDE);
    (side(pw), side(ph))
}

/// Renders one page. Opens the file through the Storage API, so it must be local content (the
/// caller skips cloud placeholders).
pub(crate) fn render_page(
    path: &Path,
    page: u32,
    max: (u32, u32),
) -> Result<PdfRender, ShellError> {
    // SAFETY: this background thread's own COM init; CoUninitialize only on success.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let result = render(path, page, max);
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

fn render(path: &Path, page: u32, max: (u32, u32)) -> Result<PdfRender, ShellError> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
    let document = PdfDocument::LoadFromFileAsync(&file)?.join()?;
    let pages = document.PageCount()?;
    if pages == 0 {
        return Err(ShellError::Unsupported);
    }
    let pdf_page = document.GetPage(page.min(pages - 1))?;
    // Size in device-independent pixels, already rotated
    let size = pdf_page.Size()?;
    let (width, height) = fit_page(size.Width, size.Height, max);
    let options = PdfPageRenderOptions::new()?;
    options.SetDestinationWidth(width)?;
    options.SetDestinationHeight(height)?;
    let stream = InMemoryRandomAccessStream::new()?;
    pdf_page
        .RenderWithOptionsToStreamAsync(&stream, &options)?
        .join()?;
    let _ = pdf_page.Close();
    let len = u32::try_from(stream.Size()?).map_err(|_| ShellError::Unsupported)?;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    let loaded = reader.LoadAsync(len)?.join()?;
    let mut png = vec![0; loaded as usize];
    reader.ReadBytes(&mut png)?;
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|e| ShellError::Io(std::io::Error::other(e)))?
        .into_rgba8();
    Ok(PdfRender {
        pages,
        image: Thumbnail {
            width: decoded.width(),
            height: decoded.height(),
            rgba: decoded.into_raw(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_fit_the_box() {
        // A4 portrait into a wide box: height-bound.
        assert_eq!(fit_page(595., 842., (1000, 500)), (353, 500));
        // Landscape into a tall box: width-bound.
        assert_eq!(fit_page(842., 595., (400, 1000)), (400, 283));
        // Degenerate input still gives a drawable size, capped at the max side.
        assert_eq!(fit_page(0., 0., (0, 0)), (1, 1));
        assert_eq!(fit_page(10., 20., (100_000, 100_000)), (2048, MAX_SIDE));
    }

    /// A minimal two-page PDF with a correct cross-reference table.
    fn tiny_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] >>",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (ix, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", ix + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// The real Windows renderer, on a file in the temp folder.
    #[test]
    fn renders_pages_with_windows() {
        let dir = std::env::temp_dir().join(format!("arke-pdf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two pages.pdf");
        std::fs::write(&path, tiny_pdf()).unwrap();
        let first = render_page(&path, 0, (400, 400)).unwrap();
        assert_eq!(first.pages, 2);
        assert_eq!((first.image.width, first.image.height), (400, 200));
        assert_eq!(first.image.rgba.len(), 400 * 200 * 4);
        // Blank page: white
        assert_eq!(&first.image.rgba[..4], &[255, 255, 255, 255]);
        let second = render_page(&path, 1, (400, 400)).unwrap();
        assert_eq!((second.image.width, second.image.height), (200, 400));
        // Past the end: the last page
        let past = render_page(&path, 9, (400, 400)).unwrap();
        assert_eq!((past.image.width, past.image.height), (200, 400));
        // Not a PDF: an error, not a panic
        std::fs::write(dir.join("bad.pdf"), b"not a pdf").unwrap();
        assert!(render_page(&dir.join("bad.pdf"), 0, (100, 100)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
