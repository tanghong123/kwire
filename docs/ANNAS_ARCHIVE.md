# Anna's Archive download lane

Anna's Archive (AA) is a meta-search over many shadow-library backends. It is a
genuinely independent download lane (different infrastructure from the libgen.li
CDN, libgen.download, and IPFS gateways), so adding it buys real failover and
parallel bandwidth. Every AA mirror serves the same files — **the md5 is the
universal id** — so the extra domains (`annas-archive.gl`, `.vg`, `.pk`, `.gd`)
are pure front-door failover against blocking, not independent backends.

The resolver lives in `crates/core/src/download.rs` as `AnnaArchiveResolver` and
is registered in `resolver_for_site` under `annas`, `annas-archive`, and any
full `annas-archive.*` domain.

## Current lane: free `slow_download` (best-effort)

`resolve(md5)` does:

1. `GET https://{host}/slow_download/{md5}/0/0` with a browser User-Agent,
   following redirects.
2. Map the result:
   - `403` / `503`, or a body that looks like a challenge ("Just a moment",
     "Verifying you are human", `cf-browser-verification`, "DDoS-Guard",
     "Checking your browser") → `DownloadError::Transient` so the
     `ResolverChain` fails over to another lane.
   - `404` → `DownloadError::Permanent` (AA has no copy for this md5).
   - Otherwise scrape the signed file URL out of the HTML (see below). Found →
     `DownloadTarget`. Not found → `Transient` (usually a waitlist/interstitial).
3. URL extraction (`extract_annas_download_url`, unit-tested without network):
   AA renders the download as an `<a href>` whose href or text carries the md5's
   **12-char prefix** — sometimes a same-origin `/...` path, sometimes a full
   off-site CDN link. We walk every anchor with `scraper` and prefer (1) an
   absolute `http(s)` href that mentions the md5 prefix (the CDN link), else (2)
   an absolute off-site `http(s)` href that isn't AA chrome
   (donate/login/social). Relative hrefs are absolutized against the mirror host.

### Why this is only best-effort

The free `slow_download` path is gated by **Cloudflare / DDoS-Guard**. A bare
`reqwest` client (no headless browser, no TLS impersonation) **often** receives a
`403`/`503` challenge interstitial instead of the download link. That's expected
and is exactly why challenge responses are classified `Transient`: the queue
simply moves on to a more reliable lane. Treat AA's slow lane as a bonus, not a
primary source.

## Robust future upgrades

### 1. (RECOMMENDED) `fast_download.json` membership API

AA exposes a clean JSON API for **paid members** that needs no browser, no
cookies, and no challenge-solving — pure `reqwest`:

```
GET https://{host}/dyn/api/fast_download.json
      ?md5=<md5>&key=<membership_key>&path_index=0&domain_index=0
```

Response shape:

```json
{
  "download_url": "https://.../<file>",
  "account_fast_download_info": { "downloads_left": 123, "downloads_per_day": 200 }
}
```

This is the most robust integration: deterministic, scriptable, no anti-bot
gate. The only requirement is a **paid AA membership key**, which should become a
future Settings field (e.g. `annas_archive_key`). When present, prefer this path
over `slow_download`; fall back to the best-effort slow lane when absent.
`path_index` / `domain_index` let the caller rotate across AA's backends/mirrors
for failover.

### 2. (OPTIONAL) FlareSolverr sidecar / TLS impersonation

To make the *free* lane reliable, defeat the Cloudflare/DDoS-Guard challenge:

- **FlareSolverr** — a headless-Chromium sidecar service. POST to its `/v1`:

  ```json
  { "cmd": "request.get", "url": "https://{host}/slow_download/{md5}/0/0" }
  ```

  It returns `solution.cookies` and `solution.userAgent`. Inject **both** the
  cookies and the exact User-Agent into the reqwest client, then re-request the
  page directly — the clearance cookie + matching UA pass the challenge.
  Reference: the MIT-licensed [`zelestcarlyone/stacks`](https://github.com/zelestcarlyone/stacks),
  which delegates AA fetching entirely to FlareSolverr.

- **TLS impersonation (no sidecar)** — use the Rust [`rquest`](https://crates.io/crates/rquest)
  crate to impersonate a real browser's TLS/HTTP2 fingerprint. This can clear
  some Cloudflare checks in-process without a separate Chromium service, though
  it is less robust than FlareSolverr against full JS challenges.

A sidecar adds an operational dependency (a running Chromium service), so the
membership API (upgrade 1) is preferred when a key is available.
