//! Search Library Genesis mirrors for candidates.
//!
//! Config-driven mirrors (see `mirrors.toml`), JSON endpoints preferred with
//! HTML scraping fallback. Network calls go through a [`Transport`] so responses
//! can be recorded to a fixtures dir and replayed offline for deterministic,
//! headless tests.

use crate::model::{BookInput, Candidate, Format};
use anyhow::{anyhow, Context, Result};
use scraper::{Html, Selector};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Mirror configuration
// ---------------------------------------------------------------------------

/// How a mirror's search response should be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorKind {
    /// Scrape the libgen.li results table (`id="tablelibgen"`).
    LibgenLiHtml,
    /// libgen.is/.rs style `json.php` endpoint.
    LibgenJson,
    /// Scrape the classic libgen.rs/.is `search.php` table.
    LibgenRsHtml,
    /// Scrape an Anna's Archive (`/search`) results page: card list keyed by
    /// `/md5/<32hex>` anchors rather than a table.
    AnnasArchiveHtml,
}

/// A configured search mirror.
#[derive(Debug, Clone, Deserialize)]
pub struct Mirror {
    pub host: String,
    /// URL template with `{query}` / `{limit}` placeholders.
    pub search_url: String,
    pub kind: MirrorKind,
    pub priority: u8,
}

/// A configured download resolver (md5 -> download page).
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadResolver {
    pub host: String,
    /// URL template with a `{md5}` placeholder.
    pub url: String,
    pub priority: u8,
}

/// Parsed `mirrors.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MirrorConfig {
    #[serde(default, rename = "search_mirror")]
    pub search_mirrors: Vec<Mirror>,
    #[serde(default, rename = "download_resolver")]
    pub download_resolvers: Vec<DownloadResolver>,
}

/// The default mirror configuration, baked into the binary via `include_str!`
/// so a standalone `kwire` (Homebrew / release tarball) runs without an external
/// `mirrors.toml`. Used as the fallback in [`MirrorConfig::load`].
pub const DEFAULT_TOML: &str = include_str!("../../../mirrors.toml");

impl MirrorConfig {
    /// Parse a `mirrors.toml` string. Mirrors/resolvers are returned sorted by
    /// ascending `priority`.
    pub fn from_toml(s: &str) -> Result<Self> {
        let mut cfg: MirrorConfig = toml::from_str(s).context("parsing mirrors.toml")?;
        cfg.search_mirrors.sort_by_key(|m| m.priority);
        cfg.download_resolvers.sort_by_key(|r| r.priority);
        Ok(cfg)
    }

    /// Load and parse a `mirrors.toml` file. If the file does not exist, fall
    /// back to the embedded [`DEFAULT_TOML`] so a standalone binary still works.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Self::from_toml(DEFAULT_TOML);
        }
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&s)
    }
}

// ---------------------------------------------------------------------------
// Transport abstraction (live / record / replay)
// ---------------------------------------------------------------------------

/// Abstracts the HTTP GET so searches can be recorded and replayed offline.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn get(&self, url: &str) -> Result<String>;
}

/// Live HTTP transport backed by reqwest.
pub struct LiveTransport {
    client: reqwest::Client,
}

impl LiveTransport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Kwire/1.0 (+https://example.invalid)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("building reqwest client");
        LiveTransport { client }
    }
}

impl Default for LiveTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Transport for LiveTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("status for {url}"))?;
        Ok(resp
            .text()
            .await
            .with_context(|| format!("body for {url}"))?)
    }
}

/// A deterministic, filesystem-safe key for a request URL. We key on the
/// `req=` query value (the search terms) when present so fixtures can be named
/// after a human-readable slug; otherwise we slugify the whole URL.
pub fn fixture_key(url: &str) -> String {
    let terms = req_param(url).unwrap_or_else(|| url.to_string());
    slugify(&terms)
}

fn req_param(url: &str) -> Option<String> {
    let q = url.split('?').nth(1)?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k == "req" {
            let v = it.next().unwrap_or("");
            return Some(url_decode(v));
        }
    }
    None
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
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
    if out.is_empty() {
        out.push_str("query");
    }
    out
}

/// Replays previously recorded responses from a fixtures directory. Lookup is
/// by [`fixture_key`] with a `.html` or `.json` extension. Fully offline.
pub struct ReplayTransport {
    dir: PathBuf,
}

impl ReplayTransport {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ReplayTransport { dir: dir.into() }
    }
}

#[async_trait::async_trait]
impl Transport for ReplayTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let key = fixture_key(url);
        for ext in ["html", "json", "txt"] {
            let path = self.dir.join(format!("{key}.{ext}"));
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("reading fixture {}", path.display()));
            }
        }
        Err(anyhow!(
            "no recorded fixture for url {url} (key {key}) in {}",
            self.dir.display()
        ))
    }
}

/// Wraps a live transport, saving every response into a fixtures directory so
/// it can later be replayed. The recorded extension is inferred from the body
/// (`.json` for JSON payloads, otherwise `.html`).
pub struct RecordingTransport {
    inner: Box<dyn Transport>,
    dir: PathBuf,
}

impl RecordingTransport {
    pub fn new(inner: Box<dyn Transport>, dir: impl Into<PathBuf>) -> Self {
        RecordingTransport {
            inner,
            dir: dir.into(),
        }
    }
}

#[async_trait::async_trait]
impl Transport for RecordingTransport {
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
// Search client
// ---------------------------------------------------------------------------

/// Client over one or more mirrors with failover.
pub struct SearchClient {
    mirrors: Vec<Mirror>,
    transport: Box<dyn Transport>,
    limit: usize,
}

impl SearchClient {
    /// Build a client from a mirror config and a transport.
    pub fn new(config: MirrorConfig, transport: Box<dyn Transport>) -> Self {
        let mut mirrors = config.search_mirrors;
        mirrors.sort_by_key(|m| m.priority);
        SearchClient {
            mirrors,
            transport,
            limit: 25,
        }
    }

    /// Convenience: live transport from a `mirrors.toml` path.
    pub fn from_config_path(path: impl AsRef<Path>) -> Result<Self> {
        let cfg = MirrorConfig::load(path)?;
        Ok(Self::new(cfg, Box::new(LiveTransport::new())))
    }

    /// Convenience: replay transport over a fixtures dir.
    pub fn replay(config: MirrorConfig, fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::new(config, Box::new(ReplayTransport::new(fixtures_dir)))
    }

    /// Convenience: live transport that records responses into `fixtures_dir`.
    pub fn recording(config: MirrorConfig, fixtures_dir: impl Into<PathBuf>) -> Self {
        let live: Box<dyn Transport> = Box::new(LiveTransport::new());
        Self::new(
            config,
            Box::new(RecordingTransport::new(live, fixtures_dir)),
        )
    }

    /// Cap on results per mirror.
    pub fn with_limit(mut self, limit: usize) -> Self {
        if limit > 0 {
            self.limit = limit;
        }
        self
    }

    /// Search configured mirrors for `input`, trying a SHORT list of progressively
    /// looser query strategies (see [`build_query_strategies`]) and stopping at the
    /// first that yields candidates. Real mirrors often miss a book when the full
    /// `"<title> <all authors>"` string is searched verbatim (e.g. a subtitle or a
    /// multi-word author dilutes the term match), but find it on a tighter
    /// `"<title> <surname>"` query — so we widen the net rather than give up.
    ///
    /// Within each strategy, mirrors are tried in priority order (failover on error
    /// / empty). Candidates accumulated across strategies are deduped by md5, so a
    /// later strategy only adds genuinely new hits. We stop as soon as a strategy
    /// produced any candidate, keeping the request count polite (a few at most).
    pub async fn search(&self, input: &BookInput) -> Result<Vec<Candidate>> {
        if self.mirrors.is_empty() {
            return Err(anyhow!("no search mirrors configured"));
        }

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<Candidate> = Vec::new();
        let mut last_err: Option<anyhow::Error> = None;

        for query in build_query_strategies(input) {
            match self.search_query(&query).await {
                Ok(cands) if !cands.is_empty() => {
                    // Does this strategy include a candidate the matcher could
                    // plausibly use (a real title-overlap with the request)? If a
                    // strict query only returned irrelevant rows, keep widening.
                    let usable = cands.iter().any(|c| looks_relevant(input, c));
                    for c in cands {
                        if seen.insert(c.md5.clone()) {
                            out.push(c);
                        }
                    }
                    if usable {
                        // Found candidates the matcher can use — stop early to stay
                        // polite. (This strategy's hits are already collected.)
                        break;
                    }
                    // Otherwise fall through to the next (looser) strategy, keeping
                    // the irrelevant rows accumulated in case nothing better turns up.
                }
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }

        if out.is_empty() {
            if let Some(e) = last_err {
                tracing::debug!(error = %e, "all search strategies exhausted");
            }
        }
        out.truncate(self.limit);
        Ok(out)
    }

    /// Run a single free-text `query` against the configured mirrors in priority
    /// order, returning the first mirror's non-empty candidate list (failover on
    /// error / empty). Returns `Ok(Vec::new())` if every mirror was tried without
    /// yielding candidates.
    async fn search_query(&self, query: &str) -> Result<Vec<Candidate>> {
        let mut last_err: Option<anyhow::Error> = None;
        for mirror in &self.mirrors {
            let url = mirror
                .search_url
                .replace("{query}", &url_encode(query))
                .replace("{limit}", &self.limit.to_string());

            match self.transport.get(&url).await {
                Ok(body) => {
                    let parsed = match mirror.kind {
                        MirrorKind::LibgenLiHtml => parse_libgen_li(&body, &mirror.host),
                        MirrorKind::LibgenRsHtml => parse_libgen_rs(&body, &mirror.host),
                        MirrorKind::LibgenJson => parse_libgen_json(&body, &mirror.host),
                        MirrorKind::AnnasArchiveHtml => parse_annas_archive(&body, &mirror.host),
                    };
                    match parsed {
                        Ok(mut cands) if !cands.is_empty() => {
                            cands.truncate(self.limit);
                            return Ok(cands);
                        }
                        Ok(_) => {
                            last_err = Some(anyhow!("{} returned no candidates", mirror.host));
                        }
                        Err(e) => {
                            last_err = Some(e.context(format!("parsing {}", mirror.host)));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(host = %mirror.host, error = %e, "mirror failed, failing over");
                    last_err = Some(e);
                }
            }
        }
        match last_err {
            Some(e) => {
                tracing::debug!(error = %e, "all mirrors exhausted for query");
                Ok(Vec::new())
            }
            None => Ok(Vec::new()),
        }
    }
}

/// Build the free-text query from a book input: title plus authors.
fn build_query(input: &BookInput) -> String {
    let mut parts = vec![input.title.trim().to_string()];
    for a in &input.authors {
        let a = a.trim();
        if !a.is_empty() {
            parts.push(a.to_string());
        }
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the ordered, deduped list of progressively looser query strategies to
/// try for `input` (strictest first), stopping criteria handled by the caller:
///   1. full title + full author strings (the precise query),
///   2. full title + author SURNAME(s) only (drops a noisy "First Middle"),
///   3. title with any `": subtitle"` / `" - subtitle"` dropped + surname(s),
///   4. title only (last resort — broadest).
///
/// Empties and exact duplicates are removed while preserving order, so a book
/// with no authors or no subtitle simply yields fewer (1–2) strategies.
pub fn build_query_strategies(input: &BookInput) -> Vec<String> {
    let title = input.title.trim();
    let main_title = strip_subtitle(title);
    let surnames: Vec<String> = input
        .authors
        .iter()
        .filter_map(|a| surname(a))
        .collect::<Vec<_>>();

    let join = |parts: &[&str]| {
        parts
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };

    let surnames_joined = surnames.join(" ");
    let strategies: Vec<String> = vec![
        // 1. title + full authors (current behavior).
        build_query(input),
        // 2. title + surnames only.
        join(&[title, &surnames_joined]),
        // 3. subtitle-stripped title + surnames.
        join(&[&main_title, &surnames_joined]),
        // 4. title only.
        title.to_string(),
    ];

    // Dedupe preserving order; drop empties.
    let mut seen = std::collections::HashSet::new();
    strategies
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// Drop a trailing subtitle from a title: anything after the first `": "`, `" - "`,
/// `" — "` (em dash) or `" – "` (en dash) separator. Returns the main title,
/// trimmed. If no separator is present the whole title is returned unchanged.
pub fn strip_subtitle(title: &str) -> String {
    let t = title.trim();
    // Check the colon first (no surrounding spaces required), then the dash forms
    // (which need surrounding spaces so we don't split hyphenated words).
    if let Some(idx) = t.find(": ") {
        return t[..idx].trim().to_string();
    }
    if let Some(idx) = t.find(':') {
        // "Title:subtitle" with no space — still a subtitle separator.
        // Only treat as such if there is non-space text on both sides.
        let (head, tail) = t.split_at(idx);
        if !head.trim().is_empty() && tail.len() > 1 {
            return head.trim().to_string();
        }
    }
    for sep in [" - ", " — ", " – "] {
        if let Some(idx) = t.find(sep) {
            return t[..idx].trim().to_string();
        }
    }
    t.to_string()
}

/// Cheap relevance gate used to decide whether a search strategy returned
/// something the matcher could plausibly use, so a strict query that only yields
/// irrelevant rows doesn't short-circuit the looser strategies. Independent of
/// the full `matching` scorer (kept lightweight + dependency-free here): a row is
/// "relevant" if a meaningful fraction of the request's (subtitle-stripped) title
/// tokens appear in the candidate's title, OR the request title is fully contained
/// in the candidate title (or vice-versa).
fn looks_relevant(input: &BookInput, cand: &Candidate) -> bool {
    let want = norm_lower(&strip_subtitle(&input.title));
    let got = norm_lower(&cand.title);
    if want.is_empty() || got.is_empty() {
        return false;
    }
    if got.contains(&want) || want.contains(&got) {
        return true;
    }
    let want_tokens: std::collections::BTreeSet<&str> =
        want.split_whitespace().filter(|t| t.len() > 2).collect();
    if want_tokens.is_empty() {
        // Very short title (all stop-ish tokens): fall back to substring only.
        return got.contains(&want);
    }
    let got_tokens: std::collections::BTreeSet<&str> = got.split_whitespace().collect();
    let overlap = want_tokens
        .iter()
        .filter(|t| got_tokens.contains(*t))
        .count();
    // At least ~60% of the significant title tokens present.
    (overlap as f32) / (want_tokens.len() as f32) >= 0.6
}

/// Lowercased, alphanumeric-token normalization (mirrors `matching::norm` but kept
/// local so `search` has no dependency on the matcher).
fn norm_lower(s: &str) -> String {
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

/// Extract an author's surname for a tighter query: the last whitespace-separated
/// token of a "First [Middle] Last" name, or the token before a comma for a
/// "Last, First" form. Returns `None` for an empty/whitespace name.
pub fn surname(author: &str) -> Option<String> {
    let a = author.trim();
    if a.is_empty() {
        return None;
    }
    // "Surname, First" → take the part before the comma.
    if let Some((last, _first)) = a.split_once(',') {
        let last = last.trim();
        if !last.is_empty() {
            return Some(last.to_string());
        }
    }
    // "First Middle Last" → the final token.
    a.split_whitespace().last().map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Parse the libgen.li results table (`id="tablelibgen"`).
///
/// Column layout: title | author | publisher | year | language | pages |
/// size | extension | mirrors. The md5 is read from the `ads.php?md5=` link in
/// the mirrors cell.
pub fn parse_libgen_li(html: &str, host: &str) -> Result<Vec<Candidate>> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table#tablelibgen tbody tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let b_sel = Selector::parse("b").unwrap();
    let a_sel = Selector::parse("a").unwrap();

    let mut out = Vec::new();
    for row in doc.select(&table_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.len() < 9 {
            continue;
        }

        // md5 from any ads.php link in the row.
        let md5 = row
            .select(&a_sel)
            .filter_map(|a| a.value().attr("href"))
            .find_map(extract_md5);
        let md5 = match md5 {
            Some(m) => m,
            None => continue,
        };

        // Title: the libgen.li title cell stacks <b>SERIES</b><br><a edition.php>
        // TITLE</a><br><font>ISBNs</font>. The real title is the FIRST edition.php
        // anchor that sits OUTSIDE the bold. The bold holds the series and, for a
        // JOURNAL/ARTICLE row, a short issue-marker edition.php link nested inside
        // it ("vol. 69 iss. 3"); the real title ("The Time Machine: An Invention
        // by H. G. Wells") is the next edition.php anchor after the bold. We
        // can't just take the first edition.php overall (that's the issue marker)
        // nor the longest (a later ISBN line can be longer than a short title) —
        // so exclude anchors nested in the bold, then take the first remaining.
        // Fall back to the bold, then the whole cell.
        let title_cell = &cells[0];
        let bold_anchor_ids: std::collections::HashSet<_> = title_cell
            .select(&b_sel)
            .next()
            .into_iter()
            .flat_map(|b| b.select(&a_sel).map(|a| a.id()))
            .collect();
        let title = title_cell
            .select(&a_sel)
            .filter(|a| {
                a.value()
                    .attr("href")
                    .is_some_and(|h| h.contains("edition.php"))
                    && !bold_anchor_ids.contains(&a.id())
            })
            .map(|a| clean_text(&a.text().collect::<String>()))
            .find(|s| !s.is_empty())
            .or_else(|| {
                title_cell
                    .select(&b_sel)
                    .next()
                    .map(|b| clean_text(&b.text().collect::<String>()))
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| clean_text(&title_cell.text().collect::<String>()));

        let author = clean_text(&cells[1].text().collect::<String>());
        let authors = split_authors(&author);
        let publisher = opt(clean_text(&cells[2].text().collect::<String>()));
        let year = parse_year(&clean_text(&cells[3].text().collect::<String>()));
        let language = opt(clean_text(&cells[4].text().collect::<String>()));
        let pages = parse_pages(&clean_text(&cells[5].text().collect::<String>()));
        let size_bytes = parse_size(&clean_text(&cells[6].text().collect::<String>()));
        let ext_raw = clean_text(&cells[7].text().collect::<String>());
        let extension = opt(ext_raw).map(|e| Format::parse(&e));

        // Cover image, when present: the title cell's preceding/sibling cover
        // cell carries `/comicscovers/<bucket>/<md5>.jpg` (comics) or `/covers/…`
        // (other), as an `<a href>` (full) and/or `<img src>` (`…_small.jpg`
        // thumb). Scan the whole row's anchors + images for the first such path
        // and absolutize it against the mirror host.
        let cover_url = row_cover_url(&row, host);

        out.push(Candidate {
            md5,
            title,
            authors,
            year,
            publisher,
            language,
            pages,
            extension,
            size_bytes,
            source_host: Some(host.to_string()),
            cover_url,
            score: 0.0,
            job: None,
        });
    }
    Ok(out)
}

/// Extract a cover image URL from a libgen result/series `<tr>`: the first
/// `/comicscovers/…` or `/covers/…` image path found among the row's anchors
/// (`<a href>`, full image) or images (`<img src>`, usually a `…_small.jpg`
/// thumb — we strip `_small` to prefer the full image). Returns an absolute URL
/// rooted at the mirror `host`, or `None` when the row has no cover cell.
fn row_cover_url(row: &scraper::ElementRef, host: &str) -> Option<String> {
    let a_sel = Selector::parse("a").unwrap();
    let img_sel = Selector::parse("img").unwrap();
    // Prefer the full-size <a href> path; fall back to the <img src> thumb.
    let from_anchor = row
        .select(&a_sel)
        .filter_map(|a| a.value().attr("href"))
        .find(|h| is_cover_path(h))
        .map(str::to_string);
    let path = from_anchor.or_else(|| {
        row.select(&img_sel)
            .filter_map(|i| i.value().attr("src"))
            .find(|s| is_cover_path(s))
            .map(|s| {
                s.replace("_small.jpg", ".jpg")
                    .replace("_small.png", ".png")
            })
    })?;
    Some(absolutize(&path, host))
}

/// Whether a URL path points at a libgen cover image
/// (`/comicscovers/…` for comics, `/covers/…` otherwise).
fn is_cover_path(href: &str) -> bool {
    let h = href.to_ascii_lowercase();
    (h.contains("/comicscovers/") || h.contains("/covers/"))
        && (h.ends_with(".jpg") || h.ends_with(".jpeg") || h.ends_with(".png"))
}

/// Absolutize a cover path against a mirror host: an already-absolute `http(s)`
/// URL is returned as-is; a root-relative `/comicscovers/…` is prefixed with
/// `https://<host>`.
fn absolutize(path: &str, host: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    let host = host.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix('/') {
        format!("https://{host}/{rest}")
    } else {
        format!("https://{host}/{path}")
    }
}

/// Parse a classic libgen.rs/.is `search.php` table (column order: id, author,
/// title, publisher, year, pages, language, size, extension, mirrors).
pub fn parse_libgen_rs(html: &str, host: &str) -> Result<Vec<Candidate>> {
    let doc = Html::parse_document(html);
    // The results table is the 3rd table on the page; select rows with an
    // md5 link to be robust to layout drift.
    let row_sel = Selector::parse("table.c tr, table[rules] tr, tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let a_sel = Selector::parse("a").unwrap();

    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.len() < 9 {
            continue;
        }
        let md5 = row
            .select(&a_sel)
            .filter_map(|a| a.value().attr("href"))
            .find_map(extract_md5);
        let md5 = match md5 {
            Some(m) => m,
            None => continue,
        };
        // classic layout: [0]=id [1]=author [2]=title [3]=publisher [4]=year
        // [5]=pages [6]=language [7]=size [8]=extension
        let authors = split_authors(&clean_text(&cells[1].text().collect::<String>()));
        let title = clean_text(&cells[2].text().collect::<String>());
        let publisher = opt(clean_text(&cells[3].text().collect::<String>()));
        let year = parse_year(&clean_text(&cells[4].text().collect::<String>()));
        let pages = parse_pages(&clean_text(&cells[5].text().collect::<String>()));
        let language = opt(clean_text(&cells[6].text().collect::<String>()));
        let size_bytes = parse_size(&clean_text(&cells[7].text().collect::<String>()));
        let extension =
            opt(clean_text(&cells[8].text().collect::<String>())).map(|e| Format::parse(&e));
        out.push(Candidate {
            md5,
            title,
            authors,
            year,
            publisher,
            language,
            pages,
            extension,
            size_bytes,
            source_host: Some(host.to_string()),
            cover_url: None,
            score: 0.0,
            job: None,
        });
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct JsonRow {
    #[serde(default)]
    md5: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    year: String,
    #[serde(default)]
    publisher: String,
    #[serde(default)]
    language: String,
    /// Page count. libgen JSON usually serializes this as a string (sometimes a
    /// non-numeric note like "300 p."); `JsonNum` accepts a string or a number.
    #[serde(default)]
    pages: JsonNum,
    #[serde(default)]
    extension: String,
    #[serde(default)]
    filesize: String,
}

/// A JSON value that may arrive as either a string or a number (libgen's JSON is
/// inconsistent across mirrors). Captured as its raw string form for parsing.
#[derive(Debug, Default)]
struct JsonNum(String);

impl<'de> Deserialize<'de> for JsonNum {
    fn deserialize<D>(de: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(de)?;
        let s = match v {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Null => String::new(),
            other => return Err(D::Error::custom(format!("unexpected number value {other}"))),
        };
        Ok(JsonNum(s))
    }
}

/// Parse a libgen `json.php` array of result objects.
pub fn parse_libgen_json(body: &str, host: &str) -> Result<Vec<Candidate>> {
    let rows: Vec<JsonRow> = serde_json::from_str(body).context("decoding libgen json")?;
    let mut out = Vec::new();
    for r in rows {
        if r.md5.is_empty() {
            continue;
        }
        out.push(Candidate {
            md5: r.md5.to_lowercase(),
            title: clean_text(&r.title),
            authors: split_authors(&r.author),
            year: parse_year(&r.year),
            publisher: opt(clean_text(&r.publisher)),
            language: opt(clean_text(&r.language)),
            pages: parse_pages(&r.pages.0),
            extension: opt(clean_text(&r.extension)).map(|e| Format::parse(&e)),
            size_bytes: r.filesize.trim().parse::<u64>().ok(),
            source_host: Some(host.to_string()),
            cover_url: None,
            score: 0.0,
            job: None,
        });
    }
    Ok(out)
}

/// Parse an Anna's Archive `/search` results page.
///
/// Anna's Archive renders each hit as a card (not a table). A card is keyed by a
/// title anchor `a.js-vim-focus` whose `href` is `/md5/<32hex>`; the card body
/// carries the author (an `<a href="/search?q=…">` tagged with the
/// `icon-[mdi--user-edit]` glyph) and a single dot-separated metadata line of the
/// form `English [en] · EPUB · 4.8MB · 2021 · 📕 Book (fiction) · …`. The cover,
/// when present, is an `<img>` inside the sibling cover anchor
/// `a.custom-a.block`.
///
/// We anchor the walk on the title anchors and look for the other fields within
/// each anchor's enclosing card `<div>`, so missing fields (everything except the
/// md5 and title) degrade to `None`. Rows without a valid 32-hex md5 are skipped.
pub fn parse_annas_archive(html: &str, host: &str) -> Result<Vec<Candidate>> {
    let doc = Html::parse_document(html);
    // The title anchor doubles as the md5 source and the per-card anchor.
    let title_sel = Selector::parse("a.js-vim-focus[href*='/md5/']").unwrap();
    let author_sel = Selector::parse("a[href*='/search?q=']").unwrap();
    let user_icon_sel = Selector::parse("span.icon-\\[mdi--user-edit\\]").unwrap();
    let meta_sel = Selector::parse("div.text-gray-800.font-semibold").unwrap();
    let img_sel = Selector::parse("img").unwrap();

    let mut out = Vec::new();
    for title_a in doc.select(&title_sel) {
        let href = match title_a.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let md5 = match extract_md5(href) {
            Some(m) => m,
            None => continue,
        };
        let title = clean_text(&title_a.text().collect::<String>());
        if title.is_empty() {
            continue;
        }

        // The enclosing card: the closest `div.flex` ancestor that wraps both the
        // cover anchor and the metadata. Walk up parents until we find one.
        let card = title_a
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .find(|el| {
                let v = el.value();
                v.name() == "div"
                    && v.attr("class")
                        .is_some_and(|c| c.split_whitespace().any(|cls| cls == "flex"))
            });

        // Author: the `/search?q=…` anchor flagged with the user-edit icon (other
        // such anchors are publisher / language facets). Search the card; fall
        // back to None when the card or the icon is absent.
        let authors = card
            .as_ref()
            .and_then(|c| {
                c.select(&author_sel)
                    .find(|a| a.select(&user_icon_sel).next().is_some())
                    .map(|a| clean_text(&a.text().collect::<String>()))
            })
            .map(|s| split_authors(&s))
            .unwrap_or_default();

        // Metadata line: `Lang [code] · EXT · SIZE · YEAR · type · …`. Split on the
        // middle-dot separator and classify each token.
        let mut language = None;
        let mut extension = None;
        let mut size_bytes = None;
        let mut year = None;
        if let Some(meta_text) = card.as_ref().and_then(|c| {
            c.select(&meta_sel)
                .next()
                .map(|m| clean_text(&m.text().collect::<String>()))
        }) {
            for raw in meta_text.split('·') {
                let tok = raw.trim();
                if tok.is_empty() {
                    continue;
                }
                // Language: "English [en]" — take the part before the bracket.
                if language.is_none() && tok.contains('[') && tok.ends_with(']') {
                    let lang = tok.split('[').next().unwrap_or("").trim();
                    language = opt(lang.to_string());
                    continue;
                }
                // Year: a bare 4-digit run.
                if year.is_none() {
                    if let Some(y) = parse_year(tok) {
                        if tok.chars().all(|c| c.is_ascii_digit()) {
                            year = Some(y);
                            continue;
                        }
                    }
                }
                // Size: "4.8MB" / "278 kB".
                if size_bytes.is_none() {
                    if let Some(sz) = parse_size(tok) {
                        size_bytes = Some(sz);
                        continue;
                    }
                }
                // Extension: a short alphanumeric token that names a known format.
                if extension.is_none()
                    && tok.len() <= 5
                    && tok.chars().all(|c| c.is_ascii_alphanumeric())
                {
                    let fmt = Format::parse(tok);
                    if !matches!(fmt, Format::Other(_)) {
                        extension = Some(fmt);
                        continue;
                    }
                }
            }
        }

        // Cover image, when present: the card's cover anchor wraps an `<img src>`.
        let cover_url = card
            .as_ref()
            .and_then(|c| {
                c.select(&img_sel)
                    .filter_map(|i| i.value().attr("src"))
                    .find(|s| !s.trim().is_empty())
            })
            .map(|s| absolutize(s, host));

        out.push(Candidate {
            md5,
            title,
            authors,
            year,
            publisher: None,
            language,
            pages: None,
            extension,
            size_bytes,
            source_host: Some(host.to_string()),
            cover_url,
            score: 0.0,
            job: None,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn clean_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn opt(s: String) -> Option<String> {
    let s = s.trim().to_string();
    if s.is_empty() || s == "0" {
        None
    } else {
        Some(s)
    }
}

fn split_authors(s: &str) -> Vec<String> {
    // Split on DEFINITIVE author separators (`;`, `&`, " and "), then handle commas
    // per-segment: libgen writes a single author surname-first as "Last, First"
    // (e.g. "Twain, Mark"), so a lone comma between two short name-parts is NOT
    // an author boundary — splitting it wrecks author matching (the epub of
    // "The Adventures of Tom Sawyer" became authors ["Twain","Mark"] → 0.5 match).
    s.split([';', '&'])
        .flat_map(|seg| seg.split(" and "))
        .flat_map(split_comma_segment)
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect()
}

/// Split one author segment on commas, EXCEPT the "Last, First" case (exactly two
/// short parts → one author kept verbatim).
fn split_comma_segment(seg: &str) -> Vec<String> {
    let parts: Vec<&str> = seg
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() == 2 && parts.iter().all(|p| p.split_whitespace().count() <= 2) {
        // "Twain, Mark" — surname-first single author.
        vec![seg.trim().to_string()]
    } else {
        parts.into_iter().map(str::to_string).collect()
    }
}

fn parse_year(s: &str) -> Option<u16> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).take(4).collect();
    let y: u16 = digits.parse().ok()?;
    if (1000..=2999).contains(&y) {
        Some(y)
    } else {
        None
    }
}

/// Parse a page count from a cell/field. libgen exposes pages as a bare number
/// ("312"), sometimes with a note ("312[300]" or "300 p.") — take the leading run
/// of digits. Returns `None` when there is no number or it is zero.
fn parse_pages(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    match digits.parse::<u32>() {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(n),
    }
}

/// Parse a human size like "2 MB", "278 kB", "1.5 GB" into bytes.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut num = String::new();
    let mut unit = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if ch.is_ascii_alphabetic() {
            unit.push(ch.to_ascii_lowercase());
        }
    }
    let value: f64 = num.parse().ok()?;
    let mult = match unit.as_str() {
        "b" | "" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Extract a 32-hex md5 from a `?md5=...`, `/main/<md5>` or similar href.
fn extract_md5(href: &str) -> Option<String> {
    let lower = href.to_ascii_lowercase();
    if let Some(idx) = lower.find("md5=") {
        let rest = &lower[idx + 4..];
        let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 32 {
            return Some(hex);
        }
    }
    // fall back: any 32-hex run in the href
    let bytes: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i + 32 <= bytes.len() {
        if bytes[i..i + 32].iter().all(|c| c.is_ascii_hexdigit()) {
            // ensure not part of a longer hex run
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_hexdigit();
            let after_ok = i + 32 == bytes.len() || !bytes[i + 32].is_ascii_hexdigit();
            if before_ok && after_ok {
                return Some(bytes[i..i + 32].iter().collect());
            }
        }
        i += 1;
    }
    None
}

/// Minimal percent-encoding for query terms (space -> +, reserved -> %XX).
pub fn url_encode(s: &str) -> String {
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

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_deterministic_and_safe() {
        assert_eq!(
            slugify("The Adventures of Tom Sawyer Mark Twain"),
            "the-adventures-of-tom-sawyer-mark-twain"
        );
        assert_eq!(
            slugify("Treasure Island — Robert Louis Stevenson!"),
            "treasure-island-robert-louis-stevenson"
        );
        assert_eq!(slugify("   "), "query");
    }

    #[test]
    fn fixture_key_uses_req_param() {
        let url = "https://libgen.li/index.php?req=Treasure+Island+Robert+Louis+Stevenson&res=25";
        assert_eq!(fixture_key(url), "treasure-island-robert-louis-stevenson");
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("2 MB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("278 kB"), Some(278 * 1024));
        assert_eq!(
            parse_size("1.5 GB"),
            Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn page_parsing() {
        assert_eq!(parse_pages("312"), Some(312));
        assert_eq!(parse_pages("128 / 128"), Some(128));
        assert_eq!(parse_pages("300 p."), Some(300));
        // Leading zero (libgen's "0 / 272" sentinel) → unknown.
        assert_eq!(parse_pages("0 / 272"), None);
        assert_eq!(parse_pages("0"), None);
        assert_eq!(parse_pages(""), None);
        assert_eq!(parse_pages("n/a"), None);
    }

    #[test]
    fn libgen_li_parser_extracts_pages() {
        // The Treasure Island results table carries a `77`-page row and a
        // `128 / 128`-page row; confirm pages are wired through from the HTML.
        let html =
            include_str!("../../../fixtures/search/treasure-island-robert-louis-stevenson.html");
        let cands = parse_libgen_li(html, "libgen.li").unwrap();
        let p77 = cands
            .iter()
            .find(|c| c.md5 == "11aa22bb33cc44dd55ee66ff00112203")
            .expect("the 77-page row is parsed");
        assert_eq!(p77.pages, Some(77));
        let p128 = cands
            .iter()
            .find(|c| c.md5 == "11aa22bb33cc44dd55ee66ff00112202")
            .expect("the 128-page row is parsed");
        assert_eq!(p128.pages, Some(128));
        // At least one row exposes a page count.
        assert!(cands.iter().any(|c| c.pages.is_some()));
    }

    #[test]
    fn libgen_li_parser_extracts_journal_article_title_not_issue_marker() {
        // Regression: a journal-article row's title cell stacks the journal name +
        // a short issue-marker edition link ("vol. 69 iss. 3") inside <b>, then the
        // REAL article title ("The Time Machine: An Invention by H. G. Wells")
        // as a separate edition.php anchor. The parser must extract the article
        // title, not the issue marker. (md5 2c3befd4… — the journal row.)
        let html =
            include_str!("../../../fixtures/search/the-time-machine-an-invention-h-g-wells.html");
        let cands = parse_libgen_li(html, "libgen.li").unwrap();
        let row = cands
            .iter()
            .find(|c| c.md5 == "2c3befd4b6991715ba78cc748879c2d8")
            .expect("the Boletim/Time Machine article row is parsed");
        assert_eq!(row.title, "The Time Machine: An Invention by H. G. Wells");
        // No row may be titled with a bare issue marker.
        assert!(
            !cands
                .iter()
                .any(|c| c.title.to_lowercase().starts_with("vol.")),
            "issue marker leaked as a title: {:?}",
            cands.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn annas_archive_parser_extracts_candidates() {
        // Synthesized Anna's Archive `/search?q=the+time+machine` results page.
        // Cards are keyed by `a.js-vim-focus` → `/md5/<32hex>`; each carries an
        // author anchor and a `Lang · EXT · SIZE · YEAR · …` metadata line.
        let html = include_str!("../../../fixtures/search/annas-archive-the-time-machine.html");
        let cands = parse_annas_archive(html, "annas-archive.gl").unwrap();
        // The page lists ~50 visible cards; expect a healthy haul.
        assert!(
            cands.len() >= 10,
            "expected several candidates, got {}",
            cands.len()
        );
        // Every candidate has a valid lowercase 32-hex md5 and a non-empty title.
        for c in &cands {
            assert_eq!(c.md5.len(), 32, "md5 not 32 chars: {:?}", c.md5);
            assert!(
                c.md5
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
                "md5 not lowercase hex: {:?}",
                c.md5
            );
            assert!(!c.title.trim().is_empty(), "empty title");
            assert_eq!(c.source_host.as_deref(), Some("annas-archive.gl"));
        }
        // At least one row exposes a known extension, a size, a language and a year.
        assert!(
            cands
                .iter()
                .any(|c| matches!(&c.extension, Some(f) if !matches!(f, Format::Other(_)))),
            "no candidate had a known extension"
        );
        assert!(cands.iter().any(|c| c.size_bytes.is_some()), "no sizes");
        assert!(cands.iter().any(|c| c.language.is_some()), "no languages");
        assert!(cands.iter().any(|c| c.year.is_some()), "no years");
        // The exact AZW3 row used as the parser anchor.
        let azw3 = cands
            .iter()
            .find(|c| c.md5 == "c24fb619df6a96ba72271622a936eaf8")
            .expect("the AZW3 The Time Machine row is parsed");
        assert_eq!(azw3.title, "The Time Machine");
        assert_eq!(azw3.authors, vec!["Wells, H. G.".to_string()]);
        assert_eq!(azw3.extension, Some(Format::Azw3));
        assert_eq!(azw3.language.as_deref(), Some("English"));
        assert_eq!(azw3.year, Some(1895));
        assert!(azw3.size_bytes.is_some());
    }

    #[test]
    fn libgen_json_parser_extracts_pages() {
        // libgen JSON serializes `pages` as a string or a number; both decode.
        let body = r#"[
            {"md5":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","title":"A","author":"X","pages":"312","extension":"epub","filesize":"100"},
            {"md5":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","title":"B","author":"Y","pages":204,"extension":"pdf","filesize":"200"},
            {"md5":"cccccccccccccccccccccccccccccccc","title":"C","author":"Z","extension":"epub","filesize":"50"}
        ]"#;
        let cands = parse_libgen_json(body, "libgen.is").unwrap();
        assert_eq!(cands[0].pages, Some(312));
        assert_eq!(cands[1].pages, Some(204));
        assert_eq!(cands[2].pages, None);
    }

    #[test]
    fn md5_extraction() {
        assert_eq!(
            extract_md5("/ads.php?md5=1df204c78842ffe549166ffcb984babc").as_deref(),
            Some("1df204c78842ffe549166ffcb984babc")
        );
        assert_eq!(
            extract_md5("https://library.lol/main/70de5275eb7d4bb6bfaa52b7589331b6").as_deref(),
            Some("70de5275eb7d4bb6bfaa52b7589331b6")
        );
        assert_eq!(extract_md5("edition.php?id=136507198"), None);
    }

    #[test]
    fn libgen_li_title_is_edition_link_not_series_bold() {
        // The title cell stacks <b>SERIES</b><br><a edition.php>TITLE</a><br>
        // <font>ISBNs</font>. The parser must use the edition-link TITLE, not the
        // bold SERIES (the "Jasmine Toguchi 1" / "13;Treehouse #1" bug).
        let html = r#"<table id="tablelibgen"><tbody>
        <tr>
          <td><b>Jasmine Toguchi 1</b><br>
              <a href="edition.php?id=123">Jasmine Toguchi, Mochi Queen</a><br>
              <a href="z"><i><font color="green">9781000000000</font></i></a></td>
          <td>Florence, Debbi Michiko</td><td>Farrar</td><td>2017</td>
          <td>English</td><td>128</td><td>18 MB</td><td>epub</td>
          <td><a href="/ads.php?md5=1df204c78842ffe549166ffcb984babc">mirror</a></td>
        </tr>
        <tr>
          <td><b>Standalone Title No Edition Link</b></td>
          <td>Doe, Jane</td><td>Pub</td><td>2020</td><td>English</td><td>50</td>
          <td>1 MB</td><td>pdf</td>
          <td><a href="/ads.php?md5=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">mirror</a></td>
        </tr>
        </tbody></table>"#;
        let cands = parse_libgen_li(html, "libgen.li").unwrap();
        assert_eq!(cands.len(), 2);
        // Edition-link title wins over the bold series.
        assert_eq!(cands[0].title, "Jasmine Toguchi, Mochi Queen");
        // No edition link → fall back to the bold text.
        assert_eq!(cands[1].title, "Standalone Title No Edition Link");
    }

    #[test]
    fn libgen_li_parses_cover_url_from_cover_cell() {
        // A libgen.li result row whose title cell is preceded by a cover cell
        // carrying `/comicscovers/<bucket>/<md5>.jpg` (full, as <a href>) and a
        // `…_small.jpg` thumb (<img src>). The parser must surface an ABSOLUTE
        // full-size cover URL.
        let html = r#"<table id="tablelibgen"><tbody>
        <tr>
          <td><a href="/comicscovers/1121000/abc123.jpg"><img src="/comicscovers/1121000/abc123_small.jpg"></a></td>
          <td><a href="edition.php?id=1">The Nursery Alice</a></td>
          <td>Lewis Carroll</td><td>Macmillan</td><td>1890</td>
          <td>English</td><td>176</td><td>40 MB</td><td>cbz</td>
          <td><a href="/ads.php?md5=1df204c78842ffe549166ffcb984babc">mirror</a></td>
        </tr>
        <tr>
          <td></td>
          <td><a href="edition.php?id=2">No Cover Title</a></td>
          <td>Someone</td><td>Pub</td><td>2020</td><td>English</td><td>50</td>
          <td>1 MB</td><td>pdf</td>
          <td><a href="/ads.php?md5=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">mirror</a></td>
        </tr>
        </tbody></table>"#;
        let cands = parse_libgen_li(html, "libgen.li").unwrap();
        assert_eq!(cands.len(), 2);
        // Full-size, absolute, _small stripped.
        assert_eq!(
            cands[0].cover_url.as_deref(),
            Some("https://libgen.li/comicscovers/1121000/abc123.jpg")
        );
        // A row with no cover cell yields None (no key written).
        assert_eq!(cands[1].cover_url, None);
    }

    #[test]
    fn cover_path_detection_and_absolutize() {
        assert!(is_cover_path("/comicscovers/1121000/abc.jpg"));
        assert!(is_cover_path("/covers/1000/def.png"));
        assert!(!is_cover_path("edition.php?id=1"));
        assert!(!is_cover_path("/comicscovers/1121000/abc_small.gif"));
        assert_eq!(
            absolutize("/comicscovers/x/y.jpg", "libgen.li"),
            "https://libgen.li/comicscovers/x/y.jpg"
        );
        assert_eq!(
            absolutize("https://cdn.example/y.jpg", "libgen.li"),
            "https://cdn.example/y.jpg"
        );
    }

    #[test]
    fn build_query_combines_title_and_authors() {
        let input = BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        };
        assert_eq!(
            build_query(&input),
            "Treasure Island Robert Louis Stevenson"
        );
    }

    #[test]
    fn strips_colon_subtitle() {
        assert_eq!(
            strip_subtitle("The Time Machine: An Invention"),
            "The Time Machine"
        );
        // No-space colon still splits.
        assert_eq!(strip_subtitle("Title:Subtitle"), "Title");
    }

    #[test]
    fn strips_dash_subtitle_but_not_hyphenated_words() {
        assert_eq!(
            strip_subtitle("Treasure Island - A Novel"),
            "Treasure Island"
        );
        assert_eq!(
            strip_subtitle("Treasure Island — A Novel"),
            "Treasure Island"
        ); // em dash
           // Hyphenated single word is untouched (no surrounding spaces).
        assert_eq!(strip_subtitle("Spider-Man"), "Spider-Man");
    }

    #[test]
    fn strip_subtitle_noop_without_separator() {
        assert_eq!(strip_subtitle("Treasure Island"), "Treasure Island");
    }

    #[test]
    fn surname_from_first_last() {
        assert_eq!(surname("Herbert Wells").as_deref(), Some("Wells"));
        assert_eq!(surname("Robert Stevenson").as_deref(), Some("Stevenson"));
        assert_eq!(surname("Madonna").as_deref(), Some("Madonna"));
    }

    #[test]
    fn surname_from_last_comma_first() {
        assert_eq!(surname("Wells, Herbert").as_deref(), Some("Wells"));
        assert!(surname("  ").is_none());
    }

    #[test]
    fn query_strategies_for_time_machine_widen_the_net() {
        let input = BookInput {
            title: "The Time Machine: An Invention".into(),
            authors: vec!["Herbert Wells".into()],
            ..Default::default()
        };
        let s = build_query_strategies(&input);
        // 1. full title + full author
        assert_eq!(s[0], "The Time Machine: An Invention Herbert Wells");
        // 2. full title + surname
        assert_eq!(s[1], "The Time Machine: An Invention Wells");
        // 3. subtitle-stripped + surname → the looser query that actually hits.
        assert_eq!(s[2], "The Time Machine Wells");
        // 4. title only
        assert_eq!(s[3], "The Time Machine: An Invention");
        // All distinct, none empty.
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn query_strategies_dedupe_and_handle_no_author() {
        // No authors, no subtitle → strategies collapse to a single query.
        let input = BookInput {
            title: "Treasure Island".into(),
            authors: vec![],
            ..Default::default()
        };
        let s = build_query_strategies(&input);
        assert_eq!(s, vec!["Treasure Island".to_string()]);
    }

    #[test]
    fn mirror_config_sorts_by_priority() {
        let toml = r#"
            [[search_mirror]]
            host = "b"
            search_url = "u"
            kind = "libgen_json"
            priority = 5
            [[search_mirror]]
            host = "a"
            search_url = "u"
            kind = "libgen_li_html"
            priority = 1
        "#;
        let cfg = MirrorConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.search_mirrors[0].host, "a");
    }
}

#[cfg(test)]
mod author_split_tests {
    use super::split_authors;

    #[test]
    fn last_first_is_one_author() {
        // "Twain, Mark" is ONE author (surname-first), not two.
        assert_eq!(split_authors("Twain, Mark"), vec!["Twain, Mark"]);
        assert_eq!(split_authors("Wells, H. G."), vec!["Wells, H. G."]);
    }

    #[test]
    fn explicit_separators_split() {
        assert_eq!(
            split_authors("Smith, John; Doe, Jane"),
            vec!["Smith, John", "Doe, Jane"]
        );
        assert_eq!(
            split_authors("Mark Twain & Jane Doe"),
            vec!["Mark Twain", "Jane Doe"]
        );
        assert_eq!(split_authors("Alice and Bob"), vec!["Alice", "Bob"]);
    }

    #[test]
    fn single_name_unchanged() {
        assert_eq!(split_authors("Mark Twain"), vec!["Mark Twain"]);
        assert_eq!(split_authors(""), Vec::<String>::new());
    }
}
