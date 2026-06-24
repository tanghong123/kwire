//! Cover-image lookup + local thumbnail storage (DESIGN: book thumbnails).
//!
//! Most libgen rows carry no cover (libgen.li only emits covers for COMIC rows),
//! so we source covers from **Open Library**: search a title/author (or ISBN),
//! take the first doc's `cover_i`, and build a `covers.openlibrary.org` URL. The
//! lookup reuses [`crate::series::OlTransport`] so the exact parse path is
//! replayable offline from recorded fixtures (mirroring `series.rs`).
//!
//! Once a cover URL is chosen for a book (Open Library here, or a libgen
//! landing-page / Anna's Archive cover surfaced elsewhere), [`store_thumbnail`]
//! downloads the image into `<list folder>/thumbnails/<key>.jpg` so the UI can
//! load a durable LOCAL file (via Tauri's asset protocol) and fall back to the
//! remote URL only when no local copy exists.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::series::{LiveOlTransport, OlTransport, RecordingOlTransport, ReplayOlTransport};

const OL_BASE: &str = "https://openlibrary.org";
const OL_COVERS: &str = "https://covers.openlibrary.org";

/// Looks a book's cover up on Open Library. All HTTP goes through an
/// [`OlTransport`] so the lookup is offline-testable (replay) / recordable.
pub struct CoverClient {
    transport: Box<dyn OlTransport>,
}

impl CoverClient {
    pub fn new(transport: Box<dyn OlTransport>) -> Self {
        CoverClient { transport }
    }

    /// Convenience: live Open Library transport.
    pub fn live() -> Self {
        Self::new(Box::new(LiveOlTransport::new()))
    }

    /// Convenience: replay transport over a fixtures dir (offline).
    pub fn replay(fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::new(Box::new(ReplayOlTransport::new(fixtures_dir)))
    }

    /// Convenience: live transport that records responses into `fixtures_dir`.
    pub fn recording(fixtures_dir: impl Into<PathBuf>) -> Self {
        let live: Box<dyn OlTransport> = Box::new(LiveOlTransport::new());
        Self::new(Box::new(RecordingOlTransport::new(live, fixtures_dir)))
    }

    /// Resolve a cover URL for a book. Prefers an ISBN lookup when one is given
    /// (most precise), else searches by title + author. Returns `Ok(None)` when
    /// Open Library has no cover for the book; network errors propagate as `Err`.
    pub async fn cover_url(
        &self,
        title: &str,
        author: &str,
        isbn: Option<&str>,
    ) -> Result<Option<String>> {
        // An ISBN, when present, resolves a cover directly off the covers CDN
        // (no search needed). Open Library serves `/b/isbn/<isbn>-M.jpg`.
        if let Some(isbn) = isbn {
            let isbn = isbn.trim();
            if !isbn.is_empty() {
                return Ok(Some(cover_url_for_isbn(isbn)));
            }
        }

        let url = format!(
            "{OL_BASE}/search.json?title={}&author={}&fields=cover_i,isbn&limit=1",
            url_encode(title),
            url_encode(author),
        );
        let body = self.transport.get(&url).await?;
        Ok(parse_cover(&body))
    }
}

/// The OL covers URL for a numeric cover id at medium size.
fn cover_url_for_id(cover_i: i64) -> String {
    format!("{OL_COVERS}/b/id/{cover_i}-M.jpg")
}

/// The OL covers URL for an ISBN at medium size.
pub fn cover_url_for_isbn(isbn: &str) -> String {
    format!("{OL_COVERS}/b/isbn/{}-M.jpg", isbn.trim())
}

#[derive(Debug, Deserialize)]
struct CoverSearchResponse {
    #[serde(default)]
    docs: Vec<CoverDoc>,
}

#[derive(Debug, Deserialize)]
struct CoverDoc {
    #[serde(default)]
    cover_i: Option<i64>,
    #[serde(default)]
    isbn: Vec<String>,
}

/// Parse an Open Library `search.json` body into a cover URL: the first doc's
/// `cover_i` (preferred), else its first ISBN. `None` when no doc carries either.
fn parse_cover(body: &str) -> Option<String> {
    let resp: CoverSearchResponse = serde_json::from_str(body).ok()?;
    let doc = resp.docs.into_iter().next()?;
    if let Some(id) = doc.cover_i {
        if id > 0 {
            return Some(cover_url_for_id(id));
        }
    }
    doc.isbn
        .into_iter()
        .find(|i| !i.trim().is_empty())
        .map(|i| cover_url_for_isbn(&i))
}

/// Minimal percent-encoding for query terms (space → `+`, reserved → `%XX`).
/// Mirrors `series::url_encode` (kept local to avoid widening that module's API).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Local thumbnail storage
// ---------------------------------------------------------------------------

/// The local thumbnail path for a book under a list folder:
/// `<list_dir>/thumbnails/<key>.jpg`. `key` is a stable, filesystem-safe id
/// (the book's md5, or its sequence number) so the file is durable across runs.
pub fn thumbnail_path(list_dir: &Path, key: &str) -> PathBuf {
    list_dir
        .join("thumbnails")
        .join(format!("{}.jpg", sanitize_key(key)))
}

/// Sanitize a thumbnail key into a single safe filename stem (alphanumerics,
/// dash, underscore kept; everything else → `_`). Empty → `cover`.
fn sanitize_key(key: &str) -> String {
    let cleaned: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "cover".to_string()
    } else {
        cleaned
    }
}

/// Download the cover image at `url` into `<list_dir>/thumbnails/<key>.jpg` and
/// return the written path. Skips the download (and returns the existing path)
/// when a non-empty file is already present, so covers are fetched at most once.
/// Pure I/O — the caller decides when/whether to call it.
pub async fn store_thumbnail(
    client: &reqwest::Client,
    list_dir: &Path,
    key: &str,
    url: &str,
) -> Result<PathBuf> {
    let path = thumbnail_path(list_dir, key);
    // Already cached — but only honor it if it's a USABLE image. A previously
    // cached placeholder (1×1 GIF) or corrupt file must NOT short-circuit; let it
    // fall through to be re-fetched / regenerated.
    if crate::cover_gen::cover_file_usable(&path) {
        return Ok(path);
    }
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching cover {url}"))?
        .error_for_status()
        .with_context(|| format!("cover status for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading cover body for {url}"))?;
    // Reject a placeholder/non-image response (a 1×1 GIF, an HTML error page, a
    // truncated download) so the caller falls through to local cover generation
    // instead of caching junk that renders blank.
    anyhow::ensure!(
        crate::cover_gen::cover_bytes_usable(&bytes),
        "cover at {url} is not a usable image ({} bytes) — placeholder/corrupt",
        bytes.len()
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cover_prefers_cover_id() {
        // A real-shaped Open Library search.json with a numeric cover id.
        let body = r#"{ "numFound": 1, "docs": [
            { "title": "Treasure Island", "cover_i": 8231856, "isbn": ["9781402714672"] }
        ]}"#;
        assert_eq!(
            parse_cover(body),
            Some("https://covers.openlibrary.org/b/id/8231856-M.jpg".to_string())
        );
    }

    #[test]
    fn parse_cover_falls_back_to_isbn() {
        let body = r#"{ "docs": [ { "title": "Treasure Island", "isbn": ["9781402714672"] } ] }"#;
        assert_eq!(
            parse_cover(body),
            Some("https://covers.openlibrary.org/b/isbn/9781402714672-M.jpg".to_string())
        );
    }

    #[test]
    fn parse_cover_none_when_no_cover_or_isbn() {
        assert_eq!(parse_cover(r#"{ "docs": [ { "title": "X" } ] }"#), None);
        assert_eq!(parse_cover(r#"{ "docs": [] }"#), None);
    }

    #[test]
    fn isbn_cover_url_shape() {
        assert_eq!(
            cover_url_for_isbn(" 9781402714672 "),
            "https://covers.openlibrary.org/b/isbn/9781402714672-M.jpg"
        );
    }

    #[tokio::test]
    async fn cover_url_uses_isbn_directly_without_network() {
        // A replay transport with NO fixtures: if the ISBN path short-circuits as
        // intended, no transport.get is attempted, so this must not error.
        let client = CoverClient::replay("/nonexistent-fixtures");
        let got = client
            .cover_url(
                "Treasure Island",
                "Robert Louis Stevenson",
                Some("9781402714672"),
            )
            .await
            .unwrap();
        assert_eq!(
            got,
            Some("https://covers.openlibrary.org/b/isbn/9781402714672-M.jpg".to_string())
        );
    }

    #[tokio::test]
    async fn cover_url_replays_search_json_fixture() {
        // Record a fixture body keyed by the lookup URL, then replay it offline.
        let dir = std::env::temp_dir().join(format!("covers-fixt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!(
            "{OL_BASE}/search.json?title={}&author={}&fields=cover_i,isbn&limit=1",
            url_encode("Treasure Island"),
            url_encode("Robert Louis Stevenson"),
        );
        let key = crate::series::fixture_key(&url);
        std::fs::write(
            dir.join(format!("{key}.json")),
            r#"{ "docs": [ { "title": "Treasure Island", "cover_i": 42 } ] }"#,
        )
        .unwrap();

        let client = CoverClient::replay(&dir);
        let got = client
            .cover_url("Treasure Island", "Robert Louis Stevenson", None)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some("https://covers.openlibrary.org/b/id/42-M.jpg".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_path_under_list_folder() {
        let p = thumbnail_path(Path::new("/books/My List"), "abc123");
        assert_eq!(p, PathBuf::from("/books/My List/thumbnails/abc123.jpg"));
        // Keys are sanitized into a safe stem.
        let p2 = thumbnail_path(Path::new("/x"), "a/b:c");
        assert_eq!(p2, PathBuf::from("/x/thumbnails/a_b_c.jpg"));
    }
}
