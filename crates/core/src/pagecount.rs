//! Best-effort page/section counting for downloaded books.
//!
//! After a download finishes and its md5 verifies, the orchestrator counts the
//! file's pages so the UI can warn when a "book" is suspiciously short (a
//! sample, the wrong file, or a corrupt download). The count is advisory: a file
//! that can't be parsed yields `None` and is simply not flagged — we never
//! panic and never block the download from completing.
//!
//! * **PDF** (`.pdf`): a real page count via [`lopdf`], which parses the document
//!   structure (xref tables/streams, object streams) robustly rather than
//!   eyeballing `/Type /Page` markers — those can live inside compressed object
//!   streams that a naive text scan would miss.
//! * **EPUB** (`.epub`): the number of `<itemref>` entries in the OPF `<spine>`
//!   (the reading order), located via `META-INF/container.xml`. This is a
//!   "section" count, not a paginated page count (epub is reflowable and has no
//!   fixed pages), but it is the right cheap proxy for "is this basically empty".
//! * Anything else → `None`.

use std::path::Path;

/// Variations with fewer than this many pages/sections are flagged as
/// suspiciously short (likely a sample, wrong, or corrupt file). Exported so both
/// the engine (which logs the warning) and the UI (which renders it) apply the
/// SAME threshold.
pub const LOW_PAGE_THRESHOLD: u32 = 10;

/// Count the pages (PDF) or spine sections (EPUB) of a downloaded file.
///
/// Returns `None` for unsupported extensions, unknown counts, or malformed
/// files. Never panics — every parse step is fallible and tolerated.
pub fn page_count(path: &Path) -> Option<u32> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => pdf_page_count(path),
        Some("epub") => epub_spine_count(path),
        // No (or unknown) extension — e.g. the CLI saves `<md5>.bin`. Fall back to
        // sniffing the file's magic bytes so the page check still works.
        _ => page_count_sniffed(path),
    }
}

/// Detect the format by magic bytes when the extension is missing/unknown.
///
/// Reads the first 4 bytes: `%PDF` → PDF page count, `PK` (zip local-file
/// header) → best-effort EPUB spine count (a non-epub zip simply yields `None`).
/// Every IO/parse step is fallible and tolerated — never panics.
fn page_count_sniffed(path: &Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic).ok()?;
    let head = &magic[..n];
    if head.starts_with(b"%PDF") {
        pdf_page_count(path)
    } else if head.starts_with(b"PK") {
        epub_spine_count(path)
    } else {
        None
    }
}

/// Real PDF page count via `lopdf`. `lopdf::Document::get_pages` walks the page
/// tree (resolving inherited `/Count`, object streams, and xref streams), so it
/// is robust to the compressed/linearized PDFs that a hand parser mishandles.
fn pdf_page_count(path: &Path) -> Option<u32> {
    let doc = lopdf::Document::load(path).ok()?;
    let n = doc.get_pages().len();
    u32::try_from(n).ok().filter(|&n| n > 0)
}

/// Count `<itemref>` entries in the OPF `<spine>` of an EPUB. Reuses the same
/// zip → `container.xml` → OPF flow the cover extractor uses. Best-effort: any
/// missing/malformed step yields `None`.
fn epub_spine_count(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;

    let container = read_zip_text(&mut zip, "META-INF/container.xml")?;
    let opf_path = find_attr_value(&container, "rootfile", "full-path")?;
    let opf = read_zip_text(&mut zip, &opf_path)?;

    // Isolate the <spine>…</spine> region, then count its <itemref> entries (the
    // reading order). If there's no spine, we can't say anything → None.
    let spine = slice_element(&opf, "spine")?;
    let n = count_tag(&spine, "itemref");
    u32::try_from(n).ok().filter(|&n| n > 0)
}

// --- tiny zip/xml helpers (mirrors of cover_gen.rs, kept local so this module
// is self-contained and doesn't widen cover_gen's public surface) ---

fn read_zip_text<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    use std::io::Read;
    let want = name.trim_start_matches("./");
    let actual = (0..zip.len()).find_map(|i| {
        let f = zip.by_index(i).ok()?;
        let n = f.name().to_string();
        if n == name || n.trim_start_matches("./") == want {
            Some(n)
        } else {
            None
        }
    })?;
    let mut f = zip.by_name(&actual).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Find the value of `attr` on the first `<tag …>` element in `xml`. Matches a
/// real tag boundary so `<rootfile …>` is not confused with `<rootfiles>`.
fn find_attr_value(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut rest = xml;
    loop {
        let idx = rest.find(&open)?;
        let after = &rest[idx + open.len()..];
        let is_boundary = after
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false);
        if is_boundary {
            let element_end = after.find('>')?;
            return attr_in(&after[..element_end], attr);
        }
        rest = after;
    }
}

/// Read `attr="value"` (or single-quoted) out of a single start-tag string.
fn attr_in(element: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let idx = element.find(&needle)?;
    let after = &element[idx + needle.len()..];
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after = &after[1..];
    let end = after.find(quote)?;
    Some(after[..end].to_string())
}

/// Return the inner text of the first `<tag …>…</tag>` element, or `None`.
fn slice_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after_open = xml[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = xml[after_open..].find(&close)? + after_open;
    Some(xml[after_open..end].to_string())
}

/// Count occurrences of `<tag` start-tags in `xml` (self-closing or not).
fn count_tag(xml: &str, tag: &str) -> usize {
    let needle = format!("<{tag}");
    let mut count = 0usize;
    let mut rest = xml;
    while let Some(idx) = rest.find(&needle) {
        // Ensure it's a real tag boundary (next char is whitespace, '>' or '/').
        let after = &rest[idx + needle.len()..];
        if after
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false)
        {
            count += 1;
        }
        rest = after;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Write a minimal, valid multi-page PDF with `n` pages to `dir`. Hand-built
    /// (no encoder) so the test has no extra deps; lopdf must parse it.
    fn write_pdf(dir: &Path, n: usize) -> PathBuf {
        // Object 1: Catalog, 2: Pages, 3..: one Page each.
        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i)).collect();
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            n
        ));
        for _ in 0..n {
            objects.push("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string());
        }

        let mut body = String::from("%PDF-1.5\n");
        let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, obj));
        }
        let xref_start = body.len();
        body.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        body.push_str("0000000000 65535 f \n");
        for off in &offsets {
            body.push_str(&format!("{:010} 00000 n \n", off));
        }
        body.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objects.len() + 1,
            xref_start
        ));

        let path = dir.join(format!("doc_{n}.pdf"));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn pdf_page_count_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_pdf(tmp.path(), 3);
        assert_eq!(page_count(&p), Some(3));
    }

    #[test]
    fn pdf_under_threshold_flags_over_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let short = write_pdf(tmp.path(), 4);
        let long = write_pdf(tmp.path(), 42);

        let short_n = page_count(&short).unwrap();
        let long_n = page_count(&long).unwrap();
        assert!(short_n < LOW_PAGE_THRESHOLD, "4-page pdf must be low");
        assert!(long_n >= LOW_PAGE_THRESHOLD, "42-page pdf must not be low");
    }

    #[test]
    fn sniffs_pdf_by_magic_when_extension_is_bin() {
        // The CLI saves `<md5>.bin`, so the extension fast-path can't fire. The
        // content-sniff fallback must still recognize the PDF and count pages —
        // matching the count it would report for the `.pdf`-named original.
        let tmp = tempfile::tempdir().unwrap();
        let pdf = write_pdf(tmp.path(), 7);
        let by_ext = page_count(&pdf).unwrap();

        let bin = tmp.path().join("aabbccddeeff00112233445566778899.bin");
        std::fs::copy(&pdf, &bin).unwrap();
        assert_eq!(page_count(&bin), Some(by_ext));
        assert_eq!(by_ext, 7);
    }

    #[test]
    fn sniffs_epub_zip_by_magic_when_extension_is_bin() {
        // A zip (PK magic) with an EPUB structure must sniff to the spine count
        // even without an `.epub` extension.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("00112233445566778899aabbccddeeff.bin");
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>"#;
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(container.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.finish().unwrap();

        assert_eq!(page_count(&path), Some(2));
    }

    #[test]
    fn unknown_magic_bin_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("mystery.bin");
        std::fs::write(&p, b"not a pdf or zip").unwrap();
        assert_eq!(page_count(&p), None);
    }

    #[test]
    fn unsupported_extension_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("notes.txt");
        std::fs::write(&p, b"hello").unwrap();
        assert_eq!(page_count(&p), None);
    }

    #[test]
    fn malformed_pdf_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("broken.pdf");
        std::fs::write(&p, b"not really a pdf at all").unwrap();
        assert_eq!(page_count(&p), None);
    }

    #[test]
    fn epub_spine_count_matches_itemrefs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("book.epub");
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="c3" href="c3.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
    <itemref idref="c3"/>
  </spine>
</package>"#;
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(container.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.finish().unwrap();

        assert_eq!(page_count(&path), Some(3));
    }

    #[test]
    fn malformed_epub_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("broken.epub");
        std::fs::write(&p, b"PK not a zip").unwrap();
        assert_eq!(page_count(&p), None);
    }
}
