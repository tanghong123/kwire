# Series detection (multi-source) + cover thumbnails — design

Two features that share one newly-validated data source: **libgen.li's series pages
and cover images**.

## Part 1 — Series detection: a multi-source resolver

### Goal
From ONE book (title + author), find every book in its series, ordered, and seed a
new reading list.

### The problem with a single source
Open Library alone is insufficient:
- **Oz** → OL has a tagged `series` field → 14 members. ✓
- **Alice's Adventures in Wonderland** → OL has NO series field, and its title-prefix fallback
  fails (OL's individual volumes aren't cataloged under the prefix). ✗
- **Tom Swift** → OL stores the series name in parentheses, not
  as a prefix → unrecoverable from OL. ✗

### Validated sources (recon, 2026-06-22)
| Source | Oz | Alice | How |
|---|---|---|---|
| **OL tagged** (`work.json` → `series`) | ✓ 14 | ✗ | existing `lookup_primary` |
| **OL title-prefix** (filter box sets) | n/a | ✗ | existing fallback (recovers Uncle Wiggily) |
| **libgen `series.php`** (NEW) | n/a | ✓ **364379** | search → series link → series page |

So **libgen's own series pages** are the missing source — and they're the BEST one:
the members are guaranteed to be on libgen (directly downloadable), and the page
carries cover thumbnails too (Part 2).

### The libgen `series.php` source (new)
1. **Find the series id.** Search libgen for the book (title only — appending the
   author can surface journal-review rows whose `series.php` link points to a
   *journal*, e.g. "Boletim…", not the book series). In the result rows, the title
   cell's `<b>` block carries `<a href="series.php?id=N">Series Name</a>`. Pick the
   series link whose name best matches the request title (normalized containment),
   and that is NOT a journal/periodical (heuristic: the matching book row's title
   starts with or contains the request title).
   - Alice → id **364379** "Alice's Adventures in Wonderland" (and 364378 "… TPB").
2. **Fetch the series page** `https://libgen.li/series.php?id=N`. It's a
   `<table id="tablelibgen">` of member rows; each row has an `edition.php?id=…`
   link, a cover image `/comicscovers/…/<md5>.jpg` (+ `_small.jpg` thumb), a title
   cell, author, year. Parse per-row (proper cell parsing, NOT a flat regex — the
   table has date/author columns that a naive regex confuses for titles).
3. **Build members**: `{ title, md5 (from the cover path or the edition page),
   position (page order / year) }`, de-duped by normalized title, drop obvious
   non-members. The md5 lets us download directly; the title lets the normal search
   path find a copy if the row's md5 isn't ideal.

### The resolver (orchestration)
`SeriesClient::lookup` tries sources in order, first to yield ≥2 members wins:
1. OL tagged → 2. OL title-prefix → 3. **libgen series.php**.
(Order rationale: OL gives clean human-ordered series cheaply; libgen is the
catch-all that also guarantees downloadable members. We could promote libgen first
for comics/graphic-novel topics later.)

### Series → list (shared in core)
The projection from a resolved `Series` to a `DownloadList`, and Manual-list
creation, live in **libgen-core** so the desktop and the TUI build identical
lists: `Series::to_download_list` (`crates/core/src/series.rs`) and
`DownloadList::manual` / `MANUAL_LIST_TITLE` (`crates/core/src/model.rs`). Both
frontends call these instead of reconstructing the shape — the same
single-source-of-truth pattern as the shared `LegTracker` (`docs/LEG_LIFECYCLE.md`).

### Tests
Record fixtures for: Oz (OL, 14), Alice (libgen 364379 — search page +
series page), a non-series book (none). Assert member counts + ordering + that
journal `series.php` links are NOT followed.

### Batch validation (the deliverable the user asked for)
A CLI `libgen expand-series <list.json> --out <dir>` that runs the resolver over
every book in a list and writes, per book, the detected series + members to disk
for human review (JSON + a readable summary). Run it over **Avery's Summer Reading
Plan** and keep the results on disk.

## Part 2 — Cover thumbnails

### Source (validated)
libgen rows — both search results AND series pages — carry cover image links:
`/comicscovers/<bucket>/<md5>.jpg` (full) and `…_small.jpg` (thumbnail). Non-comic
covers live under a parallel `/covers/…` path. The md5 in the path ties the cover
to the candidate.

### Plan
1. **Parse** the cover URL (full + thumb) from each result row in
   `parse_libgen_li` → add `Candidate.cover_url: Option<String>` (+ thumb).
   `#[serde(default)]` so it's a no-op for old rows; persisted in the candidate
   JSON blob (no schema bump).
2. **Expose** it in the view model (`ViewVariation.cover_url`).
3. **Render**:
   - **Main view**: a small thumbnail (e.g. 28–36px) at the start of each book row
     (the downloaded/selected/top candidate's cover).
   - **Detail view**: a larger cover (e.g. 120px) next to the title.
   - Preferred source: the download site's cover image (remote URL). If a row has
     no cover, fall back to a format-colored placeholder (no ebook-rendering needed
     for v1; rendering an epub/pdf cover locally is a later option).
4. Lazy-load (`loading="lazy"`) and tolerate 404s (onerror → placeholder).

### Ordering
Build Part 1 (series) first — it's the user's primary ask and it exercises the same
libgen parsing. Cover parsing (`cover_url` on the candidate) can land alongside it
since both touch `parse_libgen_li`; the UI rendering is a separate, low-risk step.

## ADDENDUM — oracle-validated corrections (the methodology in action)

The naive "follow the first `series.php` link" was WRONG, caught by reading the raw
page (kept at `/tmp/series-oracle/`) instead of trusting a regex:

- A book can belong to MULTIPLE libgen series. Alice has **364379 (type: Strip)** —
  4 by-YEAR entries, NO titles — and **364378 (type: TPB)** — the actual book
  volumes with titles + volume numbers. We must pick the **book/TPB** series, not
  the strip. Heuristic: choose the series whose member rows have real TITLES and a
  volume-number column; the page's "Series type" (Strip vs blank/TPB) is a tiebreak.
- **Member row structure** (`series.php` `tablelibgen` `<tr>`, oracle-read):
  `td0` green edition link · `td1` cover `/comicscovers/<bucket>/<md5>.jpg` (+`_small`) ·
  a YEAR cell · a VOLUME-NUMBER cell · a wide (`width=20%`) TITLE cell (the
  `edition.php` anchor text) · author. Parse the WIDE title cell, the volume number,
  and the cover md5 — NOT the year cells (a flat regex grabs the years).
- **Alice ground truth (364378)**: 7+ volumes — `1 Alice's Adventures in Wonderland`,
  `2 Unicorn on a Roll`, `3 Unicorn vs. Goblins`, `4 Razzle Dazzle Unicorn`,
  `5 Unicorn Crossing`, `6 …in the Magic Storm`, `7 Unicorn of Many Hats`.

## The validation loop (standing methodology for all scrapers)
For series detection AND cover parsing AND the search-result parser:
1. **Keep the raw source** every time we fetch (HTML/JSON) — the batch CLI writes
   `<out>/raw/<slug>.html` next to the parsed result; fixtures already retain them.
2. **Oracle pass**: read the raw (rendered/understood, not regex-guessed) to state
   the GROUND TRUTH of what the page actually contains.
3. **Diff** the deterministic parser's structured output against the ground truth.
4. **Iterate** the parser where they diverge; re-run. This is how we tune fast
   instead of guessing markup.

## REVISION — three EQUAL sources + an evaluation harness (not a fixed fallback)

Don't assume a priority order — MEASURE it. Implement all three as independent,
equal sources and let the Avery batch show which is most reliable (overall and by
genre — libgen may win for comics, OL for prose).

### Source C — Goodreads (validated)
Goodreads has no API and its /search is JS-only (blocked), BUT:
- **Autocomplete JSON works**: `GET https://www.goodreads.com/book/auto_complete?format=json&q=<title>`
  → JSON array of matches with `/book/show/<id>` + title/author. Pick the best match.
- **Book page** `/book/show/<id>` → a series link `(<Series>, #N)` → `/series/<id>`.
- **Series page** `/series/<id>` is SERVER-RENDERED (titles like "Unicorn on a Roll"
  are in the static HTML, no JSON blob) → parse member titles + the "#N" order.
Goodreads gives the cleanest human-curated ORDER, but members must then be matched
to a libgen md5 to download (title search, like the OL path).

### The three sources (equal)
| | find-the-series | members | downloadable md5? | order quality |
|--|--|--|--|--|
| **OpenLibrary** | work.json `series` / title-prefix | series_key search | no (title→libgen) | good (position) |
| **libgen series.php** | book row's series link (pick TPB/book, not Strip) | series page rows | YES (cover md5 / edition) | medium (year/vol) |
| **Goodreads** | autocomplete → book → series link | series page | no (title→libgen) | BEST (#N) |

### Evaluation harness — `libgen eval-series <list.json> --out <dir>`
For EACH book, run ALL THREE sources independently and write:
- `<out>/raw/<slug>.<source>.{html,json}` — every fetched source (the oracle loop).
- `<out>/<slug>.json` — `{ title, author, per_source: { ol:{found,count,members,err},
  libgen:{...}, goodreads:{...} } }`.
- append to `<out>/comparison.tsv` — one row per book: counts per source + agreement.
- `<out>/RELIABILITY.md` — totals: how many books each source resolved, where they
  agreed/disagreed, median member count, and notable failures.
Run it over **Avery's Summer Reading Plan**; keep everything on disk for review. The
final resolver order is chosen FROM this evidence (and can be genre-aware), not
assumed.
