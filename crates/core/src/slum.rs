//! Live shadow-library mirror availability from **open-slum.org** (SLUM — "The
//! Shadow Library Uptime Monitor"), a public [Uptime Kuma] instance.
//!
//! Two endpoints are joined by monitor `id` (the status-page **slug is `slum`**):
//!   - `/api/status-page/slum`            → monitor metadata (`id → name, url`)
//!   - `/api/status-page/heartbeat/slum`  → `heartbeatList` (latest `status`/`ping`)
//!     and `uptimeList` (`"<id>_24"` → rolling 24h uptime ratio)
//!
//! The result is a [`SlumReport`]: per-host up/down, last ping, and 24h uptime —
//! used to discover live mirrors and to bias both the search-mirror and
//! download-resolver ordering toward sites that are actually up.
//!
//! All HTTP goes through the shared [`search::Transport`] so the exact parse path
//! replays offline from recorded fixtures in tests — mirroring `search.rs` and
//! `series.rs`. The one wrinkle: SLUM (behind Cloudflare) wants a browser-like
//! `User-Agent`, so the live transport here ([`BrowserTransport`]) sets one
//! rather than reusing `search::LiveTransport`'s descriptive UA.
//!
//! [Uptime Kuma]: https://github.com/louislam/uptime-kuma

use crate::download::host_of;
use crate::search::{RecordingTransport, ReplayTransport, Transport};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default SLUM base. The `.pages.dev` mirror serves the same Uptime Kuma API.
pub const SLUM_BASE: &str = "https://open-slum.org";
/// Current status-page slug. (The older `shadow-libraries` slug is dead; we read
/// `config.slug` from the response too, but default requests use this.)
pub const SLUM_SLUG: &str = "slum";

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// One monitored site's live availability, joined from metadata + heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlumSite {
    /// Bare host/authority (e.g. `annas-archive.gl`), derived from the URL.
    pub host: String,
    /// Full monitored URL as published by SLUM.
    pub url: String,
    /// Human-readable monitor name (e.g. `Anna's Archive GL`).
    pub name: String,
    /// SLUM group the monitor belongs to (e.g. `Anna's Archive`, `Library Genesis+`).
    pub group: String,
    /// Whether the latest heartbeat reports the site up (`status == 1`).
    pub up: bool,
    /// Latest heartbeat round-trip in milliseconds, when reported.
    pub ping_ms: Option<u32>,
    /// Rolling 24h uptime ratio (0.0–1.0), when reported.
    pub uptime_24h: Option<f64>,
}

/// A parsed SLUM snapshot: every monitored site with a real URL.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlumReport {
    pub sites: Vec<SlumSite>,
}

impl SlumReport {
    /// Persist this snapshot as JSON (so the mirror-ordering at scheduler/search
    /// build time can read live availability without a network call each time).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(self).context("serializing SLUM cache")?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))
    }

    /// Load a previously [`save`](Self::save)d snapshot, or `None` if the file is
    /// missing/unreadable/stale-JSON (caller falls back to no live data).
    pub fn load(path: impl AsRef<Path>) -> Option<Self> {
        let s = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&s).ok()
    }

    /// The monitored site whose host matches `host` (scheme/path tolerated).
    pub fn site_for_host(&self, host: &str) -> Option<&SlumSite> {
        let needle = host_of(host);
        self.sites.iter().find(|s| s.host == needle)
    }

    /// Live up/down for `host`, or `None` if SLUM doesn't monitor it.
    pub fn is_up(&self, host: &str) -> Option<bool> {
        self.site_for_host(host).map(|s| s.up)
    }

    /// Hosts matching `needle` (case-insensitive substring of either the host or
    /// the SLUM group name) that are currently up, ordered best-first by 24h
    /// uptime then lowest ping. Handy for picking a live Anna's Archive / Libgen+
    /// mirror on demand. Matching the host too (not just the group) lets a query
    /// like `"libgen"` find the `libgen.*` hosts even though their group is named
    /// "Library Genesis+".
    pub fn up_hosts_in_group(&self, needle: &str) -> Vec<String> {
        let needle = needle.to_ascii_lowercase();
        let mut up: Vec<&SlumSite> = self
            .sites
            .iter()
            .filter(|s| {
                s.up && (s.host.to_ascii_lowercase().contains(&needle)
                    || s.group.to_ascii_lowercase().contains(&needle))
            })
            .collect();
        up.sort_by(|a, b| {
            let ua = a.uptime_24h.unwrap_or(0.0);
            let ub = b.uptime_24h.unwrap_or(0.0);
            ub.partial_cmp(&ua)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.ping_ms
                        .unwrap_or(u32::MAX)
                        .cmp(&b.ping_ms.unwrap_or(u32::MAX))
                })
        });
        up.into_iter().map(|s| s.host.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// Browser-UA live transport (Cloudflare in front of SLUM dislikes odd UAs)
// ---------------------------------------------------------------------------

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Live HTTP transport with a browser-like `User-Agent`, for endpoints (SLUM,
/// Anna's Archive) that reject the engine's default descriptive UA.
pub struct BrowserTransport {
    client: reqwest::Client,
}

impl BrowserTransport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("building reqwest client");
        BrowserTransport { client }
    }
}

impl Default for BrowserTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Transport for BrowserTransport {
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

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Fetches and parses a [`SlumReport`] over a [`Transport`] (live / record / replay).
pub struct SlumClient {
    transport: Box<dyn Transport>,
    base: String,
    slug: String,
}

impl SlumClient {
    /// Build over an arbitrary transport (base/slug default to open-slum.org/`slum`).
    pub fn new(transport: Box<dyn Transport>) -> Self {
        SlumClient {
            transport,
            base: SLUM_BASE.to_string(),
            slug: SLUM_SLUG.to_string(),
        }
    }

    /// Live client with a browser User-Agent.
    pub fn live() -> Self {
        Self::new(Box::new(BrowserTransport::new()))
    }

    /// Replay client over a fixtures dir (fully offline).
    pub fn replay(fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::new(Box::new(ReplayTransport::new(fixtures_dir)))
    }

    /// Live client that records responses into `fixtures_dir`.
    pub fn recording(fixtures_dir: impl Into<PathBuf>) -> Self {
        let live: Box<dyn Transport> = Box::new(BrowserTransport::new());
        Self::new(Box::new(RecordingTransport::new(live, fixtures_dir)))
    }

    /// Override the base URL (e.g. the `.pages.dev` mirror).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    fn meta_url(&self) -> String {
        format!("{}/api/status-page/{}", self.base, self.slug)
    }

    fn heartbeat_url(&self) -> String {
        format!("{}/api/status-page/heartbeat/{}", self.base, self.slug)
    }

    /// Fetch both endpoints and join them into a [`SlumReport`].
    pub async fn fetch(&self) -> Result<SlumReport> {
        let meta = self
            .transport
            .get(&self.meta_url())
            .await
            .context("fetching SLUM status page")?;
        let hb = self
            .transport
            .get(&self.heartbeat_url())
            .await
            .context("fetching SLUM heartbeats")?;
        parse_report(&meta, &hb)
    }
}

// ---------------------------------------------------------------------------
// Parsing (pure — fully testable from recorded fixtures)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StatusPage {
    #[serde(default, rename = "publicGroupList")]
    public_group_list: Vec<PublicGroup>,
}

#[derive(Debug, Deserialize)]
struct PublicGroup {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "monitorList")]
    monitor_list: Vec<Monitor>,
}

#[derive(Debug, Deserialize)]
struct Monitor {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HeartbeatPage {
    #[serde(default, rename = "heartbeatList")]
    heartbeat_list: HashMap<String, Vec<Beat>>,
    #[serde(default, rename = "uptimeList")]
    uptime_list: HashMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct Beat {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    ping: Option<f64>,
}

/// Stringify a monitor id that may arrive as a JSON number or string.
fn id_key(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Join the status-page metadata with the heartbeat payload into a report.
/// Monitors without a real URL (rollup summaries) are skipped.
pub fn parse_report(meta_json: &str, heartbeat_json: &str) -> Result<SlumReport> {
    let page: StatusPage =
        serde_json::from_str(meta_json).context("decoding SLUM status page JSON")?;
    let hb: HeartbeatPage =
        serde_json::from_str(heartbeat_json).context("decoding SLUM heartbeat JSON")?;

    let mut sites = Vec::new();
    for group in &page.public_group_list {
        for m in &group.monitor_list {
            let url = match m.url.as_deref().map(str::trim) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => continue, // rollup summary monitor — no URL
            };
            let key = match id_key(&m.id) {
                Some(k) => k,
                None => continue,
            };

            let last = hb.heartbeat_list.get(&key).and_then(|beats| beats.last());
            let up = last.map(|b| b.status == 1).unwrap_or(false);
            let ping_ms = last.and_then(|b| b.ping).map(|p| p.round() as u32);
            let uptime_24h = hb.uptime_list.get(&format!("{key}_24")).copied();

            sites.push(SlumSite {
                host: host_of(&url),
                url,
                name: m.name.clone(),
                group: group.name.clone(),
                up,
                ping_ms,
                uptime_24h,
            });
        }
    }
    Ok(SlumReport { sites })
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"{
        "config": {"slug": "slum", "title": "SLUM"},
        "publicGroupList": [
            {"name": "Overall Health Summary", "monitorList": [
                {"id": 12, "name": "Anna's Archive", "url": null}
            ]},
            {"name": "Anna's Archive", "monitorList": [
                {"id": 52, "name": "Anna's Archive GL", "url": "https://annas-archive.gl/"},
                {"id": 53, "name": "Anna's Archive VG", "url": "https://annas-archive.vg/"}
            ]},
            {"name": "Library Genesis+ (beware of popups)", "monitorList": [
                {"id": 40, "name": "Libgen+ BZ", "url": "https://libgen.bz/"},
                {"id": 7,  "name": "Libgen+ VG", "url": "https://libgen.vg/"}
            ]}
        ]
    }"#;

    const HEARTBEAT: &str = r#"{
        "heartbeatList": {
            "52": [{"status": 0, "ping": null}, {"status": 1, "ping": 812.4}],
            "53": [{"status": 0, "ping": 1500}],
            "40": [{"status": 1, "ping": 300}],
            "7":  [{"status": 1, "ping": 5933}]
        },
        "uptimeList": {"52_24": 0.99, "53_24": 0.40, "40_24": 1.0, "7_24": 0.95}
    }"#;

    fn report() -> SlumReport {
        parse_report(META, HEARTBEAT).unwrap()
    }

    #[test]
    fn skips_rollup_monitors_without_url() {
        let r = report();
        // The summary monitor (id 12, url null) is dropped; 4 real sites remain.
        assert_eq!(r.sites.len(), 4);
        assert!(r.sites.iter().all(|s| !s.host.is_empty()));
    }

    #[test]
    fn joins_status_and_ping_from_last_beat() {
        let r = report();
        let gl = r.site_for_host("annas-archive.gl").unwrap();
        // Last beat wins: status 1 → up, ping 812.4 → 812.
        assert!(gl.up);
        assert_eq!(gl.ping_ms, Some(812));
        assert_eq!(gl.uptime_24h, Some(0.99));
        assert_eq!(gl.group, "Anna's Archive");

        let vg = r.site_for_host("annas-archive.vg").unwrap();
        assert!(!vg.up); // last (only) beat status 0
    }

    #[test]
    fn is_up_tolerates_scheme_and_path() {
        let r = report();
        assert_eq!(r.is_up("https://libgen.bz/index.php"), Some(true));
        assert_eq!(r.is_up("libgen.vg"), Some(true));
        assert_eq!(r.is_up("nonexistent.example"), None);
    }

    #[test]
    fn up_hosts_in_group_ranks_by_uptime_then_ping() {
        let r = report();
        // Only the up AA mirror (gl) qualifies; vg is down.
        assert_eq!(r.up_hosts_in_group("anna"), vec!["annas-archive.gl"]);
        // Libgen+ group: bz (uptime 1.0) ranks before vg (0.95).
        assert_eq!(
            r.up_hosts_in_group("libgen"),
            vec!["libgen.bz", "libgen.vg"]
        );
    }

    #[test]
    fn parses_integer_zero_uptime() {
        // Uptime Kuma serializes a 0 uptime as the integer 0 (not 0.0).
        let meta = r#"{"publicGroupList":[{"name":"G","monitorList":[
            {"id":99,"name":"X","url":"https://x.example/"}]}]}"#;
        let hb = r#"{"heartbeatList":{"99":[{"status":1,"ping":10}]},
            "uptimeList":{"99_24":0}}"#;
        let r = parse_report(meta, hb).unwrap();
        assert_eq!(r.sites[0].uptime_24h, Some(0.0));
    }

    #[test]
    fn parses_real_recorded_slum_response() {
        // Golden test against REAL open-slum.org responses recorded into
        // fixtures/slum/ — proves the parser tracks the live Uptime Kuma shape.
        let meta = include_str!("../../../fixtures/slum/status-page.json");
        let hb = include_str!("../../../fixtures/slum/heartbeat.json");
        let r = parse_report(meta, hb).unwrap();

        // Real data: many monitored sites, all with a non-empty host, and the
        // rollup summaries (null url) filtered out.
        assert!(r.sites.len() >= 10, "got {} sites", r.sites.len());
        assert!(r.sites.iter().all(|s| !s.host.is_empty()));

        // The Anna's Archive and Libgen+ families SLUM monitors are present.
        assert!(
            r.sites.iter().any(|s| s.host.starts_with("annas-archive.")),
            "no Anna's Archive mirror in {:?}",
            r.sites.iter().map(|s| &s.host).collect::<Vec<_>>()
        );
        assert!(r.sites.iter().any(|s| s.host.starts_with("libgen.")));

        // Every site that reports up must carry a uptime ratio in [0, 1].
        for s in &r.sites {
            if let Some(u) = s.uptime_24h {
                assert!((0.0..=1.0).contains(&u), "{} uptime {u}", s.host);
            }
        }
        // At least one site is up with a sane ping (the monitor is live).
        assert!(r.sites.iter().any(|s| s.up));
    }
}
