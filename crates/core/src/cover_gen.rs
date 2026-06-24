//! Local cover GENERATION for downloaded books that have no online cover.
//!
//! The online path ([`crate::covers`]) sources covers from Open Library, but many
//! books have none there (PDFs especially, and some EPUBs). When a book has a
//! DOWNLOADED local file ([`DownloadJob::output_path`](crate::model::DownloadJob))
//! and no cover, we generate one LOCALLY, in priority order:
//!
//! 1. **EPUB** — extract the embedded cover image. Almost every epub ships one,
//!    referenced from the OPF package document (`<meta name="cover" content="…">`
//!    pointing at a manifest item, or a manifest item with
//!    `properties="cover-image"`). Pure-Rust: `zip` to read the container, a tiny
//!    hand parser for `container.xml` → OPF → the cover item's href.
//! 2. **PDF** — render the first page to an image. PDF rasterization needs a heavy
//!    native lib (pdfium/mupdf), so this is BEST-EFFORT via an external tool
//!    (`pdftoppm` or `mutool`) when one is on `PATH`; absent that it returns `None`
//!    and we fall through to (3). No compile-time native dependency.
//! 3. **Synthetic** — draw the title + author as text on a deterministic colored
//!    background ([`image`] + [`imageproc`] + an embedded font), so nothing is ever
//!    left blank.
//!
//! Every technique returns **JPEG bytes** sized to the thumbnail box, so the caller
//! writes them straight into the existing `<list>/thumbnails/<key>.jpg` cache that
//! [`crate::covers::store_thumbnail`] uses and the `cover_data_url` command serves.

use std::io::Read;
use std::path::Path;

use ab_glyph::{FontRef, PxScale};
use image::{imageops::FilterType, DynamicImage, Rgb, RgbImage};

/// Target cover dimensions (portrait, ~2:3 book ratio). The online OL thumbnails
/// are `-M` (~180px wide); we match that ballpark so generated covers sit
/// comfortably next to them in the UI.
const COVER_W: u32 = 256;
const COVER_H: u32 = 384;
const JPEG_QUALITY: u8 = 82;

/// Minimum width/height (px) for a cover to count as a REAL image. Below this it's
/// a placeholder/tracking pixel (e.g. the 1×1 transparent GIF some mirrors serve
/// as "no cover") — treat it as no cover and run the generation flow instead.
pub const MIN_USABLE_COVER_PX: u32 = 16;

/// Whether `bytes` decode as an image of usable size — i.e. a real cover, not a
/// tiny placeholder NOR a non-image file (a 43-byte 1×1 GIF, a truncated/corrupt
/// download, an HTML error page saved as `.jpg`, …). Both conditions the caller
/// asked for collapse here: undecodable → `false`, too-small → `false`.
pub fn cover_bytes_usable(bytes: &[u8]) -> bool {
    use image::GenericImageView;
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let (w, h) = img.dimensions();
            w >= MIN_USABLE_COVER_PX && h >= MIN_USABLE_COVER_PX
        }
        Err(_) => false,
    }
}

/// [`cover_bytes_usable`] for a file on disk; a missing/unreadable file is not
/// usable. Used to decide a cached `.jpg` is junk and should be regenerated.
pub fn cover_file_usable(path: &Path) -> bool {
    std::fs::read(path)
        .map(|b| cover_bytes_usable(&b))
        .unwrap_or(false)
}

/// An embedded, permissively-licensed font (Bitstream Vera / DejaVu, public
/// domain-ish) for the synthetic fallback, so text rendering never depends on a
/// system font path.
static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// Generate a cover for a downloaded book, returning JPEG bytes ready to cache.
///
/// Tries, in order: EPUB embedded cover → PDF first page (best-effort, external
/// tool) → synthetic title/author placeholder. Never fails: the synthetic path is
/// always available, so this returns `Some` for any title (it returns `None` only
/// if even synthesizing somehow fails, which the caller treats as "retry later").
///
/// `path` is the downloaded file's `output_path`; `title`/`author` feed the
/// synthetic fallback (and are otherwise unused).
pub fn generate_cover(path: &Path, title: &str, author: &str) -> Option<Vec<u8>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let extracted = match ext.as_str() {
        "epub" => extract_epub_cover(path),
        "pdf" => render_pdf_first_page(path),
        _ => None,
    };

    if let Some(bytes) = extracted {
        // Normalize whatever raw image we got (png/jpeg, arbitrary size) into a
        // thumbnail-sized JPEG. If decode fails, fall through to synthetic.
        if let Some(jpeg) = normalize_to_jpeg(&bytes) {
            return Some(jpeg);
        }
    }

    synthesize_cover(title, author)
}

// ---------------------------------------------------------------------------
// 1. EPUB embedded cover
// ---------------------------------------------------------------------------

/// Extract the embedded cover image from an EPUB, returning the RAW image bytes
/// (png/jpeg as stored in the zip). `None` when the file isn't a readable epub or
/// no cover item can be located.
///
/// EPUB layout: `META-INF/container.xml` names the OPF package document; the OPF
/// `<manifest>` lists every resource, and the cover is identified either by a
/// `<meta name="cover" content="<id>">` in `<metadata>` (EPUB2 convention) or by a
/// manifest item carrying `properties="cover-image"` (EPUB3). We resolve the
/// item's `href` relative to the OPF's own directory and read it from the zip.
pub fn extract_epub_cover(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;

    // 1. container.xml → OPF path.
    let container = read_zip_text(&mut zip, "META-INF/container.xml")?;
    let opf_path = find_attr_value(&container, "rootfile", "full-path")?;

    // 2. Read + parse the OPF manifest/metadata.
    let opf = read_zip_text(&mut zip, &opf_path)?;
    let cover_href = find_cover_href(&opf)?;

    // 3. Resolve the cover href relative to the OPF's directory and read it.
    let resolved = resolve_relative(&opf_path, &cover_href);
    read_zip_bytes(&mut zip, &resolved)
}

/// Find the cover image's manifest `href` in an OPF document, by either EPUB2
/// (`<meta name="cover" content="ID">` → `<item id="ID" href="…">`) or EPUB3
/// (`<item properties="cover-image" href="…">`) conventions.
fn find_cover_href(opf: &str) -> Option<String> {
    // EPUB3: a manifest item flagged as the cover image.
    if let Some(item) = find_element_with_attr(opf, "item", "properties", "cover-image") {
        if let Some(href) = attr_in(&item, "href") {
            return Some(href);
        }
    }
    // EPUB2: <meta name="cover" content="<item-id>"> → resolve the item by id.
    if let Some(meta) = find_meta_cover_id(opf) {
        if let Some(item) = find_element_with_attr(opf, "item", "id", &meta) {
            if let Some(href) = attr_in(&item, "href") {
                return Some(href);
            }
        }
    }
    // Last resort: any manifest item whose id LOOKS like a cover and is an image.
    find_fallback_cover_item(opf)
}

/// `<meta name="cover" content="<id>">` → the referenced item id.
fn find_meta_cover_id(opf: &str) -> Option<String> {
    for tag in iter_tags(opf, "meta") {
        let name = attr_in(&tag, "name").unwrap_or_default();
        if name.eq_ignore_ascii_case("cover") {
            if let Some(content) = attr_in(&tag, "content") {
                if !content.is_empty() {
                    return Some(content);
                }
            }
        }
    }
    None
}

/// Heuristic fallback: a manifest `<item>` whose id contains "cover" and whose
/// media-type is an image (some epubs omit both the meta and the property).
fn find_fallback_cover_item(opf: &str) -> Option<String> {
    for tag in iter_tags(opf, "item") {
        let id = attr_in(&tag, "id").unwrap_or_default().to_ascii_lowercase();
        let media = attr_in(&tag, "media-type").unwrap_or_default();
        if id.contains("cover") && media.starts_with("image/") {
            if let Some(href) = attr_in(&tag, "href") {
                return Some(href);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 2. PDF first page (best-effort, external tool)
// ---------------------------------------------------------------------------

/// Render the FIRST page of a PDF to a PNG using whatever rasterizer is on `PATH`
/// (`pdftoppm` from poppler, or `mutool` from mupdf). Returns the PNG bytes, or
/// `None` when no tool is available or rendering fails — the caller then falls back
/// to the synthetic cover. Deliberately has NO compile-time native dependency:
/// PDF cover rendering is a bonus that "lights up" when a tool is installed.
pub fn render_pdf_first_page(path: &Path) -> Option<Vec<u8>> {
    render_with_pdftoppm(path).or_else(|| render_with_mutool(path))
}

/// Resolve an external tool to an absolute path. A Finder-launched macOS app gets a
/// minimal PATH (no `/opt/homebrew/bin`, `/usr/local/bin`), so a bare command name
/// would silently fail there even with the tool installed. Check the common install
/// dirs first, then fall back to the bare name (PATH resolution) for shell launches.
fn tool_path(name: &str) -> std::path::PathBuf {
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/opt/local/bin",
        "/usr/bin",
        "/bin",
    ] {
        let p = std::path::Path::new(dir).join(name);
        if p.exists() {
            return p;
        }
    }
    std::path::PathBuf::from(name)
}

/// Render page 1 to a temp PNG, then read it back. pdftoppm's stdout ("-") output is
/// unreliable across poppler builds (some write 0 bytes to stdout with `-singlefile`),
/// so go through a temp file: `pdftoppm … <pdf> <stem>` writes `<stem>.png`.
fn render_with_pdftoppm(path: &Path) -> Option<Vec<u8>> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let stem = std::env::temp_dir().join(format!(
        "lgdl-pdfcov-{}-{:x}",
        std::process::id(),
        h.finish()
    ));
    let png = stem.with_extension("png");
    let status = std::process::Command::new(tool_path("pdftoppm"))
        .args(["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to-x"])
        .arg(COVER_W.to_string())
        .args(["-scale-to-y", "-1"])
        .arg(path)
        .arg(&stem) // output prefix; -singlefile appends ".png"
        .status()
        .ok()?;
    let bytes = std::fs::read(&png).ok().filter(|b| !b.is_empty());
    let _ = std::fs::remove_file(&png);
    if status.success() {
        bytes
    } else {
        None
    }
}

/// `mutool draw -F png -o - -w <W> <pdf> 1` renders page 1 as PNG to stdout.
fn render_with_mutool(path: &Path) -> Option<Vec<u8>> {
    let out = std::process::Command::new(tool_path("mutool"))
        .args(["draw", "-F", "png", "-o", "-", "-w"])
        .arg(COVER_W.to_string())
        .arg(path)
        .arg("1")
        .output()
        .ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 3. Synthetic fallback (title + author on a colored background)
// ---------------------------------------------------------------------------

/// Synthesize a deterministic cover: the title (wrapped) and author drawn in white
/// on a solid background whose hue is derived from the title, so the same book
/// always gets the same color. Returns JPEG bytes. `None` only if the embedded
/// font fails to load (it won't in practice).
pub fn synthesize_cover(title: &str, author: &str) -> Option<Vec<u8>> {
    let font = FontRef::try_from_slice(FONT_BYTES).ok()?;
    let (bg, fg) = palette_for(title);
    let mut img: RgbImage = RgbImage::from_pixel(COVER_W, COVER_H, bg);

    // A subtle top "spine" band for a book-ish feel.
    for y in 0..6 {
        for x in 0..COVER_W {
            img.put_pixel(x, y, fg);
        }
    }

    let title = if title.trim().is_empty() {
        "Untitled"
    } else {
        title.trim()
    };

    // Title: wrapped, larger scale, near the top third.
    let title_scale = PxScale::from(28.0);
    let title_lines = wrap_text(title, 16, 5);
    let mut y = 70i32;
    for line in &title_lines {
        imageproc::drawing::draw_text_mut(&mut img, fg, 18, y, title_scale, &font, line);
        y += 36;
    }

    // Author: smaller, lower down.
    let author = author.trim();
    if !author.is_empty() {
        let author_scale = PxScale::from(20.0);
        let author_lines = wrap_text(author, 22, 3);
        let mut ay = (COVER_H as i32) - 90;
        for line in &author_lines {
            imageproc::drawing::draw_text_mut(&mut img, fg, 18, ay, author_scale, &font, line);
            ay += 26;
        }
    }

    encode_jpeg(&DynamicImage::ImageRgb8(img))
}

/// A deterministic (background, foreground) color pair from a title hash. The
/// background is a mid-tone so white-ish text reads; the foreground is a lighter
/// tint of the same hue for the band/text.
fn palette_for(title: &str) -> (Rgb<u8>, Rgb<u8>) {
    // Cheap stable hash (FNV-1a) over the title bytes.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in title.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // Hue from the hash; fixed saturation/value for a readable mid-tone.
    let hue = (h % 360) as f32;
    let bg = hsv_to_rgb(hue, 0.45, 0.55);
    let fg = Rgb([245, 245, 245]);
    (bg, fg)
}

/// Minimal HSV→RGB (s,v in 0..=1) for the deterministic background.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgb<u8> {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Rgb([
        (((r + m) * 255.0).round()) as u8,
        (((g + m) * 255.0).round()) as u8,
        (((b + m) * 255.0).round()) as u8,
    ])
}

/// Greedily wrap `text` to at most `max_chars` per line and `max_lines` lines
/// (the last kept line is ellipsized if text remains).
fn wrap_text(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.len() + 1 + word.len() <= max_chars {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
            if lines.len() == max_lines {
                break;
            }
        }
    }
    if lines.len() < max_lines && !cur.is_empty() {
        lines.push(cur);
    }
    // Ellipsize if we ran out of room with text remaining.
    if lines.len() == max_lines {
        let joined_len: usize = lines.iter().map(|l| l.len() + 1).sum();
        if joined_len < text.len() {
            if let Some(last) = lines.last_mut() {
                let keep = max_chars.saturating_sub(1).min(last.len());
                last.truncate(keep);
                last.push('…');
            }
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Image normalization
// ---------------------------------------------------------------------------

/// Decode arbitrary image bytes (png/jpeg), resize to the cover box preserving
/// aspect (fit-inside, centered on the background), and re-encode as JPEG. `None`
/// when the bytes don't decode as a supported image.
fn normalize_to_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let fitted = img.resize(COVER_W, COVER_H, FilterType::Lanczos3);
    // Center on a black canvas so non-2:3 covers don't distort.
    let mut canvas: RgbImage = RgbImage::from_pixel(COVER_W, COVER_H, Rgb([20, 20, 24]));
    let fitted = fitted.to_rgb8();
    let ox = (COVER_W.saturating_sub(fitted.width())) / 2;
    let oy = (COVER_H.saturating_sub(fitted.height())) / 2;
    image::imageops::overlay(&mut canvas, &fitted, ox as i64, oy as i64);
    encode_jpeg(&DynamicImage::ImageRgb8(canvas))
}

/// Encode a [`DynamicImage`] as JPEG bytes at the cover quality.
fn encode_jpeg(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    img.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(buf.into_inner())
}

// ---------------------------------------------------------------------------
// Tiny zip + XML helpers (no extra deps; the OPF/container are small + simple)
// ---------------------------------------------------------------------------

/// Read a named zip entry as UTF-8 text (lossy). `None` if absent/unreadable.
fn read_zip_text<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let bytes = read_zip_bytes(zip, name)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read a named zip entry's raw bytes. Tries the exact name, then a
/// case-insensitive / leading-`./`-tolerant match (epub paths are usually exact,
/// but be forgiving). `None` if absent/unreadable.
fn read_zip_bytes<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    // Resolve the actual stored name first (an immutable scan), then read it.
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
    Some(buf)
}

/// Resolve `href` relative to the directory of `base` (an OPF path inside the zip).
/// e.g. base `OEBPS/content.opf`, href `images/cover.jpg` → `OEBPS/images/cover.jpg`.
fn resolve_relative(base: &str, href: &str) -> String {
    let href = href.split(['#', '?']).next().unwrap_or(href);
    if href.starts_with('/') {
        return href.trim_start_matches('/').to_string();
    }
    let dir = match base.rfind('/') {
        Some(i) => &base[..i],
        None => "",
    };
    let mut parts: Vec<&str> = Vec::new();
    if !dir.is_empty() {
        parts.extend(dir.split('/'));
    }
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Find the first `<tag …>` element with `attr="value"` (value compared
/// case-insensitively for the property/name conventions) and return the whole tag
/// text. A deliberately small scanner — the OPF/container are tiny, well-formed XML.
fn find_element_with_attr(xml: &str, tag: &str, attr: &str, value: &str) -> Option<String> {
    for t in iter_tags(xml, tag) {
        if let Some(v) = attr_in(&t, attr) {
            // `properties` can be a space-separated token list; match a token.
            if v.eq_ignore_ascii_case(value)
                || v.split_whitespace()
                    .any(|tok| tok.eq_ignore_ascii_case(value))
            {
                return Some(t);
            }
        }
    }
    None
}

/// Find the value of `attr` on the FIRST `<tag …>` element in `xml`.
fn find_attr_value(xml: &str, tag: &str, attr: &str) -> Option<String> {
    iter_tags(xml, tag)
        .into_iter()
        .find_map(|t| attr_in(&t, attr))
}

/// Collect the text of every `<tag …>` opening element in `xml`. Returns each
/// element's full `<…>` slice (attributes intact). Case-insensitive on the name.
fn iter_tags(xml: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;
    let needle_lower = tag.to_ascii_lowercase();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find the matching '>'.
            if let Some(end_rel) = xml[i..].find('>') {
                let end = i + end_rel;
                let inner = &xml[i + 1..end];
                // Strip a leading namespace prefix (e.g. `opf:item`).
                let name: String = inner
                    .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .next()
                    .unwrap_or("")
                    .rsplit(':')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if name == needle_lower {
                    out.push(xml[i..=end].to_string());
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Extract `attr="…"` (or `attr='…'`) from a single element's text. The attr name
/// is matched case-insensitively and allowing a namespace prefix.
fn attr_in(element: &str, attr: &str) -> Option<String> {
    let lower = element.to_ascii_lowercase();
    let attr_lower = attr.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(&attr_lower) {
        let pos = search_from + rel;
        // Ensure it's a whole attribute name: preceded by whitespace/`<`/`:` and
        // followed (after optional spaces) by `=`.
        let before_ok = pos == 0
            || matches!(
                lower.as_bytes()[pos - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'<' | b':'
            );
        let after = pos + attr_lower.len();
        let mut j = after;
        while j < lower.len() && matches!(lower.as_bytes()[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if before_ok && j < lower.len() && lower.as_bytes()[j] == b'=' {
            // Move past '=' and optional spaces to the quote.
            let mut k = j + 1;
            while k < element.len() && matches!(element.as_bytes()[k], b' ' | b'\t' | b'\n' | b'\r')
            {
                k += 1;
            }
            if k < element.len() && matches!(element.as_bytes()[k], b'"' | b'\'') {
                let quote = element.as_bytes()[k];
                let val_start = k + 1;
                if let Some(rel_end) = element[val_start..].find(quote as char) {
                    return Some(element[val_start..val_start + rel_end].to_string());
                }
            }
        }
        search_from = pos + attr_lower.len();
    }
    None
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal in-memory EPUB (zip) with a known embedded cover image and
    /// the EPUB2 `<meta name="cover">` convention, write it to a temp file, and
    /// return its path + the exact cover bytes we embedded.
    fn make_epub(dir: &Path, kind: &str) -> (std::path::PathBuf, Vec<u8>) {
        // A tiny but valid PNG (1x1) as the "cover image".
        let cover_png = {
            let img = RgbImage::from_pixel(2, 3, Rgb([10, 120, 200]));
            let mut buf = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };

        let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

        let opf = match kind {
            "epub3" => {
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata></metadata>
  <manifest>
    <item id="cov" href="images/cover.png" media-type="image/png" properties="cover-image"/>
    <item id="t1" href="text.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
</package>"#
            }
            _ => {
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata>
    <meta name="cover" content="cov"/>
  </metadata>
  <manifest>
    <item id="cov" href="images/cover.png" media-type="image/png"/>
    <item id="t1" href="text.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
</package>"#
            }
        };

        let path = dir.join(format!("book-{kind}.epub"));
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(container.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.start_file("OEBPS/images/cover.png", opts).unwrap();
        zw.write_all(&cover_png).unwrap();
        zw.finish().unwrap();

        (path, cover_png)
    }

    #[test]
    fn extracts_epub2_meta_cover() {
        let dir = std::env::temp_dir().join(format!("covgen-{}-2", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, expected) = make_epub(&dir, "epub2");
        let got = extract_epub_cover(&path).expect("cover extracted");
        assert_eq!(got, expected, "extracted bytes match the embedded cover");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_epub3_properties_cover() {
        let dir = std::env::temp_dir().join(format!("covgen-{}-3", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, expected) = make_epub(&dir, "epub3");
        let got = extract_epub_cover(&path).expect("cover extracted");
        assert_eq!(got, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_cover_normalizes_epub_to_jpeg() {
        let dir = std::env::temp_dir().join(format!("covgen-{}-g", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, _) = make_epub(&dir, "epub2");
        let bytes = generate_cover(&path, "Some Title", "Some Author").expect("cover");
        // Decodes as a JPEG of the cover dimensions.
        let img = image::load_from_memory(&bytes).expect("valid image");
        assert_eq!((img.width(), img.height()), (COVER_W, COVER_H));
        // JPEG magic.
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_cover_returns_valid_jpeg_of_expected_size() {
        let bytes = synthesize_cover("The Pragmatic Programmer", "Hunt & Thomas").expect("jpeg");
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let img = image::load_from_memory(&bytes).expect("valid image");
        assert_eq!((img.width(), img.height()), (COVER_W, COVER_H));
    }

    #[test]
    fn synthesize_cover_is_deterministic_color() {
        let (bg1, _) = palette_for("Treasure Island");
        let (bg2, _) = palette_for("Treasure Island");
        let (bg3, _) = palette_for("Different Title");
        assert_eq!(bg1, bg2, "same title → same background");
        assert_ne!(
            bg1, bg3,
            "different title → different background (very likely)"
        );
    }

    #[test]
    fn resolve_relative_handles_opf_dir_and_dotdot() {
        assert_eq!(
            resolve_relative("OEBPS/content.opf", "images/cover.jpg"),
            "OEBPS/images/cover.jpg"
        );
        assert_eq!(
            resolve_relative("OEBPS/content.opf", "../cover.jpg"),
            "cover.jpg"
        );
        assert_eq!(resolve_relative("content.opf", "cover.jpg"), "cover.jpg");
    }

    #[test]
    fn attr_in_parses_quoted_values() {
        let el = r#"<item id="cov" href='images/cover.png' media-type="image/png"/>"#;
        assert_eq!(attr_in(el, "id").as_deref(), Some("cov"));
        assert_eq!(attr_in(el, "href").as_deref(), Some("images/cover.png"));
        assert_eq!(attr_in(el, "media-type").as_deref(), Some("image/png"));
        assert_eq!(attr_in(el, "missing"), None);
    }

    #[test]
    fn find_cover_href_both_conventions() {
        let epub2 = r#"<package><metadata><meta name="cover" content="c1"/></metadata>
            <manifest><item id="c1" href="cover.jpg" media-type="image/jpeg"/></manifest></package>"#;
        assert_eq!(find_cover_href(epub2).as_deref(), Some("cover.jpg"));
        let epub3 = r#"<package><manifest>
            <item id="c1" href="img/c.png" media-type="image/png" properties="cover-image"/>
            </manifest></package>"#;
        assert_eq!(find_cover_href(epub3).as_deref(), Some("img/c.png"));
    }

    #[test]
    fn wrap_text_wraps_and_ellipsizes() {
        let lines = wrap_text("one two three four five", 9, 5);
        assert!(lines.len() >= 2);
        // Tight box forces an ellipsis on the last kept line.
        let tight = wrap_text("alpha beta gamma delta epsilon zeta eta", 6, 2);
        assert_eq!(tight.len(), 2);
        assert!(tight.last().unwrap().ends_with('…'));
    }

    #[test]
    fn cover_usable_rejects_placeholder_and_garbage_but_accepts_a_real_cover() {
        // The exact 43-byte 1×1 transparent GIF some mirrors serve as "no cover".
        let gif_1x1: [u8; 43] = [
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0xf0, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00, 0x2c,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00,
            0x3b,
        ];
        assert!(
            !cover_bytes_usable(&gif_1x1),
            "1×1 placeholder GIF must be rejected"
        );
        assert!(
            !cover_bytes_usable(b"<html>404 not found</html>"),
            "non-image must be rejected"
        );
        assert!(!cover_bytes_usable(&[]), "empty must be rejected");
        // A synthesized cover is a real, full-size image.
        let real =
            synthesize_cover("Treasure Island", "Robert Louis Stevenson").expect("synthesize");
        assert!(
            cover_bytes_usable(&real),
            "a real generated cover must be accepted"
        );
    }
}
