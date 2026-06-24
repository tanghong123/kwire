# Finding: booksdl CDN edge health is per-file and time-varying

Status: **empirical**, captured live 2026-06-23 against the real libgen.li flow.

## TL;DR

The two books that fail in our app (Treasure Island, Peter Pan) are **both
downloadable end-to-end** via the normal `ads.php → get.php → cdnN.booksdl.lc`
flow. They fail in the app because the app has **no control over, and no failover
across, the individual `cdnN.booksdl.lc` edges**. Reqwest transparently follows
the mirror's 307 to whatever edge the mirror picked, and:

- A given file lives on **some** edges and 500s on others — *at the same instant*.
- Which edge is healthy is **per-file** and **changes over time**.

So when the app's single transparent redirect lands on a sick edge, the whole
book fails, even though a sibling edge would have served it immediately.

## Evidence (live curl)

Browser UA throughout:
`Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36`

### Books / md5s chosen
- **Treasure Island** (Robert Louis Stevenson) — `11111111111111111111111111111111`, pdf, 45,295,094 B.
- **Peter Pan** (J.M. Barrie) — `22222222222222222222222222222222`, epub, 79,705,603 B.

### Resolve step works on every mirror
`GET https://<mirror>/ads.php?md5=<md5>` → 200, scrape `get.php?md5=…&key=<16char>`.
All of libgen.li / .bz / .la / .gl / .vg returned a fresh key. `get.php` 307s to
`https://cdn3.booksdl.lc/get.php?md5=…&key=…` (same edge from all five mirrors —
the mirror, not us, chooses the edge).

### Edge health diverges per file (single shared key, one instant)

Treasure Island (`1111…`), `Range: bytes=0-262143`:

```
cdn1: HTTP 000 (dead)
cdn2: HTTP 500  "It was not possible to define a repository folder"
cdn3: HTTP 206  content-range: bytes 0-262143/45295094   %PDF  <-- healthy
cdn4: HTTP 307  (redirect, no bytes)
cdn5: HTTP 500  (repository folder error)
cdn6: HTTP 500  (repository folder error)
```

Peter Pan (`2222…`), `Range: bytes=0-1048575`:

```
cdn1: HTTP 000 (dead)
cdn2: HTTP 206  PK..   <-- healthy
cdn3: HTTP 500  "It was not possible to define a repository folder"
cdn4: HTTP 307
cdn5: HTTP 500
cdn6: HTTP 000
```

So **Treasure Island is on cdn3, Peter Pan is on cdn2**, and each is a hard 500 on the other's
edge. The 500 body is a CDN-origin error (`<h1>Error</h1> It was not possible to
define a repository folder…`) — the edge simply doesn't have that file's blob.

The mirror's 307 is NOT guaranteed to point at a healthy edge. Both default 307s
went to `cdn3` — which is correct for Treasure Island but a guaranteed 500 for Peter Pan.

### Full downloads succeeded (md5-verified)

- Treasure Island: served by **cdn3**, HTTP 200, `accept-ranges: bytes`, ~200 KB/s. md5
  matches `1111…`, valid `%PDF-1.6`.
- Peter Pan: served by **cdn2**, HTTP 200, **4.5 MB/s** (≈22× faster than cdn3's
  rate), md5 matches `2222…`, valid epub (mimetype + META-INF + content.opf).

### Resume from a partial works (the path that was failing)

Pulled the first 8,065,984 B of Treasure Island with key A into a `.part`, then **re-resolved
a fresh key B** and issued `Range: bytes=8065984-` on cdn3:

```
HTTP 206
content-range: bytes 8065984-45295093/45295094
```

The CDN honored the resume offset exactly with a fresh key. (The reassembled
prefix bytes hash-matched at the correct offsets; the run was cut short by a test
timeout mid-tail, not by any HTTP error.) Key takeaways for resume:

- The `key` is per-request and short-lived — **resume must re-resolve a fresh key**,
  not reuse the one from the interrupted leg. Our queue already re-resolves on
  retry, so this is satisfied as long as the resume targets a **healthy edge**.
- 206 + `Content-Range` is honored; `.part` + `bytes=N-` is the right mechanism.

## Root cause in our app

1. `crates/core/src/queue.rs::cdn_group()` lumps **all** `cdn*.booksdl.lc` (via the
   libgen.li family) into one `"booksdl"` group, and failover **skips same-group
   hosts**. The premise ("siblings fail identically") is **false at the edge
   level** — siblings are independent per-file. We deliberately avoid the one axis
   that actually recovers these books.
2. The download client follows the mirror's 307 transparently
   (`commands.rs::build_scheduler`, reqwest default redirect policy), so we always
   take *the mirror's* edge choice and never try another `cdnN`. One sick edge ⇒
   whole-book failure.
3. Net effect: a file that is 100% available (on a different edge) is reported as a
   dead download.

## What to port to the app

**Add a booksdl edge-failover lane** — treat `cdn1..cdn6.booksdl.lc` as
independent failover targets for a libgen.li-family resolve, instead of one opaque
redirect:

1. After scraping `get.php?md5=…&key=…`, **resolve the 307 once** to learn the
   `cdnN` (or just enumerate `cdn1..cdn6`). The `key` is edge-agnostic — the same
   key works on any `cdnN` host (verified: one key returned 206 on cdn3 and 500 on
   cdn2/5/6 simultaneously, i.e. the key was accepted everywhere; only blob
   presence differed).
2. On a 500 / 000 / 307-loop from an edge, **fail over to the next `cdnN`** rather
   than abandoning the book. Classify the "repository folder" 500 and bare 000 as
   transient-but-edge-specific so we rotate edges.
3. Prefer edges by observed health/throughput (cdn2 served Peter Pan at 4.5 MB/s;
   cdn3 served Treasure Island at 0.2 MB/s — edge speed varies a lot, so a quick
   probe-and-pick or hedging across edges is worthwhile).
4. Keep resume working across the rotation: re-resolve a fresh key, pick a healthy
   edge, send `Range: bytes=<.part len>-`, expect 206 + `Content-Range`.

Concretely this means **either** (a) point the downloader at an explicit
`cdnN.booksdl.lc/get.php?md5=…&key=…` URL (disabling transparent redirect for this
lane) and registering the 6 edges as independent failover hosts, **or** (b) a
small resolver that probes edges for a 206 before handing the URL to the streamer.
Option (a) integrates cleanly with the existing per-host queue + failover chain;
the per-host `cdn_group` must then put each `cdnN.booksdl.lc` in its **own** group
(or no group) so failover does not skip them.

No alternative lane (IPFS / Anna's Archive / libgen.pw) was needed — plain booksdl
works once you spread across edges. Those remain useful as deeper failover but are
not required for these two books.

## Per-edge concurrency: measured, and why we need a cap

Tested against the healthy edge for Peter Pan (`cdn2`), one md5, parallel Range
reads (live, 2026-06-23):

| Concurrency | Per-conn speed | Aggregate | TTFB | Errors |
|---|---|---|---|---|
| 1  | 1.91 MB/s        | 1.9 MB/s | 4.4 s | none |
| 6  | 0.69–1.65 MB/s   | ~6.5 MB/s | 2.3–5.2 s | none (all 206) |
| 16 | 0.14–1.32 MB/s   | ~7.3 MB/s | **up to 29 s** | none (all 206) |

Key results:

- **The edge does NOT rate-limit or 429.** Even at 16 simultaneous streams every
  request returned 206 — no 429, no 503, no resets. So the risk is **not** an
  explicit throttle/ban.
- **Aggregate stops scaling past ~6 connections** (6.5 → 7.3 MB/s from 6→16),
  while **per-stream speed collapses and TTFB explodes** (5 s → 29 s). The edge
  just shares a fixed pipe and queues new streams.

### Why this bites us specifically (congestion collapse, self-inflicted)

Our downloader (`crates/core/src/download.rs`) bounds the headers phase at
`HEADERS_TIMEOUT = 30 s` and the body at `IDLE_STALL_TIMEOUT = 45 s`. The 16-way
test produced a **29 s time-to-first-byte** — right at the 30 s headers cliff.
When we over-parallelize a single edge:

1. New streams take ~30 s just to send headers → some trip `HEADERS_TIMEOUT` and
   are classified **Transient**.
2. The queue then **retries / fails over**, opening *more* concurrent streams to
   the *same* edge (all mirrors 307 to the same `cdnN`).
3. More streams → even higher TTFB → more headers-timeouts → more retries. This
   is a congestion-collapse spiral that, from the app's vantage point, looks
   exactly like "the booksdl CDN keeps timing out."

The current per-host cap does **not** prevent this. `HostLimits.max_concurrency`
is keyed by the **resolved mirror host** (`libgen.li`, `libgen.vg`, …), but every
mirror redirects to the **same `cdnN` edge**. So the *effective* per-edge
concurrency is `max_concurrency × (mirrors in chain) × (hedge legs)` — several
streams can pile onto one edge while each per-host counter reads ≤ 2.

### Recommendation: cap concurrency per `cdnN` edge

Add a concurrency limit keyed by the **real backend edge** (`cdnN.booksdl.lc`),
not the front-door mirror:

- A small per-edge semaphore (target ~**2–4 streams per `cdnN`**, where aggregate
  throughput is near-peak and TTFB stays well under `HEADERS_TIMEOUT`). 6 was
  fine here; 16 was self-harm — keep a safety margin.
- This pairs naturally with the edge-failover lane above: once each `cdnN` is its
  own queue/host, give each its own `max_concurrency`. Spreading load **across**
  edges (cdn2 + cdn3 + …) is how you get real parallel bandwidth — not stacking
  it on one edge.
- Also consider lifting `HEADERS_TIMEOUT` modestly (e.g. 30 → 45 s) so a busy-but-
  alive edge isn't misclassified as dead — *but* the per-edge cap is the primary
  fix; a higher timeout without the cap just lets the spiral build longer.
