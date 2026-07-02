//! Look up a book's *series* on Open Library and return its ordered members, so
//! the UI can offer "Download whole series" and seed a fresh list with every
//! entry in reading order.
//!
//! Open Library needs no API key. The lookup is a small fixed recipe (search →
//! work → series → members → order), validated against real responses
//! (Oz = 14 members). All HTTP goes through an
//! [`OlTransport`] so the exact same parse path can be replayed offline from
//! recorded fixtures in tests — mirroring `search.rs`'s `Transport`.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

const OL_BASE: &str = "https://openlibrary.org";

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// A detected series and its ordered members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    /// Open Library series key (`OL…L`, without the `/series/` prefix).
    pub key: String,
    /// Human-readable series name (best effort; falls back to the seed title).
    pub name: String,
    /// Members in reading order (by `position`, else `first_publish_year`).
    pub members: Vec<SeriesMember>,
}

impl Series {
    /// Project this series into a fresh [`crate::model::DownloadList`]: one book
    /// per member in reading order, under a single group named after the series,
    /// titled `"{name} (series)"`. A pure shape projection — the caller drives the
    /// members (sets `goal = Complete`) after attaching the list.
    ///
    /// Shared by the desktop and TUI "download whole series" commands so the list
    /// shape can never drift between the two frontends.
    pub fn to_download_list(&self) -> crate::model::DownloadList {
        use crate::model::{BookInput, BookRequest, Group};
        let mut group = Group::new(self.name.clone());
        for m in &self.members {
            // Carry the member's author into the query metadata — without it the
            // seeded list would search libgen by TITLE ALONE, losing the author
            // corroboration that disambiguates same-titled books.
            let authors: Vec<String> = m
                .author
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(|a| vec![a.to_string()])
                .unwrap_or_default();
            group.books.push(BookRequest::new(BookInput {
                title: m.title.clone(),
                authors,
                ..Default::default()
            }));
        }
        crate::model::DownloadList {
            title: format!("{} (series)", self.name),
            settings: Default::default(),
            groups: vec![group],
        }
    }
}

/// One entry in a series.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeriesMember {
    /// Display title: `title` + (`": " + subtitle` when present).
    pub title: String,
    /// Reading-order position within the series, when the source exposes one.
    pub position: Option<u32>,
    /// Downloadable md5, when the source carries one directly (libgen series
    /// pages do, via the cover path / edition link). `None` for OL/Goodreads,
    /// whose members must be matched to a libgen copy by title search.
    pub md5: Option<String>,
    /// Cover image URL, when the source carries one (libgen series pages).
    pub cover_url: Option<String>,
    /// Author(s) for this member. Sources that expose a per-member author (the
    /// libgen series page's author cell) fill it directly; the OL/Goodreads
    /// paths, whose members share the seed book's author, fall back to the seed
    /// author (see [`fill_member_authors`]). An empty author would make the
    /// seeded list's libgen query TITLE-ONLY — far less precise — so this is
    /// propagated into [`Series::to_download_list`]'s `BookInput.authors`.
    pub author: Option<String>,
}

// ---------------------------------------------------------------------------
// Transport abstraction (live / replay) — mirrors `search::Transport`.
// ---------------------------------------------------------------------------

/// Abstracts the HTTP GET against Open Library so the lookup can be replayed
/// offline from recorded fixtures.
#[async_trait::async_trait]
pub trait OlTransport: Send + Sync {
    async fn get(&self, url: &str) -> Result<String>;
}

/// Live HTTP transport backed by reqwest (Open Library asks for a descriptive
/// User-Agent).
pub struct LiveOlTransport {
    client: reqwest::Client,
}

impl LiveOlTransport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Kwire/1.0 (+https://example.invalid; series lookup)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("building reqwest client");
        LiveOlTransport { client }
    }
}

impl Default for LiveOlTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl OlTransport for LiveOlTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("status for {url}"))?;
        resp.text().await.with_context(|| format!("body for {url}"))
    }
}

/// A deterministic, filesystem-safe key for an Open Library URL: the whole URL
/// slugified (lowercased alphanumerics, every other run collapsed to a dash).
/// Distinct URLs map to distinct keys, so fixtures can be recorded per request.
pub fn fixture_key(url: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in url.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("query");
    }
    out
}

/// Replays previously recorded Open Library responses from a fixtures dir.
/// Lookup is by [`fixture_key`] with a `.json` or `.html` extension. Fully
/// offline.
pub struct ReplayOlTransport {
    dir: PathBuf,
}

impl ReplayOlTransport {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ReplayOlTransport { dir: dir.into() }
    }
}

#[async_trait::async_trait]
impl OlTransport for ReplayOlTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let key = fixture_key(url);
        for ext in ["json", "html", "txt"] {
            let path = self.dir.join(format!("{key}.{ext}"));
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("reading fixture {}", path.display()));
            }
        }
        Err(anyhow!(
            "no recorded Open Library fixture for url {url} (key {key}) in {}",
            self.dir.display()
        ))
    }
}

/// Wraps a transport, saving every response into a fixtures dir keyed by
/// [`fixture_key`] so it can later be replayed. Used by the CLI's `--record`.
pub struct RecordingOlTransport {
    inner: Box<dyn OlTransport>,
    dir: PathBuf,
}

impl RecordingOlTransport {
    pub fn new(inner: Box<dyn OlTransport>, dir: impl Into<PathBuf>) -> Self {
        RecordingOlTransport {
            inner,
            dir: dir.into(),
        }
    }
}

#[async_trait::async_trait]
impl OlTransport for RecordingOlTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let body = self.inner.get(url).await?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let trimmed = body.trim_start();
        let ext = if trimmed.starts_with('[') || trimmed.starts_with('{') {
            "json"
        } else {
            "html"
        };
        let path = self.dir.join(format!("{}.{ext}", fixture_key(url)));
        std::fs::write(&path, &body)
            .with_context(|| format!("writing fixture {}", path.display()))?;
        Ok(body)
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Looks a book's series up on Open Library and returns its ordered members.
pub struct SeriesClient {
    transport: Box<dyn OlTransport>,
}

impl SeriesClient {
    pub fn new(transport: Box<dyn OlTransport>) -> Self {
        SeriesClient { transport }
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

    /// Look up the series `title`/`author` belongs to, returning its ordered
    /// members. `Ok(None)` means the book exists but is NOT part of a known
    /// series (so there's nothing to seed). Network errors propagate as `Err`.
    ///
    /// Strategy: try the *primary* path (Open Library's `series` field). When
    /// that yields nothing — either no work matched, or the work simply has no
    /// `series` field — fall back to a TITLE-PREFIX heuristic that reconstructs
    /// untagged series from sibling titles (box sets filtered out). See
    /// [`SeriesClient::title_prefix_fallback`].
    pub async fn lookup(&self, title: &str, author: &str) -> Result<Option<Series>> {
        let mut series = self.lookup_primary(title, author).await?;
        // Slow fallback: the primary path found nothing, OR only the seed itself
        // (a 1-member "series" — its work JSON named a series key that the members
        // search couldn't expand). OL's SEARCH INDEX often still tags member docs
        // with a usable `series_key`, so scan the results for one.
        if series.as_ref().map(|s| s.members.len() < 2).unwrap_or(true) {
            if let Some(s) = self.series_from_search_index(title, author).await? {
                series = Some(s);
            }
        }
        // Title-prefix heuristic, only when we still have nothing at all.
        if series.is_none() {
            series = self.title_prefix_fallback(title, author).await?;
        }
        // OL members carry no author in our search responses, so members inherit
        // the seed book's author (a series is by one author in the OL/prefix
        // paths). Keeps the seeded list's libgen query from going title-only.
        if let Some(s) = series.as_mut() {
            fill_member_authors(s, author);
        }
        Ok(series)
    }

    /// The primary lookup: Open Library's explicit `series` field. `Ok(None)`
    /// when no work matches the request or the matched work has no `series`.
    async fn lookup_primary(&self, title: &str, author: &str) -> Result<Option<Series>> {
        // 1. Find the best-matching work.
        let search_url = format!(
            "{OL_BASE}/search.json?title={}{}&fields=key,title,subtitle,author_name&limit=5",
            url_encode(title),
            author_param(author),
        );
        let search_body = self.transport.get(&search_url).await?;
        let work = match pick_work(&search_body, title, author)? {
            Some(w) => w,
            None => return Ok(None),
        };

        // 2. This work's series key + position.
        let work_url = format!("{OL_BASE}{}.json", work.key);
        let work_body = self.transport.get(&work_url).await?;
        let seed = WorkSeed {
            key: work.key.clone(),
            title: display_title(&work.title, work.subtitle.as_deref()),
        };
        let series_key = match work_series_key(&work_body)? {
            Some(k) => k,
            None => return Ok(None), // not in a series
        };

        // 3. Members of the series (search, HTML fallback).
        let mut raw = self.raw_members_for_series_key(&series_key).await?;

        // 5. A series key but zero members even after the HTML scrape → fall back
        //    to just the seed book (a one-item series is fine).
        if raw.is_empty() {
            raw.push(RawMember {
                key: seed.key.clone(),
                title: seed.title.clone(),
                position: None,
                first_publish_year: None,
            });
        }

        // 4. Order the members: by `position` (fetched from each member work)
        //    when available, else by `first_publish_year`.
        let members = self.order_members(raw).await?;

        Ok(Some(Series {
            key: series_key,
            name: series_name(&seed.title),
            members,
        }))
    }

    /// Fetch a series' members directly by its Open Library **series key** (the
    /// bare `OL…L`, or a `/series/OL…L` path / full URL — see [`parse_series_ref`]).
    /// Powers the explicit `:series <url>` command: the user pastes an Open
    /// Library series page and we seed a list from it, with no seed book needed.
    ///
    /// `name_hint` is the human name recovered from the URL slug
    /// (`.../A_Series_of_Unfortunate_Events`); it is preferred over guessing the
    /// series name from the first member's book title. `Ok(None)` when the key
    /// resolves to no members at all.
    pub async fn series_by_key(
        &self,
        key_or_ref: &str,
        name_hint: Option<&str>,
    ) -> Result<Option<Series>> {
        let (key, slug_name) = match parse_series_ref(key_or_ref) {
            Some(kv) => kv,
            None => return Ok(None),
        };
        let raw = self.raw_members_for_series_key(&key).await?;
        if raw.is_empty() {
            return Ok(None);
        }
        let members = self.order_members(raw).await?;
        // Prefer an explicit caller hint, then the URL slug, then the series
        // page's own `name`, then a best-effort name from the first member title.
        let name = match name_hint
            .map(str::to_string)
            .or(slug_name)
            .filter(|s| !s.trim().is_empty())
        {
            Some(n) => n,
            None => self
                .series_display_name(&key)
                .await
                .unwrap_or_else(|| series_name(&members[0].title)),
        };
        Ok(Some(Series { key, name, members }))
    }

    /// Best-effort human series name from OL's `/series/{key}.json` `name` field
    /// (e.g. `A Series of Unfortunate Events`). `None` when unavailable — the
    /// caller then falls back to a member-title guess.
    async fn series_display_name(&self, series_key: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct S {
            #[serde(default)]
            name: String,
        }
        let url = format!("{OL_BASE}/series/{series_key}.json");
        let body = self.transport.get(&url).await.ok()?;
        let s: S = serde_json::from_str(&body).ok()?;
        let n = s.name.trim();
        if n.is_empty() {
            None
        } else {
            Some(n.to_string())
        }
    }

    /// Slow fallback for the reverse (book→series) lookup: OL's SEARCH INDEX
    /// tags member docs with a `series_key` even when the work JSON's `series`
    /// field is empty (the common case — e.g. every "A Series of Unfortunate
    /// Events" member). Search by title, take the `series_key` of the
    /// best-matching doc that has one, and expand it into the full series.
    /// `Ok(None)` when no result carries a series key.
    async fn series_from_search_index(&self, title: &str, author: &str) -> Result<Option<Series>> {
        let url = format!(
            "{OL_BASE}/search.json?title={}{}&fields=key,title,author_name,series_key&limit=20",
            url_encode(title),
            author_param(author),
        );
        // A missing replay fixture / network blip here is just "no fallback hit".
        let body = match self.transport.get(&url).await {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let key = match best_series_key_in_search(&body, title, author) {
            Some(k) => k,
            None => return Ok(None),
        };
        let raw = self.raw_members_for_series_key(&key).await?;
        if raw.is_empty() {
            return Ok(None);
        }
        let members = self.order_members(raw).await?;
        let name = self
            .series_display_name(&key)
            .await
            .unwrap_or_else(|| series_name(&members[0].title));
        Ok(Some(Series { key, name, members }))
    }

    /// The members-search + HTML-scrape half of the primary lookup, factored out
    /// so both [`lookup_primary`](Self::lookup_primary) and
    /// [`series_by_key`](Self::series_by_key) share the exact same member
    /// discovery. Returns the raw (unordered, position-less) members; may be
    /// empty (the caller decides whether that's a one-item series or a miss).
    async fn raw_members_for_series_key(&self, series_key: &str) -> Result<Vec<RawMember>> {
        let members_url = format!(
            "{OL_BASE}/search.json?q=series_key:{}&fields=key,title,subtitle,first_publish_year&limit=60",
            series_key,
        );
        let members_body = self.transport.get(&members_url).await?;
        let mut raw = parse_members(&members_body)?;
        // Fallback: the search yields nothing → scrape the HTML series page for
        // `/works/OL…W` links and synthesize members from each work's JSON.
        if raw.is_empty() {
            let html_url = format!("{OL_BASE}/series/{series_key}");
            if let Ok(html) = self.transport.get(&html_url).await {
                raw = self.members_from_html(&html).await;
            }
        }
        Ok(raw)
    }

    /// Title-prefix fallback for series Open Library does NOT tag with a
    /// `series` field. Runs only after the primary path yields nothing.
    ///
    /// Recipe:
    /// 1. Derive a candidate series name from the *request* title — the part
    ///    before the first `:` (or ` - `). Require ≥ 2 words; else no fallback.
    /// 2. Search Open Library for siblings by that name + author.
    /// 3. Keep titles that START WITH the prefix and are NOT box sets /
    ///    collections / number-range bundles (see [`is_collection`]).
    /// 4. Require ≥ 2 distinct surviving volumes (avoids "<Title>: A Novel"
    ///    false positives); else `Ok(None)`.
    /// 5. Order by an embedded volume number when present, else by
    ///    `first_publish_year`; assign 1-based positions.
    async fn title_prefix_fallback(&self, title: &str, author: &str) -> Result<Option<Series>> {
        let prefix = match series_prefix(title) {
            Some(p) => p,
            None => return Ok(None),
        };

        let search_url = format!(
            "{OL_BASE}/search.json?title={}{}&fields=key,title,subtitle,first_publish_year&limit=40",
            url_encode(&prefix),
            author_param(author),
        );
        let body = self.transport.get(&search_url).await?;
        let volumes = sibling_volumes(&body, &prefix)?;
        if volumes.len() < 2 {
            return Ok(None);
        }

        let members = order_prefix_volumes(volumes);
        Ok(Some(Series {
            key: format!("prefix:{}", slugify(&prefix)),
            name: prefix,
            members,
        }))
    }

    /// Extract `/works/OL…W` links from a series HTML page (Percy-Jackson
    /// fallback) and synthesize members from each work's `{key}.json`.
    async fn members_from_html(&self, html: &str) -> Vec<RawMember> {
        let mut out = Vec::new();
        for key in work_keys_in_html(html) {
            let url = format!("{OL_BASE}{key}.json");
            if let Ok(body) = self.transport.get(&url).await {
                if let Some(m) = raw_member_from_work(&key, &body) {
                    out.push(m);
                }
            }
        }
        out
    }

    /// Resolve each member's `position` by fetching its work JSON (the search
    /// endpoint doesn't expose series position), then order: position first
    /// (ascending), members without a position after, by `first_publish_year`.
    async fn order_members(&self, raw: Vec<RawMember>) -> Result<Vec<SeriesMember>> {
        let mut enriched: Vec<RawMember> = Vec::with_capacity(raw.len());
        for mut m in raw {
            if m.position.is_none() {
                let url = format!("{OL_BASE}{}.json", m.key);
                if let Ok(body) = self.transport.get(&url).await {
                    m.position = work_position(&body);
                }
            }
            enriched.push(m);
        }
        Ok(order_raw_members(enriched))
    }
}

// ---------------------------------------------------------------------------
// Pure parse / ordering helpers (unit-tested against fixtures)
// ---------------------------------------------------------------------------

/// A work picked from the search results.
#[derive(Debug, Clone)]
struct WorkDoc {
    key: String,
    title: String,
    subtitle: Option<String>,
}

/// The seed work (the book the user opened).
struct WorkSeed {
    key: String,
    title: String,
}

/// A member before ordering / position-resolution.
#[derive(Debug, Clone)]
struct RawMember {
    key: String,
    title: String,
    position: Option<u32>,
    first_publish_year: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    docs: Vec<SearchDoc>,
}

#[derive(Debug, Deserialize)]
struct SearchDoc {
    #[serde(default)]
    key: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    author_name: Vec<String>,
    #[serde(default)]
    first_publish_year: Option<i64>,
    /// Series keys OL's SEARCH INDEX carries on member docs (`["OL…L"]`). Present
    /// even when the work JSON's `series` field is empty — the slow-fallback path
    /// relies on this. Only populated when the search requests `series_key`.
    #[serde(default)]
    series_key: Vec<String>,
}

/// Pick the work whose title best matches the requested `title`, preferring docs
/// whose `author_name` contains the request author's surname. Returns `None`
/// when nothing plausibly matches (so the caller reports "not in a series").
fn pick_work(body: &str, want_title: &str, want_author: &str) -> Result<Option<WorkDoc>> {
    let resp: SearchResponse =
        serde_json::from_str(body).context("decoding Open Library search.json")?;
    let want = norm(want_title);
    let surname = surname_of(want_author);

    let mut best: Option<(i32, WorkDoc)> = None;
    for doc in resp.docs {
        if doc.key.is_empty() || doc.title.is_empty() {
            continue;
        }
        let got = norm(&doc.title);
        // Title relevance: containment either way (the seed "The Wonderful
        // Wizard of Oz" is contained in "The Marvelous Land of Oz" siblings).
        let title_score = if got == want {
            3
        } else if got.contains(&want) || want.contains(&got) {
            2
        } else if title_tokens_overlap(&want, &got) {
            1
        } else {
            0
        };
        if title_score == 0 {
            continue;
        }
        // Author corroboration: +2 when the author's surname appears.
        let author_score = match &surname {
            Some(s) if doc.author_name.iter().any(|a| norm(a).contains(s)) => 2,
            _ => 0,
        };
        let score = title_score + author_score;
        let cand = WorkDoc {
            key: doc.key,
            title: doc.title,
            subtitle: doc.subtitle.filter(|s| !s.trim().is_empty()),
        };
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, cand));
        }
    }
    Ok(best.map(|(_, w)| w))
}

/// Pick the `series_key` of the best-matching search doc that carries one
/// (slow-fallback path). Requires the doc's title to plausibly match the request
/// (so an unrelated same-search result can't inject a wrong series), and prefers
/// docs whose `author_name` contains the request author's surname. Returns the
/// bare `OL…L` key, or `None` when no result carries a usable series key.
fn best_series_key_in_search(body: &str, want_title: &str, want_author: &str) -> Option<String> {
    let resp: SearchResponse = serde_json::from_str(body).ok()?;
    let want = norm(want_title);
    let surname = surname_of(want_author);

    let mut best: Option<(i32, String)> = None;
    for doc in resp.docs {
        let raw_key = match doc
            .series_key
            .into_iter()
            .find(|k| !k.trim().is_empty())
            .and_then(|k| strip_series_prefix(&k))
        {
            Some(k) => k,
            None => continue,
        };
        if doc.title.is_empty() {
            continue;
        }
        let got = norm(&doc.title);
        // Title relevance gate (same shape as `pick_work`): the member doc's
        // title must relate to the request title.
        let title_score = if got == want {
            3
        } else if got.contains(&want) || want.contains(&got) {
            2
        } else if title_tokens_overlap(&want, &got) {
            1
        } else {
            0
        };
        if title_score == 0 {
            continue;
        }
        let author_score = match &surname {
            Some(s) if doc.author_name.iter().any(|a| norm(a).contains(s)) => 2,
            _ => 0,
        };
        let score = title_score + author_score;
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, raw_key));
        }
    }
    best.map(|(_, k)| k)
}

#[derive(Debug, Deserialize)]
struct WorkResponse {
    #[serde(default)]
    series: Vec<WorkSeriesEntry>,
}

#[derive(Debug, Deserialize)]
struct WorkSeriesEntry {
    #[serde(default)]
    series: Option<WorkSeriesRef>,
    #[serde(default)]
    position: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct WorkSeriesRef {
    #[serde(default)]
    key: String,
}

/// Extract a work's series key (`OL…L`, without the `/series/` prefix) from its
/// work JSON. `Ok(None)` when the work has no `series` (not in a series).
fn work_series_key(body: &str) -> Result<Option<String>> {
    let resp: WorkResponse =
        serde_json::from_str(body).context("decoding Open Library work.json")?;
    for entry in resp.series {
        if let Some(r) = entry.series {
            if let Some(k) = strip_series_prefix(&r.key) {
                return Ok(Some(k));
            }
        }
    }
    Ok(None)
}

/// Extract a work's series `position` (as `u32`) from its work JSON, when
/// present. Open Library serializes it as a string (`"1"`) or a number.
fn work_position(body: &str) -> Option<u32> {
    let resp: WorkResponse = serde_json::from_str(body).ok()?;
    for entry in resp.series {
        if let Some(p) = entry.position.as_ref().and_then(value_as_u32) {
            return Some(p);
        }
    }
    None
}

/// Parse the series-members search response into raw (unordered) members.
fn parse_members(body: &str) -> Result<Vec<RawMember>> {
    let resp: SearchResponse =
        serde_json::from_str(body).context("decoding Open Library members search.json")?;
    let mut out = Vec::new();
    for doc in resp.docs {
        if doc.key.is_empty() || doc.title.is_empty() {
            continue;
        }
        out.push(RawMember {
            key: doc.key,
            title: display_title(&doc.title, doc.subtitle.as_deref()),
            position: None,
            first_publish_year: doc.first_publish_year,
        });
    }
    Ok(out)
}

/// Build a [`RawMember`] from a single work's JSON (HTML-fallback path).
fn raw_member_from_work(key: &str, body: &str) -> Option<RawMember> {
    #[derive(Deserialize)]
    struct W {
        #[serde(default)]
        title: String,
        #[serde(default)]
        subtitle: Option<String>,
    }
    let w: W = serde_json::from_str(body).ok()?;
    if w.title.is_empty() {
        return None;
    }
    Some(RawMember {
        key: key.to_string(),
        title: display_title(&w.title, w.subtitle.as_deref()),
        position: work_position(body),
        first_publish_year: None,
    })
}

/// Order raw members: those WITH a position first (ascending by position),
/// then those without, ordered by `first_publish_year` (then title) for
/// stability. De-duplicates by work key, keeping the first occurrence.
fn order_raw_members(raw: Vec<RawMember>) -> Vec<SeriesMember> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<RawMember> = Vec::with_capacity(raw.len());
    for m in raw {
        if seen.insert(m.key.clone()) {
            deduped.push(m);
        }
    }
    deduped.sort_by(|a, b| match (a.position, b.position) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a
            .first_publish_year
            .cmp(&b.first_publish_year)
            .then_with(|| a.title.cmp(&b.title)),
    });
    deduped
        .into_iter()
        .map(|m| SeriesMember {
            title: m.title,
            position: m.position,
            ..Default::default()
        })
        .collect()
}

/// Extract distinct `/works/OL…W` keys from a series HTML page, in order of
/// appearance (Percy-Jackson fallback).
fn work_keys_in_html(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bytes = html.as_bytes();
    let needle = b"/works/OL";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            // capture "/works/OL...W"
            let mut j = i + "/works/".len();
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            let key = &html[i..j];
            if key.ends_with('W') && seen.insert(key.to_string()) {
                out.push(key.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Member display title: `title` + (`": " + subtitle` when present).
fn display_title(title: &str, subtitle: Option<&str>) -> String {
    let t = title.trim();
    match subtitle.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sub) => format!("{t}: {sub}"),
        None => t.to_string(),
    }
}

/// Best-effort human series name from the seed book's title: drop a trailing
/// subtitle (the per-book part) so "Ozma of Oz: The Royal Book" → "Ozma of Oz".
fn series_name(seed_title: &str) -> String {
    let t = seed_title.trim();
    if let Some(idx) = t.find(": ") {
        return t[..idx].trim().to_string();
    }
    t.to_string()
}

// ---------------------------------------------------------------------------
// Title-prefix fallback helpers (untagged series)
// ---------------------------------------------------------------------------

/// A surviving sibling volume in the prefix fallback, before ordering.
#[derive(Debug, Clone)]
struct PrefixVolume {
    /// Full Open Library title (display form, subtitle appended) — used as the
    /// member title so each volume's libgen search carries its subtitle.
    title: String,
    /// Normalized title, for de-duping.
    norm_title: String,
    /// Volume number extracted from the title, when present (e.g. "#6", "Book 3").
    volume: Option<u32>,
    /// First publish year, the ordering fallback when no volume number exists.
    first_publish_year: Option<i64>,
}

/// Derive a candidate series name from a *request* title: the part before the
/// first `:` or ` - ` separator. Returns `None` (no fallback) when there's no
/// separator or the prefix is fewer than 2 words.
fn series_prefix(title: &str) -> Option<String> {
    let t = title.trim();
    // Prefer the earliest separator (`:` or ` - `).
    let colon = t.find(':');
    let dash = t.find(" - ");
    let cut = match (colon, dash) {
        (Some(c), Some(d)) => Some(c.min(d)),
        (Some(c), None) => Some(c),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }?;
    let prefix = t[..cut].trim();
    if prefix.split_whitespace().count() < 2 {
        return None;
    }
    Some(prefix.to_string())
}

/// Collection / box-set / bundle keywords that mark a title as NOT a single
/// volume. Shared by [`is_collection`] and the series-seed selector
/// ([`order_series_seeds`]) — the latter additionally IGNORES any keyword shared
/// by a majority of a book's candidates (it's part of the series name, not a
/// bundle marker).
const COLLECTION_KEYWORDS: &[&str] = &[
    "box set",
    "boxed set",
    "boxset",
    "box-set",
    "collection",
    "complete",
    "omnibus",
    "series",
    "gift set",
    "bundle",
];

/// Box-set / collection / bundle detector. Returns `true` for titles that are
/// NOT a single volume: box sets, omnibuses, "complete"/"series" bundles, and
/// number-range bundles like "1-11", "vol. 1-12", "books 1-3", "N-book". Used to
/// prune non-member entries out of an already-discovered member list (the OL
/// title-prefix and Goodreads paths).
fn is_collection(title: &str) -> bool {
    let lower = title.to_lowercase();
    if COLLECTION_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return true;
    }
    has_number_range(&lower)
}

/// Detects number-range / count patterns that signal a bundle rather than a
/// single volume: "1-11", "1 - 12", "books 1-3", "vol. 1-12", "N-book",
/// "set of 12", "X 10".
fn has_number_range(lower: &str) -> bool {
    let bytes = lower.as_bytes();

    // "<digits> <sep> <digits>" where sep is '-' (optionally space-padded) →
    // a range like "1-11" or "1 - 12".
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'-' {
                j += 1;
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    return true;
                }
            }
            // "N-book" / "N book" bundle ("3-book box set").
            let after = lower[i..].trim_start_matches([' ', '-']);
            if after.starts_with("book") && start != i {
                return true;
            }
        } else {
            i += 1;
        }
    }

    lower.contains("set of ")
}

/// Parse the prefix-fallback search response into surviving sibling volumes:
/// titles that (normalized) START WITH the prefix and are not collections.
/// De-dupes by normalized title.
fn sibling_volumes(body: &str, prefix: &str) -> Result<Vec<PrefixVolume>> {
    let resp: SearchResponse =
        serde_json::from_str(body).context("decoding Open Library prefix search.json")?;
    let want = norm(prefix);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for doc in resp.docs {
        if doc.key.is_empty() || doc.title.is_empty() {
            continue;
        }
        let full = display_title(&doc.title, doc.subtitle.as_deref());
        let n = norm(&full);
        // Must start with the series-name prefix (normalized).
        if !n.starts_with(&want) {
            continue;
        }
        // Drop box sets / collections / bundles.
        if is_collection(&full) {
            continue;
        }
        if !seen.insert(n.clone()) {
            continue;
        }
        out.push(PrefixVolume {
            volume: volume_number(&full),
            title: full,
            norm_title: n,
            first_publish_year: doc.first_publish_year,
        });
    }
    Ok(out)
}

/// Extract a leading/embedded volume number from a title: "#N", "Book N",
/// "Vol. N" / "Volume N", or a trailing " N". Returns `None` when absent.
fn volume_number(title: &str) -> Option<u32> {
    let lower = title.to_lowercase();
    let bytes = lower.as_bytes();

    // Marker-prefixed numbers: "#", "book", "vol", "volume", "no", "part".
    const MARKERS: &[&str] = &["#", "book ", "volume ", "vol. ", "vol ", "no. ", "part "];
    for m in MARKERS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(m) {
            let after = from + rel + m.len();
            let digits: String = lower[after..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some(n);
            }
            from = after;
            if from >= bytes.len() {
                break;
            }
        }
    }

    // Trailing number ("... Mud Turtle Tale 7"): last whitespace-delimited
    // token if it's all digits.
    if let Some(tok) = lower.split_whitespace().last() {
        if tok.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = tok.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// Order surviving volumes: by embedded volume number when present (ascending),
/// those without a number after, ordered by `first_publish_year` (then title)
/// for stability. Assigns 1-based positions.
fn order_prefix_volumes(mut volumes: Vec<PrefixVolume>) -> Vec<SeriesMember> {
    volumes.sort_by(|a, b| match (a.volume, b.volume) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.norm_title.cmp(&b.norm_title)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a
            .first_publish_year
            .cmp(&b.first_publish_year)
            .then_with(|| a.norm_title.cmp(&b.norm_title)),
    });
    volumes
        .into_iter()
        .enumerate()
        .map(|(i, v)| SeriesMember {
            title: v.title,
            position: Some((i + 1) as u32),
            ..Default::default()
        })
        .collect()
}

/// Filesystem/key-safe slug of a series name: lowercased alphanumerics, every
/// other run collapsed to a single dash.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Parse an Open Library **series reference** into `(bare key, name hint)`.
///
/// Accepts any of:
///   * a full URL — `https://openlibrary.org/series/OL326111L/A_Series_of_Unfortunate_Events`
///   * a path — `/series/OL326111L` (with or without a trailing slug)
///   * a bare key — `OL326111L`
///
/// The key must look like an Open Library series id (`OL` + digits + `L`). The
/// name hint, when present, is the trailing URL slug de-slugified
/// (`A_Series_of_Unfortunate_Events` → `A Series of Unfortunate Events`).
/// Returns `None` when no plausible series key is found.
pub fn parse_series_ref(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // Reduce to the path after a `/series/` marker when present; otherwise treat
    // the whole (possibly bare-key) input as the candidate.
    let rest = match s.find("/series/") {
        Some(idx) => &s[idx + "/series/".len()..],
        None => s,
    };
    // The key is the first path segment; anything after the next `/` is the slug.
    let mut segs = rest.trim_start_matches('/').splitn(2, '/');
    let key_seg = segs.next().unwrap_or("").trim();
    let slug = segs.next().unwrap_or("");
    let key = key_seg.trim_end_matches('/');
    if !is_series_key(key) {
        return None;
    }
    let name = deslugify(slug);
    Some((
        key.to_string(),
        if name.trim().is_empty() { None } else { Some(name) },
    ))
}

/// Whether `k` looks like an Open Library series key: `OL`, then ≥1 digits,
/// then a trailing `L` (e.g. `OL326111L`).
fn is_series_key(k: &str) -> bool {
    let core = match k.strip_prefix("OL").or_else(|| k.strip_prefix("ol")) {
        Some(c) => c,
        None => return false,
    };
    let digits = core.strip_suffix('L').or_else(|| core.strip_suffix('l'));
    match digits {
        Some(d) => !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Turn a URL slug into a display name: `_`/`-`/`+` runs → single spaces, then
/// trim. `A_Series_of_Unfortunate_Events` → `A Series of Unfortunate Events`.
fn deslugify(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| if matches!(c, '_' | '-' | '+') { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip a leading `/series/` from a series key, returning the bare `OL…L`.
fn strip_series_prefix(key: &str) -> Option<String> {
    let k = key.trim();
    let bare = k.strip_prefix("/series/").unwrap_or(k);
    if bare.is_empty() {
        None
    } else {
        Some(bare.to_string())
    }
}

/// Coerce a JSON value (string or number) into a `u32` (Open Library serializes
/// `position` inconsistently).
fn value_as_u32(v: &serde_json::Value) -> Option<u32> {
    match v {
        serde_json::Value::String(s) => {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        serde_json::Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
        _ => None,
    }
}

/// Lowercased, alphanumeric-token normalization (matches `search`'s local
/// helper): for title/author comparison.
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_space = false;
        } else if !last_space && !out.is_empty() {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// At least ~60% of the (significant) request title tokens appear in `got`.
fn title_tokens_overlap(want: &str, got: &str) -> bool {
    let want_tokens: std::collections::BTreeSet<&str> =
        want.split_whitespace().filter(|t| t.len() > 2).collect();
    if want_tokens.is_empty() {
        return false;
    }
    let got_tokens: std::collections::BTreeSet<&str> = got.split_whitespace().collect();
    let overlap = want_tokens
        .iter()
        .filter(|t| got_tokens.contains(*t))
        .count();
    (overlap as f32) / (want_tokens.len() as f32) >= 0.6
}

/// The author's surname, normalized: the last token of "First Last" or the part
/// before a comma in "Last, First". `None` for an empty name.
fn surname_of(author: &str) -> Option<String> {
    let a = author.trim();
    if a.is_empty() {
        return None;
    }
    if let Some((last, _first)) = a.split_once(',') {
        let last = norm(last);
        if !last.is_empty() {
            return Some(last);
        }
    }
    norm(a).split_whitespace().last().map(|s| s.to_string())
}

/// The `&author=<enc>` query fragment — but EMPTY when the author is blank.
/// Open Library returns **500 Internal Server Error** for a bare `author=` with
/// no value, so an author-less lookup must omit the parameter entirely.
fn author_param(author: &str) -> String {
    let a = author.trim();
    if a.is_empty() {
        String::new()
    } else {
        format!("&author={}", url_encode(a))
    }
}

/// Minimal percent-encoding for query terms (space → `+`, reserved → `%XX`).
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

// ===========================================================================
// Source B — libgen `series.php`
// ===========================================================================
//
// Recipe (validated against the raw pages, kept under fixtures/series/):
//   1. Search libgen for the TITLE only. Each result row's title cell carries
//      one or more `series.php?id=N` links. A book can be in MULTIPLE series —
//      Alice has 364378 (TPB: real titles + a volume column) and 364379
//      (Strip: by-year, NO titles). We must pick the BOOK/TPB series.
//   2. Fetch each candidate `series.php?id=N` page and parse its `tablelibgen`
//      rows. The TPB page's member rows have a non-empty wide (`width=20%`)
//      TITLE cell + a volume number; the Strip page's title cells are empty.
//      Pick the series with the most rows that carry a real title.
//   3. Each member: title (wide cell), volume number (the populated narrow
//      numeric cell that is NOT the year), cover md5 (`/comicscovers/…/<md5>.jpg`).

const LIBGEN_BASE: &str = "https://libgen.li";

/// Looks a book's series up on libgen's own `series.php` pages and returns its
/// ordered members (with downloadable cover md5s). Transport-abstracted (live /
/// replay / recording) via [`crate::search::Transport`] so it is offline-testable.
pub struct LibgenSeriesClient {
    transport: Box<dyn crate::search::Transport>,
    host: String,
}

impl LibgenSeriesClient {
    pub fn new(transport: Box<dyn crate::search::Transport>) -> Self {
        LibgenSeriesClient {
            transport,
            host: "libgen.li".to_string(),
        }
    }

    /// Live libgen transport.
    pub fn live() -> Self {
        Self::new(Box::new(crate::search::LiveTransport::new()))
    }

    /// Replay transport over a fixtures dir (offline).
    pub fn replay(fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::new(Box::new(crate::search::ReplayTransport::new(
            fixtures_dir.into(),
        )))
    }

    /// Live transport that records responses into `fixtures_dir`.
    pub fn recording(fixtures_dir: impl Into<PathBuf>) -> Self {
        let live: Box<dyn crate::search::Transport> = Box::new(crate::search::LiveTransport::new());
        Self::new(Box::new(crate::search::RecordingTransport::new(
            live,
            fixtures_dir.into(),
        )))
    }

    /// Look up `title` (author unused for the search — appending it can surface
    /// journal-review rows pointing at the wrong `series.php`). `Ok(None)` when
    /// no usable book series is found.
    pub async fn lookup(&self, title: &str, author: &str) -> Result<Option<Series>> {
        // 1. Search the title; collect candidate series ids from the result rows.
        let search_url = format!(
            "{LIBGEN_BASE}/index.php?req={}&res=25",
            search_url_encode(title)
        );
        let search_body = self.transport.get(&search_url).await?;
        let candidates = series_ids_in_search(&search_body, title);
        if candidates.is_empty() {
            return Ok(None);
        }

        // 2. Fetch each candidate series page; parse members; pick the best.
        //    A candidate is ACCEPTED only when it is plausibly THIS book's
        //    series — its name (the series link's anchor text or the page's own
        //    name) matches the request title, OR a member title matches it. This
        //    rejects an unrelated series that merely co-listed on the book's row
        //    (e.g. "The Wonderful Wizard of Oz" co-listed alongside unrelated series
        //    series). Among accepted candidates, the one with the most titled
        //    rows wins (TPB over Strip), ties broken by id.
        let want = norm(title);
        let mut best: Option<(usize, u64, Series)> = None;
        for cand in candidates {
            let url = format!("{LIBGEN_BASE}/series.php?id={}", cand.id);
            let body = match self.transport.get(&url).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (page_name, members) = parse_series_page(&body, &self.host);
            let titled = members.iter().filter(|m| !m.title.is_empty()).count();
            if titled == 0 {
                continue; // a Strip-style page (no titles) is not a book series
            }

            // Relevance: the request title must relate to the series name (from
            // the search-row anchor or the page heading) or to a member title.
            let name_for_match = if cand.name.is_empty() {
                &page_name
            } else {
                &cand.name
            };
            let name_ok = {
                let n = norm(name_for_match);
                n.contains(&want) || want.contains(&n) || tokens_overlap(&want, &n)
            };
            // A SINGLE coincidental member match (e.g. one "Tijuana Bibles" strip
            // titled "…Oz…" among many) must NOT accept an unrelated series —
            // require a substantial fraction of members to relate to the request
            // title, OR the series name itself to match.
            let related = members
                .iter()
                .filter(|m| {
                    let mt = norm(&m.title);
                    mt.contains(&want) || want.contains(&mt)
                })
                .count();
            let member_ok =
                !members.is_empty() && (related * 100 / members.len()) >= 30 && related >= 2;
            if !name_ok && !member_ok {
                continue; // an unrelated co-listed series — reject
            }

            let series = Series {
                key: format!("libgen:{}", cand.id),
                name: if page_name.is_empty() {
                    series_name(title)
                } else {
                    page_name
                },
                members,
            };
            let key = (titled, cand.id);
            if best
                .as_ref()
                .map(|(t, i, _)| key > (*t, *i))
                .unwrap_or(true)
            {
                best = Some((titled, cand.id, series));
            }
        }

        let mut series = best.map(|(_, _, s)| s);
        // Series-page rows carry a per-member author cell; rows missing one fall
        // back to the seed author so no member is left author-less.
        if let Some(s) = series.as_mut() {
            fill_member_authors(s, author);
        }
        Ok(series)
    }
}

/// One candidate series found on a search page: its id + the anchor text of the
/// `series.php` link (the series NAME as libgen shows it on the row).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeriesCandidate {
    id: u64,
    name: String,
}

/// Extract candidate `series.php?id=N` links from a libgen SEARCH page, in order
/// of first appearance, restricted to rows whose title text plausibly matches
/// the request `title` (normalized containment) so a co-listed unrelated series
/// link isn't followed. Each candidate carries the series link's ANCHOR TEXT so
/// the caller can gate again on the series NAME (a single row can carry links to
/// several series, only one of which is this book's).
fn series_ids_in_search(html: &str, want_title: &str) -> Vec<SeriesCandidate> {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse("table#tablelibgen tbody tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let a_sel = Selector::parse("a").unwrap();
    let want = norm(want_title);

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.is_empty() {
            continue;
        }
        // Series links live in the title cell (cell 0).
        let title_cell = &cells[0];
        // Gate on relevance: the row's title-cell text should contain (or be
        // contained by) the request title.
        let cell_text = norm(&title_cell.text().collect::<String>());
        let relevant = cell_text.contains(&want)
            || want.contains(&cell_text)
            || tokens_overlap(&want, &cell_text);
        if !relevant {
            continue;
        }
        for a in title_cell.select(&a_sel) {
            if let Some(href) = a.value().attr("href") {
                if let Some(id) = series_id_in_href(href) {
                    if seen.insert(id) {
                        let name = clean(&a.text().collect::<String>());
                        out.push(SeriesCandidate { id, name });
                    }
                }
            }
        }
    }
    out
}

/// Parse a `series.php?id=N` numeric id from an href like `series.php?id=364378`.
fn series_id_in_href(href: &str) -> Option<u64> {
    let lower = href.to_ascii_lowercase();
    let idx = lower.find("series.php?id=")?;
    let rest = &lower[idx + "series.php?id=".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse a libgen `series.php` page into `(series_name, members)`.
///
/// Row structure (oracle-validated): `td0` green edition link · `td1` cover
/// `/comicscovers/<bucket>/<md5>.jpg` · a YEAR cell · narrow numeric VOLUME
/// cells · a wide (`width=20%`) TITLE cell (the `edition.php` anchor text) ·
/// author. We take the TITLE from the wide cell, the VOLUME from the populated
/// narrow numeric cell that is NOT the year, and the md5 from the cover path.
/// Members are returned ordered by volume number (ascending), titleless rows
/// dropped.
fn parse_series_page(html: &str, host: &str) -> (String, Vec<SeriesMember>) {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse("table#tablelibgen tbody tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let a_sel = Selector::parse("a").unwrap();

    let name = series_page_name(&doc);

    let mut members: Vec<(Option<u32>, SeriesMember)> = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.len() < 8 {
            continue;
        }
        // Wide cells: the FIRST `td` with `width="20%"` is the title; the
        // SECOND is the author.
        let wide: Vec<_> = cells
            .iter()
            .filter(|c| cell_width_is(c, "20%"))
            .copied()
            .collect();
        let title = match wide.first() {
            Some(c) => clean(&c.text().collect::<String>()),
            None => continue,
        };
        if title.is_empty() {
            continue; // Strip-style row with no title — not a book member
        }
        let author = wide
            .get(1)
            .map(|c| clean(&c.text().collect::<String>()))
            .filter(|a| !a.is_empty());

        // Cover md5 from the row's `/comicscovers/…/<md5>.jpg`.
        let cover_href = row
            .select(&a_sel)
            .filter_map(|a| a.value().attr("href"))
            .find(|h| h.contains("/comicscovers/") || h.contains("/covers/"))
            .map(str::to_string);
        let md5 = cover_href.as_deref().and_then(md5_in_cover_path);
        let cover_url = cover_href.map(|h| {
            let full = h.replace("_small.jpg", ".jpg");
            if full.starts_with("http") {
                full
            } else {
                format!(
                    "https://{}/{}",
                    host.trim_end_matches('/'),
                    full.trim_start_matches('/')
                )
            }
        });

        // Volume number: the value in a narrow (`width="4%"`) numeric cell that
        // is NOT the year (year is the FIRST such cell). Take the populated one
        // whose value is a small sequence number.
        let volume = volume_from_narrow_cells(&cells);

        members.push((
            volume,
            SeriesMember {
                title,
                position: volume,
                md5,
                cover_url,
                author,
            },
        ));
    }

    // Order by volume number ascending; titleless already dropped. Members
    // without a parsed volume keep page order after the numbered ones.
    members.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let out: Vec<SeriesMember> = members.into_iter().map(|(_, m)| m).collect();
    (name, out)
}

/// Whether an element's `width` attribute equals `want` (e.g. `"20%"`).
fn cell_width_is(cell: &scraper::ElementRef, want: &str) -> bool {
    cell.value()
        .attr("width")
        .map(|w| w.trim() == want)
        .unwrap_or(false)
}

/// Extract the volume number from a series-page row's narrow numeric cells. The
/// FIRST `width="4%"` cell is the YEAR (e.g. 2018); the volume is a later narrow
/// cell holding a small number (1..=999). Returns the first such small number
/// after the year cell.
fn volume_from_narrow_cells(cells: &[scraper::ElementRef]) -> Option<u32> {
    let mut narrow_seen = 0usize;
    for c in cells {
        if !cell_width_is(c, "4%") {
            continue;
        }
        narrow_seen += 1;
        // Skip the first narrow cell (the year).
        if narrow_seen == 1 {
            continue;
        }
        let txt = clean(&c.text().collect::<String>());
        if let Ok(n) = txt.parse::<u32>() {
            if (1..=999).contains(&n) {
                return Some(n);
            }
        }
    }
    None
}

/// Best-effort series name from a `series.php` page: the `<h1>`/`<title>` text,
/// stripped of a trailing " - libgen" style suffix. Empty when not found.
fn series_page_name(doc: &Html) -> String {
    for sel in ["h1", "title"] {
        if let Ok(s) = Selector::parse(sel) {
            if let Some(el) = doc.select(&s).next() {
                let t = clean(&el.text().collect::<String>());
                if !t.is_empty() {
                    // Drop a "Series: " prefix and any trailing site suffix.
                    let t = t.split('|').next().unwrap_or(&t).trim().to_string();
                    if !t.is_empty() {
                        return t;
                    }
                }
            }
        }
    }
    String::new()
}

/// Extract the 32-hex md5 from a cover path `/comicscovers/<bucket>/<md5>.jpg`
/// (or `_small.jpg`).
fn md5_in_cover_path(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file
        .trim_end_matches(".jpg")
        .trim_end_matches(".jpeg")
        .trim_end_matches(".png")
        .trim_end_matches("_small");
    if stem.len() == 32 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(stem.to_ascii_lowercase())
    } else {
        None
    }
}

// ===========================================================================
// Source C — Goodreads (autocomplete → book → series)
// ===========================================================================
//
// Recipe (validated against the raw pages, kept under fixtures/series/):
//   1. `GET /book/auto_complete?format=json&q=<title>` → JSON matches; each has
//      a `bookId` and a `title` like "Alice's Adventures in Wonderland (Alice ... #1)".
//      Pick the best title match.
//   2. `GET /book/show/<id>` → find the `(<Series>, #N)` link → `/series/<id>`.
//   3. `GET /series/<id>` is SERVER-RENDERED: each member is `<h3>Book N</h3>`
//      followed by a `<div class="responsiveBook">` whose `itemprop="name"` span
//      is the title. Parse member titles + the "Book N" order.

const GOODREADS_BASE: &str = "https://www.goodreads.com";

/// Looks a book's series up on Goodreads and returns its ordered members.
/// Transport-abstracted via [`crate::search::Transport`]; offline-testable.
pub struct GoodreadsClient {
    transport: Box<dyn crate::search::Transport>,
}

impl GoodreadsClient {
    pub fn new(transport: Box<dyn crate::search::Transport>) -> Self {
        GoodreadsClient { transport }
    }

    pub fn live() -> Self {
        Self::new(Box::new(crate::search::LiveTransport::new()))
    }

    pub fn replay(fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::new(Box::new(crate::search::ReplayTransport::new(
            fixtures_dir.into(),
        )))
    }

    pub fn recording(fixtures_dir: impl Into<PathBuf>) -> Self {
        let live: Box<dyn crate::search::Transport> = Box::new(crate::search::LiveTransport::new());
        Self::new(Box::new(crate::search::RecordingTransport::new(
            live,
            fixtures_dir.into(),
        )))
    }

    /// Look up the series `title` belongs to on Goodreads. `Ok(None)` when no
    /// match, or the matched book is a standalone.
    pub async fn lookup(&self, title: &str, author: &str) -> Result<Option<Series>> {
        // 1. Autocomplete → best book id.
        let ac_url = format!(
            "{GOODREADS_BASE}/book/auto_complete?format=json&q={}",
            search_url_encode(title)
        );
        let ac_body = self.transport.get(&ac_url).await?;
        let book_id = match pick_goodreads_book(&ac_body, title) {
            Some(id) => id,
            None => return Ok(None),
        };

        // 2. Book page → series id.
        let book_url = format!("{GOODREADS_BASE}/book/show/{book_id}");
        let book_body = self.transport.get(&book_url).await?;
        let series_id = match goodreads_series_id(&book_body) {
            Some(s) => s,
            None => return Ok(None), // standalone
        };

        // 3. Series page → ordered members.
        let series_url = format!("{GOODREADS_BASE}/series/{series_id}");
        let series_body = self.transport.get(&series_url).await?;
        let (name, members) = parse_goodreads_series(&series_body);
        if members.is_empty() {
            return Ok(None);
        }
        let mut series = Series {
            key: format!("goodreads:{series_id}"),
            name: if name.is_empty() {
                series_name(title)
            } else {
                name
            },
            members,
        };
        // Goodreads members share the seed book's author; fill it in so the
        // seeded list's libgen query isn't title-only.
        fill_member_authors(&mut series, author);
        Ok(Some(series))
    }
}

#[derive(Debug, Deserialize)]
struct GrMatch {
    #[serde(default, rename = "bookId")]
    book_id: String,
    #[serde(default)]
    title: String,
}

/// Pick the best Goodreads `bookId` from the autocomplete JSON: prefer the match
/// whose title (sans the trailing `(Series #N)`) best matches the request, with
/// the JSON's own rank order as the tiebreak (first = best).
fn pick_goodreads_book(body: &str, want_title: &str) -> Option<String> {
    let matches: Vec<GrMatch> = serde_json::from_str(body).ok()?;
    let want = norm(want_title);
    let mut best: Option<(i32, usize, String)> = None;
    for (idx, m) in matches.iter().enumerate() {
        if m.book_id.is_empty() {
            continue;
        }
        let bare = norm(&strip_series_suffix(&m.title));
        let score = if bare == want {
            3
        } else if bare.contains(&want) || want.contains(&bare) {
            2
        } else if tokens_overlap(&want, &bare) {
            1
        } else {
            0
        };
        if score == 0 {
            continue;
        }
        // Higher score wins; on a tie the earlier (higher-ranked) match wins.
        let better = match &best {
            Some((s, i, _)) => score > *s || (score == *s && idx < *i),
            None => true,
        };
        if better {
            best = Some((score, idx, m.book_id.clone()));
        }
    }
    best.map(|(_, _, id)| id)
}

/// Drop a trailing `(Series Name #N)` from a Goodreads title.
fn strip_series_suffix(title: &str) -> String {
    match title.rfind('(') {
        Some(idx) => title[..idx].trim().to_string(),
        None => title.trim().to_string(),
    }
}

/// Extract the Goodreads `/series/<id>` numeric id a book page links to (the
/// `(<Series>, #N)` link). Returns the id as a string slug-free number.
fn goodreads_series_id(html: &str) -> Option<String> {
    let needle = "/series/";
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(rel) = html[i..].find(needle) {
        let start = i + rel + needle.len();
        let digits: String = html[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
        i = start;
        if i >= bytes.len() {
            break;
        }
    }
    None
}

/// Parse a server-rendered Goodreads `/series/<id>` page into `(name, members)`.
/// Each member is an `<h3 ...>Book N</h3>` header followed by a
/// `<div class="responsiveBook">` whose first `itemprop="name"` span holds the
/// title. Ordered by the `Book N` number.
fn parse_goodreads_series(html: &str) -> (String, Vec<SeriesMember>) {
    let name = goodreads_series_name(html);
    let mut members = Vec::new();

    // Split on the "Book N" h3 headers; the title is the next itemprop="name".
    let bytes = html.as_bytes();
    let h3 = "<h3";
    let mut i = 0;
    while let Some(rel) = html[i..].find(h3) {
        let tag_start = i + rel;
        // Find the end of this <h3 ...> tag and its text up to </h3>.
        let after = match html[tag_start..].find('>') {
            Some(g) => tag_start + g + 1,
            None => break,
        };
        let close = match html[after..].find("</h3>") {
            Some(c) => after + c,
            None => break,
        };
        let label = clean(&strip_tags(&html[after..close]));
        i = close + "</h3>".len();
        // Is this a "Book N" header?
        let n = book_label_number(&label);
        if let Some(num) = n {
            // The title is the next `itemprop="name">…</span>` after this header,
            // but BEFORE the next <h3 (so we don't borrow a later book's title).
            let next_h3 = html[i..].find(h3).map(|r| i + r).unwrap_or(bytes.len());
            let segment = &html[i..next_h3];
            if let Some(title) = first_itemprop_name(segment) {
                // Goodreads series pages list box sets / omnibuses / "N-Book Set"
                // bundles AS members, often sharing a volume number with the real
                // book (e.g. "…Box Set Volume 1-4" at #1). Drop them so the order
                // stays one-real-book-per-position.
                if !is_collection(&title) {
                    members.push(SeriesMember {
                        title,
                        position: Some(num),
                        md5: None,
                        cover_url: None,
                        author: None,
                    });
                }
            }
        }
        if i >= bytes.len() {
            break;
        }
    }

    members.sort_by_key(|m| m.position.unwrap_or(u32::MAX));
    // Keep the FIRST real book at each position (drop later same-#N duplicates,
    // e.g. "Two Books in One" omnibuses that survived the collection filter).
    let mut seen = std::collections::HashSet::new();
    members.retain(|m| match m.position {
        Some(p) => seen.insert(p),
        None => true,
    });
    (name, members)
}

/// Parse a "Book N" label into its number.
fn book_label_number(label: &str) -> Option<u32> {
    let l = label.trim();
    let rest = l
        .strip_prefix("Book ")
        .or_else(|| l.strip_prefix("book "))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// First `itemprop="name">TEXT</span>` in a fragment.
fn first_itemprop_name(frag: &str) -> Option<String> {
    let needle = "itemprop=\"name\"";
    let idx = frag.find(needle)?;
    let after = frag[idx..].find('>').map(|g| idx + g + 1)?;
    let close = frag[after..].find("</span>").map(|c| after + c)?;
    let txt = clean(&strip_tags(&frag[after..close]));
    if txt.is_empty() {
        None
    } else {
        Some(txt)
    }
}

/// Series name from a Goodreads series page `<h1>` (e.g. "Alice's Adventures in Wonderland
/// Series"); the trailing " Series" word is dropped.
fn goodreads_series_name(html: &str) -> String {
    let doc = Html::parse_document(html);
    if let Ok(sel) = Selector::parse("h1") {
        if let Some(el) = doc.select(&sel).next() {
            let t = clean(&el.text().collect::<String>());
            let t = t
                .strip_suffix(" Series")
                .or_else(|| t.strip_suffix(" series"))
                .unwrap_or(&t)
                .to_string();
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Shared small helpers for the libgen/goodreads sources
// ---------------------------------------------------------------------------

use scraper::{Html, Selector};

/// Collapse whitespace runs to single spaces and trim.
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip HTML tags from a fragment, leaving the text.
fn strip_tags(frag: &str) -> String {
    let mut out = String::with_capacity(frag.len());
    let mut in_tag = false;
    for ch in frag.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// ≥60% of the significant (>2 char) tokens of `a` appear in `b`.
fn tokens_overlap(a: &str, b: &str) -> bool {
    let at: std::collections::BTreeSet<&str> =
        a.split_whitespace().filter(|t| t.len() > 2).collect();
    if at.is_empty() {
        return false;
    }
    let bt: std::collections::BTreeSet<&str> = b.split_whitespace().collect();
    let overlap = at.iter().filter(|t| bt.contains(*t)).count();
    (overlap as f32) / (at.len() as f32) >= 0.6
}

/// libgen/goodreads query encoding (same rules as the OL `url_encode`).
fn search_url_encode(s: &str) -> String {
    url_encode(s)
}

/// Fill in any member whose `author` is missing/blank with the seed book's
/// author. Series members share an author in the OL/Goodreads paths (and on
/// libgen rows that omit the author cell), so this keeps every seeded book from
/// falling back to a title-only libgen query. A no-op when the seed author is
/// itself empty.
fn fill_member_authors(series: &mut Series, seed_author: &str) {
    let seed = seed_author.trim();
    if seed.is_empty() {
        return;
    }
    for m in &mut series.members {
        let has_author = m.author.as_deref().map(|a| !a.trim().is_empty()) == Some(true);
        if !has_author {
            m.author = Some(seed.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Shared multi-source resolver (Open Library → libgen → Goodreads)
// ---------------------------------------------------------------------------

/// Where the multi-source resolver should read from: `Live` hits the network,
/// `Replay(dir)` replays recorded fixtures from `dir` (offline). All three
/// sources share the one dir.
#[derive(Clone, Copy)]
pub enum SeriesSource<'a> {
    Live,
    Replay(&'a std::path::Path),
}

/// Consult the THREE equal sources in turn — Open Library, libgen `series.php`,
/// Goodreads — and return the FIRST that yields a usable series (≥ 2 members),
/// paired with the name of the source that produced it
/// (`"open_library"` / `"libgen"` / `"goodreads"`).
///
/// A source erroring (a network blip, a missing replay fixture) is treated like
/// "not found here" and never blocks the others. Shared by the CLI, the TUI, and
/// the desktop "download series" commands so the resolution order can never
/// drift between frontends. Returns `None` when no source finds a series.
///
/// **Author degradation:** an over-specified or wrong author can block the Open
/// Library work match. So if the authored attempt finds nothing AND an author
/// was given, we retry the whole sweep once with NO author (libgen/Goodreads
/// find the series by title alone). This is why a manual add with a backfilled
/// author still resolves even when that author is imperfect.
pub async fn resolve_series(
    title: &str,
    author: &str,
    source: SeriesSource<'_>,
) -> Option<(Series, &'static str)> {
    if let Some(hit) = resolve_series_once(title, author, source).await {
        return Some(hit);
    }
    if !author.trim().is_empty() {
        if let Some(hit) = resolve_series_once(title, "", source).await {
            return Some(hit);
        }
    }
    None
}

/// Cap on the number of candidate titles the multi-seed resolver tries.
const MAX_SEEDS: usize = 5;

/// A book's discovered download candidate, reduced to what series-seed selection
/// needs. Both frontends map their own candidate/variation type onto this.
#[derive(Debug, Clone)]
pub struct SeedCandidate {
    /// The candidate's own (bibliographic) title, as the mirror reported it.
    pub title: String,
    /// The candidate's author(s), joined; may be empty.
    pub author: String,
    /// Matcher confidence 0.0..=1.0.
    pub score: f32,
    /// `true` when this copy is already selected / armed / downloading — the
    /// user's explicit pick.
    pub armed: bool,
}

/// Order candidate titles into a preference-ranked list of `(title, author)`
/// seeds for the reverse (book→series) lookup — best first, de-duplicated, at
/// most [`MAX_SEEDS`]. A manual add's INPUT title may be a bare series name that
/// never resolves, so we seed from real member CANDIDATES instead. Cases:
///
///  1. **Armed copies present** → those first (by score), then top up the
///     remainder from the unselected candidates (by score).
///  2. **< 3 candidates** → all of them, by score.
///  3. **≥ 3 candidates** → drop box-set titles, but IGNORE any collection
///     keyword shared by a MAJORITY of candidates (it is part of the series
///     name, not a bundle marker); keep the survivors by score (fall back to all
///     by score if the filter removes everything).
///
/// With no usable candidate the book `input` is the sole (last-resort) seed.
pub fn order_series_seeds(
    cands: &[SeedCandidate],
    input_title: &str,
    input_author: &str,
) -> Vec<(String, String)> {
    let non_empty: Vec<&SeedCandidate> =
        cands.iter().filter(|c| !c.title.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return vec![(input_title.to_string(), input_author.to_string())];
    }
    let by_score = |v: &mut Vec<&SeedCandidate>| {
        v.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    };

    let armed: Vec<&SeedCandidate> = non_empty.iter().copied().filter(|c| c.armed).collect();
    let mut ranked: Vec<&SeedCandidate> = if !armed.is_empty() {
        // Case 1: armed first, then fill from the unselected.
        let mut a = armed.clone();
        by_score(&mut a);
        let mut rest: Vec<&SeedCandidate> =
            non_empty.iter().copied().filter(|c| !c.armed).collect();
        by_score(&mut rest);
        a.extend(rest);
        a
    } else if non_empty.len() < 3 {
        // Case 2: all candidates by score.
        let mut a = non_empty.clone();
        by_score(&mut a);
        a
    } else {
        // Case 3: majority-keyword-aware member filter, then by score.
        let members = majority_member_filter(&non_empty);
        let mut m = if members.is_empty() {
            non_empty.clone()
        } else {
            members
        };
        by_score(&mut m);
        m
    };

    // A candidate's seed author: its own, else the book input, else any candidate.
    let backfill = |c: &SeedCandidate| -> String {
        if !c.author.trim().is_empty() {
            c.author.clone()
        } else if !input_author.trim().is_empty() {
            input_author.to_string()
        } else {
            non_empty
                .iter()
                .map(|x| x.author.clone())
                .find(|a| !a.trim().is_empty())
                .unwrap_or_default()
        }
    };

    // De-dupe by normalized title, cap at MAX_SEEDS.
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    ranked.retain(|c| seen.insert(norm(&c.title)));
    for c in ranked.into_iter().take(MAX_SEEDS) {
        out.push((c.title.clone(), backfill(c)));
    }
    out
}

/// Case-3 helper: keep candidates that are NOT box sets, treating a collection
/// keyword shared by a MAJORITY of candidates as part of the series name (so it
/// is ignored) and number-range bundles as always box sets.
fn majority_member_filter<'a>(cands: &[&'a SeedCandidate]) -> Vec<&'a SeedCandidate> {
    let n = cands.len();
    let lowers: Vec<String> = cands.iter().map(|c| c.title.to_lowercase()).collect();
    let ambient: Vec<&str> = COLLECTION_KEYWORDS
        .iter()
        .copied()
        .filter(|kw| lowers.iter().filter(|t| t.contains(kw)).count() * 2 > n)
        .collect();
    cands
        .iter()
        .copied()
        .enumerate()
        .filter(|(i, _)| {
            let lower = &lowers[*i];
            let boxy = COLLECTION_KEYWORDS
                .iter()
                .copied()
                .any(|kw| !ambient.contains(&kw) && lower.contains(kw))
                || has_number_range(lower);
            !boxy
        })
        .map(|(_, c)| c)
        .collect()
}

/// Multi-seed reverse lookup (the robust path): try each ordered seed (see
/// [`order_series_seeds`]) through [`resolve_series`] until one yields a series
/// (≥ 2 members). The rules-picked primary seed is tried first; the rest are
/// assurance against a mislabeled or unresolvable top pick. `None` when no seed
/// resolves. Normally the first seed resolves (a single lookup).
pub async fn resolve_series_from_candidates(
    cands: &[SeedCandidate],
    input_title: &str,
    input_author: &str,
    source: SeriesSource<'_>,
) -> Option<(Series, &'static str)> {
    for (title, author) in order_series_seeds(cands, input_title, input_author) {
        if let Some(hit) = resolve_series(&title, &author, source).await {
            return Some(hit);
        }
    }
    None
}

/// One pass over the three sources with a fixed `(title, author)`. See
/// [`resolve_series`], which wraps this with the author-degradation retry.
async fn resolve_series_once(
    title: &str,
    author: &str,
    source: SeriesSource<'_>,
) -> Option<(Series, &'static str)> {
    // Open Library.
    let ol = match source {
        SeriesSource::Replay(dir) => SeriesClient::replay(dir),
        SeriesSource::Live => SeriesClient::live(),
    };
    if let Ok(Some(s)) = ol.lookup(title, author).await {
        if s.members.len() >= 2 {
            return Some((s, "open_library"));
        }
    }

    // libgen series.php.
    let libgen = match source {
        SeriesSource::Replay(dir) => LibgenSeriesClient::replay(dir),
        SeriesSource::Live => LibgenSeriesClient::live(),
    };
    if let Ok(Some(s)) = libgen.lookup(title, author).await {
        if s.members.len() >= 2 {
            return Some((s, "libgen"));
        }
    }

    // Goodreads.
    let goodreads = match source {
        SeriesSource::Replay(dir) => GoodreadsClient::replay(dir),
        SeriesSource::Live => GoodreadsClient::live(),
    };
    if let Ok(Some(s)) = goodreads.lookup(title, author).await {
        if s.members.len() >= 2 {
            return Some((s, "goodreads"));
        }
    }

    None
}

#[cfg(test)]
mod ref_parse_tests {
    use super::*;

    #[test]
    fn parses_full_url_with_slug() {
        let (key, name) = parse_series_ref(
            "https://openlibrary.org/series/OL326111L/A_Series_of_Unfortunate_Events",
        )
        .expect("should parse");
        assert_eq!(key, "OL326111L");
        assert_eq!(name.as_deref(), Some("A Series of Unfortunate Events"));
    }

    #[test]
    fn parses_path_and_bare_key() {
        assert_eq!(
            parse_series_ref("/series/OL326111L"),
            Some(("OL326111L".to_string(), None))
        );
        assert_eq!(
            parse_series_ref("OL326111L"),
            Some(("OL326111L".to_string(), None))
        );
        // Trailing slash after the key, no slug.
        assert_eq!(
            parse_series_ref("openlibrary.org/series/OL1L/"),
            Some(("OL1L".to_string(), None))
        );
    }

    #[test]
    fn rejects_non_series_refs() {
        // A works key is not a series key.
        assert_eq!(parse_series_ref("OL45804W"), None);
        assert_eq!(parse_series_ref("/works/OL45804W"), None);
        assert_eq!(parse_series_ref("not a key"), None);
        assert_eq!(parse_series_ref(""), None);
    }

    fn seed(title: &str, author: &str, score: f32, armed: bool) -> SeedCandidate {
        SeedCandidate {
            title: title.into(),
            author: author.into(),
            score,
            armed,
        }
    }

    #[test]
    fn order_seeds_majority_keyword_is_ignored_case3() {
        // The reported case: every candidate carries "series" (majority → part of
        // the name, ignored), while "collection"/number-range are minority → box
        // sets and get dropped. Members rank by score; the series name is NOT a seed.
        let cands = vec![
            seed("A Series of Unfortunate Events Collection", "Lemony, Lemony A", 0.62, false),
            seed("A Series Of Unfortunate Events 10 Slippery Slope", "Snicket, Lemony", 0.49, false),
            seed("A Series of Unfortunate Events: The Beatrice Letters", "Lemony Snicket", 0.48, false),
            seed("A Series of Unfortunate Events Collection: Books 4-6", "Lemony Snicket", 0.45, false),
            seed("A series of unfortunate events, 4. The miserable mill", "Lemony Snicket", 0.45, false),
        ];
        let seeds = order_series_seeds(&cands, "a series of unfortunate events", "");
        let titles: Vec<&str> = seeds.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles[0], "A Series Of Unfortunate Events 10 Slippery Slope");
        assert!(
            !titles.iter().any(|t| t.contains("Collection")),
            "box sets dropped: {titles:?}"
        );
        assert!(
            !titles.iter().any(|t| *t == "a series of unfortunate events"),
            "the bare series name is never a seed: {titles:?}"
        );
        assert_eq!(seeds[0].1, "Snicket, Lemony", "author backfilled from the seed");
    }

    #[test]
    fn order_seeds_armed_first_then_fill_case1() {
        // An armed (user-picked) copy leads, even at a lower score; the rest fill
        // in by score behind it.
        let cands = vec![
            seed("Top Match Box", "A", 0.9, false),
            seed("Picked Volume Two", "B", 0.4, true),
            seed("Another Volume", "C", 0.7, false),
        ];
        let seeds = order_series_seeds(&cands, "input", "");
        assert_eq!(seeds[0].0, "Picked Volume Two", "armed copy leads");
        assert_eq!(seeds[1].0, "Top Match Box", "then unselected by score");
        assert_eq!(seeds[2].0, "Another Volume");
    }

    #[test]
    fn order_seeds_few_candidates_by_score_case2() {
        let cands = vec![seed("Low", "", 0.3, false), seed("High", "", 0.8, false)];
        let seeds = order_series_seeds(&cands, "input", "");
        let titles: Vec<&str> = seeds.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, ["High", "Low"]);
    }

    #[test]
    fn order_seeds_no_candidates_falls_back_to_input() {
        let seeds = order_series_seeds(&[], "The Bad Beginning", "Lemony Snicket");
        assert_eq!(seeds, vec![("The Bad Beginning".to_string(), "Lemony Snicket".to_string())]);
    }

    #[test]
    fn deslugify_normalizes_separators() {
        assert_eq!(deslugify("A_Series_of-Unfortunate+Events"), "A Series of Unfortunate Events");
        assert_eq!(deslugify(""), "");
    }

    /// `series_by_key` fetches members straight from the members search and
    /// orders them by `first_publish_year`; the name honors an explicit hint and
    /// otherwise falls back to a name derived from the first member's title.
    /// Fully offline: one recorded members-search fixture, replayed. (The
    /// per-member `work.json` position fetches simply miss in replay, so ordering
    /// falls back to publish year — exactly the path exercised here.)
    #[tokio::test]
    async fn series_by_key_builds_ordered_series_from_members_search() {
        let dir = tempfile::tempdir().unwrap();
        // The exact URL `raw_members_for_series_key` builds for key "OL999L".
        let members_url = format!(
            "{OL_BASE}/search.json?q=series_key:OL999L&fields=key,title,subtitle,first_publish_year&limit=60"
        );
        // Deliberately out of publish order in the doc list, to prove ordering.
        let body = r#"{"docs":[
          {"key":"/works/OL3W","title":"Third","first_publish_year":2003},
          {"key":"/works/OL1W","title":"First","first_publish_year":2001},
          {"key":"/works/OL2W","title":"Second","subtitle":"A Tale","first_publish_year":2002}
        ]}"#;
        std::fs::write(dir.path().join(format!("{}.json", fixture_key(&members_url))), body).unwrap();

        let client = SeriesClient::replay(dir.path());

        // With an explicit name hint (e.g. from the URL slug).
        let series = client
            .series_by_key("OL999L", Some("A Series of Unfortunate Events"))
            .await
            .unwrap()
            .expect("members present → a series");
        assert_eq!(series.key, "OL999L");
        assert_eq!(series.name, "A Series of Unfortunate Events");
        let titles: Vec<&str> = series.members.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, ["First", "Second: A Tale", "Third"], "ordered by year");

        // Without a hint, the name falls back to the first member's title.
        let series2 = client
            .series_by_key("/series/OL999L", None)
            .await
            .unwrap()
            .expect("members present → a series");
        assert_eq!(series2.name, "First");
    }

    #[test]
    fn author_param_omits_empty_author() {
        // An empty `author=` makes Open Library 500 — the param must be omitted.
        assert_eq!(author_param(""), "");
        assert_eq!(author_param("   "), "");
        assert_eq!(author_param("Lemony Snicket"), "&author=Lemony+Snicket");
    }

    #[test]
    fn best_series_key_from_search_index() {
        // A member doc carries `series_key` in the SEARCH INDEX even though its
        // work JSON's `series` field is empty. Pick that key; ignore keyless and
        // title-irrelevant docs.
        let body = r#"{"docs":[
          {"key":"/works/OLxW","title":"Unrelated Book","series_key":["OL999L"]},
          {"key":"/works/OL1W","title":"The Bad Beginning","series_key":["/series/OL326111L"]},
          {"key":"/works/OL2W","title":"The Bad Beginning (annotated)"}
        ]}"#;
        assert_eq!(
            best_series_key_in_search(body, "The Bad Beginning", "Lemony Snicket").as_deref(),
            Some("OL326111L"),
            "picks the relevant doc's series_key, stripped of the /series/ prefix"
        );
        // No doc carries a series key → None.
        let none = r#"{"docs":[{"key":"/works/OL9W","title":"A Series of Unfortunate Events Box"}]}"#;
        assert!(best_series_key_in_search(none, "A Series of Unfortunate Events", "").is_none());
    }

    /// The reverse lookup recovers the series via the SEARCH-INDEX `series_key`
    /// when the primary path (work JSON `series`) is empty — fully offline.
    #[tokio::test]
    async fn lookup_recovers_series_via_search_index_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let write = |url: &str, body: &str| {
            std::fs::write(dir.path().join(format!("{}.json", fixture_key(url))), body).unwrap()
        };
        // 1. Primary title search → one work, no series field on its work JSON.
        write(
            &format!(
                "{OL_BASE}/search.json?title={}&fields=key,title,subtitle,author_name&limit=5",
                url_encode("The Bad Beginning")
            ),
            r#"{"docs":[{"key":"/works/OL1W","title":"The Bad Beginning"}]}"#,
        );
        write(
            &format!("{OL_BASE}/works/OL1W.json"),
            r#"{"title":"The Bad Beginning"}"#, // no `series` → primary yields None
        );
        // 2. Slow-fallback title search (fields include series_key) → member doc
        //    carrying the series key.
        write(
            &format!(
                "{OL_BASE}/search.json?title={}&fields=key,title,author_name,series_key&limit=20",
                url_encode("The Bad Beginning")
            ),
            r#"{"docs":[{"key":"/works/OL1W","title":"The Bad Beginning","series_key":["OL326111L"]}]}"#,
        );
        // 3. Members search for that key.
        write(
            &format!(
                "{OL_BASE}/search.json?q=series_key:OL326111L&fields=key,title,subtitle,first_publish_year&limit=60"
            ),
            r#"{"docs":[
              {"key":"/works/OL1W","title":"The Bad Beginning","first_publish_year":1999},
              {"key":"/works/OL2W","title":"The Reptile Room","first_publish_year":1999}
            ]}"#,
        );
        // 4. Series page name.
        write(
            &format!("{OL_BASE}/series/OL326111L.json"),
            r#"{"name":"A Series of Unfortunate Events"}"#,
        );

        let series = SeriesClient::replay(dir.path())
            .lookup("The Bad Beginning", "")
            .await
            .unwrap()
            .expect("search-index fallback recovers the series");
        assert_eq!(series.key, "OL326111L");
        assert_eq!(series.name, "A Series of Unfortunate Events");
        assert_eq!(series.members.len(), 2);
    }

    /// `series_by_key` on a key with no members (empty search, no HTML links) is
    /// a miss, not an error.
    #[tokio::test]
    async fn series_by_key_no_members_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let members_url = format!(
            "{OL_BASE}/search.json?q=series_key:OL999L&fields=key,title,subtitle,first_publish_year&limit=60"
        );
        std::fs::write(
            dir.path().join(format!("{}.json", fixture_key(&members_url))),
            r#"{"docs":[]}"#,
        )
        .unwrap();
        // No HTML series-page fixture recorded → the scrape fallback also misses.
        let client = SeriesClient::replay(dir.path());
        assert!(client.series_by_key("OL999L", None).await.unwrap().is_none());
    }
}

#[cfg(test)]
mod libgen_goodreads_tests {
    use super::*;

    // --- libgen series.php ------------------------------------------------

    #[test]
    fn libgen_series_id_parsed_from_href() {
        assert_eq!(series_id_in_href("series.php?id=364378"), Some(364378));
        assert_eq!(series_id_in_href("/series.php?id=364379&x=1"), Some(364379));
        assert_eq!(series_id_in_href("edition.php?id=5"), None);
    }

    #[test]
    fn libgen_member_row_title_from_wide_cell_not_year() {
        // A series-page row mirroring the validated layout: green link, cover,
        // YEAR (width=4%), then a numeric volume cell (width=4%), then the wide
        // (width=20%) TITLE cell. The parser must take the TITLE, not the year.
        let html = r#"<table id="tablelibgen"><thead><tr><th>h</th></tr></thead><tbody>
        <tr>
          <td bgcolor="green"><a href="edition.php?id=1">&nbsp;</a></td>
          <td wigth=50><a href="/comicscovers/1121000/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jpg"><img src="/comicscovers/1121000/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_small.jpg"></a></td>
          <td width=4%><nobr><a href="edition.php?id=1">2015</a></td>
          <td width=4%><nobr><a href="edition.php?id=1"></a></td>
          <td width=4%><nobr><a href="edition.php?id=1">2</a></td>
          <td width=4%><nobr></td>
          <td width=4%><nobr></td>
          <td width=20%><a href="edition.php?id=1">Through the Looking-Glass</a></td>
          <td width=20%>Lewis Carroll</td>
          <td></td>
        </tr>
        </tbody></table>"#;
        let (_name, members) = parse_series_page(html, "libgen.li");
        assert_eq!(members.len(), 1);
        let m = &members[0];
        assert_eq!(
            m.title, "Through the Looking-Glass",
            "title must be the wide cell"
        );
        assert_eq!(
            m.position,
            Some(2),
            "volume from the numeric cell, not 2015"
        );
        assert_eq!(
            m.md5.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "md5 from the cover path"
        );
        assert_eq!(
            m.cover_url.as_deref(),
            Some("https://libgen.li/comicscovers/1121000/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jpg")
        );
        assert_eq!(
            m.author.as_deref(),
            Some("Lewis Carroll"),
            "author from the SECOND width=20% cell"
        );
    }

    #[test]
    fn libgen_picks_tpb_over_strip_by_titled_rows() {
        // The Alice fixtures: search page lists BOTH 364378 (TPB) and 364379
        // (Strip). The resolver picks the series whose rows carry real titles.
        let dir = fixtures_dir();
        let search =
            std::fs::read_to_string(dir.join("alice-s-adventures-in-wonderland.html")).unwrap();
        let ids: Vec<u64> = series_ids_in_search(&search, "Alice's Adventures in Wonderland")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(ids.contains(&364378), "TPB id present: {ids:?}");
        assert!(ids.contains(&364379), "Strip id present: {ids:?}");

        let tpb =
            std::fs::read_to_string(dir.join("https-libgen-li-series-php-id-364378.html")).unwrap();
        let strip =
            std::fs::read_to_string(dir.join("https-libgen-li-series-php-id-364379.html")).unwrap();
        let (_n1, tpb_members) = parse_series_page(&tpb, "libgen.li");
        let (_n2, strip_members) = parse_series_page(&strip, "libgen.li");
        // TPB rows have titles; the Strip page's title cells are empty → dropped.
        let tpb_titled = tpb_members.iter().filter(|m| !m.title.is_empty()).count();
        let strip_titled = strip_members.iter().filter(|m| !m.title.is_empty()).count();
        assert!(tpb_titled >= 6, "TPB has titled members: {tpb_titled}");
        assert_eq!(strip_titled, 0, "Strip rows carry NO titles");
    }

    #[test]
    fn md5_from_cover_path_strips_small_and_ext() {
        assert_eq!(
            md5_in_cover_path("/comicscovers/1121000/b3964bc5fd7210ac4e5116bd7289ae07.jpg")
                .as_deref(),
            Some("b3964bc5fd7210ac4e5116bd7289ae07")
        );
        assert_eq!(
            md5_in_cover_path("/comicscovers/1121000/b3964bc5fd7210ac4e5116bd7289ae07_small.jpg")
                .as_deref(),
            Some("b3964bc5fd7210ac4e5116bd7289ae07")
        );
        assert_eq!(md5_in_cover_path("/comicscovers/1121000/notamd5.jpg"), None);
    }

    // --- Goodreads --------------------------------------------------------

    #[test]
    fn goodreads_autocomplete_picks_best_book() {
        let body = r#"[
          {"bookId":"22710140","title":"Alice's Adventures in Wonderland (Alice's Adventures in Wonderland #1)"},
          {"bookId":"23492248","title":"Through the Looking-Glass (Alice's Adventures in Wonderland #2)"}
        ]"#;
        // Exact base-title match wins over the rank-2 different title.
        assert_eq!(
            pick_goodreads_book(body, "Alice's Adventures in Wonderland").as_deref(),
            Some("22710140")
        );
    }

    #[test]
    fn goodreads_series_suffix_and_id() {
        assert_eq!(
            strip_series_suffix("Through the Looking-Glass (Alice's Adventures in Wonderland #2)"),
            "Through the Looking-Glass"
        );
        let html = r#"<a class="series" href="/series/146183-alice-s-adventures-in-wonderland">Alice's Adventures in Wonderland</a>"#;
        assert_eq!(goodreads_series_id(html).as_deref(), Some("146183"));
    }

    #[test]
    fn goodreads_series_page_parses_ordered_members() {
        // The Alice series page → ordered #N members, bundles filtered.
        let html = std::fs::read_to_string(
            fixtures_dir().join("https-www-goodreads-com-series-146183.html"),
        )
        .unwrap();
        let (name, members) = parse_goodreads_series(&html);
        assert_eq!(name, "Alice's Adventures in Wonderland");
        assert!(members.len() >= 6, "got {}", members.len());
        assert_eq!(members[0].title, "Alice's Adventures in Wonderland");
        assert_eq!(members[0].position, Some(1));
        assert_eq!(members[1].title, "Through the Looking-Glass");
        assert_eq!(members[2].title, "The Hunting of the Snark");
        // One real book per position (no duplicate #N from omnibuses).
        let mut positions: Vec<u32> = members.iter().filter_map(|m| m.position).collect();
        let before = positions.len();
        positions.sort_unstable();
        positions.dedup();
        assert_eq!(before, positions.len(), "positions must be unique");
    }

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("series")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_download_list_projects_members_in_order_under_one_group() {
        let series = Series {
            key: "OL123S".into(),
            name: "Oz".into(),
            members: vec![
                SeriesMember {
                    title: "The Wonderful Wizard of Oz".into(),
                    ..Default::default()
                },
                SeriesMember {
                    title: "The Marvelous Land of Oz".into(),
                    ..Default::default()
                },
            ],
        };
        let list = series.to_download_list();
        assert_eq!(list.title, "Oz (series)");
        assert_eq!(list.groups.len(), 1);
        assert_eq!(list.groups[0].name, "Oz");
        let titles: Vec<&str> = list.groups[0]
            .books
            .iter()
            .map(|b| b.input.title.as_str())
            .collect();
        assert_eq!(
            titles,
            ["The Wonderful Wizard of Oz", "The Marvelous Land of Oz"]
        );
    }

    #[test]
    fn to_download_list_carries_member_authors() {
        // A member WITH an author projects it into the query metadata; a member
        // WITHOUT one yields empty `authors` (no phantom author).
        let series = Series {
            key: "OL123S".into(),
            name: "Oz".into(),
            members: vec![
                SeriesMember {
                    title: "The Wonderful Wizard of Oz".into(),
                    author: Some("L. Frank Baum".into()),
                    ..Default::default()
                },
                SeriesMember {
                    title: "Anonymous Volume".into(),
                    author: None,
                    ..Default::default()
                },
            ],
        };
        let list = series.to_download_list();
        let books = &list.groups[0].books;
        assert_eq!(books[0].input.authors, vec!["L. Frank Baum".to_string()]);
        assert!(
            books[1].input.authors.is_empty(),
            "no author → no phantom authors entry"
        );
    }

    #[test]
    fn fill_member_authors_uses_seed_only_when_missing() {
        let mut series = Series {
            key: "k".into(),
            name: "n".into(),
            members: vec![
                SeriesMember {
                    title: "Has Author".into(),
                    author: Some("Real Author".into()),
                    ..Default::default()
                },
                SeriesMember {
                    title: "Blank Author".into(),
                    author: Some("   ".into()),
                    ..Default::default()
                },
                SeriesMember {
                    title: "No Author".into(),
                    author: None,
                    ..Default::default()
                },
            ],
        };
        fill_member_authors(&mut series, "Seed Author");
        assert_eq!(series.members[0].author.as_deref(), Some("Real Author"));
        assert_eq!(series.members[1].author.as_deref(), Some("Seed Author"));
        assert_eq!(series.members[2].author.as_deref(), Some("Seed Author"));

        // An empty seed never overwrites — leaves members untouched.
        let mut s2 = Series {
            key: "k".into(),
            name: "n".into(),
            members: vec![SeriesMember {
                title: "t".into(),
                author: None,
                ..Default::default()
            }],
        };
        fill_member_authors(&mut s2, "   ");
        assert_eq!(s2.members[0].author, None);
    }

    #[test]
    fn manual_list_shape_is_canonical() {
        let m = crate::model::DownloadList::manual();
        assert_eq!(m.title, crate::model::MANUAL_LIST_TITLE);
        assert!(m.settings.is_manual);
        assert_eq!(m.settings.naming_template, "{authors} - {title}.{ext}");
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.groups[0].name, crate::model::MANUAL_LIST_TITLE);
        assert!(m.groups[0].books.is_empty());
    }

    #[test]
    fn fixture_key_is_deterministic_and_distinct_per_url() {
        let a = fixture_key("https://openlibrary.org/works/OL17610986W.json");
        let b = fixture_key("https://openlibrary.org/works/OL17798838W.json");
        assert_ne!(a, b);
        assert_eq!(a, "https-openlibrary-org-works-ol17610986w-json");
        // Stable across calls.
        assert_eq!(
            a,
            fixture_key("https://openlibrary.org/works/OL17610986W.json")
        );
    }

    #[test]
    fn picks_exact_title_with_author() {
        let body = r#"{"docs":[
          {"key":"/works/OL1W","title":"The Marvelous Land of Oz","author_name":["L. Frank Baum"]},
          {"key":"/works/OL2W","title":"The Wonderful Wizard of Oz","author_name":["L. Frank Baum"]},
          {"key":"/works/OL3W","title":"The Wonderful Wizard of Oz","author_name":["Someone Else"]}
        ]}"#;
        let w = pick_work(body, "The Wonderful Wizard of Oz", "L. Frank Baum")
            .unwrap()
            .unwrap();
        // Exact title + matching author wins over the longer title and the
        // wrong-author exact match.
        assert_eq!(w.key, "/works/OL2W");
    }

    #[test]
    fn pick_returns_none_when_nothing_matches() {
        let body = r#"{"docs":[
          {"key":"/works/OL1W","title":"Totally Unrelated Manual","author_name":["X"]}
        ]}"#;
        assert!(
            pick_work(body, "The Wonderful Wizard of Oz", "L. Frank Baum")
                .unwrap()
                .is_none()
        );
        // Empty docs.
        assert!(pick_work(
            r#"{"docs":[]}"#,
            "The Wonderful Wizard of Oz",
            "L. Frank Baum"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn extracts_series_key_or_none() {
        let with = r#"{"series":[{"series":{"key":"/series/OL329664L"},"position":"1"}]}"#;
        assert_eq!(work_series_key(with).unwrap().as_deref(), Some("OL329664L"));
        // Bare key (no /series/ prefix) still parses.
        let bare = r#"{"series":[{"series":{"key":"OL42L"}}]}"#;
        assert_eq!(work_series_key(bare).unwrap().as_deref(), Some("OL42L"));
        // No series field → None.
        assert!(work_series_key(r#"{"title":"Standalone"}"#)
            .unwrap()
            .is_none());
        assert!(work_series_key(r#"{"series":[]}"#).unwrap().is_none());
    }

    #[test]
    fn position_handles_string_and_number() {
        assert_eq!(
            work_position(r#"{"series":[{"series":{"key":"/series/OLxL"},"position":"7"}]}"#),
            Some(7)
        );
        assert_eq!(
            work_position(r#"{"series":[{"series":{"key":"/series/OLxL"},"position":3}]}"#),
            Some(3)
        );
        assert_eq!(work_position(r#"{"series":[]}"#), None);
    }

    #[test]
    fn display_title_appends_subtitle() {
        assert_eq!(
            display_title("Ozma of Oz", Some("The Royal Book")),
            "Ozma of Oz: The Royal Book"
        );
        assert_eq!(display_title("Ozma of Oz", None), "Ozma of Oz");
        assert_eq!(display_title("Ozma of Oz", Some("  ")), "Ozma of Oz");
    }

    #[test]
    fn ordering_position_first_then_year() {
        let raw = vec![
            RawMember {
                key: "c".into(),
                title: "C".into(),
                position: None,
                first_publish_year: Some(2001),
            },
            RawMember {
                key: "a".into(),
                title: "A".into(),
                position: Some(2),
                first_publish_year: None,
            },
            RawMember {
                key: "b".into(),
                title: "B".into(),
                position: Some(1),
                first_publish_year: None,
            },
            RawMember {
                key: "d".into(),
                title: "D".into(),
                position: None,
                first_publish_year: Some(1999),
            },
        ];
        let ordered = order_raw_members(raw);
        let titles: Vec<&str> = ordered.iter().map(|m| m.title.as_str()).collect();
        // positions 1,2 first; then no-position by year ascending (1999, 2001).
        assert_eq!(titles, vec!["B", "A", "D", "C"]);
    }

    #[test]
    fn ordering_dedupes_by_key() {
        let raw = vec![
            RawMember {
                key: "a".into(),
                title: "A".into(),
                position: Some(1),
                first_publish_year: None,
            },
            RawMember {
                key: "a".into(),
                title: "A-dup".into(),
                position: Some(1),
                first_publish_year: None,
            },
        ];
        assert_eq!(order_raw_members(raw).len(), 1);
    }

    #[test]
    fn work_keys_in_html_extracts_distinct_in_order() {
        let html = r#"<a href="/works/OL17610986W">One</a>
            <a href="/works/OL17623412W">Two</a>
            <a href="/works/OL17610986W">One again</a>"#;
        assert_eq!(
            work_keys_in_html(html),
            vec!["/works/OL17610986W", "/works/OL17623412W"]
        );
    }

    #[test]
    fn series_name_drops_per_book_subtitle() {
        assert_eq!(series_name("Ozma of Oz: The Royal Book"), "Ozma of Oz");
        assert_eq!(
            series_name("The Wonderful Wizard of Oz"),
            "The Wonderful Wizard of Oz"
        );
    }

    #[test]
    fn surname_extraction() {
        assert_eq!(surname_of("L. Frank Baum").as_deref(), Some("baum"));
        assert_eq!(surname_of("Baum, L. Frank").as_deref(), Some("baum"));
        assert!(surname_of("  ").is_none());
    }

    // --- Title-prefix fallback helpers ------------------------------------

    #[test]
    fn series_prefix_requires_separator_and_two_words() {
        // Colon prefix.
        assert_eq!(
            series_prefix("Uncle Wiggily Adventures: The Skillery Scallery Alligator").as_deref(),
            Some("Uncle Wiggily Adventures")
        );
        // ` - ` dash separator.
        assert_eq!(
            series_prefix("Uncle Wiggily Adventures - Mud Turtle Tale").as_deref(),
            Some("Uncle Wiggily Adventures")
        );
        // Earliest separator wins.
        assert_eq!(
            series_prefix("Tom Swift Inventions: The Electric Rifle").as_deref(),
            Some("Tom Swift Inventions")
        );
        // Single-word prefix → no fallback.
        assert!(series_prefix("Frankenstein: A Modern Prometheus").is_none());
        assert!(series_prefix("Heidi: A Story for Children").is_none());
        // No separator → no fallback.
        assert!(series_prefix("Robinson Crusoe").is_none());
    }

    #[test]
    fn collection_filter_drops_box_sets_and_ranges() {
        for t in [
            "Tom Swift Inventions Third 3-Book Box Set",
            "Tom Swift Inventions 3-Book Box Set",
            "The Five Little Peppers Box Set",
            "The Bobbsey Twins Complete Hardcover Gift Set",
            "Uncle Wiggily Adventures Boxed Set #1-4",
            "Uncle Wiggily Adventures Collection by Howard R. Garis",
            "Tom Swift Inventions Series 1-11",
            "Five Little Peppers: Books 29-32",
            "The Bobbsey Twins Books 1-4 Boxset",
            "Cumbersome Collection (Books 1-11)",
            "An Omnibus Edition",
            "Series, Vol. 1-12 Collection Set of 12 Books",
        ] {
            assert!(is_collection(t), "should be a collection: {t}");
        }
        // Real single volumes survive.
        for t in [
            "Uncle Wiggily Adventures - Mud Turtle Tale",
            "The Submarine Boat (Tom Swift Inventions #6)",
            "The Bobbsey Twins at the Seashore",
            "The Marvelous Land of Oz",
        ] {
            assert!(!is_collection(t), "should NOT be a collection: {t}");
        }
    }

    #[test]
    fn number_range_patterns() {
        assert!(has_number_range("books 1-3"));
        assert!(has_number_range("vol. 1-12"));
        assert!(has_number_range("series 1 - 11"));
        assert!(has_number_range("third 3-book box set"));
        assert!(has_number_range("set of 12 books"));
        // A bare single number is not a range.
        assert!(!has_number_range("weirdo halloween 7"));
        assert!(!has_number_range("part 2"));
    }

    #[test]
    fn volume_number_extraction() {
        assert_eq!(
            volume_number("The Submarine Boat (Tom Swift Inventions #6)"),
            Some(6)
        );
        assert_eq!(volume_number("Some Title Book 3"), Some(3));
        assert_eq!(volume_number("Some Title Vol. 9"), Some(9));
        assert_eq!(volume_number("Some Title Volume 2"), Some(2));
        // Trailing bare number.
        assert_eq!(
            volume_number("Uncle Wiggily Adventures - Volume 5"),
            Some(5)
        );
        // No number.
        assert_eq!(
            volume_number("Uncle Wiggily Adventures - Mud Turtle Tale"),
            None
        );
    }

    #[test]
    fn sibling_volumes_keeps_real_volumes_filters_box_sets() {
        // Mirrors the live Uncle Wiggily Adventures search shape.
        let body = r#"{"docs":[
          {"key":"/works/OL1W","title":"Sammy Littletail - A Side Tale","first_publish_year":1994},
          {"key":"/works/OL2W","title":"Uncle Wiggily Adventures - Mud Turtle Tale","first_publish_year":2010},
          {"key":"/works/OL3W","title":"Uncle Wiggily Adventures - Bow Wow Tale","first_publish_year":2010},
          {"key":"/works/OL4W","title":"Uncle Wiggily Adventures Collection by Howard R. Garis","first_publish_year":2013},
          {"key":"/works/OL5W","title":"Uncle Wiggily Adventures Boxed Set #1-4","first_publish_year":2009},
          {"key":"/works/OL6W","title":"Uncle Wiggily Adventures - Mud Turtle Tale","first_publish_year":2010}
        ]}"#;
        let vols = sibling_volumes(body, "Uncle Wiggily Adventures").unwrap();
        let titles: Vec<&str> = vols.iter().map(|v| v.title.as_str()).collect();
        // Drops the non-prefix doc, the Collection, the Boxed Set, and the dup.
        assert_eq!(
            titles,
            vec![
                "Uncle Wiggily Adventures - Mud Turtle Tale",
                "Uncle Wiggily Adventures - Bow Wow Tale",
            ]
        );
    }

    #[test]
    fn order_prefix_volumes_by_number_then_year() {
        let vols = vec![
            PrefixVolume {
                title: "C".into(),
                norm_title: "c".into(),
                volume: None,
                first_publish_year: Some(2012),
            },
            PrefixVolume {
                title: "A (#1)".into(),
                norm_title: "a 1".into(),
                volume: Some(1),
                first_publish_year: Some(2010),
            },
            PrefixVolume {
                title: "B (#2)".into(),
                norm_title: "b 2".into(),
                volume: Some(2),
                first_publish_year: Some(2011),
            },
            PrefixVolume {
                title: "D".into(),
                norm_title: "d".into(),
                volume: None,
                first_publish_year: Some(2009),
            },
        ];
        let ordered = order_prefix_volumes(vols);
        let got: Vec<(Option<u32>, &str)> = ordered
            .iter()
            .map(|m| (m.position, m.title.as_str()))
            .collect();
        // Numbered first (1,2), then no-number by year (2009, 2012); positions 1..=4.
        assert_eq!(
            got,
            vec![
                (Some(1), "A (#1)"),
                (Some(2), "B (#2)"),
                (Some(3), "D"),
                (Some(4), "C"),
            ]
        );
    }

    #[test]
    fn slugify_series_name() {
        assert_eq!(
            slugify("Uncle Wiggily Adventures"),
            "uncle-wiggily-adventures"
        );
        assert_eq!(slugify("Tom Swift Inventions"), "tom-swift-inventions");
    }
}
