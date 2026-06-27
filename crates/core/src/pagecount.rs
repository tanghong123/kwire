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

/// **PDF** files with fewer than this many real pages (lopdf) are flagged as
/// suspiciously short (likely a sample, wrong, or corrupt file). EPUB does NOT
/// use this — an epub's spine-section count is reflowable and a perfectly good
/// book can be a single section (see [`EPUB_LOW_TEXT_THRESHOLD`]). Exported so
/// both the engine (which logs the warning) and the UI (which renders it) apply
/// the SAME threshold.
pub const LOW_PAGE_THRESHOLD: u32 = 10;

/// **EPUB** files whose total readable text (tags stripped, whitespace not
/// counted, summed across the spine documents) is fewer than this many
/// characters are flagged as suspiciously short. This is deliberately measured
/// from TEXT, not the spine-SECTION count: a monolithic single-section epub of a
/// full book must never be mis-flagged as a stub, while a near-empty
/// "buy the full version" sample (a few hundred chars) is caught. ~1000 chars is
/// well below any real book yet far above a placeholder/sample.
pub const EPUB_LOW_TEXT_THRESHOLD: usize = 1000;

/// The unit a [`PageStats::count`] is expressed in, so callers can label the
/// number correctly. PDFs have real reader pages; EPUBs are reflowable and the
/// count is spine SECTIONS, not pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountUnit {
    /// Real paginated reader pages (PDF, via lopdf).
    Pages,
    /// Spine sections / `<itemref>` reading-order entries (EPUB).
    Sections,
}

impl CountUnit {
    /// Plural label for display: `"pages"` / `"sections"`.
    pub fn label(self) -> &'static str {
        match self {
            CountUnit::Pages => "pages",
            CountUnit::Sections => "sections",
        }
    }
}

/// Format-aware page/section statistics for a finished download.
///
/// `count` + `unit` are for DISPLAY ("64 sections", "300 pages"). `low` is the
/// format-aware "suspiciously short" flag and is computed independently of
/// `count` for EPUB (it uses readable text length, NOT the section count) — so a
/// good single-section epub is never flagged. See [`page_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageStats {
    /// The number to DISPLAY (PDF pages or EPUB spine sections).
    pub count: u32,
    /// The unit `count` is in, for labeling.
    pub unit: CountUnit,
    /// Whether the file is suspiciously short for its format (the FLAG): PDF →
    /// `count < LOW_PAGE_THRESHOLD`; EPUB → readable text `< EPUB_LOW_TEXT_THRESHOLD`.
    pub low: bool,
}

/// Count the pages (PDF) or spine sections (EPUB) of a downloaded file.
///
/// Returns `None` for unsupported extensions, unknown counts, or malformed
/// files. Never panics — every parse step is fallible and tolerated.
pub fn page_count(path: &Path) -> Option<u32> {
    page_stats(path).map(|s| s.count)
}

/// Format-aware page/section stats: the DISPLAY count + its unit + the
/// format-correct "too short" flag (see [`PageStats`]). This is the entry point
/// callers should use when they need to LABEL the count ("N sections" vs
/// "N pages") and/or apply the per-format low flag.
///
/// Returns `None` for unsupported extensions, unknown counts, or malformed
/// files. Never panics.
pub fn page_stats(path: &Path) -> Option<PageStats> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => pdf_stats(path),
        Some("epub") => epub_stats(path),
        // No (or unknown) extension — e.g. the CLI saves `<md5>.bin`. Fall back to
        // sniffing the file's magic bytes so the page check still works.
        _ => stats_sniffed(path),
    }
}

/// Detect the format by magic bytes when the extension is missing/unknown, then
/// produce [`PageStats`] (`%PDF` → PDF, `PK` zip → EPUB, else `None`).
fn stats_sniffed(path: &Path) -> Option<PageStats> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic).ok()?;
    let head = &magic[..n];
    if head.starts_with(b"%PDF") {
        pdf_stats(path)
    } else if head.starts_with(b"PK") {
        epub_stats(path)
    } else {
        None
    }
}

/// PDF stats: real page count (lopdf), flagged low below [`LOW_PAGE_THRESHOLD`].
fn pdf_stats(path: &Path) -> Option<PageStats> {
    let count = pdf_page_count(path)?;
    Some(PageStats {
        count,
        unit: CountUnit::Pages,
        low: count < LOW_PAGE_THRESHOLD,
    })
}

/// EPUB stats: the spine-SECTION count for display, but the low flag is computed
/// from total readable TEXT length (NOT the section count) so a good
/// single-section book is never mis-flagged. If the text can't be extracted the
/// file is NOT flagged (we never flag a file we couldn't measure).
fn epub_stats(path: &Path) -> Option<PageStats> {
    let count = epub_spine_count(path)?;
    let low = match epub_text_chars(path) {
        Some(chars) => chars < EPUB_LOW_TEXT_THRESHOLD,
        None => false,
    };
    Some(PageStats {
        count,
        unit: CountUnit::Sections,
        low,
    })
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

/// Sum the readable TEXT length (characters, tags stripped, whitespace not
/// counted) across an EPUB's spine documents. Used ONLY for the low-text flag —
/// the spine SECTION count ([`epub_spine_count`]) remains the display value.
///
/// Resolves each spine `<itemref idref>` through the manifest to its `href`, reads
/// that XHTML from the zip, and counts its visible non-whitespace characters.
/// Best-effort: any unreadable section contributes 0; a wholly unreadable book
/// yields `Some(0)` (which the caller treats as "flag as short"), while a zip we
/// can't open at all yields `None` (not flagged).
fn epub_text_chars(path: &Path) -> Option<usize> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;

    let container = read_zip_text(&mut zip, "META-INF/container.xml")?;
    let opf_path = find_attr_value(&container, "rootfile", "full-path")?;
    let opf = read_zip_text(&mut zip, &opf_path)?;
    let opf_dir = opf_path.rfind('/').map(|i| &opf_path[..i]).unwrap_or("");

    let items = manifest_items(&opf);
    let idrefs = spine_idrefs(&opf);

    let mut total = 0usize;
    for idref in &idrefs {
        if let Some((_, href)) = items.iter().find(|(id, _)| id == idref) {
            let full = join_zip_path(opf_dir, href);
            if let Some(doc) = read_zip_text(&mut zip, &full) {
                total += visible_text_len(&doc);
            }
        }
    }
    Some(total)
}

/// Count non-whitespace characters OUTSIDE markup tags in an (X)HTML document.
/// Whitespace is not counted (so indentation/pretty-printing doesn't inflate the
/// total) and tag interiors (`<…>`) are skipped entirely. Entities like `&amp;`
/// count their raw chars — close enough for a "is this basically empty" proxy.
fn visible_text_len(html: &str) -> usize {
    let mut count = 0usize;
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {}
            _ => count += 1,
        }
    }
    count
}

/// All `(id, href)` pairs declared in the OPF `<manifest>`, in document order.
fn manifest_items(opf: &str) -> Vec<(String, String)> {
    let manifest = slice_element(opf, "manifest").unwrap_or_default();
    let mut items = Vec::new();
    let mut rest = manifest.as_str();
    while let Some(idx) = rest.find("<item") {
        let after = &rest[idx + 5..];
        // Boundary check: `<item ` (whitespace next) so `<itemref` never matches.
        let is_item = after
            .chars()
            .next()
            .map(|c| c.is_whitespace())
            .unwrap_or(false);
        let Some(end) = after.find('>') else { break };
        if is_item {
            let tag = &after[..end];
            if let (Some(id), Some(href)) = (attr_in(tag, "id"), attr_in(tag, "href")) {
                items.push((id, href));
            }
        }
        rest = &after[end..];
    }
    items
}

/// The spine reading order: every `<itemref idref>` in the OPF `<spine>`, in order.
fn spine_idrefs(opf: &str) -> Vec<String> {
    let spine = slice_element(opf, "spine").unwrap_or_default();
    let mut ids = Vec::new();
    let mut rest = spine.as_str();
    while let Some(idx) = rest.find("<itemref") {
        let after = &rest[idx + 8..];
        let Some(end) = after.find('>') else { break };
        if let Some(idref) = attr_in(&after[..end], "idref") {
            ids.push(idref);
        }
        rest = &after[end..];
    }
    ids
}

/// Resolve a manifest `href` (relative to the OPF directory) into a zip entry
/// path, normalizing `.`/`..`/`//` and dropping any `#fragment`.
fn join_zip_path(dir: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(seg),
        }
    }
    parts.join("/")
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

    // ── page_stats: per-format unit + low flag ──────────────────────────────

    /// Write a minimal EPUB whose spine has one section per `bodies` entry, each
    /// body wrapped in `<p>…</p>`. The manifest/spine are generated to match.
    fn write_epub(dir: &Path, bodies: &[&str]) -> PathBuf {
        let mut manifest = String::new();
        let mut spine = String::new();
        for (i, _) in bodies.iter().enumerate() {
            manifest.push_str(&format!(
                "    <item id=\"c{i}\" href=\"c{i}.xhtml\" media-type=\"application/xhtml+xml\"/>\n"
            ));
            spine.push_str(&format!("    <itemref idref=\"c{i}\"/>\n"));
        }
        let opf = format!(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
{manifest}  </manifest>
  <spine>
{spine}  </spine>
</package>"#
        );
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

        let path = dir.join(format!("book_{}.epub", bodies.len()));
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(container.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        for (i, body) in bodies.iter().enumerate() {
            zw.start_file(format!("OEBPS/c{i}.xhtml"), opts).unwrap();
            let doc = format!("<?xml version=\"1.0\"?>\n<html><body><p>{body}</p></body></html>");
            zw.write_all(doc.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn epub_text_flag_does_not_fire_on_multiparagraph_single_section() {
        // A GOOD book delivered as ONE monolithic spine section: only 1 section,
        // but plenty of text. It must report unit=Sections, count=1, and NOT be
        // flagged as short — the section count must never drive the flag.
        let tmp = tempfile::tempdir().unwrap();
        let body = "word ".repeat(800); // ~4000 chars of readable text, 1 section
        let p = write_epub(tmp.path(), &[&body]);
        let stats = page_stats(&p).unwrap();
        assert_eq!(stats.count, 1, "single section");
        assert_eq!(stats.unit, CountUnit::Sections);
        assert!(
            !stats.low,
            "a multi-paragraph single-section epub must NOT be flagged short"
        );
    }

    #[test]
    fn epub_text_flag_fires_on_tiny_stub() {
        // A near-empty sample: one section with a few words → below the EPUB text
        // threshold → flagged, even though it parses fine.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_epub(tmp.path(), &["Buy the full version online."]);
        let stats = page_stats(&p).unwrap();
        assert_eq!(stats.unit, CountUnit::Sections);
        assert!(stats.low, "a tiny stub epub must be flagged short");
    }

    #[test]
    fn epub_many_sections_but_little_text_is_flagged() {
        // Several sections, each only a word or two: the SECTION count (5) is above
        // LOW_PAGE_THRESHOLD, yet the TEXT is tiny → still flagged (proves the flag
        // is text-driven, not section-driven).
        let tmp = tempfile::tempdir().unwrap();
        let p = write_epub(tmp.path(), &["a", "b", "c", "d", "e"]);
        let stats = page_stats(&p).unwrap();
        assert_eq!(stats.count, 5, "five sections displayed");
        assert!(stats.low, "tiny text across many sections still flags");
    }

    #[test]
    fn pdf_stats_unit_is_pages_and_flags_on_count() {
        let tmp = tempfile::tempdir().unwrap();
        let short = write_pdf(tmp.path(), 4);
        let long = write_pdf(tmp.path(), 42);
        let s = page_stats(&short).unwrap();
        let l = page_stats(&long).unwrap();
        assert_eq!(s.unit, CountUnit::Pages);
        assert_eq!(l.unit, CountUnit::Pages);
        assert!(s.low, "4-page pdf flagged");
        assert!(!l.low, "42-page pdf not flagged");
    }

    #[test]
    fn unit_labels_are_format_aware() {
        assert_eq!(CountUnit::Pages.label(), "pages");
        assert_eq!(CountUnit::Sections.label(), "sections");
    }
}
