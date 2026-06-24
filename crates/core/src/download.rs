//! Resolve a candidate's md5 to a concrete download URL, then fetch it with
//! HTTP Range (resumable) and verify md5.
//!
//! Resolvers are pluggable per download mirror. Tests run against a local mock
//! HTTP server for full headless coverage (no live mirrors).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use md5::{Digest, Md5};
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// A resolved, directly-downloadable target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadTarget {
    pub url: String,
    pub host: String,
    pub expected_md5: Option<String>,
    pub total_bytes: Option<u64>,
}

/// Classify failures so the queue can retry transient errors but fail fast on
/// permanent ones (404, md5 mismatch, …).
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// Transient: timeouts, resets, 5xx, 429 — worth retrying / failing over.
    #[error("transient: {0}")]
    Transient(String),
    /// Permanent: 404, gone, md5 mismatch — do not retry the same target.
    #[error("permanent: {0}")]
    Permanent(String),
    /// The caller cancelled (or paused) the download via its cancellation token.
    /// Not retried or failed-over; the queue surfaces it as a deliberate stop and
    /// leaves the `.part` on disk so a paused job can resume from `resume_offset`.
    #[error("cancelled after {bytes_written} byte(s)")]
    Cancelled {
        /// Bytes written to the `.part` in this call before cancelling.
        bytes_written: u64,
    },
}

impl DownloadError {
    pub fn is_transient(&self) -> bool {
        matches!(self, DownloadError::Transient(_))
    }

    /// True if this was a deliberate cancel/pause rather than an error.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, DownloadError::Cancelled { .. })
    }
}

/// Map a reqwest error to transient/permanent.
fn classify_reqwest(e: &reqwest::Error) -> DownloadError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        DownloadError::Transient(e.to_string())
    } else if let Some(status) = e.status() {
        classify_status(status, e.to_string())
    } else {
        // Body/stream errors mid-transfer are typically transient (reset).
        DownloadError::Transient(e.to_string())
    }
}

/// Map an HTTP status to transient/permanent.
fn classify_status(status: StatusCode, msg: String) -> DownloadError {
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 408
    {
        DownloadError::Transient(format!("HTTP {status}: {msg}"))
    } else {
        DownloadError::Permanent(format!("HTTP {status}: {msg}"))
    }
}

/// Pluggable per-mirror resolver: md5 → concrete [`DownloadTarget`].
///
/// Each download mirror lays out its link differently, so resolvers are a
/// trait. URL bases are configurable so a mock server can be injected in tests.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    /// Stable identifier (e.g. "libgen.li") — used for logging/diagnostics.
    fn name(&self) -> &str;

    /// Resolve an md5 to a directly-downloadable target.
    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError>;
}

/// A resolver that builds a direct URL from a configurable base, e.g.
/// `https://host/get/{md5}`. This models the common "direct get" mirror and is
/// trivially pointed at a mock server in tests.
#[derive(Debug, Clone)]
pub struct DirectUrlResolver {
    name: String,
    /// Base with `{md5}` placeholder, e.g. `http://127.0.0.1:9000/get/{md5}`.
    template: String,
    client: Client,
    /// If true, issue a HEAD to discover total_bytes / detect 404 eagerly.
    probe: bool,
}

impl DirectUrlResolver {
    pub fn new(name: impl Into<String>, template: impl Into<String>, client: Client) -> Self {
        DirectUrlResolver {
            name: name.into(),
            template: template.into(),
            client,
            probe: false,
        }
    }

    /// Enable a HEAD probe during resolve (fills total_bytes, fails fast on 404).
    pub fn with_probe(mut self, probe: bool) -> Self {
        self.probe = probe;
        self
    }

    fn build_url(&self, md5: &str) -> String {
        self.template.replace("{md5}", md5)
    }
}

/// Extract a host string ("authority") from a URL without pulling in a URL
/// parser dependency. Falls back to the whole string on odd input.
pub fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo if present.
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    authority.to_string()
}

#[async_trait::async_trait]
impl Resolver for DirectUrlResolver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        let url = self.build_url(md5);
        let host = host_of(&url);
        let mut total_bytes = None;

        if self.probe {
            let resp = self
                .client
                .head(&url)
                .send()
                .await
                .map_err(|e| classify_reqwest(&e))?;
            if !resp.status().is_success() {
                return Err(classify_status(resp.status(), "HEAD probe failed".into()));
            }
            total_bytes = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
        }

        Ok(DownloadTarget {
            url,
            host,
            expected_md5: Some(md5.to_string()),
            total_bytes,
        })
    }
}

/// An ordered set of resolvers tried in turn (priority/failover at resolve
/// time). The queue uses this both for initial resolution and to fail over to
/// an alternate mirror after repeated download failures.
#[derive(Clone)]
pub struct ResolverChain {
    resolvers: Vec<Arc<dyn Resolver>>,
}

impl ResolverChain {
    pub fn new(resolvers: Vec<Arc<dyn Resolver>>) -> Self {
        ResolverChain { resolvers }
    }

    pub fn is_empty(&self) -> bool {
        self.resolvers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.resolvers.len()
    }

    /// Resolve using resolver at `index`, returning its target.
    pub async fn resolve_with(
        &self,
        index: usize,
        md5: &str,
    ) -> Result<DownloadTarget, DownloadError> {
        let r = self
            .resolvers
            .get(index)
            .ok_or_else(|| DownloadError::Permanent(format!("no resolver at index {index}")))?;
        r.resolve(md5).await
    }

    /// Try resolvers starting at `start`, returning the first success and the
    /// index that produced it (so the caller can fail over from there).
    pub async fn resolve_from(
        &self,
        start: usize,
        md5: &str,
    ) -> Result<(usize, DownloadTarget), DownloadError> {
        let mut last_err = DownloadError::Permanent("no resolvers configured".to_string());
        for idx in start..self.resolvers.len() {
            match self.resolvers[idx].resolve(md5).await {
                Ok(t) => return Ok((idx, t)),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
}

/// Resolver for the **libgen.li family** (libgen.li, libgen.vg, libgen.la — all
/// share this exact flow). The download is a two-step dance: fetch the
/// `ads.php?md5=...` page, scrape the short-lived `get.php?md5=...&key=...`
/// link it contains, and return that as an absolute URL. `get.php` then
/// 307-redirects to a CDN (e.g. `cdn3.booksdl.lc`) that serves the file with
/// `Accept-Ranges: bytes` (so our resumable downloader works).
///
/// NOTE: the family fronts the same `booksdl.lc` CDN, so the extra hosts buy
/// front-door failover (if one domain is blocked/down) more than independent
/// bandwidth. Genuinely independent lanes (IPFS, other mirrors) get their own
/// resolvers.
///
/// The `key` is per-request and short-lived, so resolution must happen close
/// to the actual download (the queue re-resolves on retry/failover, which keeps
/// the key fresh). No cookies are required — only a fresh key.
#[derive(Debug, Clone)]
pub struct LibgenLiResolver {
    name: String,
    /// Site base without a trailing slash, e.g. `https://libgen.li`.
    base: String,
    client: Client,
}

impl LibgenLiResolver {
    pub fn new(base: impl Into<String>, client: Client) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        let name = host_of(&base);
        LibgenLiResolver { name, base, client }
    }
}

#[async_trait::async_trait]
impl Resolver for LibgenLiResolver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        let ads_url = format!("{}/ads.php?md5={}", self.base, md5);
        let resp = self
            .client
            .get(&ads_url)
            .send()
            .await
            .map_err(|e| classify_reqwest(&e))?;
        if !resp.status().is_success() {
            return Err(classify_status(resp.status(), "ads.php failed".into()));
        }
        let body = resp.text().await.map_err(|e| classify_reqwest(&e))?;
        let rel = extract_get_link(&body).ok_or_else(|| {
            DownloadError::Permanent(format!(
                "no get.php?md5=...&key=... link found on ads.php for {md5} \
                 (page layout may have changed)"
            ))
        })?;
        let url = format!("{}/{}", self.base, rel.trim_start_matches('/'));
        Ok(DownloadTarget {
            url,
            host: host_of(&self.base),
            expected_md5: Some(md5.to_string()),
            total_bytes: None,
        })
    }
}

/// Extract the relative `get.php?md5=...&key=...` link from a libgen.li
/// `ads.php` page body. Returns the link text (without scheme/host), or `None`
/// if absent. Deliberately dependency-free (no HTML parser): the link is
/// unambiguous and this stays robust to surrounding markup changes.
fn extract_get_link(html: &str) -> Option<String> {
    let needle = "get.php?md5=";
    let start = html.find(needle)?;
    let tail = &html[start..];
    // The link ends at the first quote, angle bracket, or whitespace.
    let end = tail
        .find(['"', '\'', '<', '>', ' ', '\t', '\n', '\r', '\\'])
        .unwrap_or(tail.len());
    let link = &tail[..end];
    // A valid link carries the per-request key.
    if link.contains("key=") {
        Some(link.to_string())
    } else {
        None
    }
}

/// Extract a cover-image URL from a libgen `ads.php` (or file-detail) page body.
/// The landing page usually carries a `<img src="/covers/<bucket>/<md5>.jpg">`
/// (comics use `/comicscovers/…`). Returns an ABSOLUTE URL (joined onto `base`)
/// for the first such `src`, or `None` when the page has no cover image.
/// Dependency-free string scan, robust to surrounding markup (mirrors
/// [`extract_get_link`]).
pub fn extract_cover_url(html: &str, base: &str) -> Option<String> {
    // Find an img src pointing at a covers path. Accept both `/covers/` and
    // `/comicscovers/`, with or without a leading slash.
    for needle in ["covers/", "comicscovers/"] {
        let mut search_from = 0;
        while let Some(rel) = html[search_from..].find(needle) {
            let at = search_from + rel;
            // Walk back to the start of the URL token (after the opening quote).
            let start = html[..at]
                .rfind(['"', '\'', '('])
                .map(|q| q + 1)
                .unwrap_or(at);
            let tail = &html[start..];
            let end = tail
                .find(['"', '\'', '<', '>', ' ', '\t', '\n', '\r', ')'])
                .unwrap_or(tail.len());
            let link = &tail[..end];
            // Must look like an image file under a covers directory.
            let looks_image = link.ends_with(".jpg")
                || link.ends_with(".jpeg")
                || link.ends_with(".png")
                || link.ends_with(".webp");
            if link.contains(needle) && looks_image {
                if link.starts_with("http://") || link.starts_with("https://") {
                    return Some(link.to_string());
                }
                return Some(format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    link.trim_start_matches('/')
                ));
            }
            search_from = at + needle.len();
        }
    }
    None
}

/// Resolver for the **libgen.pw / randombook.org** family — a Nuxt SPA backed by
/// a JSON API. Two steps: `GET {api_base}/api/search/by-id?id={md5}` yields a
/// numeric `id`, then the file streams from the **independent `libgen.download`
/// CDN** at `/api/download?id={id}` (no token needed). Because the bytes come
/// from a different CDN than the libgen.li family, this is genuinely independent
/// bandwidth — valuable for parallelism.
///
/// NOTE: `libgen.download` does NOT support HTTP Range, so these downloads are
/// not resumable; a failed transfer restarts from 0 (the downloader handles a
/// server that ignores `Range` by restarting cleanly).
#[derive(Debug, Clone)]
pub struct LibgenPwResolver {
    name: String,
    /// Discovery API base, e.g. `https://libgen.pw` or `https://randombook.org`.
    api_base: String,
    client: Client,
}

impl LibgenPwResolver {
    pub fn new(api_base: impl Into<String>, client: Client) -> Self {
        let api_base = api_base.into().trim_end_matches('/').to_string();
        let name = host_of(&api_base);
        LibgenPwResolver {
            name,
            api_base,
            client,
        }
    }
}

#[async_trait::async_trait]
impl Resolver for LibgenPwResolver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        let api = format!("{}/api/search/by-id?id={}", self.api_base, md5);
        let resp = self
            .client
            .get(&api)
            .send()
            .await
            .map_err(|e| classify_reqwest(&e))?;
        if !resp.status().is_success() {
            return Err(classify_status(resp.status(), "by-id lookup failed".into()));
        }
        let body = resp.text().await.map_err(|e| classify_reqwest(&e))?;
        // The JSON can contain raw control chars in other fields, so scan for the
        // numeric `id` rather than parsing the whole document.
        let id = first_json_string_field(&body, "id")
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| {
                DownloadError::Permanent(format!("no numeric id for md5 {md5} on {}", self.name))
            })?;
        Ok(DownloadTarget {
            // The actual bytes come from the independent libgen.download CDN; route
            // the per-host queue by that real backend host.
            url: format!("https://libgen.download/api/download?id={id}"),
            host: "libgen.download".to_string(),
            expected_md5: Some(md5.to_string()),
            total_bytes: None,
        })
    }
}

/// Extract the first `"field": "value"` string value from a JSON body via a
/// lightweight scan (robust to invalid control chars elsewhere in the document).
fn first_json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut from = 0;
    while let Some(rel) = json[from..].find(&needle) {
        let after = &json[from + rel + needle.len()..];
        let after = after.trim_start();
        if let Some(rest) = after.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix('"') {
                if let Some(end) = r.find('"') {
                    return Some(r[..end].to_string());
                }
            }
        }
        from = from + rel + needle.len();
    }
    None
}

/// Resolver for the **public IPFS network** — a genuinely independent download
/// lane. The libgen.li family all front one CDN (booksdl.lc) and libgen.pw /
/// randombook.org share another (libgen.download); IPFS bytes are served by the
/// distributed IPFS network via public gateways, so it is the most independent
/// mirror for parallelism.
///
/// libgen.li exposes an IPFS CID on the FILE-DETAIL page (not on `ads.php`), so
/// `resolve(md5)` is a two-step lookup against `lookup_base` followed by a
/// direct gateway URL:
///   1. `index.php?req={md5}&res=25` → scrape the results row's
///      `file.php?id={numericId}` link for this md5.
///   2. `file.php?id={numericId}` → scrape an `/ipfs/{CID}` link (the CID is a
///      `bafy…` content id, repeated across cloudflare-ipfs / gateway.ipfs.io /
///      pinata / localhost gateway links).
///   3. Return a `{gateway}/ipfs/{CID}` target served by `gateway_base`.
///
/// Stateless (no caching). The CID is content-addressed and stable, so the same
/// CID is fetchable from any gateway — registering several gateways under one
/// `--site ipfs` gives genuine resolve-time failover (the `ResolverChain`
/// already retries the next resolver). Gateways are rate-limited/flaky
/// (403/429/504 are common gateway issues, not CID issues), and 429/5xx are
/// classified Transient so the queue retries / fails over to another gateway.
///
/// NOTE: gateways may ignore HTTP Range; the downloader already restarts at 0
/// when a server replies 200 to a Range request, so no resumability change is
/// needed.
#[derive(Debug, Clone)]
pub struct IpfsResolver {
    name: String,
    /// libgen.li-family base for the md5→CID lookup, e.g. `https://libgen.li`.
    lookup_base: String,
    /// IPFS gateway base, e.g. `https://ipfs.io` or `https://dweb.link`.
    gateway_base: String,
    client: Client,
}

impl IpfsResolver {
    pub fn new(
        lookup_base: impl Into<String>,
        gateway_base: impl Into<String>,
        client: Client,
    ) -> Self {
        let lookup_base = lookup_base.into().trim_end_matches('/').to_string();
        let gateway_base = gateway_base.into().trim_end_matches('/').to_string();
        // Name by gateway host so the per-host download queue routes by gateway.
        let name = host_of(&gateway_base);
        IpfsResolver {
            name,
            lookup_base,
            gateway_base,
            client,
        }
    }

    /// A browser-ish User-Agent: libgen.li serves the search results table only
    /// to non-trivial agents.
    const UA: &'static str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
}

#[async_trait::async_trait]
impl Resolver for IpfsResolver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        // Step 1: md5 → numeric file id via the search results page.
        let search_url = format!("{}/index.php?req={}&res=25", self.lookup_base, md5);
        let search_resp = self
            .client
            .get(&search_url)
            .header(reqwest::header::USER_AGENT, Self::UA)
            .send()
            .await
            .map_err(|e| classify_reqwest(&e))?;
        if !search_resp.status().is_success() {
            return Err(classify_status(
                search_resp.status(),
                "libgen.li search failed".into(),
            ));
        }
        let search_body = search_resp.text().await.map_err(|e| classify_reqwest(&e))?;
        let id = extract_file_id_for_md5(&search_body, md5).ok_or_else(|| {
            DownloadError::Permanent(format!(
                "no file.php?id=... row for md5 {md5} on {} \
                 (not indexed, or page layout changed)",
                self.lookup_base
            ))
        })?;

        // Step 2: numeric id → IPFS CID via the file-detail page.
        let file_url = format!("{}/file.php?id={}", self.lookup_base, id);
        let file_resp = self
            .client
            .get(&file_url)
            .header(reqwest::header::USER_AGENT, Self::UA)
            .send()
            .await
            .map_err(|e| classify_reqwest(&e))?;
        if !file_resp.status().is_success() {
            return Err(classify_status(
                file_resp.status(),
                "libgen.li file.php failed".into(),
            ));
        }
        let file_body = file_resp.text().await.map_err(|e| classify_reqwest(&e))?;
        let cid = extract_ipfs_cid(&file_body).ok_or_else(|| {
            DownloadError::Permanent(format!(
                "no /ipfs/<cid> link on file.php?id={id} for md5 {md5} \
                 (no IPFS copy, or page layout changed)"
            ))
        })?;

        // Step 3: a content-addressed gateway URL — fetchable from any gateway.
        let url = format!("{}/ipfs/{}", self.gateway_base, cid);
        Ok(DownloadTarget {
            host: host_of(&self.gateway_base),
            url,
            expected_md5: Some(md5.to_string()),
            total_bytes: None,
        })
    }
}

/// Scan a libgen.li search results page for the numeric `file.php?id=<id>` that
/// belongs to `md5`. The results table has one `<tr>` per file; we locate the
/// row containing the md5 and pull its `file.php?id=` link, so we don't grab an
/// unrelated row's id. Dependency-free string scan (no HTML parser).
fn extract_file_id_for_md5(html: &str, md5: &str) -> Option<String> {
    let md5_lower = md5.to_ascii_lowercase();
    // Find the row (<tr>…</tr>) that mentions this md5, then the file id in it.
    let mut from = 0;
    while let Some(rel) = html[from..].to_ascii_lowercase().find("<tr") {
        let tr_start = from + rel;
        let tr_end = html[tr_start..]
            .to_ascii_lowercase()
            .find("</tr>")
            .map(|e| tr_start + e + "</tr>".len())
            .unwrap_or(html.len());
        let row = &html[tr_start..tr_end];
        if row.to_ascii_lowercase().contains(&md5_lower) {
            if let Some(id) = extract_file_id(row) {
                return Some(id);
            }
        }
        from = tr_end;
    }
    // Fallback: no row-level match (layout differs) — take the first id on the
    // page only when the md5 appears somewhere on it (single-result search).
    if html.to_ascii_lowercase().contains(&md5_lower) {
        return extract_file_id(html);
    }
    None
}

/// Extract the first `file.php?id=<digits>` numeric id from `html`.
fn extract_file_id(html: &str) -> Option<String> {
    let needle = "file.php?id=";
    let start = html.find(needle)?;
    let digits: String = html[start + needle.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Extract the first IPFS CID from an `/ipfs/<cid>` link in a libgen.li
/// `file.php` page. CIDv1 (`bafy…`) is base32 lowercase alphanumeric; we stop at
/// the first non-CID char (`?`, `"`, `<`, whitespace, …). Dependency-free scan.
fn extract_ipfs_cid(html: &str) -> Option<String> {
    let needle = "/ipfs/";
    let mut from = 0;
    while let Some(rel) = html[from..].find(needle) {
        let after = &html[from + rel + needle.len()..];
        let cid: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        // CIDv1 content ids start with `bafy` and are comfortably long; guard
        // against accidentally matching a short non-CID path segment.
        if cid.starts_with("bafy") && cid.len() >= 20 {
            return Some(cid);
        }
        from += rel + needle.len();
    }
    None
}

/// Public IPFS gateways tried in order under `--site ipfs`. Content-addressed,
/// so any gateway serves any CID; this list is pure failover.
pub const IPFS_GATEWAYS: [&str; 3] = [
    "https://ipfs.io",
    "https://dweb.link",
    "https://gateway.pinata.cloud",
];

/// `--site ipfs`: a single resolver fronting several public gateways for
/// resolve-time failover. The md5→CID lookup (against `lookup_base`) is
/// gateway-independent, so it runs once; the CID is then offered on the first
/// gateway in `gateways`, and on each successive `resolve` call (the queue
/// re-resolves on mirror failover) we advance to the next gateway. This gives
/// the same effect as registering one `IpfsResolver` per gateway, but as a
/// single `Arc<dyn Resolver>` (matching the `resolver_for_site` registry shape).
///
/// Gateways are content-addressed and interchangeable, so rotating across them
/// is pure failover — no gateway is more authoritative than another.
#[derive(Debug, Clone)]
pub struct IpfsChainResolver {
    inner: IpfsResolver,
    gateways: Vec<String>,
    // Round-robin cursor advanced on every resolve so retries hit a fresh
    // gateway; wraps modulo `gateways.len()`.
    next: Arc<std::sync::atomic::AtomicUsize>,
}

impl IpfsChainResolver {
    pub fn new(
        lookup_base: impl Into<String>,
        gateways: impl IntoIterator<Item = impl Into<String>>,
        client: Client,
    ) -> Self {
        let gateways: Vec<String> = gateways
            .into_iter()
            .map(|g| g.into().trim_end_matches('/').to_string())
            .collect();
        // The inner resolver does the (gateway-independent) md5→CID lookup; its
        // gateway_base is overridden per-call, so seed it with the first gateway.
        let first = gateways
            .first()
            .cloned()
            .unwrap_or_else(|| "https://ipfs.io".to_string());
        let inner = IpfsResolver::new(lookup_base, first, client);
        IpfsChainResolver {
            inner,
            gateways,
            next: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl Resolver for IpfsChainResolver {
    fn name(&self) -> &str {
        // Name by the first gateway host (the default lane), consistent with
        // the other resolvers naming themselves by their primary host.
        self.inner.name()
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        // Resolve once via the inner resolver (its seeded gateway), then rewrite
        // the target onto the gateway chosen for this attempt. The expensive
        // libgen.li lookup happens inside the inner resolve.
        let base = self.inner.resolve(md5).await?;
        let idx =
            self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.gateways.len();
        let gateway = &self.gateways[idx];
        // Rebuild the URL on the chosen gateway: keep everything after `/ipfs/`.
        let url = match base.url.split_once("/ipfs/") {
            Some((_, cid_and_path)) => format!("{gateway}/ipfs/{cid_and_path}"),
            None => base.url.clone(),
        };
        Ok(DownloadTarget {
            host: host_of(gateway),
            url,
            expected_md5: base.expected_md5,
            total_bytes: base.total_bytes,
        })
    }
}

/// Resolver for **Anna's Archive** (`annas-archive.gl` and mirrors) — a
/// genuinely independent lane that aggregates many shadow-library backends. It is
/// **best-effort**: the free "slow download" path is gated by Cloudflare /
/// DDoS-Guard, so a bare reqwest client frequently gets a challenge page instead
/// of the file link. When that happens we return [`DownloadError::Transient`] so
/// the [`ResolverChain`] fails over to another lane.
///
/// `resolve(md5)` fetches `https://{host}/slow_download/{md5}/0/0` with a
/// browser User-Agent (following redirects) and scrapes the signed file link out
/// of the returned HTML — the link whose href/text carries the md5's 12-char
/// prefix, or an off-site `http(s)` CDN link. See `docs/ANNAS_ARCHIVE.md` for the
/// two robust future upgrades (the `fast_download.json` membership API and a
/// FlareSolverr sidecar).
///
/// All AA mirrors serve the same file (md5 is the universal id), so the extra
/// domains are pure front-door failover against blocking.
#[derive(Debug, Clone)]
pub struct AnnaArchiveResolver {
    /// Names the resolver by host so the per-host download queue routes by it.
    name: String,
    /// Mirror host, e.g. `annas-archive.gl` (no scheme, no trailing slash).
    host: String,
    client: Client,
}

impl AnnaArchiveResolver {
    /// Default mirror used when no usable AA host is supplied.
    pub const DEFAULT_HOST: &'static str = "annas-archive.gl";

    /// A browser-like User-Agent: AA's gateways serve plain `reqwest`-looking
    /// agents a challenge page far more often.
    const UA: &'static str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

    /// Build a resolver for the given AA `host`. A full `https://host` form or a
    /// bare host both work; anything that does not look like an AA domain falls
    /// back to [`DEFAULT_HOST`](Self::DEFAULT_HOST).
    pub fn new(host: impl Into<String>, client: Client) -> Self {
        let raw = host.into();
        let stripped = raw
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let host = if is_annas_archive_host(stripped) {
            stripped.to_ascii_lowercase()
        } else {
            Self::DEFAULT_HOST.to_string()
        };
        let name = host.clone();
        AnnaArchiveResolver { name, host, client }
    }
}

#[async_trait::async_trait]
impl Resolver for AnnaArchiveResolver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
        let md5_lower = md5.trim().to_ascii_lowercase();
        let page_url = format!("https://{}/slow_download/{}/0/0", self.host, md5_lower);
        let resp = self
            .client
            .get(&page_url)
            .header(reqwest::header::USER_AGENT, Self::UA)
            .send()
            .await
            .map_err(|e| classify_reqwest(&e))?;
        let status = resp.status();
        // Cloudflare / DDoS-Guard front the slow path with 403/503 challenges.
        if status == StatusCode::FORBIDDEN || status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(DownloadError::Transient(format!(
                "Anna's Archive challenge (HTTP {status}) on {} — likely Cloudflare/DDoS-Guard",
                self.host
            )));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(DownloadError::Permanent(format!(
                "Anna's Archive has no slow_download page for md5 {md5_lower} on {}",
                self.host
            )));
        }
        if !status.is_success() {
            return Err(classify_status(status, "slow_download failed".into()));
        }
        let body = resp.text().await.map_err(|e| classify_reqwest(&e))?;

        // A challenge body can come back with a 200, so sniff it too.
        if looks_like_challenge(&body) {
            return Err(DownloadError::Transient(format!(
                "Anna's Archive challenge page on {} (Cloudflare/DDoS-Guard interstitial)",
                self.host
            )));
        }

        match extract_annas_download_url(&body, &md5_lower, &self.host) {
            Some(url) => Ok(DownloadTarget {
                host: host_of(&url),
                url,
                expected_md5: Some(md5_lower),
                total_bytes: None,
            }),
            // No link and no obvious challenge — usually a waitlist/"please wait"
            // interstitial. Transient so the queue fails over to another lane.
            None => Err(DownloadError::Transient(format!(
                "no Anna's Archive download link for md5 {md5_lower} on {} \
                 (waitlist/interstitial, or page layout changed)",
                self.host
            ))),
        }
    }
}

/// True if `host` looks like an Anna's Archive domain (`annas-archive.*` or the
/// short `annas-archive`/`annas` aliases). Used to decide whether a supplied
/// site string is a usable AA host or we should fall back to the default mirror.
fn is_annas_archive_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    h == "annas-archive" || h == "annas" || h.starts_with("annas-archive.")
}

/// Heuristic: does this body look like a Cloudflare / DDoS-Guard interstitial
/// rather than the real slow_download page? Matched case-insensitively against a
/// handful of marker strings these challenge pages reliably carry.
fn looks_like_challenge(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    const MARKERS: [&str; 6] = [
        "just a moment",
        "verifying you are human",
        "cf-browser-verification",
        "cf-challenge",
        "ddos-guard",
        "checking your browser",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Extract the real signed file URL from an Anna's Archive `slow_download` page.
///
/// AA renders the download as an anchor whose href (or surrounding text) carries
/// the md5's 12-char prefix — sometimes a same-origin `/...` path, sometimes a
/// full off-site CDN link. We scan every `<a href="...">` and prefer:
///   1. an absolute `http(s)` href that mentions the md5 prefix (the CDN link),
///   2. otherwise an absolute `http(s)` href under a different host than the AA
///      page (an off-site file link), resolving relative hrefs against `host`.
///
/// Dependency-light: a `scraper` parse to walk anchors, then plain string checks.
/// Returns `None` if nothing download-like is found (caller treats that as a
/// transient waitlist/challenge).
fn extract_annas_download_url(html: &str, md5: &str, host: &str) -> Option<String> {
    use scraper::{Html, Selector};

    let md5_lower = md5.to_ascii_lowercase();
    // The 12-char prefix AA embeds in the signed link.
    let prefix: String = md5_lower.chars().take(12).collect();
    let doc = Html::parse_document(html);
    let anchor = Selector::parse("a[href]").ok()?;

    // First pass: an href that mentions the md5 prefix is unambiguously the file.
    let mut offsite_fallback: Option<String> = None;
    for el in doc.select(&anchor) {
        let href = match el.value().attr("href") {
            Some(h) => h.trim(),
            None => continue,
        };
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let abs = absolutize(href, host);
        let abs_lower = abs.to_ascii_lowercase();
        // Strong signal: the signed link carries the md5 prefix.
        if !prefix.is_empty() && abs_lower.contains(&prefix) {
            return Some(abs);
        }
        // Weaker signal: a full off-site http(s) link that is not AA chrome.
        if (abs.starts_with("http://") || abs.starts_with("https://"))
            && host_of(&abs) != host
            && offsite_fallback.is_none()
            && looks_like_file_link(&abs_lower)
        {
            offsite_fallback = Some(abs);
        }
    }
    offsite_fallback
}

/// Resolve a possibly-relative href against the AA mirror `host`. Absolute
/// `http(s)` links pass through; root-relative (`/x`) and bare-relative links
/// are joined onto `https://{host}`.
fn absolutize(href: &str, host: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(rest) = href.strip_prefix('/') {
        format!("https://{host}/{rest}")
    } else {
        format!("https://{host}/{href}")
    }
}

/// Heuristic for an off-site link that plausibly points at a downloadable file
/// rather than navigation chrome (donate/login/about/social links).
fn looks_like_file_link(url_lower: &str) -> bool {
    const NON_FILE: [&str; 8] = [
        "/donate",
        "/login",
        "/account",
        "/about",
        "facebook.com",
        "twitter.com",
        "t.me",
        "reddit.com",
    ];
    !NON_FILE.iter().any(|n| url_lower.contains(n))
}

/// Anna's Archive mirror hosts, in default failover order. All serve the same
/// files (md5 is the universal id); the extra domains buy front-door failover.
pub const ANNAS_ARCHIVE_SITES: [&str; 4] = [
    "annas-archive.gl",
    "annas-archive.vg",
    "annas-archive.pk",
    "annas-archive.gd",
];

/// Download sites this build can resolve, in a sensible default failover order.
/// The libgen.li family share one CDN (front-door failover); libgen.pw /
/// randombook.org use the independent libgen.download CDN (extra bandwidth).
pub const LIBGEN_FAMILY_SITES: [&str; 3] = ["libgen.li", "libgen.vg", "libgen.la"];

/// Every download site this build knows, for help text / "use all mirrors".
pub const ALL_SITES: [&str; 7] = [
    "libgen.li",
    "libgen.vg",
    "libgen.la",
    "libgen.pw",
    "randombook.org",
    "ipfs",
    "annas-archive.gl",
];

/// Build a [`Resolver`] for a named download `site`. The single registry shared
/// by every front end (CLI, Tauri). Unknown names return an error listing the
/// supported sites. Accepts bare hosts or full `https://host` forms.
pub fn resolver_for_site(site: &str, client: &Client) -> Result<Arc<dyn Resolver>> {
    let key = site
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    match key.as_str() {
        "libgen.li" | "libgenli" => Ok(Arc::new(LibgenLiResolver::new(
            "https://libgen.li",
            client.clone(),
        ))),
        "libgen.vg" | "libgenvg" => Ok(Arc::new(LibgenLiResolver::new(
            "https://libgen.vg",
            client.clone(),
        ))),
        "libgen.la" | "libgenla" => Ok(Arc::new(LibgenLiResolver::new(
            "https://libgen.la",
            client.clone(),
        ))),
        "libgen.pw" | "libgenpw" => Ok(Arc::new(LibgenPwResolver::new(
            "https://libgen.pw",
            client.clone(),
        ))),
        "randombook.org" | "randombook" => Ok(Arc::new(LibgenPwResolver::new(
            "https://randombook.org",
            client.clone(),
        ))),
        // The independent IPFS lane: md5→CID lookup via libgen.li, bytes served
        // by the public IPFS network through a chain of gateways (failover).
        "ipfs" => Ok(Arc::new(IpfsChainResolver::new(
            "https://libgen.li",
            IPFS_GATEWAYS,
            client.clone(),
        ))),
        // Anna's Archive: best-effort slow_download lane. `annas`/`annas-archive`
        // use the default mirror; a full AA domain is used as the host directly.
        "annas" | "annas-archive" => Ok(Arc::new(AnnaArchiveResolver::new(
            AnnaArchiveResolver::DEFAULT_HOST,
            client.clone(),
        ))),
        _ if is_annas_archive_host(&key) => {
            Ok(Arc::new(AnnaArchiveResolver::new(key, client.clone())))
        }
        other => Err(anyhow!(
            "unknown download site '{other}'. Supported: {}",
            ALL_SITES.join(", ")
        )),
    }
}

/// Default top-level resolve entry point used by the simple CLI path. Builds a
/// single direct-URL resolver against `base` (with `{md5}` placeholder) and
/// resolves `md5`.
pub async fn resolve(md5: &str) -> Result<DownloadTarget> {
    // Without configured mirrors there is nothing to resolve against; callers
    // that want real behavior construct a `ResolverChain`/`DirectUrlResolver`.
    Err(anyhow!(
        "resolve({md5}): no resolver configured; build a ResolverChain (e.g. DirectUrlResolver) and use it"
    ))
}

/// Resolve `md5` against a single direct-URL `base` template (used by the CLI
/// with `--mock`). `base` contains a `{md5}` placeholder.
pub async fn resolve_direct(md5: &str, base: &str, client: &Client) -> Result<DownloadTarget> {
    let resolver = DirectUrlResolver::new(host_of(base), base.to_string(), client.clone());
    resolver
        .resolve(md5)
        .await
        .map_err(|e| anyhow!(e.to_string()))
}

/// Download `target` to `dest`, resuming from `resume_offset`, verifying md5.
/// Returns the number of bytes written **in this call** (not counting the
/// pre-existing `resume_offset`).
///
/// Streams to a sibling `dest.part` file, then atomically renames to `dest` on
/// success. On md5 mismatch the `.part` file is removed and a permanent error
/// is returned.
pub async fn download_to(target: &DownloadTarget, dest: &Path, resume_offset: u64) -> Result<u64> {
    let client = Client::builder().build().context("building http client")?;
    download_with_client(&client, target, dest, resume_offset)
        .await
        .map_err(|e| anyhow!(e.to_string()))
}

/// Like [`download_to`] but with a caller-supplied client (so the queue can
/// share connection pools and timeouts) and typed errors for retry decisions.
pub async fn download_with_client(
    client: &Client,
    target: &DownloadTarget,
    dest: &Path,
    resume_offset: u64,
) -> Result<u64, DownloadError> {
    // No cancellation: pass a token that is never triggered.
    download_with_client_cancellable(
        client,
        target,
        dest,
        resume_offset,
        &CancellationToken::new(),
        None, // no scheduler channel → no diagnostic notes
        None,
    )
    .await
}

/// Bound the response-HEADERS phase. A mirror/edge that connects but never sends
/// headers would otherwise hang a (global + per-host) slot forever. 45s leaves
/// headroom for a busy `booksdl` edge whose time-to-first-byte climbs under load
/// (see docs/CDN_EDGE_FINDING.md) without wedging the slot indefinitely.
const HEADERS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Max concurrent streams to a SINGLE `cdnN.booksdl.lc` edge, process-wide.
///
/// The booksdl edges don't rate-limit/429 under load, but their time-to-first-byte
/// degrades sharply once an edge is saturated: live measurements (docs/
/// CDN_EDGE_FINDING.md) showed ~5s TTFB at 6 concurrent streams but up to ~29s at
/// 16 — right at our [`HEADERS_TIMEOUT`] cliff. A leg that times out is retried /
/// failed-over, piling MORE streams onto the same edge: a self-inflicted
/// congestion-collapse spiral that looks like "the CDN keeps timing out."
///
/// The scheduler's per-host cap is keyed by the *mirror* host (libgen.li, …), but
/// every mirror 307-redirects to the same `cdnN` edge, so it does NOT bound
/// per-edge load. This cap closes that gap. 3 keeps aggregate throughput near its
/// plateau while holding TTFB comfortably under the headers timeout.
pub const MAX_CONCURRENT_PER_EDGE: usize = 3;

/// Process-wide registry of per-edge concurrency limiters, keyed by edge host
/// (`cdn3.booksdl.lc`). Lazily created; one [`Semaphore`] of
/// [`MAX_CONCURRENT_PER_EDGE`] permits per edge. Shared across every download leg
/// so the cap is global, not per-call.
fn edge_semaphores() -> &'static Mutex<HashMap<String, Arc<Semaphore>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Process-wide registry of the CDN edge (`cdn2.booksdl.lc`) each md5 is CURRENTLY
/// downloading from — so the scheduler can report the real serving edge (not just
/// the mirror front-door) in progress events and the per-book history.
fn edge_in_use() -> &'static Mutex<HashMap<String, String>> {
    static R: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
fn record_edge(md5: &str, edge: &str) {
    if let Ok(mut m) = edge_in_use().lock() {
        m.insert(md5.to_string(), edge.to_string());
    }
}
/// The booksdl edge md5 is currently being served from, if a download has chosen
/// one (`cdn2.booksdl.lc`). `None` before an edge is picked / for non-booksdl lanes.
pub fn current_edge(md5: &str) -> Option<String> {
    edge_in_use().lock().ok()?.get(md5).cloned()
}

/// The booksdl edge a URL targets, if any (`cdn3.booksdl.lc`). Other hosts (mocks,
/// IPFS gateways, libgen.download, …) return `None` and are not edge-capped.
#[doc(hidden)] // pub for the `edge_failover_probe` harness; not a stable API.
pub fn booksdl_edge_host(url: &str) -> Option<String> {
    let host = reqwest::Url::parse(url).ok()?.host_str()?.to_string();
    host.ends_with(".booksdl.lc").then_some(host)
}

/// In-flight stream count for a booksdl `edge_host` right now (0 if the edge has no
/// limiter yet). Observability for the `edge_failover_probe` harness to confirm the
/// per-edge cap never exceeds [`MAX_CONCURRENT_PER_EDGE`] under concurrent load.
#[doc(hidden)]
pub fn edge_inflight(edge_host: &str) -> usize {
    let reg = edge_semaphores().lock().expect("edge registry poisoned");
    reg.get(edge_host)
        .map(|s| MAX_CONCURRENT_PER_EDGE.saturating_sub(s.available_permits()))
        .unwrap_or(0)
}

/// Acquire a per-edge concurrency permit for `url` if it targets a booksdl edge.
/// Returns `None` for non-booksdl hosts (no cap). The permit is owned, so the
/// caller holds it for the whole transfer (headers + body stream); dropping it
/// frees the edge slot. Cancellable: returns [`DownloadError::Cancelled`] if the
/// token fires while we wait for a slot.
async fn acquire_edge_permit(
    url: &str,
    cancel: &CancellationToken,
) -> Result<Option<OwnedSemaphorePermit>, DownloadError> {
    let Some(edge) = booksdl_edge_host(url) else {
        return Ok(None);
    };
    let sem = {
        let mut reg = edge_semaphores().lock().expect("edge registry poisoned");
        reg.entry(edge)
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PER_EDGE)))
            .clone()
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(DownloadError::Cancelled { bytes_written: 0 }),
        // `Semaphore` is never closed, so acquire only errors on close — unwrap is safe.
        permit = sem.acquire_owned() => Ok(Some(permit.expect("edge semaphore closed"))),
    }
}

/// Send one GET (optionally ranged from `range_start`), bounding the headers phase.
async fn send_get(
    client: &Client,
    url: &str,
    range_start: u64,
) -> Result<reqwest::Response, DownloadError> {
    let mut req = client.get(url);
    if range_start > 0 {
        req = req.header(RANGE, format!("bytes={range_start}-"));
    }
    match tokio::time::timeout(HEADERS_TIMEOUT, req.send()).await {
        Ok(r) => r.map_err(|e| classify_reqwest(&e)),
        Err(_) => Err(DownloadError::Transient(format!(
            "no response headers within {}s",
            HEADERS_TIMEOUT.as_secs()
        ))),
    }
}

/// The `booksdl.lc` CDN has independent edges `cdn1..cdn6`; a file present on one
/// edge can hard-500 ("repository folder" error) on another that doesn't hold the
/// blob — edge health is per-FILE and time-varying. The signed `get.php` key is
/// edge-AGNOSTIC, so given the edge URL we landed on, the SAME file is fetchable on
/// a sibling edge. Returns alternate-edge URLs (same path + signed query) to try,
/// or `None` if `url` isn't a booksdl edge. See docs/CDN_EDGE_FINDING.md.
#[doc(hidden)] // pub for the `edge_failover_probe` harness; not a stable API.
pub fn booksdl_alternate_edges(url: &reqwest::Url) -> Option<Vec<String>> {
    let host = url.host_str()?;
    if !host.ends_with(".booksdl.lc") {
        return None;
    }
    let mut out = Vec::new();
    for n in 1..=6 {
        let h = format!("cdn{n}.booksdl.lc");
        if h == host {
            continue;
        }
        let mut u = url.clone();
        if u.set_host(Some(&h)).is_ok() {
            out.push(u.to_string());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A redirect-DISABLED client used only to resolve a mirror `get.php` 307 to its
/// cdn-edge `Location` WITHOUT following it — so resolution stays fast even when the
/// target edge would hang on the body, and we learn the edge URL up front.
fn redirect_probe_client() -> &'static Client {
    static C: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("redirect-probe client")
    })
}

/// Resolve a mirror `get.php` URL to the cdn-edge URL it 307-redirects to, via a fast
/// redirect-disabled request. Returns the edge URL on success; the ORIGINAL url if it is
/// already a booksdl edge, has no booksdl redirect, or resolution fails (the caller then
/// falls back to the prior follow-the-redirect-inside-the-GET behavior).
///
/// Why: the download client follows redirects, so the first `send_get` to the mirror
/// follows the 307 INSIDE one request. If the redirected edge then HANGS, that request
/// times out as an opaque error carrying no edge URL — and edge rotation can't fire
/// (it needs a booksdl edge to rotate FROM). Resolving the edge up front means the
/// capped GET + rotation run on the real edge, so a hanging edge rotates to siblings on
/// the FIRST attempt instead of dead-ending on the mirror.
#[doc(hidden)] // pub for the resolve_probe example to verify against the live network
pub async fn resolve_to_edge(url: &str, cancel: &CancellationToken) -> String {
    if booksdl_edge_host(url).is_some() {
        return url.to_string(); // already an edge (a retry / rotation URL)
    }
    // Only probe a libgen mirror `get.php` (which 307s to a booksdl edge). Other URLs —
    // mocks, direct-serve lanes — serve the BODY, so probing them is wasteful and could
    // open a second transfer (it inflated the test mock's concurrency). Leave them on the
    // prior follow-the-redirect-inside-the-GET path.
    if !url.contains("get.php") {
        return url.to_string();
    }
    let send = redirect_probe_client().get(url).send();
    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => return url.to_string(),
        r = tokio::time::timeout(HEADERS_TIMEOUT, send) => match r {
            Ok(Ok(resp)) => resp,
            _ => return url.to_string(), // timeout / error → fall back to the old path
        },
    };
    if resp.status().is_redirection() {
        if let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        {
            // Absolute Location is the norm (https://cdnN.booksdl.lc/…); resolve a
            // relative one against the request URL just in case.
            let abs = reqwest::Url::parse(url)
                .ok()
                .and_then(|base| base.join(loc).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| loc.to_string());
            if booksdl_edge_host(&abs).is_some() {
                return abs;
            }
        }
    }
    url.to_string()
}

/// Emit a best-effort diagnostic [`Progress::Note`] on the scheduler channel so the
/// persisted history records an otherwise-invisible download-path event (edge
/// rotation outcome; Range-ignored restart). Behavior-neutral: requires both a
/// channel and an md5, uses `try_send` so it NEVER blocks or awaits, and ignores
/// every send error (full/closed channel) — the real control flow is untouched.
fn note(
    events: Option<&tokio::sync::mpsc::Sender<crate::queue::Progress>>,
    md5: Option<&str>,
    detail: String,
) {
    if let (Some(tx), Some(md5)) = (events, md5) {
        let _ = tx.try_send(crate::queue::Progress::Note {
            md5: md5.to_string(),
            detail,
        });
    }
}

/// Short label for an edge URL used in rotation notes: the `cdnN` edge host
/// (`cdn3.booksdl.lc`) when it is a booksdl edge, else the bare host.
fn edge_label(url: &str) -> String {
    booksdl_edge_host(url).unwrap_or_else(|| {
        reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "?".to_string())
    })
}

/// One-word outcome for a failed probe (used in a rotation note): "timeout" for a
/// headers timeout / connect stall, else "error".
fn probe_err_kind(e: &DownloadError) -> &'static str {
    match e {
        DownloadError::Transient(m) if m.contains("no response headers") => "timeout",
        _ => "error",
    }
}

/// Fetch a streamable response, FAILING OVER across booksdl CDN edges. The mirror's
/// `get.php` 307-redirects to ONE edge; if that edge 500s (doesn't hold the file)
/// we rotate to its siblings (`cdn1..cdn6`), which commonly DO. Returns the first
/// 2xx/206 response; otherwise the last response so the caller's status handling
/// fails over to another mirror. Non-booksdl URLs (mocks, other lanes) are a single
/// pass — identical to the prior behavior.
///
/// Each edge attempt acquires a [per-edge permit](acquire_edge_permit) first, so no
/// more than [`MAX_CONCURRENT_PER_EDGE`] streams hit one `cdnN` at once. The permit
/// for the WINNING edge is returned alongside the response and must be held for the
/// whole body transfer (it's dropped when the returned permit drops); permits for
/// edges we probed-and-rejected drop immediately. Probing an edge holds a slot only
/// for its (bounded) headers phase, so a sick edge can't tie up a slot for long.
async fn fetch_with_edge_rotation(
    client: &Client,
    url: &str,
    range_start: u64,
    cancel: &CancellationToken,
    events: Option<&tokio::sync::mpsc::Sender<crate::queue::Progress>>,
    md5: Option<&str>,
) -> Result<(reqwest::Response, Option<OwnedSemaphorePermit>), DownloadError> {
    // The signed get.php URL the mirror handed us. The first send_get follows the
    // 307 to the actual edge, so we cap on the URL we're about to hit; reqwest's
    // redirect happens inside send_get, so we acquire on `url`'s host (the mirror)
    // only when it's already a booksdl edge — the common case is the resolver
    // returning the mirror get.php, which is NOT booksdl, so the first hop is
    // uncapped (cheap redirect) and the rotation below caps the real edges.
    let first_permit = acquire_edge_permit(url, cancel).await?;
    let first = send_get(client, url, range_start).await;
    if let Ok(resp) = &first {
        if resp.status().is_success() || resp.status() == StatusCode::PARTIAL_CONTENT {
            // Re-key the permit onto the edge we actually landed on after the 307, so
            // the body transfer is counted against the real edge, not the mirror.
            let resp = first.expect("checked Ok");
            let permit = match first_permit {
                Some(p) => Some(p),
                None => acquire_edge_permit(resp.url().as_str(), cancel).await?,
            };
            return Ok((resp, permit));
        }
    }
    drop(first_permit); // free the rejected/failed edge's slot before probing siblings

    // Decide which edge URL to rotate FROM. On a bad *response* it's the edge we
    // landed on (after the 307). On a connection *error* (a dead edge — no response
    // to read a URL from) we can only rotate if the URL we hit was itself a booksdl
    // edge; if it was the mirror get.php we never saw an edge, so surface the error
    // and let the scheduler fail over to another mirror.
    let edge_ref: Option<reqwest::Url> = match &first {
        Ok(resp) => Some(resp.url().clone()),
        Err(_) => reqwest::Url::parse(url)
            .ok()
            .filter(|u| booksdl_edge_host(u.as_str()).is_some()),
    };
    let alts = match edge_ref.as_ref().and_then(booksdl_alternate_edges) {
        Some(a) => a,
        None => return first.map(|r| (r, None)), // can't rotate → surface first outcome
    };

    // PROBE the sibling edges CONCURRENTLY (not one-by-one): each probe acquires its
    // own per-edge permit then sends. The FIRST to return 2xx/206 wins — we keep its
    // permit (held through the body transfer) and DROP the rest, which cancels their
    // in-flight requests and releases their permits. A slow/dead edge no longer
    // serializes the others, so the worst case is one HEADERS_TIMEOUT window instead
    // of N. A bad response is kept as a fallback so the caller can still fail over.
    let mut probes = FuturesUnordered::new();
    for alt in alts {
        probes.push(async move {
            // Carry the edge label so each probe's outcome can be chronicled as a
            // diagnostic Note (best-effort; never affects the rotation decision).
            let edge = edge_label(&alt);
            match acquire_edge_permit(&alt, cancel).await {
                // `Some(result)` = we actually probed; `None` = cancelled before probing.
                Ok(permit) => (
                    Some(send_get(client, &alt, range_start).await),
                    permit,
                    edge,
                ),
                Err(_) => (None, None, edge), // cancelled while waiting for a slot
            }
        });
    }
    let mut fallback = first.ok();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return match fallback {
                    Some(r) => Ok((r, None)),
                    None => Err(DownloadError::Cancelled { bytes_written: 0 }),
                };
            }
            next = probes.next() => match next {
                Some((Some(Ok(r)), permit, edge))
                    if r.status().is_success() || r.status() == StatusCode::PARTIAL_CONTENT =>
                {
                    note(events, md5, format!("rotate {edge} → {} (won)", r.status().as_u16()));
                    return Ok((r, permit)); // winner — keep its permit; drop the rest
                }
                Some((Some(Ok(r)), _permit, edge)) => {
                    note(events, md5, format!("rotate {edge} → {}", r.status().as_u16()));
                    fallback = Some(r); // rejected edge; permit freed
                }
                Some((Some(Err(e)), _permit, edge)) => {
                    note(events, md5, format!("rotate {edge} → {}", probe_err_kind(&e)));
                }
                Some((None, _permit, edge)) => {
                    // cancelled while queued for a slot — never actually probed
                    note(events, md5, format!("rotate {edge} → skipped"));
                }
                None => break, // all probes done, none good
            }
        }
    }
    match fallback {
        Some(r) => Ok((r, None)),
        None => Err(DownloadError::Transient(
            "all booksdl edges unreachable".into(),
        )),
    }
}

/// Like [`download_with_client`] but observes a [`CancellationToken`]: if it is
/// cancelled mid-transfer the streamed bytes already flushed to the `.part` are
/// kept on disk (so a *paused* job can resume from where it stopped) and a
/// [`DownloadError::Cancelled`] carrying the bytes written this call is returned.
/// The caller decides whether to remove the `.part` (hard cancel) or keep it
/// (pause).
pub async fn download_with_client_cancellable(
    client: &Client,
    target: &DownloadTarget,
    dest: &Path,
    resume_offset: u64,
    cancel: &CancellationToken,
    // Diagnostic channel for best-effort [`Progress::Note`]s (edge rotation,
    // Range-ignored restart). `None` for the many direct callers/tests that don't
    // observe a scheduler — emitting nothing changes no behavior. Paired with `md5`
    // so notes are tagged onto the right variation's history.
    events: Option<&tokio::sync::mpsc::Sender<crate::queue::Progress>>,
    md5: Option<&str>,
) -> Result<u64, DownloadError> {
    let part = part_path(dest);

    // Fast path: already cancelled before any I/O.
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled { bytes_written: 0 });
    }

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| DownloadError::Transient(format!("mkdir {parent:?}: {e}")))?;
        }
    }

    let existing = match fs::metadata(&part).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    // The on-disk `.part` is the SOURCE OF TRUTH for progress: bytes are appended
    // sequentially, so its first `existing` bytes are always valid. `resume_offset`
    // is only a hint — and it is 0 for a download interrupted WITHOUT an explicit
    // pause (e.g. an app kill / relaunch), so treating 0 as "start fresh" would
    // delete the partial and restart from scratch (the resume regression). Resume
    // from whatever the `.part` holds; only restart clean if the caller asked to
    // resume from an offset BEYOND what's on disk (a hole would otherwise result).
    // A deliberate hard-cancel already removed the `.part` before we get here, so a
    // genuine fresh download has `existing == 0`.
    let mut start = if existing == 0 {
        0
    } else if resume_offset > existing {
        let _ = fs::remove_file(&part).await;
        0
    } else {
        existing
    };

    // Fetch headers, failing over across booksdl CDN edges (cdn1..cdn6) when the
    // edge the mirror redirected us to doesn't hold this file (hard 500). The
    // HEADERS phase is bounded inside (a connected-but-silent edge frees the slot
    // as Transient); the idle-stall guard below covers the body STREAM.
    //
    // Resolve the mirror's 307 to the actual cdn EDGE up front (fast, redirect-disabled)
    // so the capped GET + rotation run on the edge — a hanging edge then rotates to
    // siblings on the FIRST attempt instead of timing out opaquely on the mirror.
    let fetch_url = resolve_to_edge(&target.url, cancel).await;
    // Record the resolved edge BEFORE the GET, so progress reports the real edge while
    // still connecting (not the mirror front-door).
    if let (Some(md5), Some(edge)) = (
        target.expected_md5.as_deref(),
        booksdl_edge_host(&fetch_url),
    ) {
        record_edge(md5, &edge);
    }

    // `_edge_permit` caps concurrent streams to the winning `cdnN` edge.
    let (resp, _edge_permit) =
        fetch_with_edge_rotation(client, &fetch_url, start, cancel, events, md5).await?;
    let status = resp.status();

    // Re-record in case rotation moved us to a DIFFERENT edge than we resolved.
    if let (Some(md5), Some(edge)) = (
        target.expected_md5.as_deref(),
        booksdl_edge_host(resp.url().as_str()),
    ) {
        record_edge(md5, &edge);
    }

    // We asked to resume (Range) but the server replied 200 — it IGNORED the Range and
    // is streaming the whole file from 0. The libgen CDN normally serves 206; a 200 here
    // means this file's edges won't honor a resume, and (observed) the SAME edge 200s on
    // every retry — so failing over to "preserve" the partial just loops forever and the
    // download never completes. Per the contract, DOWNLOAD FROM SCRATCH instead: drop the
    // partial and take the full stream from 0. (`start == 0` is already the fresh path.)
    if start > 0 && status == StatusCode::OK {
        tracing::warn!(
            partial_bytes = start,
            host = %resp.url().host_str().unwrap_or("?"),
            "host ignored Range (HTTP 200) — restarting download from scratch"
        );
        // Chronicle BOTH halves of this transition so the history proves the 200 path
        // RESTARTS (rather than the old stuck-loop that failed over to "preserve" a
        // partial the edge would never honor). The doctor pairs these two notes.
        note(
            events,
            md5,
            format!(
                "host ignored Range (HTTP 200) on {}",
                resp.url().host_str().unwrap_or("?")
            ),
        );
        note(
            events,
            md5,
            format!("restarting from scratch (dropped {start}-byte partial)"),
        );
        start = 0; // the file is (re)opened with truncate below, overwriting the partial
    }

    if !(status.is_success() || status == StatusCode::PARTIAL_CONTENT) {
        return Err(classify_status(status, "GET failed".into()));
    }

    // Open the part file at the right position.
    let mut file: File = if start > 0 {
        let mut f = OpenOptions::new()
            .write(true)
            .read(true)
            .open(&part)
            .await
            .map_err(|e| DownloadError::Transient(format!("open part: {e}")))?;
        f.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| DownloadError::Transient(format!("seek part: {e}")))?;
        f
    } else {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part)
            .await
            .map_err(|e| DownloadError::Transient(format!("create part: {e}")))?
    };

    // Idle-stall guard: if a mirror sends headers then stops trickling (no bytes
    // for this long), abort as Transient so the failover loop frees the slot and
    // retries another mirror — instead of hanging the (global + per-host) slot
    // forever. The timeout resets on every chunk, so a slow-but-progressing
    // transfer is never aborted. The `.part` is flushed/kept for a Range resume.
    const IDLE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Flush what we have so a paused job can resume from here, then
                // report the deliberate stop.
                let _ = file.flush().await;
                drop(file);
                return Err(DownloadError::Cancelled {
                    bytes_written: written,
                });
            }
            next = tokio::time::timeout(IDLE_STALL_TIMEOUT, stream.next()) => match next {
                Err(_elapsed) => {
                    // Stalled: no data for IDLE_STALL_TIMEOUT. Keep the partial and
                    // fail over (Transient → retry/next-mirror), freeing the slot.
                    let _ = file.flush().await;
                    drop(file);
                    return Err(DownloadError::Transient(format!(
                        "stalled: no data for {}s",
                        IDLE_STALL_TIMEOUT.as_secs()
                    )));
                }
                Ok(Some(c)) => c.map_err(|e| classify_reqwest(&e))?,
                Ok(None) => break,
            },
        };
        file.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::Transient(format!("write part: {e}")))?;
        written += chunk.len() as u64;
    }
    file.flush()
        .await
        .map_err(|e| DownloadError::Transient(format!("flush part: {e}")))?;
    drop(file);

    // Verify md5 over the full assembled part file.
    if let Some(expected) = &target.expected_md5 {
        let actual = md5_of_file(&part)
            .await
            .map_err(|e| DownloadError::Transient(format!("hashing {part:?}: {e}")))?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&part).await;
            return Err(DownloadError::Permanent(format!(
                "md5 mismatch: expected {expected}, got {actual}"
            )));
        }
    }

    // Atomic completion.
    fs::rename(&part, dest)
        .await
        .map_err(|e| DownloadError::Transient(format!("rename {part:?} -> {dest:?}: {e}")))?;

    Ok(written)
}

/// Sibling `.part` path for a destination.
pub fn part_path(dest: &Path) -> std::path::PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    std::path::PathBuf::from(s)
}

/// Compute the hex md5 of a file's full contents.
pub async fn md5_of_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("read {path:?}"))?;
    let mut hasher = Md5::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Compute the hex md5 of an in-memory byte slice (test helper, also handy for
/// callers verifying expected blobs).
pub fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_edge_records_and_reads_the_serving_edge() {
        record_edge("manualtestmd5aaaaaaaaaaaaaaaaaa", "cdn2.booksdl.lc");
        assert_eq!(
            current_edge("manualtestmd5aaaaaaaaaaaaaaaaaa").as_deref(),
            Some("cdn2.booksdl.lc")
        );
        assert_eq!(current_edge("never-downloaded-md5"), None);
    }

    #[test]
    fn booksdl_edges_rotate_host_and_keep_signed_path() {
        let url = reqwest::Url::parse("https://cdn3.booksdl.lc/get.php?md5=abc&key=SIGNED123&x=1")
            .unwrap();
        let alts = booksdl_alternate_edges(&url).expect("booksdl edge has alternates");
        // Five siblings (cdn1,2,4,5,6 — not the current cdn3), same path + query.
        assert_eq!(alts.len(), 5);
        assert!(alts
            .iter()
            .all(|u| u.contains("/get.php?md5=abc&key=SIGNED123&x=1")));
        assert!(alts
            .iter()
            .any(|u| u.starts_with("https://cdn2.booksdl.lc/")));
        assert!(alts
            .iter()
            .all(|u| !u.starts_with("https://cdn3.booksdl.lc/")));
        // Non-booksdl hosts are not rotated (single-pass — prior behavior).
        let other = reqwest::Url::parse("https://libgen.li/get.php?md5=abc&key=K").unwrap();
        assert!(booksdl_alternate_edges(&other).is_none());
    }

    #[test]
    fn booksdl_edge_host_identifies_only_booksdl_edges() {
        assert_eq!(
            booksdl_edge_host("https://cdn3.booksdl.lc/get.php?md5=abc&key=K").as_deref(),
            Some("cdn3.booksdl.lc")
        );
        // Mirror front-doors, mocks, and other lanes are NOT edge-capped.
        assert!(booksdl_edge_host("https://libgen.li/get.php?md5=abc&key=K").is_none());
        assert!(booksdl_edge_host("https://libgen.download/api/download?id=1").is_none());
        assert!(booksdl_edge_host("http://127.0.0.1:9000/get/abc").is_none());
        // A lookalike that isn't actually a booksdl.lc subdomain is rejected.
        assert!(booksdl_edge_host("https://evil-booksdl.lc.example.com/x").is_none());
    }

    #[tokio::test]
    async fn edge_permit_caps_concurrency_per_edge_and_releases_on_drop() {
        let cancel = CancellationToken::new();
        let url = "https://cdn5.booksdl.lc/get.php?md5=abc&key=K";
        // Hold MAX_CONCURRENT_PER_EDGE permits for this edge.
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_PER_EDGE {
            held.push(
                acquire_edge_permit(url, &cancel)
                    .await
                    .unwrap()
                    .expect("booksdl edge yields a permit"),
            );
        }
        // The edge is now saturated: a further acquire must not resolve immediately.
        let blocked = acquire_edge_permit(url, &cancel);
        tokio::pin!(blocked);
        assert!(
            futures::poll!(&mut blocked).is_pending(),
            "edge at capacity should block the next acquire"
        );
        // Releasing one held permit frees a slot, so the pending acquire resolves.
        held.pop();
        let freed = tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .expect("acquire should resolve once a slot frees")
            .unwrap();
        assert!(freed.is_some());

        // A non-booksdl URL is never capped (returns None without consuming a slot).
        assert!(
            acquire_edge_permit("https://libgen.li/get.php?md5=abc", &cancel)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn edge_permit_respects_cancellation_while_waiting() {
        let cancel = CancellationToken::new();
        let url = "https://cdn4.booksdl.lc/get.php?md5=abc&key=K";
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_PER_EDGE {
            held.push(acquire_edge_permit(url, &cancel).await.unwrap().unwrap());
        }
        // Saturated edge + a cancelled token ⇒ the waiter returns Cancelled, not a hang.
        cancel.cancel();
        let res = acquire_edge_permit(url, &cancel).await;
        assert!(matches!(res, Err(DownloadError::Cancelled { .. })));
    }

    #[test]
    fn extracts_get_link_from_ads_page() {
        // Mirrors the real libgen.li ads.php markup: an anchor whose href is a
        // relative get.php link carrying a short-lived key.
        let html = r#"<div><a href="get.php?md5=1df204c78842ffe549166ffcb984babc&key=H81SB7XVJ3G9N3GT"><h2>GET</h2></a></div>"#;
        let link = extract_get_link(html).expect("should find get.php link");
        assert_eq!(
            link,
            "get.php?md5=1df204c78842ffe549166ffcb984babc&key=H81SB7XVJ3G9N3GT"
        );
    }

    #[test]
    fn ignores_get_link_without_key() {
        // A bare get.php with no key is not a usable download link.
        let html = r#"<a href="get.php?md5=abc">x</a>"#;
        assert!(extract_get_link(html).is_none());
    }

    #[test]
    fn no_get_link_returns_none() {
        assert!(extract_get_link("<html>nothing here</html>").is_none());
    }

    #[test]
    fn extracts_cover_from_landing_page() {
        // A relative /covers/<bucket>/<md5>.jpg img src becomes an absolute URL
        // joined onto the mirror base.
        let html = r#"<div><img src="/covers/1000000/1df204c78842ffe549166ffcb984babc.jpg" alt="cover"></div>"#;
        assert_eq!(
            extract_cover_url(html, "https://libgen.li/"),
            Some(
                "https://libgen.li/covers/1000000/1df204c78842ffe549166ffcb984babc.jpg".to_string()
            )
        );
        // Comics use /comicscovers/.
        let comic = r#"<img src='comicscovers/123/abc.jpg'>"#;
        assert_eq!(
            extract_cover_url(comic, "https://libgen.li"),
            Some("https://libgen.li/comicscovers/123/abc.jpg".to_string())
        );
        // An already-absolute cover URL is returned verbatim.
        let abs = r#"<img src="https://libgen.li/covers/0/x.png">"#;
        assert_eq!(
            extract_cover_url(abs, "https://libgen.li"),
            Some("https://libgen.li/covers/0/x.png".to_string())
        );
    }

    #[test]
    fn no_cover_on_page_returns_none() {
        assert!(
            extract_cover_url("<html><img src=\"/logo.svg\"></html>", "https://libgen.li")
                .is_none()
        );
        // A covers path that is not an image is ignored.
        assert!(
            extract_cover_url("<a href=\"/covers/index.php\">x</a>", "https://libgen.li").is_none()
        );
    }

    #[test]
    fn libgenli_resolver_names_itself_by_host() {
        let client = Client::new();
        let r = LibgenLiResolver::new("https://libgen.li/", client);
        assert_eq!(r.name(), "libgen.li");
    }

    #[test]
    fn resolver_for_site_supports_the_family_and_routes_by_host() {
        let client = Client::new();
        for site in LIBGEN_FAMILY_SITES {
            let r = resolver_for_site(site, &client).expect("known site");
            // The resolver routes (per-host queue key) by the site's host.
            assert_eq!(r.name(), site);
        }
        // Bare and full-URL forms both resolve.
        assert!(resolver_for_site("https://libgen.vg/", &client).is_ok());
        assert!(resolver_for_site("nope.example", &client).is_err());
        // The independent libgen.pw / randombook.org family resolves too.
        assert_eq!(
            resolver_for_site("libgen.pw", &client).unwrap().name(),
            "libgen.pw"
        );
        assert_eq!(
            resolver_for_site("randombook.org", &client).unwrap().name(),
            "randombook.org"
        );
    }

    // ---- IPFS lane ----

    /// Real libgen.li `file.php?id=103990261` response (md5
    /// c8e947a9c5b9b292367b89443f941737, "Treasure Island"), synthesized.
    const IPFS_FILE_FIXTURE: &str = include_str!("../../../fixtures/ipfs/file_103990261.html");
    /// The results-table row for that md5, carrying its `file.php?id=` link.
    const IPFS_SEARCH_ROW_FIXTURE: &str = include_str!("../../../fixtures/ipfs/search_row.html");

    const TI_MD5: &str = "c8e947a9c5b9b292367b89443f941737";
    const TI_FILE_ID: &str = "103990261";
    const TI_CID: &str = "bafykbzacec7ejqgbe6uovllzwrgo2yzum324th6piemi4pozvy6b6knws6ty2";

    #[test]
    fn extracts_file_id_for_md5_from_search_row() {
        let id = extract_file_id_for_md5(IPFS_SEARCH_ROW_FIXTURE, TI_MD5)
            .expect("should find the file id in the md5's row");
        assert_eq!(id, TI_FILE_ID);
    }

    #[test]
    fn extract_file_id_for_md5_picks_the_matching_row() {
        // Two rows; only the second carries our md5. Must return the second id.
        let html = format!(
            "<table>\
             <tr><td>other</td><td><a href=\"file.php?id=111\">x</a></td>\
             <td>deadbeefdeadbeefdeadbeefdeadbeef</td></tr>\
             <tr><td>Treasure Island</td><td><a href=\"file.php?id=222\">x</a></td>\
             <td>{TI_MD5}</td></tr>\
             </table>"
        );
        assert_eq!(
            extract_file_id_for_md5(&html, TI_MD5).as_deref(),
            Some("222")
        );
    }

    #[test]
    fn extract_file_id_for_md5_none_when_md5_absent() {
        let html = "<tr><a href=\"file.php?id=999\">x</a>nomatch</tr>";
        assert!(extract_file_id_for_md5(html, TI_MD5).is_none());
    }

    #[test]
    fn extracts_ipfs_cid_from_file_page() {
        let cid = extract_ipfs_cid(IPFS_FILE_FIXTURE).expect("should find a bafy CID");
        assert_eq!(cid, TI_CID);
    }

    #[test]
    fn extract_ipfs_cid_ignores_short_non_cid_segments() {
        // A `/ipfs/` path that is not a CIDv1 must be skipped in favor of a real
        // `bafy…` CID later on the page.
        let html = format!("<a href=\"/ipfs/js\">x</a> <a href=\"https://ipfs.io/ipfs/{TI_CID}?filename=x\">dl</a>");
        assert_eq!(extract_ipfs_cid(&html).as_deref(), Some(TI_CID));
    }

    #[test]
    fn extract_ipfs_cid_none_when_absent() {
        assert!(extract_ipfs_cid("<html>no ipfs here</html>").is_none());
    }

    #[test]
    fn ipfs_resolver_names_itself_by_gateway_host() {
        let client = Client::new();
        let r = IpfsResolver::new("https://libgen.li/", "https://dweb.link/", client);
        assert_eq!(r.name(), "dweb.link");
    }

    #[test]
    fn ipfs_chain_rotates_gateways_and_rebuilds_url() {
        // The chain rewrites a base target onto each gateway in turn.
        let chain = IpfsChainResolver::new(
            "https://libgen.li",
            ["https://ipfs.io", "https://dweb.link/"],
            Client::new(),
        );
        // Drive the gateway selection deterministically without a live lookup.
        let base = DownloadTarget {
            url: format!("https://ipfs.io/ipfs/{TI_CID}"),
            host: "ipfs.io".to_string(),
            expected_md5: Some(TI_MD5.to_string()),
            total_bytes: None,
        };
        // Mirror the rewrite logic the resolver applies per gateway.
        for gw in ["https://ipfs.io", "https://dweb.link"] {
            let (_, rest) = base.url.split_once("/ipfs/").unwrap();
            let url = format!("{gw}/ipfs/{rest}");
            assert!(url.ends_with(TI_CID));
            assert_eq!(host_of(&url), host_of(gw));
        }
        // Names itself by the first gateway host.
        assert_eq!(chain.name(), "ipfs.io");
    }

    #[test]
    fn resolver_for_site_builds_ipfs_chain_named_by_gateway() {
        let client = Client::new();
        let r = resolver_for_site("ipfs", &client).expect("ipfs site is registered");
        // Names itself by the primary (first) gateway host.
        assert_eq!(r.name(), host_of(IPFS_GATEWAYS[0]));
        assert_eq!(r.name(), "ipfs.io");
        // Listed in the help/all-sites set.
        assert!(ALL_SITES.contains(&"ipfs"));
    }

    // ---- Anna's Archive lane ----

    const AA_MD5: &str = "1df204c78842ffe549166ffcb984babc";

    #[test]
    fn extracts_annas_url_from_slow_download_page() {
        // A representative slow_download success page: the signed file link is an
        // anchor whose href carries the md5's 12-char prefix on an off-site CDN.
        let html = format!(
            r#"<html><body>
               <p>If you don't want to wait, become a member.</p>
               <a href="https://momentummedia.example/d/{prefix}deadbeef/Some%20Book.epub">
                 Download now
               </a>
               <a href="/donate">Donate</a>
               </body></html>"#,
            prefix = &AA_MD5[..12]
        );
        let url = extract_annas_download_url(&html, AA_MD5, "annas-archive.gl")
            .expect("should find the signed CDN link");
        assert!(url.starts_with("https://momentummedia.example/d/"));
        assert!(url.contains(&AA_MD5[..12]));
    }

    #[test]
    fn extracts_annas_url_resolves_relative_href() {
        // A same-origin signed link (root-relative) carrying the md5 prefix is
        // absolutized against the mirror host.
        let html = format!(
            r#"<a href="/dyn/api/fast_download/{prefix}xyz/0/0">Download</a>"#,
            prefix = &AA_MD5[..12]
        );
        let url = extract_annas_download_url(&html, AA_MD5, "annas-archive.gl")
            .expect("should find the same-origin link");
        assert_eq!(
            url,
            format!(
                "https://annas-archive.gl/dyn/api/fast_download/{}xyz/0/0",
                &AA_MD5[..12]
            )
        );
    }

    #[test]
    fn annas_challenge_snippet_has_no_download_link() {
        // A Cloudflare interstitial: no usable link, and flagged as a challenge.
        let html = r#"<html><head><title>Just a moment...</title></head>
            <body><div class="cf-browser-verification">Checking your browser
            before accessing.</div></body></html>"#;
        assert!(looks_like_challenge(html));
        assert!(extract_annas_download_url(html, AA_MD5, "annas-archive.gl").is_none());
    }

    #[test]
    fn annas_ddos_guard_snippet_is_a_challenge() {
        let html = r#"<html><body>DDoS-Guard checking your browser</body></html>"#;
        assert!(looks_like_challenge(html));
    }

    #[test]
    fn annas_plain_page_is_not_a_challenge() {
        assert!(!looks_like_challenge(
            "<html><body>download ready</body></html>"
        ));
    }

    #[test]
    fn extract_annas_url_none_without_link() {
        // A waitlist interstitial with no anchors at all → None (caller: transient).
        let html = "<html><body>You are in the waitlist, please wait 30s.</body></html>";
        assert!(extract_annas_download_url(html, AA_MD5, "annas-archive.gl").is_none());
    }

    #[test]
    fn annas_resolver_names_itself_by_host() {
        let client = Client::new();
        let r = AnnaArchiveResolver::new("https://annas-archive.vg/", client);
        assert_eq!(r.name(), "annas-archive.vg");
    }

    #[test]
    fn annas_resolver_defaults_unknown_host() {
        let client = Client::new();
        let r = AnnaArchiveResolver::new("not-an-aa-host.example", client);
        assert_eq!(r.name(), AnnaArchiveResolver::DEFAULT_HOST);
    }

    #[test]
    fn is_annas_archive_host_recognizes_aliases_and_domains() {
        assert!(is_annas_archive_host("annas"));
        assert!(is_annas_archive_host("annas-archive"));
        assert!(is_annas_archive_host("annas-archive.gl"));
        assert!(is_annas_archive_host("ANNAS-ARCHIVE.PK"));
        assert!(!is_annas_archive_host("libgen.li"));
    }

    #[test]
    fn resolver_for_site_builds_annas_lanes() {
        let client = Client::new();
        // Aliases use the default mirror.
        assert_eq!(
            resolver_for_site("annas", &client).unwrap().name(),
            AnnaArchiveResolver::DEFAULT_HOST
        );
        assert_eq!(
            resolver_for_site("annas-archive", &client).unwrap().name(),
            AnnaArchiveResolver::DEFAULT_HOST
        );
        // Full AA domains route by their own host.
        for site in ANNAS_ARCHIVE_SITES {
            assert_eq!(resolver_for_site(site, &client).unwrap().name(), site);
        }
        // Full-URL form works too.
        assert_eq!(
            resolver_for_site("https://annas-archive.gl/", &client)
                .unwrap()
                .name(),
            "annas-archive.gl"
        );
        // Listed in the help/all-sites set.
        assert!(ALL_SITES.contains(&"annas-archive.gl"));
    }

    #[test]
    fn extracts_numeric_id_from_by_id_json() {
        let body = r#"{"result":{"id":"103990261","title":"Treasure Island","fileSize":7118138},"isError":false}"#;
        assert_eq!(
            first_json_string_field(body, "id").as_deref(),
            Some("103990261")
        );
        assert_eq!(
            first_json_string_field(body, "title").as_deref(),
            Some("Treasure Island")
        );
        assert_eq!(first_json_string_field(body, "missing"), None);
    }
}
