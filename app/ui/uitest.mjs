// Headless test harness for the index.html UI script. Extracts the inline
// <script>, runs it under a minimal DOM + Tauri stub, then drives render() with
// fixtures to catch throws (e.g. a detail-panel exception that would abort
// render() before hydrateCovers and blank all thumbnails).
import { readFileSync } from "node:fs";
import vm from "node:vm";

const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
// The real script tag is on its own line (an earlier HTML comment mentions
// "<script>" inline, so match the line-delimited tag, not the first occurrence).
const script = html.split(/\n<script>\n/)[1].split(/\n<\/script>/)[0].replace(/boot\(\);\s*$/, "");

// --- minimal DOM stub ---------------------------------------------------
function makeEl() {
  const el = {
    textContent: "", value: "", className: "", style: {},
    dataset: {}, children: [], hidden: false,
    classList: { toggle() {}, add() {}, remove() {}, contains() { return false; } },
    addEventListener() {}, removeEventListener() {}, appendChild() {}, setAttribute() {},
    querySelector() { return makeEl(); }, querySelectorAll() { return []; },
    closest() { return null }, focus() {}, click() {},
  };
  // Track innerHTML writes so a test can assert the list isn't re-rendered when its
  // markup is unchanged (the flicker guard).
  let _html = ""; el._htmlWrites = 0;
  Object.defineProperty(el, "innerHTML", {
    get() { return _html; }, set(v) { _html = v; el._htmlWrites++; }, enumerable: true,
  });
  return el;
}
const els = {};
const document = {
  getElementById(id) { return (els[id] ||= makeEl()); },
  querySelector() { return makeEl(); }, querySelectorAll() { return []; },
  createElement() { return makeEl(); }, addEventListener() {}, body: makeEl(),
};
const invokeLog = [];
const window = {
  __TAURI__: {
    core: {
      invoke(cmd, args) {
        invokeLog.push({ cmd, args });
        if (cmd === "cover_data_url") return Promise.resolve("data:image/jpeg;base64,AAAA");
        if (cmd === "library") return Promise.resolve({ lists: [], current: "" });
        return Promise.resolve(null);
      },
    },
    event: { listen() { return Promise.resolve(() => {}); } },
  },
  addEventListener() {}, matchMedia() { return { matches: false, addEventListener() {} }; },
};
const ctx = {
  window, document, console,
  setTimeout, clearTimeout, setInterval, clearInterval,
  Date, Math, JSON, Object, Array, String, Number, Boolean, RegExp, parseInt, parseFloat, isNaN,
  navigator: { platform: "MacIntel" },
};
ctx.globalThis = ctx;
vm.createContext(ctx);
vm.runInContext(script, ctx, { filename: "index.html#script" });

// --- fixtures -----------------------------------------------------------
// A book in the "Check download"/review state with a local cached cover,
// a done variation, a recommended alternative, and a history chronicle.
function reviewBook() {
  return {
    id: "bk0", list: "L1", title: "Pashmina", author: "Nidhi Chanani", seq: 1,
    priority: false, discovery: "matched", review: true,
    recommended_md5: "b".repeat(32),
    history: [
      { at_ms: 1700000000000, md5: "a".repeat(32), fmt: "epub", kind: "downloading", detail: "started on libgen.li" },
      { at_ms: 1700000005000, md5: "a".repeat(32), fmt: "epub", kind: "done", detail: "completed on libgen.li (12 MB)" },
    ],
    versions: [
      { md5: "a".repeat(32), fmt: "epub", size: 12, year: 2017, publisher: "First Second",
        title: "Pashmina", author: "Nidhi Chanani", language: "English", pages: 176,
        state: "done", progress: 100, host: "libgen.li",
        output_path: "/x/01 - Pashmina.epub",
        cover_url: "/Users/x/thumbnails/aaaa.jpg", score: 0.6,
        downloaded_bytes: null, total_bytes: null, speed_bps: null, eta_secs: null },
      { md5: "b".repeat(32), fmt: "epub", size: 13, year: 2018, publisher: "Other",
        title: "Pashmina (Special Ed)", author: "Nidhi Chanani", language: "English", pages: 180,
        state: "available", progress: 0, host: null, output_path: null,
        cover_url: "/Users/x/thumbnails/bbbb.jpg", score: 0.9,
        downloaded_bytes: null, total_bytes: null, speed_bps: null, eta_secs: null },
    ],
  };
}
function listOf(...books) {
  return { id: "L1", title: "List", subtitle: "", collapsedAll: false,
    groups: [{ name: "G1", collapsed: false, books }] };
}

let failures = 0;
function check(name, fn) {
  ctx.COVER_CACHE = {}; // isolate cover-cache state per check
  ctx.LAST_RENDER_ERROR = null;
  invokeLog.length = 0;
  try { fn(); console.log("ok   -", name); }
  catch (e) { failures++; console.log("FAIL -", name, "\n      ", e && (e.stack || e.message || e)); }
}
// Any render phase that threw is recorded by the resilience wrapper in
// LAST_RENDER_ERROR; surface it so a MASKED throw still fails the test.
function assertNoRenderThrow() {
  if (ctx.LAST_RENDER_ERROR)
    throw new Error("a render phase threw (masked by resilience): " + ctx.LAST_RENDER_ERROR);
}

// --- tests --------------------------------------------------------------
check("render() with a SELECTED review book: no phase throws + covers hydrate", () => {
  ctx.LISTS = [listOf(reviewBook())];
  ctx.CURRENT = "L1"; ctx.SELECTED = "bk0"; ctx.DETAIL_COLLAPSED = false;
  ctx.render();
  assertNoRenderThrow(); // detects the root detail-panel throw even though masked
  const askedForCover = invokeLog.some((c) => c.cmd === "cover_data_url");
  if (!askedForCover) throw new Error("hydrateCovers never ran (no cover_data_url invoke)");
});

check("bookCover returns the local cover path for the review book", () => {
  const bk = reviewBook();
  const got = ctx.bookCover(bk);
  if (got !== "/Users/x/thumbnails/aaaa.jpg") throw new Error("bookCover gave: " + got);
});

check("historyRows renders a populated chronicle without throwing", () => {
  const out = ctx.historyRows(reviewBook());
  if (!/downloading|done/.test(out)) throw new Error("history rows not rendered: " + out.slice(0, 80));
});

check("bookById is list-scoped: two lists with colliding backend ids resolve correctly", () => {
  // Backend numbers books from bk0 PER LIST, so two lists both have a "bk0".
  ctx.applyLibrary({
    current: "__all__",
    lists: [
      { id: "L1", title: "One", groups: [{ name: "G", books: [{ id: "bk0", title: "Alpha", versions: [] }] }] },
      { id: "L2", title: "Two", groups: [{ name: "G", books: [{ id: "bk0", title: "Beta", versions: [] }] }] },
    ],
  });
  var all = ctx.everyBook();
  if (all.length !== 2) throw new Error("expected 2 books, got " + all.length);
  if (all[0].id === all[1].id) throw new Error("UI ids must be unique across lists, got " + all[0].id);
  var a = all.filter(function (b) { return b.list === "L1"; })[0];
  var b = all.filter(function (b) { return b.list === "L2"; })[0];
  if (ctx.bookById(a.id).title !== "Alpha") throw new Error("L1 book resolved to wrong title");
  if (ctx.bookById(b.id).title !== "Beta") throw new Error("L2 book resolved to wrong title (the bug)");
  if (a.bid !== "bk0" || b.bid !== "bk0") throw new Error("per-list backend id (bid) must be preserved");
});

check("statusOf: active when downloading, paused otherwise, null when idle", () => {
  var dl = { versions: [{ state: "downloading" }] };
  var pa = { versions: [{ state: "paused" }] };
  var idle = { versions: [{ state: "done" }] };
  if (ctx.statusOf([dl]).kind !== "active") throw new Error("downloading → active");
  if (ctx.statusOf([pa]).kind !== "paused") throw new Error("paused → paused");
  if (ctx.statusOf([dl, pa]).kind !== "active") throw new Error("active wins over paused");
  if (ctx.statusOf([idle]) !== null) throw new Error("idle → no badge");
  if (ctx.statusBadge([dl]).indexOf("active") < 0) throw new Error("badge HTML carries the kind");
});

check("language pref: empty/None and match-title both resolve to the match-title default", () => {
  if (ctx.langValue("") !== "match-title") throw new Error("empty must map to match-title (no Any)");
  if (ctx.langValue(null) !== "match-title") throw new Error("null must map to match-title");
  if (ctx.langValue("match-title") !== "match-title") throw new Error("match-title preserved");
  if (ctx.langValue("English") !== "English") throw new Error("specific language preserved");
  if (ctx.LANGUAGES.indexOf("") !== -1) throw new Error('"Any" ("") option must be removed');
  if (ctx.LANGUAGES[0] !== "match-title") throw new Error("match-title must be the default (first) option");
});

check("i18n: t() resolves the active language, falling back en → key", () => {
  const saved = ctx.LANG;
  ctx.LANG = "zh";
  if (ctx.t("status.done") !== "已完成") throw new Error("zh lookup failed: " + ctx.t("status.done"));
  ctx.LANG = "en";
  if (ctx.t("status.done") !== "Done") throw new Error("en lookup failed: " + ctx.t("status.done"));
  if (ctx.t("no.such.key") !== "no.such.key") throw new Error("missing key must fall back to the key itself");
  ctx.LANG = saved;
});

check("i18n: en and zh catalogs have identical key sets (no untranslated chrome)", () => {
  const en = Object.keys(ctx.I18N.en).sort(), zh = Object.keys(ctx.I18N.zh).sort();
  const enOnly = en.filter((k) => !(k in ctx.I18N.zh)), zhOnly = zh.filter((k) => !(k in ctx.I18N.en));
  if (enOnly.length || zhOnly.length) throw new Error("catalog mismatch — en-only: [" + enOnly + "] zh-only: [" + zhOnly + "]");
});

check("visibleBooks returns the filtered books for keyboard nav", () => {
  ctx.LISTS = [listOf(reviewBook())]; ctx.CURRENT = "L1"; ctx.FILTER = "all"; ctx.FMT_FILTER = "";
  var vb = ctx.visibleBooks();
  if (vb.length !== 1 || vb[0].id !== "bk0")
    throw new Error("visibleBooks wrong: " + JSON.stringify(vb.map(function (b) { return b.id; })));
});

check("per-variation categories: a book's copies are counted under each state", () => {
  const doneFailed = { id: "bk0", bid: "bk0", title: "Mixed", discovery: "matched", review: false,
    versions: [
      { md5: "a".repeat(32), state: "done", fmt: "epub" },
      { md5: "b".repeat(32), state: "failed", fmt: "pdf" },
    ] };
  if (!ctx.inCategory(doneFailed, "done")) throw new Error("Done+Failed should match Done");
  if (!ctx.inCategory(doneFailed, "cantdl")) throw new Error("Done+Failed should match Cannot-download");
  if (ctx.inCategory(doneFailed, "queued")) throw new Error("Done+Failed must NOT match Queued");
  // The key new case: a Done copy AND an in-progress copy → matches BOTH Done and In-progress.
  const doneActive = { id: "bk1", bid: "bk1", title: "Both", discovery: "matched", review: false,
    versions: [
      { md5: "c".repeat(32), state: "done", fmt: "pdf" },
      { md5: "d".repeat(32), state: "downloading", fmt: "epub", speed_bps: 1000 },
    ] };
  if (!ctx.inCategory(doneActive, "done")) throw new Error("Done+downloading should match Done");
  if (!ctx.inCategory(doneActive, "active")) throw new Error("Done+downloading should match In-progress (the double-count)");
});

check("'Cannot download' counts BOTH not-found books and failed variations", () => {
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [
    { id: "L1/b0", bid: "b0", list: "L1", title: "NotFound", discovery: "not_found", versions: [] },
    { id: "L1/b1", bid: "b1", list: "L1", title: "Failed", discovery: "matched",
      versions: [{ md5: "a".repeat(32), state: "failed", fmt: "epub" }] },
  ] }] }];
  ctx.CURRENT = "L1"; ctx.FILTER = "all"; ctx.FMT_FILTER = "";
  if (!ctx.inCategory(ctx.everyBook()[0], "cantdl")) throw new Error("not-found book must match cantdl");
  ctx.renderFilters();
  var m = els["filters"].innerHTML.match(/data-filter="cantdl"[^>]*>Cannot download <span class="n">(\d+)</);
  if (!m) throw new Error("no Cannot-download chip rendered");
  if (m[1] !== "2") throw new Error("expected Cannot download = 2 (1 not-found + 1 failed), got " + m[1]);
});

check("openHistory fills the modal body from the book's chronicle", () => {
  ctx.LISTS = [listOf(reviewBook())]; ctx.CURRENT = "L1";
  ctx.openHistory("bk0");
  const body = els["histBody"].innerHTML;
  ctx.closeHistory();
  if (!/started on libgen\.li|completed on libgen\.li/.test(body))
    throw new Error("history modal not filled: " + body.slice(0, 120));
});

check("active download: a pure progress change does NOT re-write the list DOM (anti-flicker guard)", () => {
  const bk = {
    id: "bk0", list: "L1", bid: "bk0", title: "Downloading", author: "X", seq: 1,
    priority: false, discovery: "matched", review: false,
    versions: [{ md5: "a".repeat(32), fmt: "epub", state: "downloading", progress: 50,
      host: "cdn2.booksdl.lc", downloaded_bytes: 50, total_bytes: 100,
      speed_bps: 1000, eta_secs: 10, cover_url: null, output_path: null }],
  };
  ctx.LISTS = [listOf(bk)]; ctx.CURRENT = "L1"; ctx.SELECTED = null;
  ctx.renderList();
  const w1 = els["listwrap"]._htmlWrites;
  if (!w1) throw new Error("list never rendered");
  bk.versions[0].downloaded_bytes = 80; // progress advances — the list markup must NOT change
  ctx.renderList();
  const w2 = els["listwrap"]._htmlWrites;
  if (w2 !== w1) throw new Error("list DOM rebuilt on a pure progress change (flicker): " + w1 + " → " + w2);
  ctx.updateRowProgress(); // must not throw (fills bars in place)
});

check("applyLibrary preserves live in-flight progress across a DB-lagged rebuild (no connecting flicker)", () => {
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [
    { id: "L1/bk0", bid: "bk0", list: "L1", title: "Bk", versions: [
      { md5: "a".repeat(32), state: "downloading", downloaded_bytes: 5000000, total_bytes: 9000000,
        speed_bps: 800000, eta_secs: 5, progress: 55, fmt: "epub" }] }] }] }];
  // Backend rebuild with the persisted job lagging at 0 bytes (the flicker trigger).
  ctx.applyLibrary({ current: "L1", lists: [
    { id: "L1", title: "L", format_pref: [], groups: [{ name: "G", books: [
      { id: "bk0", title: "Bk", discovery: "matched", versions: [
        { md5: "a".repeat(32), state: "downloading", downloaded_bytes: 0, total_bytes: 9000000,
          speed_bps: null, eta_secs: null, progress: 0, fmt: "epub" }] }] }] }] });
  const v = ctx.everyBook()[0].versions[0];
  if ((v.downloaded_bytes || 0) === 0) throw new Error("live download bytes reset to 0 by rebuild (flicker)");
  if (v.downloaded_bytes !== 5000000) throw new Error("expected preserved 5000000, got " + v.downloaded_bytes);
});

check("active panel: a CONNECTING resume shows its partial %, not a 0% 'from scratch' bar", () => {
  // Sisters: 86% on disk, but stuck rotating edges (no booksdl host, no speed) so
  // isTransferring()=false. The bar must show 86% + "resuming", never 0%/indeterminate.
  const bk = {
    id: "bk0", list: "L1", bid: "bk0", title: "Sisters", author: "X", seq: 1,
    priority: false, discovery: "matched", review: false,
    versions: [{ md5: "a".repeat(32), fmt: "epub", state: "downloading", progress: 86,
      host: "libgen.la", downloaded_bytes: 67582075, total_bytes: 78643200,
      speed_bps: null, eta_secs: null, cover_url: null, output_path: null }],
  };
  ctx.LISTS = [listOf(bk)]; ctx.CURRENT = "L1"; ctx.SELECTED = null;
  ctx.ACTIVE_COLLAPSED = false;
  ctx.renderActivePanel();
  const html = els["apBody"].innerHTML;
  if (/width:\s*0%/.test(html)) throw new Error("connecting resume rendered a 0% bar (looks like a restart): " + html);
  if (/\bindet\b/.test(html)) throw new Error("connecting resume used the indeterminate sliver, hiding the 86% partial: " + html);
  if (!/width:\s*86%/.test(html)) throw new Error("connecting resume bar not at 86%: " + html);
  if (!/resuming 86%/.test(html)) throw new Error("connecting resume missing 'resuming 86%' label: " + html);
});

check("active panel: two books sharing one md5 render ONE row, not two (no flicker)", () => {
  // Tom Sawyer (list 1) and Huckleberry Finn (list 2) ended up pointing at the same
  // file (same md5). It's one underlying download job — the panel must show a single row.
  function dl(title, md5) {
    return { id: title, list: "L1", bid: title, title: title, author: "X", seq: 1,
      priority: false, discovery: "matched", review: false,
      versions: [{ md5: md5, fmt: "epub", state: "downloading", progress: 40,
        host: "cdn2.booksdl.lc", downloaded_bytes: 40, total_bytes: 100,
        speed_bps: 1000, eta_secs: 10, cover_url: null, output_path: null }] };
  }
  const shared = "b".repeat(32);
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false,
    books: [dl("The Adventures of Tom Sawyer", shared), dl("Adventures of Huckleberry Finn", shared)] }] }];
  ctx.CURRENT = "L1"; ctx.SELECTED = null; ctx.ACTIVE_COLLAPSED = false;
  ctx.renderActivePanel();
  const rows = (els["apBody"].innerHTML.match(/class="ap-row"/g) || []).length;
  if (rows !== 1) throw new Error("expected 1 deduped active row for the shared md5, got " + rows);
});

check("active panel: hedge legs render as separate, badged lines (primary + hedge)", () => {
  ctx.LEGS = {};
  const md5 = "a".repeat(32);
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "Sisters", author: "X", seq: 1,
    discovery: "matched", review: false,
    versions: [{ md5, fmt: "epub", state: "downloading", host: "cdn2.booksdl.lc", progress: 30,
      downloaded_bytes: 30, total_bytes: 100, speed_bps: 1000 }] };
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.CURRENT = "L1"; ctx.ACTIVE_COLLAPSED = false;
  // Primary = leg 0; a real hedge = leg 1 (distinct leg_id).
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 0, is_hedge: false, host: "cdn2.booksdl.lc", bytes_done: 30, total_bytes: 100, speed_bps: 1000 });
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 1, is_hedge: true, host: "cdn5.booksdl.lc", bytes_done: 10, total_bytes: 100, speed_bps: 800 });
  ctx.renderActivePanel();
  const html = els["apBody"].innerHTML;
  const rows = (html.match(/class="ap-row"/g) || []).length;
  if (rows !== 2) throw new Error("expected 2 leg rows (primary + hedge), got " + rows);
  if ((html.match(/hedge-badge/g) || []).length !== 1) throw new Error("expected exactly one hedge badge");
  if (!/cdn2\.booksdl\.lc/.test(html) || !/cdn5\.booksdl\.lc/.test(html)) throw new Error("both leg hosts should appear");
  // A terminal event clears the legs (race over).
  ctx.noteLeg({ kind: "done", md5 });
  if (ctx.LEGS[md5]) throw new Error("legs should be cleared on done");
  ctx.LEGS = {};
});

check("active panel: a host change within ONE leg (failover / cdn edge-rotation) stays ONE line", () => {
  ctx.LEGS = {};
  const md5 = "f".repeat(32);
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "Sisters", author: "X", seq: 1,
    discovery: "matched", review: false,
    versions: [{ md5, fmt: "pdf", state: "downloading", host: "cdn3.booksdl.lc", progress: 42,
      downloaded_bytes: 42, total_bytes: 100, speed_bps: 900 }] };
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.CURRENT = "L1"; ctx.ACTIVE_COLLAPSED = false;
  // ONE leg (leg_id 0) whose host moves cdn2→cdn3: first via failover, then a bare
  // host change (cdn edge-rotation emits no failing_over). Same leg_id → one line.
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 0, is_hedge: false, host: "cdn2.booksdl.lc", bytes_done: 40, total_bytes: 100, speed_bps: 1000 });
  ctx.noteLeg({ kind: "failing_over", md5, leg_id: 0, is_hedge: false, from_host: "cdn2.booksdl.lc" });
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 0, is_hedge: false, host: "cdn3.booksdl.lc", bytes_done: 42, total_bytes: 100, speed_bps: 900 });
  ctx.renderActivePanel();
  const html = els["apBody"].innerHTML;
  if ((html.match(/class="ap-row"/g) || []).length !== 1) throw new Error("a host change within one leg must stay ONE line, not two");
  if (/hedge-badge/.test(html)) throw new Error("a single leg changing host must NOT be badged as hedge");
  if (!/cdn3\.booksdl\.lc/.test(html)) throw new Error("the leg's current host should be shown");
  ctx.LEGS = {};
});

check("active panel: primary leg_ended promotes the surviving hedge to primary", () => {
  ctx.LEGS = {};
  const md5 = "c".repeat(32);
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "Sisters", author: "X", seq: 1,
    discovery: "matched", review: false,
    versions: [{ md5, fmt: "epub", state: "downloading", host: "cdn2.booksdl.lc", progress: 30,
      downloaded_bytes: 30, total_bytes: 100, speed_bps: 1000 }] };
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.CURRENT = "L1"; ctx.ACTIVE_COLLAPSED = false;
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 0, is_hedge: false, host: "cdn2.booksdl.lc", bytes_done: 30, total_bytes: 100, speed_bps: 1000 });
  ctx.noteLeg({ kind: "bytes", md5, leg_id: 1, is_hedge: true, host: "cdn5.booksdl.lc", bytes_done: 60, total_bytes: 100, speed_bps: 2000 });
  // The primary (leg 0) ends; leg 1 survives and must become the primary (no badge).
  ctx.noteLeg({ kind: "leg_ended", md5, leg_id: 0 });
  ctx.renderActivePanel();
  const html = els["apBody"].innerHTML;
  if ((html.match(/class="ap-row"/g) || []).length !== 1) throw new Error("after primary leg_ended, exactly one leg should remain");
  if (/hedge-badge/.test(html)) throw new Error("the promoted survivor must NOT be badged as hedge");
  if (!/cdn5\.booksdl\.lc/.test(html)) throw new Error("the surviving leg's host should be shown");
  ctx.LEGS = {};
});

check("active panel: a stalled-but-silent leg (no bytes) is kept alive, not dropped", () => {
  ctx.LEGS = {};
  const md5 = "d".repeat(32);
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "Sisters", author: "X", seq: 1,
    discovery: "matched", review: false,
    versions: [{ md5, fmt: "epub", state: "downloading", host: "cdn2.booksdl.lc", progress: 0,
      downloaded_bytes: 0, total_bytes: 100, speed_bps: 0 }] };
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.CURRENT = "L1"; ctx.ACTIVE_COLLAPSED = false;
  // A leg that only reports liveness (resolved then stalled, never bytes) must still
  // be tracked + shown — the keep-alive that the 60s TTL is just a backstop for.
  ctx.noteLeg({ kind: "resolved", md5, leg_id: 0, is_hedge: false, host: "cdn2.booksdl.lc", total_bytes: 100 });
  ctx.noteLeg({ kind: "stalled", md5, leg_id: 0, is_hedge: false, host: "cdn2.booksdl.lc", bytes_done: 0, speed_bps: 0 });
  if (!ctx.LEGS[md5] || !ctx.LEGS[md5].byLeg[0]) throw new Error("a stalled/resolved leg must be tracked (keep-alive)");
  ctx.renderActivePanel();
  if ((els["apBody"].innerHTML.match(/class="ap-row"/g) || []).length !== 1) throw new Error("a stalled-but-alive leg should render one line");
  ctx.LEGS = {};
});

check("active panel: a COLD connect (no partial) still shows the indeterminate sliver", () => {
  const bk = {
    id: "bk0", list: "L1", bid: "bk0", title: "Fresh", author: "X", seq: 1,
    priority: false, discovery: "matched", review: false,
    versions: [{ md5: "a".repeat(32), fmt: "epub", state: "downloading", progress: 0,
      host: "libgen.la", downloaded_bytes: 0, total_bytes: null,
      speed_bps: null, eta_secs: null, cover_url: null, output_path: null }],
  };
  ctx.LISTS = [listOf(bk)]; ctx.CURRENT = "L1"; ctx.SELECTED = null;
  ctx.ACTIVE_COLLAPSED = false;
  ctx.renderActivePanel();
  const html = els["apBody"].innerHTML;
  if (!/\bindet\b/.test(html)) throw new Error("cold connect lost its indeterminate sliver: " + html);
});

check("variation manager: a non-done copy (cancelled/available/failed) can be removed", () => {
  const bk = {
    id: "bk0", list: "L1", bid: "bk0", title: "The Adventures of Tom Sawyer", author: "Mark Twain", seq: 1,
    priority: false, discovery: "matched", review: false,
    versions: [
      { md5: "a".repeat(32), fmt: "epub", state: "done", title: "The Adventures of Tom Sawyer", author: "Mark Twain", progress: 100 },
      { md5: "b".repeat(32), fmt: "epub", state: "cancelled", title: "Adventures of Huckleberry Finn", author: "Mark Twain" },
      { md5: "c".repeat(32), fmt: "pdf", state: "available", title: "The Adventures of Tom Sawyer", author: "Mark Twain" },
    ],
  };
  const html = ctx.renderVariationManager(bk);
  if (!html.includes('data-remove="bk0:' + "b".repeat(32) + '"'))
    throw new Error("cancelled variation has no Remove control: " + html);
  if (!html.includes('data-remove="bk0:' + "c".repeat(32) + '"'))
    throw new Error("available variation has no Remove control");
});

check("variation manager: a downloading variation shows its .part path + Reveal", () => {
  const part = "/Users/x/Downloads/Manual/01 - A - T - aaaaaa.epub";
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "T", author: "A", seq: 1,
    priority: false, discovery: "matched", review: false, recommended_md5: null,
    versions: [{ md5: "a".repeat(32), fmt: "epub", state: "downloading", progress: 40, size: 5,
      title: "T", author: "A", publisher: "", output_path: part }] };
  const html = ctx.renderVariationManager(bk);
  if (!html.includes(part + ".part")) throw new Error("downloading variation must show its .part path: " + html);
  if (!html.includes('data-reveal="' + part + '.part"')) throw new Error("no Reveal .part affordance");
  // A done variation shows no .part line (the row's output_path is the final file).
  const done = JSON.parse(JSON.stringify(bk));
  done.versions[0].state = "done"; done.versions[0].progress = 100;
  if (/partpath/.test(ctx.renderVariationManager(done))) throw new Error("a done variation must not show a .part path");
});

check("cover preload: throttled to COVER_CONCURRENCY, rest queued", () => {
  // Reset cover state.
  ctx.COVER_CACHE = {}; ctx.COVER_INFLIGHT = {}; ctx.COVER_FAILS = {};
  ctx.COVER_QUEUE = []; ctx.COVER_ACTIVE = 0;
  // 10 books, each with a distinct LOCAL cover path.
  const books = [];
  for (let i = 0; i < 10; i++) {
    books.push({ id: "b" + i, list: "L1", bid: "b" + i, title: "Bk" + i, discovery: "matched",
      versions: [{ md5: String(i).repeat(32).slice(0, 32), fmt: "epub", state: "done",
        cover_url: "/thumbs/c" + i + ".jpg" }] });
  }
  ctx.LISTS = [{ id: "L1", title: "L", groups: [{ name: "G", collapsed: false, books: books }] }];
  ctx.CURRENT = "__all__";
  ctx.hydrateCovers();
  // Only COVER_CONCURRENCY fetches run at once; the remaining 4 wait in the queue.
  if (ctx.COVER_ACTIVE !== ctx.COVER_CONCURRENCY)
    throw new Error("expected " + ctx.COVER_CONCURRENCY + " active, got " + ctx.COVER_ACTIVE);
  if (ctx.COVER_QUEUE.length !== 10 - ctx.COVER_CONCURRENCY)
    throw new Error("expected " + (10 - ctx.COVER_CONCURRENCY) + " queued, got " + ctx.COVER_QUEUE.length);
  // Reset: in this synchronous harness the resolve callbacks (which decrement
  // COVER_ACTIVE) never run, so leave the globals clean for later tests.
  ctx.COVER_CACHE = {}; ctx.COVER_INFLIGHT = {}; ctx.COVER_FAILS = {};
  ctx.COVER_QUEUE = []; ctx.COVER_ACTIVE = 0;
});

check("cover preload: coverNeedsFetch skips remote, loaded, in-flight, and maxed-out paths", () => {
  ctx.COVER_CACHE = {}; ctx.COVER_INFLIGHT = {}; ctx.COVER_FAILS = {}; ctx.COVER_QUEUE = [];
  if (ctx.coverNeedsFetch("https://x/y.jpg")) throw new Error("remote URL must not be fetched");
  if (!ctx.coverNeedsFetch("/thumbs/fresh.jpg")) throw new Error("a fresh local path must be fetched");
  ctx.COVER_CACHE["/thumbs/loaded.jpg"] = "data:image/jpeg;base64,AAAA";
  if (ctx.coverNeedsFetch("/thumbs/loaded.jpg")) throw new Error("a loaded cover must be sticky (not re-fetched)");
  ctx.COVER_INFLIGHT["/thumbs/flying.jpg"] = true;
  if (ctx.coverNeedsFetch("/thumbs/flying.jpg")) throw new Error("an in-flight path must not be re-queued");
  ctx.COVER_FAILS["/thumbs/dead.jpg"] = ctx.COVER_MAX_FAILS;
  if (ctx.coverNeedsFetch("/thumbs/dead.jpg")) throw new Error("a maxed-out path must give up");
});

check("detail: edit form renders + onEditBook invokes edit_book with trimmed values", () => {
  const bk = { id: "bk0", bid: "bk0", list: "L1", title: "D'Aulaires' Book of Greek Myths",
    author: "Ingri d'Aulaire", discovery: "not_found", review: false, versions: [] };
  ctx.LISTS = [listOf(bk)]; ctx.SELECTED = "bk0"; ctx.CURRENT = "L1"; ctx.DETAIL_COLLAPSED = false;
  ctx.EDIT_BOOK = null; ctx.renderDetail();
  if (!els["dBody"].innerHTML.includes('data-editbook="bk0"')) throw new Error("no Edit button by default");
  ctx.EDIT_BOOK = "bk0"; ctx.renderDetail();
  const h = els["dBody"].innerHTML;
  if (!h.includes('id="editTitle"') || !h.includes('data-editsave="bk0"'))
    throw new Error("edit form not rendered when EDIT_BOOK set: " + h.slice(0, 160));
  invokeLog.length = 0;
  ctx.onEditBook(bk, "  Greek Myths  ", "D'Aulaires'");
  const call = invokeLog.find((c) => c.cmd === "edit_book");
  if (!call) throw new Error("edit_book not invoked");
  if (call.args.title !== "Greek Myths") throw new Error("title not trimmed: '" + call.args.title + "'");
  if (call.args.author !== "D'Aulaires'") throw new Error("author wrong: " + call.args.author);
  if (call.args.bookId !== "bk0") throw new Error("bookId wrong: " + call.args.bookId);
  if (ctx.EDIT_BOOK !== null) throw new Error("EDIT_BOOK should clear after save");
});

check("detail: an auto re-render does NOT rebuild the OPEN edit form (preserves typing/focus)", () => {
  const bk = { id: "bk0", bid: "bk0", list: "L1", title: "T", author: "A", discovery: "not_found", review: false, versions: [] };
  ctx.LISTS = [listOf(bk)]; ctx.SELECTED = "bk0"; ctx.CURRENT = "L1"; ctx.DETAIL_COLLAPSED = false;
  ctx.EDIT_BOOK = null; ctx.renderDetail();              // baseline (form closed)
  ctx.EDIT_BOOK = "bk0"; ctx.renderDetail();             // open the form (signature changed → rendered)
  if (!els["dBody"].innerHTML.includes('id="editTitle"')) throw new Error("edit form should be open");
  els["dBody"].innerHTML += "<!--typed-->";             // stand in for the user's in-progress edit
  ctx.renderDetail();                                    // auto re-render: same EDIT_BOOK/SELECTED/collapse → must SKIP
  if (!els["dBody"].innerHTML.includes("<!--typed-->")) throw new Error("auto re-render wiped the open edit form");
  ctx.EDIT_BOOK = null; ctx.renderDetail();              // cancel → re-renders normally
  if (els["dBody"].innerHTML.includes("<!--typed-->")) throw new Error("clearing EDIT_BOOK should re-render the detail");
});

check("detail: Remove-from-list shows ONLY for a Manual-list book (is_manual gate)", () => {
  const bk = { id: "bk0", bid: "bk0", list: "L1", title: "Some Book", author: "A",
    discovery: "not_found", review: false, versions: [] };
  ctx.SELECTED = "bk0"; ctx.CURRENT = "L1"; ctx.DETAIL_COLLAPSED = false; ctx.EDIT_BOOK = null;
  // Imported (non-manual) list → no remove-book affordance (entries are immutable).
  ctx.LISTS = [{ id: "L1", title: "Imported", is_manual: false, groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.renderDetail();
  if (els["dBody"].innerHTML.includes("data-removebook")) throw new Error("an imported-list book must NOT show Remove-from-list");
  // Manual list → remove-book affordance present.
  ctx.LISTS = [{ id: "L1", title: "Manual", is_manual: true, groups: [{ name: "G", collapsed: false, books: [bk] }] }];
  ctx.renderDetail();
  if (!els["dBody"].innerHTML.includes('data-removebook="bk0"')) throw new Error("a Manual-list book must show Remove-from-list");
});

check("detail: onEditBook rejects an empty title (no invoke)", () => {
  const bk = { id: "b1", bid: "b1", list: "L1", title: "X", author: "Y", discovery: "not_found", versions: [] };
  invokeLog.length = 0; ctx.EDIT_BOOK = "b1";
  ctx.onEditBook(bk, "   ", "Z");
  if (invokeLog.some((c) => c.cmd === "edit_book")) throw new Error("empty title must not invoke edit_book");
});

check("reorganize Details: renders the from→to pairs when REORG_DIFF is loaded", () => {
  ctx.LISTS = []; ctx.REORG_COUNT = 4;
  ctx.REORG_DIFF = [
    ["/books/Avery/06 - X.epub", "/books/Avery/Sub/06 - X.epub"],
    ["/books/Avery/old name.pdf", "/books/Avery/02 - Y - Z.pdf"],
  ];
  ctx.renderSettings();
  const h = els["setBody"] ? els["setBody"].innerHTML : "";
  // Falls back to scanning any element that got the settings HTML.
  const all = Object.keys(els).map((k) => els[k].innerHTML || "").join("\n");
  const hay = h || all;
  if (!/data-action="reorganize-details"/.test(hay)) throw new Error("no Details button rendered");
  if (!/→ /.test(hay)) throw new Error("diff arrows (→) not rendered: " + hay.slice(0, 200));
  if (!/06 - X\.epub/.test(hay)) throw new Error("diff entry not shown");
  // Toggle hides it.
  ctx.onReorganizeDetails();
  if (ctx.REORG_DIFF !== null) throw new Error("Details toggle should hide (null) when already shown");
});

check("book row: variations stack as per-variation lines (type/size/status/progress)", () => {
  const bk = { id: "bk0", list: "L1", bid: "bk0", title: "T", author: "A", seq: 1,
    discovery: "matched", review: false, versions: [
      { md5: "a".repeat(32), fmt: "epub", state: "done", size: 12 },
      { md5: "b".repeat(32), fmt: "pdf", state: "downloading", size: 40, progress: 30 },
      { md5: "c".repeat(32), fmt: "epub", state: "queued", size: 5 },
    ] };
  const html = ctx.bookRow(bk);
  const lines = (html.match(/class="vline"/g) || []).length;
  if (lines !== 3) throw new Error("expected 3 variation lines, got " + lines);
  if (!/hourglass run/.test(html)) throw new Error("downloading line should have an animated hourglass");
  if (!html.includes('data-vprog="bk0:' + "b".repeat(32) + '"'))
    throw new Error("downloading line missing data-vprog for in-place updates");
  if (!/12 MB/.test(html) || !/40 MB/.test(html)) throw new Error("per-variation sizes missing");
  if (!/✓ Done/.test(html) || !/Queued/.test(html)) throw new Error("per-variation statuses missing");
});

check("book row: a not-found book shows a single book-status line (no variation rows)", () => {
  const bk = { id: "b1", list: "L1", bid: "b1", title: "Gone", author: "X", seq: 2,
    discovery: "not_found", review: false, versions: [] };
  const html = ctx.bookRow(bk);
  if ((html.match(/class="vline/g) || []).length !== 1) throw new Error("expected one book-status line");
  if (!/Not found/.test(html)) throw new Error("not-found status missing: " + html.slice(0, 200));
});

check("'Search again' re-queries ONLY that book (per-book retry), not the whole list", () => {
  invokeLog.length = 0;
  ctx.onSearchAgain({ id: "L1/B7", bid: "B7", list: "L1", title: "Gone", author: "X", discovery: "not_found" });
  const call = invokeLog.find((c) => c.cmd === "retry");
  if (!call) throw new Error("'Search again' must invoke the per-book 'retry'");
  if (call.args.bookId !== "B7") throw new Error("retry must target THIS book: " + JSON.stringify(call.args));
  if (invokeLog.some((c) => c.cmd === "requery")) throw new Error("'Search again' must NOT trigger the per-list 'requery'");
});

check("book row: not-found cover is neutral + a low_pages variation flags ⚠", () => {
  // A not-found book that still carries a removed/wrong pdf variation must show a
  // NEUTRAL cover, never a PDF-colored placeholder.
  const nf = { id: "nf0", list: "L1", bid: "nf0", title: "Gone", author: "X", seq: 1,
    discovery: "not_found", review: false,
    versions: [{ md5: "x".repeat(32), fmt: "pdf", state: "cancelled", cover_url: "" }] };
  const ch = ctx.coverImg(nf, "cv-sm");
  if (/pdf/i.test(ch)) throw new Error("not-found cover must not carry the format: " + ch);
  if (!/—/.test(ch)) throw new Error("not-found cover should be the neutral placeholder: " + ch);
  // A done variation flagged low_pages shows the ⚠ page warning.
  const lp = { id: "lp0", list: "L1", bid: "lp0", title: "Tiny", author: "X", seq: 1,
    discovery: "matched", review: false,
    versions: [{ md5: "y".repeat(32), fmt: "pdf", state: "done", progress: 100, counted_pages: 3, low_pages: true }] };
  const vh = ctx.varLine(lp, lp.versions[0]);
  if (!/vwarn/.test(vh) || !/3p/.test(vh)) throw new Error("low_pages should show ⚠ Np: " + vh);
  // The DETAIL view (variation manager) must ALSO flag a downloaded low_pages copy.
  const dh = ctx.renderVariationManager(lp);
  if (!/vwarn/.test(dh) || !/3p/.test(dh)) throw new Error("detail view low_pages should show ⚠ Np: " + dh);
});

check("book row: meta line shows author · year (no pages), marks back-filled author/year", () => {
  const bk = { id: "m0", bid: "m0", list: "L1", title: "T", author: "A. Author", seq: 1,
    discovery: "matched", review: false, year: 2011, pages: 320, backfilled: ["authors", "year"], versions: [] };
  const html = ctx.bookRow(bk);
  if (!/2011/.test(html)) throw new Error("meta line missing year: " + html);
  if (/320p/.test(html)) throw new Error("page count must NOT appear in the list meta line (detail-only): " + html);
  if ((html.match(/class="bf"/g) || []).length !== 2) throw new Error("author+year should be marked back-filled: " + html);
  // User-provided fields (nothing in backfilled) must NOT be marked.
  const bk2 = Object.assign({}, bk, { backfilled: [] });
  if (/class="bf"/.test(ctx.bookRow(bk2))) throw new Error("user-provided fields must not carry the auto marker");
});

check("render() with a selected DONE (non-review) book also hydrates covers", () => {
  const b = reviewBook(); b.review = false; b.discovery = "matched";
  ctx.LISTS = [listOf(b)]; ctx.SELECTED = "bk0";
  invokeLog.length = 0;
  ctx.render();
  if (!invokeLog.some((c) => c.cmd === "cover_data_url")) throw new Error("no cover invoke for done book");
});

check("i18n: t(key, args) substitutes {n} placeholders", () => {
  const saved = ctx.LANG;
  ctx.LANG = "en";
  const result = ctx.t("event.matched", { n: 3, ext: "epub" });
  if (result !== "3 candidate(s) → matched (auto-selected epub)")
    throw new Error("placeholder substitution failed: " + result);
  // Missing param leaves the placeholder intact.
  const partial = ctx.t("event.matched", { n: 5 });
  if (!/\{ext\}/.test(partial)) throw new Error("missing param should leave {ext} in place: " + partial);
  ctx.LANG = saved;
});

check("i18n: decodeI18n translates a known key with no params", () => {
  const saved = ctx.LANG;
  ctx.LANG = "en";
  const result = ctx.decodeI18n("event.notfound");
  if (result !== "no candidates found")
    throw new Error("decodeI18n key-only failed: " + result);
  ctx.LANG = saved;
});

check("i18n: decodeI18n translates a known key with params (U+001F encoded)", () => {
  const saved = ctx.LANG;
  ctx.LANG = "en";
  const US = "";
  const encoded = "event.done" + US + "host=libgen.li" + US + "mb=12";
  const result = ctx.decodeI18n(encoded);
  if (result !== "completed on libgen.li (12 MB)")
    throw new Error("decodeI18n param substitution failed: " + result);
  ctx.LANG = saved;
});

check("i18n: decodeI18n returns plain English string unchanged", () => {
  const raw = "started on libgen.li";
  const result = ctx.decodeI18n(raw);
  if (result !== raw) throw new Error("decodeI18n must pass through unknown strings unchanged: " + result);
});

check("i18n: decodeI18n works in zh locale", () => {
  const saved = ctx.LANG;
  ctx.LANG = "zh";
  const result = ctx.decodeI18n("event.notfound");
  if (result !== "未找到候选项")
    throw new Error("decodeI18n zh failed: " + result);
  ctx.LANG = saved;
});

// --- REAL-DATA sweep: drive every book's detail panel with the actual ViewLibrary
// dumped from the DB (app/src-tauri/examples/dump_vm.rs), catching any render-phase
// throw that would blank thumbnails. This is the test that mirrors what the user sees.
import { existsSync } from "node:fs";
const realPath = "/tmp/real_vm.json";
if (existsSync(realPath)) {
  const real = JSON.parse(readFileSync(realPath, "utf8"));
  check("applyLibrary(real) then LIST render hydrates covers, no throw", () => {
    ctx.applyLibrary(real);
    ctx.SELECTED = null; ctx.CURRENT = "__all__";
    ctx.render();
    assertNoRenderThrow();
    if (!invokeLog.some((c) => c.cmd === "cover_data_url")) {
      // Count how many books actually have a local cover, to disambiguate.
      const books = ctx.everyBook();
      const withLocal = books.filter((b) => { const u = ctx.bookCover(b); return u && !/^https?:/i.test(u); }).length;
      throw new Error(`list render invoked NO cover_data_url though ${withLocal}/${books.length} books have a local cover`);
    }
  });

  // Sweep every book's DETAIL panel with real data.
  ctx.applyLibrary(real);
  const allBooks = ctx.everyBook();
  let detailThrows = 0, firstThrow = null;
  for (const b of allBooks) {
    ctx.COVER_CACHE = {}; invokeLog.length = 0; ctx.LAST_RENDER_ERROR = null;
    ctx.SELECTED = b.id; ctx.CURRENT = "__all__"; ctx.DETAIL_COLLAPSED = false;
    try { ctx.render(); } catch (e) { /* shouldn't happen w/ resilience */ }
    if (ctx.LAST_RENDER_ERROR) { detailThrows++; if (!firstThrow) firstThrow = b.title + ": " + ctx.LAST_RENDER_ERROR; }
  }
  check(`detail panel renders for all ${allBooks.length} books without a phase throwing`, () => {
    if (detailThrows) throw new Error(`${detailThrows} book(s) threw in a render phase. First: ${firstThrow}`);
  });
} else {
  console.log("note - /tmp/real_vm.json not present; skipped real-data sweep");
}

console.log(failures ? `\n${failures} FAILED` : "\nALL PASSED");
process.exit(failures ? 1 : 0);
