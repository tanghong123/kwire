#!/usr/bin/env python3
"""Deterministic generator for the offline test fixtures.

Every fixture under ``fixtures/search``, ``fixtures/ipfs`` and ``fixtures/series``
that carries *book metadata* is synthesized here from a small declarative table
of PUBLIC-DOMAIN books, so the test corpus contains no copyrighted titles. The
md5s are fixed, lowercase, 32-hex values assigned by hand (NOT derived from any
real file) purely to keep replay assertions stable. Nothing here touches the
network — run it and commit the output.

Run from the repo root:

    python3 tools/gen_fixtures.py

It regenerates:
  * fixtures/search/<slugify("<title> <authors>")>.html   (libgen.li result tables)
  * fixtures/ipfs/{file_*.html,search_row.html}            (IPFS lane pages)
  * fixtures/series/*                                        (Open Library JSON,
        libgen series.php HTML, Goodreads autocomplete/show/series pages)

The fixture keys must match `fixture_key`/`slugify` in `crates/core/src/search.rs`
and `crates/core/src/series.rs`. See those for the exact rules.
"""

from __future__ import annotations

import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SEARCH = os.path.join(ROOT, "fixtures", "search")
IPFS = os.path.join(ROOT, "fixtures", "ipfs")
SERIES = os.path.join(ROOT, "fixtures", "series")


# ---------------------------------------------------------------------------
# Slug helpers (mirror search.rs::slugify and the OL-side fixture_key)
# ---------------------------------------------------------------------------
def slugify(s: str) -> str:
    out = []
    last_dash = False
    for ch in s:
        if ch.isascii() and ch.isalnum():
            out.append(ch.lower())
            last_dash = False
        elif not last_dash and out:
            out.append("-")
            last_dash = True
    while out and out[-1] == "-":
        out.pop()
    return "".join(out) or "query"


def search_query(title: str, authors: list[str]) -> str:
    parts = [title.strip()] + [a.strip() for a in authors if a.strip()]
    return " ".join(p for p in parts if p)


def search_slug(title: str, authors: list[str]) -> str:
    return slugify(search_query(title, authors))


def ol_key(url: str) -> str:
    """OL fixture_key: slugify the whole URL (series.rs::fixture_key)."""
    return slugify(url)


def write(path: str, content: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)
    print("wrote", os.path.relpath(path, ROOT))


# ---------------------------------------------------------------------------
# libgen.li search result table
# ---------------------------------------------------------------------------
# A row is a dict: title, author, publisher, year, lang, pages, size, ext, md5,
# and an optional `fid` (file.php id, also reused as edition id), `series` (bold
# series line above the title) and `cover` (comicscovers bucket path).
def li_row(r: dict) -> str:
    fid = r.get("fid", r["md5"][:6])
    series = r.get("series", r["title"])
    cover = r.get("cover")
    cover_cell = ""
    if cover:
        cover_cell = (
            f'<a href="{cover}"><img src="{cover.replace(".jpg", "_small.jpg")}"></a>'
        )
    isbn = r.get("isbn", "9780000000000")
    title = r["title"]
    author = r["author"]
    return f"""<tr>
{f'<td>{cover_cell}</td>' if cover else ''}
<td><b>{series}</b><br><a data-toggle="tooltip" data-placement="right" data-html="true" title="Add/Edit : {r['year']}-01-01/{r['year']}-01-01; ID: {fid}<br>{author} - {title} ({r['year']}, {r['publisher']})" href="edition.php?id={fid}">{title} <i></i></a><br><a data-toggle="tooltip" data-placement="right" data-html="true" title="Add/Edit : {r['year']}-01-01/{r['year']}-01-01; ID: {fid}" href="edition.php?id={fid}"><i><font color="green"> {isbn}</font></a></i>
<nobr><span class="badge badge-primary"><a data-toggle="tooltip" data-placement="bottom" data-html="true" title="Book">b</a></span>
<span class="badge badge-secondary"">f {fid}</span></nobr>
</td>
<td>{author}</td>
<td>{r['publisher']}</td>
<td><nobr>{r['year']}</nobr></td>
<td>{r['lang']}</td>
<td>{r['pages']}</td>
<td><nobr><a href="/file.php?id={fid}">{r['size']}</a></nobr></td>
<td>{r['ext']}</td>
<td><nobr><a data-toggle="tooltip" data-placement="bottom" data-html="true" title="libgen" href="/ads.php?md5={r['md5']}"><span class="badge badge-primary">1</span></a> <a data-toggle="tooltip" data-placement="bottom" data-html="true" title="anna's archive" href="https://en.annas-archive.gl/md5/{r['md5']}"><span class="badge badge-primary">2</span></a> </nobr></td>
</tr>"""


def li_page(req: str, rows: list[dict], raw_rows: list[str] | None = None) -> str:
    body = "".join(li_row(r) for r in rows)
    if raw_rows:
        body += "".join(raw_rows)
    return f"""<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head>
\t<meta http-equiv="Content-Type" content="text/html; charset=utf-8" />
\t<title>Library Genesis</title>
</head>
<body>
<form class="card p-2 needs-validation" id="formlibgen" action="index.php" METHOD="GET">
\t<input type="text" class="form-control" name="req" value="{req}">
</form>
<table class="table  table-striped" id="tablelibgen"><thead><tr>
<th scope="col" class="first_col"><nobr>ID Time add. Title Series</nobr></th>
<th scope="col"><nobr>Author(s)</nobr></th>
<th scope="col"><nobr>Publisher</nobr></th>
<th scope="col"><nobr>Year</nobr></th>
<th scope="col">Language</th>
<th scope="col">Pages</th>
<th scope="col"><nobr>Size</nobr></th>
<th scope="col"><nobr>Ext.</nobr></th>
<th scope="col">Mirrors</th>
</tr></thead><tbody>{body}</tbody></table>
</body>
</html>
"""


# A "journal article" row: <b>JOURNAL <a edition issue-marker></a></b> then the
# real article title as a separate edition.php anchor (exercises the title-cell
# extraction that must skip the issue marker). Mirrors the journal-article regression row.
def li_journal_row(journal: str, issue: str, article_title: str, md5: str, fid: str) -> str:
    return f"""<tr>
<td><b><a href="series.php?id=3116">{journal} </a><a data-toggle="tooltip" data-placement="right" data-html="true" title="" href="edition.php?id={fid}"><i> {issue}</i></a> pp.173—173</b><br><a data-toggle="tooltip" data-placement="right" data-html="true" title="" href="edition.php?id={fid}">{article_title} <i></i></a><br><a data-toggle="tooltip" data-placement="right" data-html="true" title="" href="edition.php?id={fid}"><i><font color="green">DOI: 10.1000/x</font></i></a></td>
<td>Spisak, April</td>
<td>Sociedade</td>
<td><nobr>2021</nobr></td>
<td>English</td>
<td>1</td>
<td><nobr><a href="/file.php?id={fid}">300 kB</a></nobr></td>
<td>pdf</td>
<td><nobr><a data-toggle="tooltip" title="libgen" href="/ads.php?md5={md5}"><span class="badge badge-primary">1</span></a></nobr></td>
</tr>"""


# ---------------------------------------------------------------------------
# Book corpus (PUBLIC DOMAIN). Each entry knows the fixtures it owns.
# ---------------------------------------------------------------------------
def std_rows(title, author, pub, year, isbn13, base_md5, extra_titles=None):
    """A standard multi-row result table for one book: epub first (preferred,
    auto-match winner), then pdf/mobi copies, then a few distinct sibling rows so
    the table is realistic (>=10 rows for the workhorse). `base_md5` seeds the
    fixed md5s. Returns a list of row dicts."""
    md = base_md5
    rows = [
        dict(title=title, author=author, publisher=pub, year=year, lang="English",
             pages="0 / 272", size="3 MB", ext="epub", md5=md + "0", isbn=isbn13, fid=md[:6] + "01"),
        dict(title=title, author=author, publisher=pub, year=year + 1, lang="English",
             pages="0 / 280", size="6 MB", ext="pdf", md5=md + "1", isbn=isbn13, fid=md[:6] + "02"),
        dict(title=title, author=author, publisher=pub, year=year + 2, lang="English",
             pages="0 / 272", size="2 MB", ext="mobi", md5=md + "2", isbn=isbn13, fid=md[:6] + "03"),
    ]
    return rows


# Pad an md5 stem (31 hex) — we keep stems 31 chars and append a 1-hex suffix.
def stem(s: str) -> str:
    assert len(s) == 31 and all(c in "0123456789abcdef" for c in s), s
    return s


# ---- The workhorse: Treasure Island ---------------------------------------
TI_AUTHOR = "Robert Louis Stevenson"
TI_TITLE = "Treasure Island"
TI_ISBN13 = "9781402714672"  # arbitrary placeholder ISBN-13 (not a real edition)
# 12 rows: a healthy table. md5s are fixed 32-hex.
TI_ROWS = [
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Cassell", year=1883,
         lang="English", pages="0 / 292", size="3 MB", ext="epub",
         md5="11aa22bb33cc44dd55ee66ff00112201", isbn=TI_ISBN13, fid="7100001"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Scribner", year=1911,
         lang="English", pages="128 / 128", size="6 MB", ext="pdf",
         md5="11aa22bb33cc44dd55ee66ff00112202", isbn=TI_ISBN13, fid="7100002"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Roberts Bros", year=1884,
         lang="English", pages="77", size="2 MB", ext="mobi",
         md5="11aa22bb33cc44dd55ee66ff00112203", isbn=TI_ISBN13, fid="7100003"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Penguin", year=1994,
         lang="English", pages="0 / 311", size="4 MB", ext="azw3",
         md5="11aa22bb33cc44dd55ee66ff00112204", isbn=TI_ISBN13, fid="7100004"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Dover", year=1990,
         lang="English", pages="0 / 200", size="5 MB", ext="djvu",
         md5="11aa22bb33cc44dd55ee66ff00112205", isbn=TI_ISBN13, fid="7100005"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Macmillan", year=1913,
         lang="English", pages="0 / 273", size="7 MB", ext="epub",
         md5="11aa22bb33cc44dd55ee66ff00112206", isbn=TI_ISBN13, fid="7100006"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Harper", year=1924,
         lang="English", pages="0 / 250", size="3 MB", ext="pdf",
         md5="11aa22bb33cc44dd55ee66ff00112207", isbn=TI_ISBN13, fid="7100007"),
    dict(title="Treasure Island Illustrated", author=TI_AUTHOR, publisher="Sterling", year=2004,
         lang="English", pages="0 / 320", size="9 MB", ext="epub",
         md5="11aa22bb33cc44dd55ee66ff00112208", isbn=TI_ISBN13, fid="7100008"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Bantam", year=1981,
         lang="English", pages="0 / 240", size="2 MB", ext="fb2",
         md5="11aa22bb33cc44dd55ee66ff00112209", isbn=TI_ISBN13, fid="7100009"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Signet", year=1965,
         lang="English", pages="0 / 232", size="1.5 MB", ext="txt",
         md5="11aa22bb33cc44dd55ee66ff0011220a", isbn=TI_ISBN13, fid="7100010"),
    dict(title="Treasure Island (Annotated)", author=TI_AUTHOR, publisher="Norton", year=2011,
         lang="English", pages="0 / 400", size="11 MB", ext="pdf",
         md5="11aa22bb33cc44dd55ee66ff0011220b", isbn=TI_ISBN13, fid="7100011"),
    dict(title="Treasure Island", author=TI_AUTHOR, publisher="Everyman", year=1992,
         lang="English", pages="0 / 260", size="3 MB", ext="epub",
         md5="11aa22bb33cc44dd55ee66ff0011220c", isbn=TI_ISBN13, fid="7100012"),
]

# ---- Tom Sawyer — auto-match epub ------------------------------------------
TS_AUTHOR = "Mark Twain"
TS_TITLE = "The Adventures of Tom Sawyer"
TS_ROWS = [
    dict(title=TS_TITLE, author=TS_AUTHOR, publisher="American Publishing", year=1876,
         lang="English", pages="0 / 274", size="3 MB", ext="epub",
         md5="22bb33cc44dd55ee66ff00112233aa01", isbn="9781593082000", fid="7200001"),
    dict(title=TS_TITLE, author=TS_AUTHOR, publisher="Harper", year=1903,
         lang="English", pages="0 / 292", size="6 MB", ext="pdf",
         md5="22bb33cc44dd55ee66ff00112233aa02", isbn="9781593082000", fid="7200002"),
    dict(title=TS_TITLE, author=TS_AUTHOR, publisher="Penguin", year=1986,
         lang="English", pages="0 / 220", size="2 MB", ext="mobi",
         md5="22bb33cc44dd55ee66ff00112233aa03", isbn="9781593082000", fid="7200003"),
]

# ---- Anne of Green Gables — standalone, NOT a series -----------------------
AGG_AUTHOR = "L. M. Montgomery"
AGG_TITLE = "Anne of Green Gables"
AGG_ROWS = [
    dict(title=AGG_TITLE, author=AGG_AUTHOR, publisher="L.C. Page", year=1908,
         lang="English", pages="0 / 429", size="3 MB", ext="epub",
         md5="33cc44dd55ee66ff00112233aabb0c01", isbn="9780553213133", fid="7300001"),
    dict(title=AGG_TITLE, author=AGG_AUTHOR, publisher="Bantam", year=1976,
         lang="English", pages="0 / 320", size="5 MB", ext="pdf",
         md5="33cc44dd55ee66ff00112233aabb0c02", isbn="9780553213133", fid="7300002"),
    dict(title="Anne of Avonlea", author=AGG_AUTHOR, publisher="L.C. Page", year=1909,
         lang="English", pages="0 / 367", size="3 MB", ext="mobi",
         md5="33cc44dd55ee66ff00112233aabb0c03", isbn="9780553213140", fid="7300003"),
]

# ---- The Time Machine — subtitle + author-in-title + journal row -
TM_AUTHOR = "H. G. Wells"
TM_TITLE = "The Time Machine: An Invention"
TM_ARTICLE_MD5 = "2c3befd4b6991715ba78cc748879c2d8"  # kept: the journal row md5
JOURNAL = "Boletim da Sociedade Brasileira de Matemática"
TM_ARTICLE_TITLE = "The Time Machine: An Invention by H. G. Wells"


def time_machine_strict():
    # strict "<title> <full author>" → only the journal-article row (its title
    # cell stacks the issue marker in <b>, then the real article title).
    return li_page(
        "The Time Machine: An Invention H. G. Wells",
        [],
        raw_rows=[li_journal_row(JOURNAL, "vol. 69 iss. 3", TM_ARTICLE_TITLE, TM_ARTICLE_MD5, "86870911")],
    )


def time_machine_loose():
    # widened "The Time Machine Wells" → foreign editions + the journal rows.
    rows = [
        dict(title="The Time Machine - 02 - La Machine à explorer le temps", author=TM_AUTHOR,
             publisher="Milan", year=2016, lang="French", pages="0 / 96", size="20 MB", ext="cbz",
             md5="846cd019a019d3c0fce8877adaeaf643", isbn="9780000000001", fid="7400001",
             series="The Time Machine"),
        dict(title="The Time Machine - Die Zeitmaschine", author=TM_AUTHOR,
             publisher="Diogenes", year=2016, lang="German", pages="0 / 120", size="5 MB", ext="epub",
             md5="c56648dfbb184984caa5ce9e2616d7a4", isbn="9780000000002", fid="7400002",
             series="The Time Machine"),
    ]
    raw = [
        li_journal_row(JOURNAL, "vol. 70 iss. 5", "The Time Machine: An Invention by H. G. Wells",
                       "b2be9ca3d531a7e00f9a0829a133e273", "86341916"),
        li_journal_row(JOURNAL, "vol. 69 iss. 3", TM_ARTICLE_TITLE, TM_ARTICLE_MD5, "86870911"),
        li_journal_row(JOURNAL, "vol. 69 iss. 4", "The Time Machine: An Invention by H. G. Wells",
                       "a9aa2cd94680f6613e151d3f7655caef", "86929216"),
    ]
    return li_page("The Time Machine Wells", rows, raw_rows=raw)


# ---- Heidi — NeedsSelection ---------------------------
# The request title is fully contained in the candidate title (so it's plainly
# the same book → carries a candidate), but the author FIELD is the translator,
# not the requested author "Johanna Spyri" (and the requested author does not
# appear in the title). That caps confidence below the auto band → NeedsSelection
# with a candidate to act on. Mirrors the original NeedsSelection shape.
HEIDI_AUTHOR = "Johanna Spyri"
HEIDI_TITLE = "Heidi: Her Years of Wandering and Learning"
HEIDI_ROWS = [
    dict(title="Heidi: Her Years of Wandering and Learning (a free translation)",
         author="Louise Brooks", publisher="DeWolfe",
         year=1885, lang="English", pages="0 / 280", size="3 MB", ext="epub",
         md5="44dd55ee66ff00112233aabbccdd0001", isbn="9780553213430", fid="7500001"),
    dict(title="Heidi: Her Years of Wandering and Learning (school reader)",
         author="Helen B. Dole", publisher="Ginn",
         year=1899, lang="English", pages="0 / 250", size="4 MB", ext="pdf",
         md5="44dd55ee66ff00112233aabbccdd0002", isbn="9780553213447", fid="7500002"),
]

# ---- The Wind in the Willows — NeedsSelection -----
# Same shape: request title contained in candidate title, but the author field is
# an illustrator/adapter (request author "Kenneth Grahame" absent) → capped
# confidence → NeedsSelection with candidates.
WW_AUTHOR = "Kenneth Grahame"
WW_TITLE = "The Wind in the Willows"
WW_ROWS = [
    dict(title="The Wind in the Willows: Illustrated Riverbank Edition",
         author="Ernest H. Shepard", publisher="Methuen",
         year=1908, lang="English", pages="0 / 302", size="5 MB", ext="pdf",
         md5="77eac0c02eb33d18431d529c871f410e", isbn="9780143039099", fid="7600001"),
    dict(title="The Wind in the Willows: A Reader's Adaptation",
         author="A. A. Milne", publisher="Scribner",
         year=1929, lang="English", pages="0 / 96", size="2 MB", ext="epub",
         md5="77eac0c02eb33d18431d529c871f410f", isbn="9780143039100", fid="7600002"),
]

# ---- The Jungle Book — Matches -------------------------
JB_AUTHOR = "Rudyard Kipling"
JB_TITLE = "The Jungle Book"
JB_ROWS = [
    dict(title="The Jungle Book", author=JB_AUTHOR, publisher="Macmillan", year=1894,
         lang="English", pages="0 / 212", size="4 MB", ext="epub",
         md5="65e30974c8b20b7dd303b6923bd7d3aa", isbn="9780486410241", fid="7700001"),
    dict(title="The Jungle Book", author=JB_AUTHOR, publisher="Century", year=1895,
         lang="English", pages="0 / 303", size="6 MB", ext="pdf",
         md5="6a42adaa6c26caafb9b011708fa138a0", isbn="9780486410241", fid="7700002"),
    dict(title="The Second Jungle Book", author=JB_AUTHOR, publisher="Macmillan", year=1895,
         lang="English", pages="0 / 256", size="5 MB", ext="mobi",
         md5="730cfa7980300f18e33826dfe04dc0b8", isbn="9780486411927", fid="7700003"),
]

# ---- The Wonderful Wizard of Oz — Matches + series -----------
OZ_AUTHOR = "L. Frank Baum"
OZ_TITLE = "The Wonderful Wizard of Oz"
OZ_ROWS = [
    dict(title="The Wonderful Wizard of Oz", author=OZ_AUTHOR, publisher="Geo. M. Hill", year=1900,
         lang="English", pages="0 / 259", size="6 MB", ext="epub",
         md5="0278e72ec1bb7ff1541a27f4cfbfb0aa", isbn="9780486206912", fid="7800001"),
    dict(title="The Marvelous Land of Oz", author=OZ_AUTHOR, publisher="Reilly & Britton", year=1904,
         lang="English", pages="0 / 287", size="7 MB", ext="pdf",
         md5="1be698cf735a0d5942e10a12b9c7db3a", isbn="9780486206913", fid="7800002"),
    dict(title="Ozma of Oz", author=OZ_AUTHOR, publisher="Reilly & Britton", year=1907,
         lang="English", pages="0 / 270", size="5 MB", ext="mobi",
         md5="207fcf127413ece88cbf1cb45f2d06ca", isbn="9780486206914", fid="7800003"),
]

# ---- Alice's Adventures in Wonderland — ranking + series ------
ALICE_AUTHOR = "Lewis Carroll"
ALICE_TITLE = "Alice's Adventures in Wonderland"
# The exact base title "<title> by <author>" must rank #1; a different sibling
# volume that merely contains the title tokens must not out-rank it.
ALICE_ROWS = [
    dict(title="Wonderland Revisited: Another Alice's Adventures in Wonderland", author=ALICE_AUTHOR,
         publisher="Macmillan", year=1890, lang="English", pages="0 / 200", size="4 MB", ext="epub",
         md5="138c4b83f76d63ff1e8105d503a4e84a", isbn="9781000000001", fid="7900001"),
    dict(title="Alice's Adventures in Wonderland by Lewis Carroll", author=ALICE_AUTHOR,
         publisher="Macmillan", year=1865, lang="English", pages="0 / 192", size="3 MB", ext="epub",
         md5="13a66c36a83cd36cb90c2efcc89d4247", isbn="9781000000002", fid="7900002"),
    dict(title="Through the Looking-Glass", author=ALICE_AUTHOR,
         publisher="Macmillan", year=1871, lang="English", pages="0 / 224", size="3 MB", ext="pdf",
         md5="3cc51fcce7d164efa9d34f03ed322bfe", isbn="9781000000003", fid="7900003"),
]


# ---------------------------------------------------------------------------
# Emit search fixtures
# ---------------------------------------------------------------------------
def gen_search():
    # Workhorse + auto-match books
    write(os.path.join(SEARCH, search_slug(TI_TITLE, [TI_AUTHOR]) + ".html"),
          li_page("Treasure Island Robert Louis Stevenson", TI_ROWS))
    write(os.path.join(SEARCH, search_slug(TS_TITLE, [TS_AUTHOR]) + ".html"),
          li_page("The Adventures of Tom Sawyer Mark Twain", TS_ROWS))
    write(os.path.join(SEARCH, search_slug(AGG_TITLE, [AGG_AUTHOR]) + ".html"),
          li_page("Anne of Green Gables L. M. Montgomery", AGG_ROWS))

    # Time Machine: strict (full author + surname share one fixture) + loose.
    write(os.path.join(SEARCH, "the-time-machine-an-invention-h-g-wells.html"),
          time_machine_strict())
    write(os.path.join(SEARCH, "the-time-machine-an-invention-wells.html"),
          time_machine_strict())
    write(os.path.join(SEARCH, "the-time-machine-wells.html"), time_machine_loose())

    # Heidi (NeedsSelection): full author + surname-only fixtures.
    heidi_page = li_page("Heidi Her Years of Wandering and Learning Johanna Spyri", HEIDI_ROWS)
    write(os.path.join(SEARCH, search_slug(HEIDI_TITLE, [HEIDI_AUTHOR]) + ".html"), heidi_page)
    # surname-only widening: "Heidi: Her Years... Spyri"
    write(os.path.join(SEARCH, slugify("Heidi: Her Years of Wandering and Learning Spyri") + ".html"),
          heidi_page)
    # subtitle-stripped widening: "Heidi Spyri"
    write(os.path.join(SEARCH, "heidi-spyri.html"), heidi_page)

    # The Wind in the Willows (NeedsSelection): full + surname.
    ww_page = li_page("The Wind in the Willows Kenneth Grahame", WW_ROWS)
    write(os.path.join(SEARCH, search_slug(WW_TITLE, [WW_AUTHOR]) + ".html"), ww_page)
    write(os.path.join(SEARCH, "the-wind-in-the-willows-grahame.html"), ww_page)

    # The Jungle Book (Matches).
    write(os.path.join(SEARCH, search_slug("The Jungle Book", ["Rudyard Kipling"]) + ".html"),
          li_page("The Jungle Book Rudyard Kipling", JB_ROWS))

    # Oz + Alice.
    write(os.path.join(SEARCH, search_slug(OZ_TITLE, [OZ_AUTHOR]) + ".html"),
          li_page("The Wonderful Wizard of Oz L. Frank Baum", OZ_ROWS))
    write(os.path.join(SEARCH, search_slug(ALICE_TITLE, [ALICE_AUTHOR]) + ".html"),
          li_page("Alice's Adventures in Wonderland Lewis Carroll", ALICE_ROWS))


# ---------------------------------------------------------------------------
# IPFS lane fixtures (the Treasure Island file.php page + search row)
# ---------------------------------------------------------------------------
IPFS_MD5 = "c8e947a9c5b9b292367b89443f941737"
IPFS_FID = "103990261"
IPFS_EDID = "146943579"
IPFS_CID = "bafykbzacec7ejqgbe6uovllzwrgo2yzum324th6piemi4pozvy6b6knws6ty2"
IPFS_TITLE = "Treasure Island"
IPFS_AUTHOR = "Robert Louis Stevenson"


def gen_ipfs():
    label = f"[{IPFS_TITLE}] {IPFS_TITLE}{{{IPFS_AUTHOR}}}(1883, Cassell){{{IPFS_EDID}}}"
    file_page = f"""<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><meta charset="utf-8" />
<title>LG+: {label} libgen.li.epub</title></head>
<body>
<h2>File info</h2>
<div class="container-fluid"><div class="row"><div class="col">
<div><a href="/fictioncovers/6688000/{IPFS_MD5}.jpg"><img src="/fictioncovers/6688000/{IPFS_MD5}.jpg" width=200></a></div>
<div><h4>Editions:</h4><a href="edition.php?id={IPFS_EDID}">{label}</a><br></div>
<div><h4>Mirrors: </h4>
<a title="libgen" href="/ads.php?md5={IPFS_MD5}"><span class="badge badge-primary">Libgen</span></a>
<a title="anna's archive" href="https://en.annas-archive.gl/md5/{IPFS_MD5}"><span class="badge badge-primary">Anna's archive</span></a>
<a title="Local IPFS" href="http://localhost:8080/ipfs/{IPFS_CID}?filename={label}.epub"><span class="badge badge-primary">Local IPFS</span></a>
<a title="IPFS cloudflare" href="https://cloudflare-ipfs.com/ipfs/{IPFS_CID}?filename={label}.epub"><span class="badge badge-primary">IPFS cloudflare</span></a>
<a title="IPFS.io" href="https://gateway.ipfs.io/ipfs/{IPFS_CID}?filename={label}.epub"><span class="badge badge-primary">IPFS.io</span></a>
</div>
<div><strong>Filesize:</strong> <nobr>3 MB (3118138 B)</nobr>
<p><strong>Extension:</strong> epub
<p><strong>File name:</strong> {IPFS_TITLE} - {IPFS_AUTHOR}.epub</p></div>
<div><h4>Hashes: </h4><strong>MD5: </strong>{IPFS_MD5}
<p><strong>IPFS CID: </strong>{IPFS_CID}</div>
</div></div></div>
<div class="col"><p>File ID: {IPFS_FID}</p></div>
</body></html>
"""
    write(os.path.join(IPFS, f"file_{IPFS_FID}.html"), file_page)

    search_row = f"""<table>
<tr>
<td><b>{IPFS_TITLE}</b><br><a data-toggle="tooltip" title="ID: {IPFS_FID}<br>{IPFS_TITLE} - {IPFS_AUTHOR}" href="edition.php?id={IPFS_EDID}">{IPFS_TITLE} <i></i></a><br><a href="edition.php?id={IPFS_EDID}"><i><font color="green"> 9781402714672</font></a></i>
<nobr><span class="badge badge-primary"><a title="Book">b</a></span>
<span class="badge badge-secondary"">f {IPFS_FID}</span></nobr>
</td>
<td>{IPFS_AUTHOR}</td>
<td>Cassell</td>
<td><nobr>1883</nobr></td>
<td>English</td>
<td>0 / 292</td>
<td><nobr><a href="/file.php?id={IPFS_FID}">3 MB</a></nobr></td>
<td>epub</td>
<td><a title="libgen" href="/ads.php?md5={IPFS_MD5}"><span class="badge badge-primary">1</span></a> <a title="anna's archive" href="https://en.annas-archive.gl/md5/{IPFS_MD5}"><span class="badge badge-primary">3</span></a> </td>
</tr>
</table>
"""
    write(os.path.join(IPFS, "search_row.html"), search_row)


# ---------------------------------------------------------------------------
# Anna's Archive search page. PD: The Time Machine.
# Card list keyed by a.js-vim-focus -> /md5/<32hex>. The unit test asserts a
# specific AZW3 row by md5 c24fb619df6a96ba72271622a936eaf8.
# ---------------------------------------------------------------------------
AA_MD5_AZW3 = "c24fb619df6a96ba72271622a936eaf8"


def aa_card(md5, title, author, meta):
    return f"""
<div class="flex">
  <a class="custom-a block" href="/md5/{md5}"><img src="/img/{md5}.jpg"></a>
  <div>
    <a class="js-vim-focus custom-a" href="/md5/{md5}">{title}</a>
    <a href="/search?q={slugify(author)}"><span class="icon-[mdi--user-edit]"></span>{author}</a>
    <div class="text-gray-800 font-semibold">{meta}</div>
  </div>
</div>"""


def gen_annas_archive():
    cards = []
    cards.append(aa_card(AA_MD5_AZW3, "The Time Machine", "Wells, H. G.",
                         "English [en] · AZW3 · 1.2MB · 1895 · 📕 Book (fiction)"))
    # a healthy haul of >=10 cards with varied formats/langs/years/sizes.
    extra = [
        ("a1b2c3d4e5f6708192a3b4c5d6e7f801", "The Time Machine", "Wells, H. G.",
         "English [en] · EPUB · 0.4MB · 1895 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f802", "The Time Machine", "Wells, H. G.",
         "English [en] · PDF · 5.1MB · 1924 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f803", "La Machine à explorer le temps", "Wells, H. G.",
         "French [fr] · EPUB · 0.6MB · 2016 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f804", "Die Zeitmaschine", "Wells, H. G.",
         "German [de] · MOBI · 0.5MB · 2010 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f805", "The Time Machine (Annotated)", "Wells, H. G.",
         "English [en] · PDF · 8.0MB · 2011 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f806", "The Time Machine", "Wells, H. G.",
         "English [en] · FB2 · 0.3MB · 1965 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f807", "The Time Machine", "Wells, H. G.",
         "English [en] · TXT · 0.2MB · 1895 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f808", "The Time Machine", "Wells, H. G.",
         "English [en] · EPUB · 0.5MB · 1992 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f809", "The Time Machine", "Wells, H. G.",
         "English [en] · DJVU · 6.0MB · 1990 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f80a", "The Time Machine", "Wells, H. G.",
         "English [en] · AZW3 · 0.7MB · 2004 · 📕 Book (fiction)"),
        ("a1b2c3d4e5f6708192a3b4c5d6e7f80b", "The Time Machine", "Wells, H. G.",
         "English [en] · PDF · 3.0MB · 1981 · 📕 Book (fiction)"),
    ]
    for md5, t, a, m in extra:
        cards.append(aa_card(md5, t, a, m))
    page = f"""<!DOCTYPE html><html><head><title>Anna's Archive</title></head>
<body><div id="results">{''.join(cards)}</div></body></html>
"""
    write(os.path.join(SEARCH, "annas-archive-the-time-machine.html"), page)


# ---------------------------------------------------------------------------
# Series fixtures
# ---------------------------------------------------------------------------
# Source A — Open Library. The Oz series has a 14-member shape.
OZ_SERIES_KEY = "OL329664L"
# 14 (work_id, position, title, subtitle) — real public-domain Oz volumes.
OZ_MEMBERS = [
    ("OL17610986W", 1, "The Wonderful Wizard of Oz", None),
    ("OL17623412W", 2, "The Marvelous Land of Oz", None),
    # A per-book subtitle: renders as "Ozma of Oz: The Royal Book" (Title: Subtitle).
    ("OL17798838W", 3, "Ozma of Oz", "The Royal Book"),
    ("OL17844203W", 4, "Dorothy and the Wizard in Oz", None),
    ("OL17922359W", 5, "The Road to Oz", None),
    ("OL18147862W", 6, "The Emerald City of Oz", None),
    ("OL20085413W", 7, "The Patchwork Girl of Oz", None),
    ("OL20573281W", 8, "Tik-Tok of Oz", None),
    ("OL20788144W", 9, "The Scarecrow of Oz", None),
    ("OL22017172W", 10, "Rinkitink in Oz", None),
    ("OL28801103W", 11, "The Lost Princess of Oz", None),
    ("OL37505378W", 12, "The Tin Woodman of Oz", None),
    ("OL38121567W", 13, "The Magic of Oz", None),
    ("OL44366796W", 14, "Glinda of Oz", None),
]


def ol_search_json(docs):
    import json
    return json.dumps({"numFound": len(docs), "start": 0, "numFoundExact": True,
                       "num_found": len(docs), "docs": docs})


def ol_work_json(key, title, subtitle=None, series_key=None, position=None):
    import json
    obj = {"title": title, "key": f"/works/{key}", "type": {"key": "/type/work"}}
    if subtitle:
        obj["subtitle"] = subtitle
    if series_key:
        obj["series"] = [{"series": {"key": f"/series/{series_key}"}, "position": str(position)}]
    return json.dumps(obj)


def gen_series_openlibrary():
    base = "https://openlibrary.org"

    # --- Oz: seed search, members search, 14 work JSONs.
    seed_url = (f"{base}/search.json?title={enc('The Wonderful Wizard of Oz')}"
                f"&author={enc('L. Frank Baum')}&fields=key,title,subtitle,author_name&limit=5")
    seed_docs = [{"key": "/works/OL17610986W", "title": "The Wonderful Wizard of Oz",
                  "author_name": ["L. Frank Baum"]}]
    # add a few near matches to fill 5
    for wid, _pos, t, _s in OZ_MEMBERS[1:5]:
        seed_docs.append({"key": f"/works/{wid}", "title": t, "author_name": ["L. Frank Baum"]})
    write(os.path.join(SERIES, ol_key(seed_url) + ".json"), ol_search_json(seed_docs))

    members_url = (f"{base}/search.json?q=series_key:{OZ_SERIES_KEY}"
                   f"&fields=key,title,subtitle,first_publish_year&limit=60")
    member_docs = []
    for wid, pos, t, sub in OZ_MEMBERS:
        d = {"first_publish_year": 1899 + pos, "key": f"/works/{wid}", "title": t}
        if sub:
            d["subtitle"] = sub
        member_docs.append(d)
    write(os.path.join(SERIES, ol_key(members_url) + ".json"), ol_search_json(member_docs))

    for wid, pos, t, sub in OZ_MEMBERS:
        w_url = f"{base}/works/{wid}.json"
        write(os.path.join(SERIES, ol_key(w_url) + ".json"),
              ol_work_json(wid, t, sub, OZ_SERIES_KEY, pos))

    # --- Standalone: Anne of Green Gables, NOT a series.
    agg_seed = (f"{base}/search.json?title={enc('Anne of Green Gables')}"
                f"&author={enc('L. M. Montgomery')}&fields=key,title,subtitle,author_name&limit=5")
    write(os.path.join(SERIES, ol_key(agg_seed) + ".json"),
          ol_search_json([{"key": "/works/OLANNEW", "title": "Anne of Green Gables",
                           "author_name": ["L. M. Montgomery"]}]))
    write(os.path.join(SERIES, ol_key(f"{base}/works/OLANNEW.json") + ".json"),
          ol_work_json("OLANNEW", "Anne of Green Gables"))  # no series

    # --- Untagged series recovered by title-prefix fallback.
    #     The seed title carries a ": subtitle"; series_prefix() cuts at the first
    #     ':' → "Uncle Wiggily" (≥2 words). Sibling volumes share that prefix; the
    #     drop-cases (collections / boxed sets / counts / foreign / non-prefix)
    #     must be filtered out.
    gb_title = "Uncle Wiggily: The Bedtime Stories"
    gb_author = "Howard R. Garis"
    gb_prefix = "Uncle Wiggily"
    gb_seed = (f"{base}/search.json?title={enc(gb_title)}&author={enc(gb_author)}"
               f"&fields=key,title,subtitle,author_name&limit=5")
    write(os.path.join(SERIES, ol_key(gb_seed) + ".json"), ol_search_json([]))  # empty → fallback
    gb_fallback = (f"{base}/search.json?title={enc(gb_prefix)}&author={enc(gb_author)}"
                   f"&fields=key,title,subtitle,first_publish_year&limit=40")
    gb_docs = [
        {"key": "/works/OLUW1W", "title": "Uncle Wiggily's Adventures", "first_publish_year": 1912},
        {"key": "/works/OLUW2W", "title": "Uncle Wiggily's Airship", "first_publish_year": 1915},
        {"key": "/works/OLUW3W", "title": "Uncle Wiggily and Old Mother Hubbard", "first_publish_year": 1918},
        {"key": "/works/OLUW4W", "title": "Uncle Wiggily's Travels", "first_publish_year": 1920},
        # drop-cases (collections / boxed sets / counts / foreign / non-prefix):
        {"key": "/works/OLUWC1W", "title": "Uncle Wiggily Collection", "first_publish_year": 1990},
        {"key": "/works/OLUWC2W", "title": "Uncle Wiggily Boxed Set #1-4", "first_publish_year": 1991},
        {"key": "/works/OLUWC3W", "title": "Uncle Wiggily Series Set of 20 Books", "first_publish_year": 1992},
        {"key": "/works/OLUWC4W", "title": "Uncle Wiggily 10", "first_publish_year": 1993},
        {"key": "/works/OLUWF1W", "title": "Sammy Littletail Volume 2", "first_publish_year": 1995},
        {"key": "/works/OLUWNW", "title": "The Little Book of Bedtime", "first_publish_year": 1905},
    ]
    write(os.path.join(SERIES, ol_key(gb_fallback) + ".json"), ol_search_json(gb_docs))

    # --- Standalone with subtitle, NOT a series via fallback (The Body repl.)
    body_title = "Walden: Life in the Woods"
    body_author = "Henry David Thoreau"
    body_seed = (f"{base}/search.json?title={enc(body_title)}&author={enc(body_author)}"
                 f"&fields=key,title,subtitle,author_name&limit=5")
    write(os.path.join(SERIES, ol_key(body_seed) + ".json"), ol_search_json([]))
    body_fallback = (f"{base}/search.json?title={enc('Walden')}&author={enc(body_author)}"
                     f"&fields=key,title,subtitle,first_publish_year&limit=40")
    write(os.path.join(SERIES, ol_key(body_fallback) + ".json"),
          ol_search_json([
              {"key": "/works/OLWALDENW", "title": "Walden", "first_publish_year": 1854},
              {"key": "/works/OLWALDEN2W", "title": "A Week on the Concord River",
               "first_publish_year": 1849},  # does NOT start with "walden" → dropped
          ]))

    # --- Zero members → falls back to the single book (Lonely Tome repl.)
    lonely_key = "OL999999L"
    lonely_seed = (f"{base}/search.json?title={enc('A Solitary Volume')}"
                   f"&author={enc('Sole Author')}&fields=key,title,subtitle,author_name&limit=5")
    write(os.path.join(SERIES, ol_key(lonely_seed) + ".json"),
          ol_search_json([{"key": "/works/OLLONELYW", "title": "A Solitary Volume",
                           "author_name": ["Sole Author"]}]))
    write(os.path.join(SERIES, ol_key(f"{base}/works/OLLONELYW.json") + ".json"),
          ol_work_json("OLLONELYW", "A Solitary Volume", None, lonely_key, 1))
    lonely_members = (f"{base}/search.json?q=series_key:{lonely_key}"
                      f"&fields=key,title,subtitle,first_publish_year&limit=60")
    write(os.path.join(SERIES, ol_key(lonely_members) + ".json"), ol_search_json([]))
    write(os.path.join(SERIES, ol_key(f"{base}/series/{lonely_key}") + ".html"),
          "<html><body><h1>Lonely series</h1><p>no work links here</p></body></html>")


# Source B — libgen series.php (Alice TPB vs Strip).
ALICE_LIBGEN_TPB = 364378
ALICE_LIBGEN_STRIP = 364379
ALICE_LIBGEN_MEMBERS = [
    (1, "Alice's Adventures Under Ground", "12443e069823598ee34693d632c2fbc1"),
    (2, "Alice's Adventures in Wonderland", "61c160a467b6a7f53a00d35c99c79723"),
    (3, "Through the Looking-Glass", "74f4dda26650c1b223f974631762290d"),
    (4, "The Hunting of the Snark", "f11dabf34ff75a7e9356bebea4954ceb"),
    (5, "Sylvie and Bruno", "9cc6ad0bcfca131be0ac27300222afb0"),
    (6, "Sylvie and Bruno Concluded", "7ddd2376c1c2d9ba2ed05832e9e6782f"),
    (7, "The Nursery Alice", "b3964bc5fd7210ac4e5116bd7289ae07"),
]


def libgen_series_member_row(vol, title, md5):
    cover = f"/comicscovers/1121000/{md5}.jpg"
    return f"""<tr>
<td bgcolor="green"><a href="edition.php?id={9000000 + vol}">&nbsp;</a></td>
<td wigth=50><a href="{cover}"><img src="{cover.replace('.jpg', '_small.jpg')}" style="max-height:60px;"></a></td>
<td width=4%><nobr><a href="edition.php?id={9000000 + vol}">{1864 + vol}</a><nobr></td>
<td width=4%><nobr><a href="edition.php?id={9000000 + vol}"></a><nobr></td>
<td width=4%><nobr><a href="edition.php?id={9000000 + vol}">{vol}</a><nobr></td>
<td width=4%><nobr><a href="edition.php?id={9000000 + vol}"></a><nobr></td>
<td width=4%><nobr><a href="edition.php?id={9000000 + vol}"></a><nobr></td>
<td width=20%><a href="edition.php?id={9000000 + vol}">{title}</a> <i></i></td>
<td width=20%>Lewis Carroll</td>
<td></td>
<td><nobr><nobr></td>
<td></td>
</tr>"""


def libgen_series_page(name, rows):
    return f"""<!DOCTYPE html><html><head><title>{name}</title></head><body>
<h1>{name}</h1>
<table id="tablelibgen"><thead><tr><th>h</th></tr></thead><tbody>
{''.join(rows)}
</tbody></table>
</body></html>
"""


def libgen_search_page_with_series(req, series_links):
    """A libgen /index.php search page whose result rows carry series.php links
    in their title cells (used by LibgenSeriesClient::series_ids_in_search)."""
    rows = []
    for sid, name, count in series_links:
        for _ in range(count):
            rows.append(f"""<tr>
<td><b><a href="series.php?id={sid}">{name}</a></b><br><a href="edition.php?id={sid}0">Alice's Adventures in Wonderland <i></i></a></td>
<td>Lewis Carroll</td><td>Macmillan</td><td>1865</td><td>English</td><td>0</td>
<td><nobr><a href="/file.php?id={sid}0">3 MB</a></nobr></td><td>epub</td>
<td><a href="/ads.php?md5={('%032x' % (sid))}"><span>1</span></a></td>
</tr>""")
    body = "".join(rows)
    return f"""<!DOCTYPE html><html><head><title>Library Genesis</title></head><body>
<form id="formlibgen"><input name="req" value="{req}"></form>
<table id="tablelibgen"><thead><tr><th>h</th></tr></thead><tbody>{body}</tbody></table>
</body></html>
"""


def gen_series_libgen():
    # Alice search page → 7 links to TPB (364378), 4 to Strip (364379).
    write(os.path.join(SERIES, "alice-s-adventures-in-wonderland.html"),
          libgen_search_page_with_series(
              "Alice's Adventures in Wonderland",
              [(ALICE_LIBGEN_TPB, "Alice's Adventures in Wonderland (Collected Editions)", 7),
               (ALICE_LIBGEN_STRIP, "Alice (Comic Strip Reprints)", 4)]))

    # TPB: 7 titled member rows.
    tpb_rows = [libgen_series_member_row(v, t, m) for v, t, m in ALICE_LIBGEN_MEMBERS]
    write(os.path.join(SERIES, f"https-libgen-li-series-php-id-{ALICE_LIBGEN_TPB}.html"),
          libgen_series_page("Alice's Adventures in Wonderland (Collected Editions)", tpb_rows))

    # Strip: rows with EMPTY width=20% title cells → 0 titled (loses to TPB).
    strip_rows = []
    for v in range(1, 5):
        strip_rows.append(f"""<tr>
<td bgcolor="green"><a href="edition.php?id={8000000 + v}">&nbsp;</a></td>
<td wigth=50></td>
<td width=4%><nobr><a href="edition.php?id={8000000 + v}">{1900 + v}</a><nobr></td>
<td width=4%><nobr></nobr></td>
<td width=4%><nobr><a href="edition.php?id={8000000 + v}">{v}</a><nobr></td>
<td width=4%><nobr></nobr></td>
<td width=4%><nobr></nobr></td>
<td width=20%></td>
<td width=20%></td>
<td></td>
</tr>""")
    write(os.path.join(SERIES, f"https-libgen-li-series-php-id-{ALICE_LIBGEN_STRIP}.html"),
          libgen_series_page("Alice (Comic Strip Reprints)", strip_rows))

    # Oz at libgen MUST yield None: the search page surfaces only UNRELATED
    # co-listed series (no Oz members), so the resolver rejects them.
    write(os.path.join(SERIES, "the-wonderful-wizard-of-oz.html"),
          libgen_search_page_with_series(
              "The Wonderful Wizard of Oz",
              [(355001, "Flying Machine Stories", 3),
               (355002, "Kansas Local Histories", 2)]))
    # Provide their series pages with unrelated members so the GET succeeds but
    # the relevance gate rejects them.
    for sid, nm, titles in [
        (355001, "Flying Machine Stories",
         ["Airships of the Future", "The Great Balloon Race", "Gliders and Kites"]),
        (355002, "Kansas Local Histories",
         ["A History of Sedgwick County", "Prairie Towns"]),
    ]:
        rows = [libgen_series_member_row(i + 1, t, "%032x" % (sid * 100 + i))
                for i, t in enumerate(titles)]
        write(os.path.join(SERIES, f"https-libgen-li-series-php-id-{sid}.html"),
              libgen_series_page(nm, rows))


# Source C — Goodreads (Alice).
GR_BOOK_ID = "22710140"
GR_SERIES_ID = "146183"
GR_MEMBERS = [
    "Alice's Adventures in Wonderland",
    "Through the Looking-Glass",
    "The Hunting of the Snark",
    "Sylvie and Bruno",
    "Sylvie and Bruno Concluded",
    "The Nursery Alice",
    "Alice's Adventures Under Ground",
    "Rhyme? And Reason?",
]


def gen_series_goodreads():
    import json
    # autocomplete JSON array.
    auto = [{
        "imageUrl": "https://example.invalid/cover.jpg",
        "bookId": GR_BOOK_ID, "workId": "42234106",
        "bookUrl": f"/book/show/{GR_BOOK_ID}-alices-adventures-in-wonderland",
        "rank": 1,
        "title": "Alice's Adventures in Wonderland (Alice's Adventures in Wonderland #1)",
        "bookTitleBare": "Alice's Adventures in Wonderland",
        "author": {"id": 8164, "name": "Lewis Carroll"},
    }]
    for i, t in enumerate(GR_MEMBERS[1:], start=2):
        auto.append({"bookId": str(22710140 + i), "rank": i,
                     "title": f"{t} (Alice's Adventures in Wonderland #{i})",
                     "bookTitleBare": t, "author": {"name": "Lewis Carroll"}})
    # The runtime URL url-encodes the query (apostrophe -> %27, space -> +); the
    # fixture key slugifies that encoded URL, so build the key from the encoded form.
    auto_q = enc("Alice's Adventures in Wonderland")
    auto_url = "https://www.goodreads.com/book/auto_complete?format=json&q=" + auto_q
    write(os.path.join(SERIES, ol_key(auto_url) + ".json"), json.dumps(auto))

    # book/show page → /series/<id> link.
    write(os.path.join(SERIES, f"https-www-goodreads-com-book-show-{GR_BOOK_ID}.html"),
          f"""<!DOCTYPE html><html><body>
<h1>Alice's Adventures in Wonderland</h1>
<a href="/series/{GR_SERIES_ID}-alice-s-adventures-in-wonderland">Alice's Adventures in Wonderland Series</a>
</body></html>""")

    # series page: <h1>…Series</h1> then Book N headers + itemprop="name".
    blocks = []
    for i, t in enumerate(GR_MEMBERS, start=1):
        blocks.append(f"""<h3 class="gr-h3">Book {i}</h3>
<div class="responsiveBook" itemscope itemtype="http://schema.org/Book">
<span itemprop="name">{t}</span></div>""")
    write(os.path.join(SERIES, f"https-www-goodreads-com-series-{GR_SERIES_ID}.html"),
          f"""<!DOCTYPE html><html><body>
<h1 class="gr-h1">Alice's Adventures in Wonderland Series</h1>
{''.join(blocks)}
</body></html>""")


# url_encode mirroring series.rs::url_encode (space->+, ':'->%3A, etc.)
def enc(s: str) -> str:
    out = []
    for b in s.encode("utf-8"):
        c = chr(b)
        if c == " ":
            out.append("+")
        elif c.isalnum() and b < 128 or c in "-_.~":
            out.append(c)
        else:
            out.append("%%%02X" % b)
    return "".join(out)


def main():
    gen_search()
    gen_annas_archive()
    gen_ipfs()
    gen_series_openlibrary()
    gen_series_libgen()
    gen_series_goodreads()
    print("done")


if __name__ == "__main__":
    main()
