#!/usr/bin/env node
// Headless UX test — drives the REAL file:// path through headless Chrome via the
// DevTools Protocol (no external deps; Node 22+ has global WebSocket + fetch).
//
// This exists because the prototype is opened by double-click (file://). ES
// modules do NOT execute over file://, so an earlier mock silently did nothing.
// This test loads the page exactly as a user would and asserts that the JS ran,
// there are zero console errors/exceptions, and the core interactions work.
//
// Usage:  node app/ui-mock/headless-test.mjs [path/to/index.html]
// Exit 0 = all pass, 1 = failure.

import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const HTML = resolve(process.argv[2] || join(import.meta.dirname, "index.html"));
const FILE_URL = pathToFileURL(HTML).href;

const CHROME =
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PORT = 9456;

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

// --- launch headless Chrome with remote debugging ---
const userDir = mkdtempSync(join(tmpdir(), "libgen-ux-"));
const chrome = spawn(CHROME, [
  "--headless=new",
  `--remote-debugging-port=${PORT}`,
  `--user-data-dir=${userDir}`,
  "--no-first-run", "--no-default-browser-check", "--disable-gpu",
  "--window-size=1200,820",
  "about:blank",
], { stdio: ["ignore", "ignore", "pipe"] });

let chromeErr = "";
chrome.stderr.on("data", (d) => (chromeErr += d));

function cleanup(code) { try { chrome.kill("SIGKILL"); } catch {} process.exit(code); }

// --- minimal CDP client over the browser-level WebSocket ---
async function browserWsUrl() {
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json/version`);
      const j = await r.json();
      if (j.webSocketDebuggerUrl) return j.webSocketDebuggerUrl;
    } catch {}
    await sleep(100);
  }
  throw new Error("Chrome DevTools endpoint never came up:\n" + chromeErr);
}

function makeClient(ws) {
  let id = 0;
  const pending = new Map();
  const events = [];
  const waiters = [];
  ws.addEventListener("message", (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
    } else if (msg.method) {
      events.push(msg);
      waiters.forEach((w) => w(msg));
    }
  });
  function send(method, params = {}, sessionId) {
    return new Promise((resolve, reject) => {
      const m = { id: ++id, method, params };
      if (sessionId) m.sessionId = sessionId;
      pending.set(m.id, { resolve, reject });
      ws.send(JSON.stringify(m));
    });
  }
  return { send, events };
}

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: !!cond, detail });
  console.log(`${cond ? "  ✓" : "  ✗"} ${name}${cond ? "" : "  — " + (detail || "")}`);
}

async function main() {
  const wsUrl = await browserWsUrl();
  const ws = new WebSocket(wsUrl);
  await new Promise((r, rej) => { ws.addEventListener("open", r); ws.addEventListener("error", rej); });
  const { send } = makeClient(ws);

  // Open a fresh tab for our file:// page and attach to it.
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const S = sessionId;

  // Capture console errors + uncaught exceptions BEFORE navigating.
  const consoleErrors = [];
  const exceptions = [];
  ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    if (m.sessionId !== S) return;
    if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error")
      consoleErrors.push(m.params.args.map((a) => a.value || a.description).join(" "));
    if (m.method === "Runtime.exceptionThrown")
      exceptions.push(m.params.exceptionDetails.exception?.description ||
        m.params.exceptionDetails.text);
  });
  await send("Runtime.enable", {}, S);
  await send("Page.enable", {}, S);

  await send("Page.navigate", { url: FILE_URL }, S);
  // Wait for boot marker the app sets at the end of its script.
  let ready = false;
  for (let i = 0; i < 60; i++) {
    const r = await evalJS("document.documentElement.getAttribute('data-ready')");
    if (r === "1") { ready = true; break; }
    await sleep(100);
  }

  async function evalJS(expr) {
    const r = await send("Runtime.evaluate",
      { expression: expr, returnByValue: true, awaitPromise: true }, S);
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " in: " + expr);
    return r.result.value;
  }

  console.log(`\nHeadless UX test — ${FILE_URL}\n`);

  // 1) The script actually executed over file://
  check("page boots over file:// (data-ready=1)", ready, "JS did not run — likely an ES-module/file:// issue");

  // 2) No console errors / uncaught exceptions
  check("no uncaught exceptions", exceptions.length === 0, exceptions.join(" | "));
  check("no console errors", consoleErrors.length === 0, consoleErrors.join(" | "));

  // 3) Model + rendering
  const total = await evalJS("window.__APP__ ? window.__APP__.counts().all : -1");
  check("model has 100 books", total === 100, "got " + total);
  const rows = await evalJS("document.querySelectorAll('.book').length");
  check("book rows rendered", rows > 0, "rendered " + rows + " rows");
  const groups = await evalJS("document.querySelectorAll('.group-h').length");
  check("5 batch groups shown", groups === 5, "got " + groups);

  // 4) "Needs you" filter narrows to attn/fail only
  await evalJS("document.querySelector('[data-filter=\"attn\"]').click()");
  await sleep(50);
  const fdata = await evalJS(`(function(){
    var rows=[].slice.call(document.querySelectorAll('.book'));
    var bad=rows.filter(function(r){return !r.querySelector('.s-fail,.rowbtn.amber');});
    return {filter: window.__APP__.getFilter(), rows: rows.length, bad: bad.length};
  })()`);
  check("Needs-you filter active", fdata.filter === "attn", JSON.stringify(fdata));
  check("Needs-you shows only attn/fail rows", fdata.rows > 0 && fdata.bad === 0, JSON.stringify(fdata));

  // 5) Reset to All, click a book → detail drawer opens
  await evalJS("document.querySelector('[data-filter=\"all\"]').click()");
  await sleep(30);
  await evalJS("document.querySelector('.book').click()");
  await sleep(50);
  const drawerOpen = await evalJS("document.getElementById('drawer').classList.contains('open') && !!window.__APP__.getSelected()");
  check("clicking a book opens the detail drawer", drawerOpen);

  // 6) Import sheet opens; uploading a file shows a parsed preview
  await evalJS("document.querySelector('[data-action=\"open-import\"]').click()");
  await sleep(50);
  const importOpen = await evalJS("document.getElementById('sheet').classList.contains('open')");
  check("import sheet opens", importOpen);
  const hasFileInput = await evalJS("!!document.getElementById('fileInput') && !!document.getElementById('dropzone')");
  check("import offers a file upload (not paste)", hasFileInput);
  // Simulate choosing a markdown file via the file input.
  await evalJS(`(function(){
    var f = new File(["# L\\n## Picks\\n- Treasure Island — Robert Louis Stevenson\\n- Kidnapped — Robert Louis Stevenson\\n"], "list.md", {type:"text/markdown"});
    var dt = new DataTransfer(); dt.items.add(f);
    var inp = document.getElementById('fileInput'); inp.files = dt.files;
    inp.dispatchEvent(new Event('change', {bubbles:true}));
  })()`);
  await sleep(80);
  const uploaded = await evalJS("/Treasure Island/.test(document.getElementById('importPreview').textContent) && /list\\.md/.test(document.getElementById('fileChosen').textContent)");
  check("uploading a .md file previews its books", uploaded);

  // 7) Group collapse toggles
  await evalJS("document.querySelector('[data-action=\"close-import\"]').click()");
  await sleep(30);
  const beforeRows = await evalJS("document.querySelectorAll('.book').length");
  await evalJS("document.querySelector('.group-h').click()");
  await sleep(40);
  const afterRows = await evalJS("document.querySelectorAll('.book').length");
  check("collapsing a group hides its rows", afterRows < beforeRows, `before=${beforeRows} after=${afterRows}`);

  // 8) Multiple lists: sidebar shows 3 lists + an aggregate, switching works
  const nLists = await evalJS("window.__APP__.lists().length");
  check("three lists in the library", nLists === 3, "got " + nLists);
  const sideItems = await evalJS("document.querySelectorAll('.sidebar [data-list]').length");
  check("sidebar lists all lists + 'All downloads'", sideItems === 4, "got " + sideItems);
  await evalJS("document.querySelector('[data-list=\"scifi\"]').click()");
  await sleep(40);
  const switched = await evalJS("window.__APP__.getCurrent()");
  const sciTitle = await evalJS("document.getElementById('listTitle').textContent");
  check("switching list updates the view", switched === "scifi" && /Sci-Fi/.test(sciTitle), switched + " / " + sciTitle);

  // 9) Aggregate "All downloads" view groups by list, and global counts > a single list
  await evalJS("document.querySelector('[data-list=\"__all__\"]').click()");
  await sleep(40);
  const allView = await evalJS("(function(){var g=document.querySelectorAll('.group-h').length;var tot=window.__APP__.globalCounts().all;return {groups:g, total:tot};})()");
  check("aggregate view groups by list (>=3 sections)", allView.groups >= 3, JSON.stringify(allView));
  check("global total spans all lists (>100)", allView.total > 100, "total=" + allView.total);

  // 10) Move-to-top: a queued book can be prioritized ("Next up")
  await evalJS("document.querySelector('[data-list=\"jeremy\"]').click()");
  await evalJS("document.querySelector('[data-filter=\"queued\"]').click()");
  await sleep(40);
  const topClicked = await evalJS(`(function(){
    var b=document.querySelector('[data-top]'); if(!b) return false; b.click(); return true;
  })()`);
  await sleep(40);
  const nextUp = await evalJS("(document.body.textContent.match(/Next up/)||[]).length > 0");
  check("move-to-top marks a queued book 'Next up'", topClicked && nextUp);

  // 11) The redundant sidebar 'Show in view' filter section is gone
  const noSideFilters = await evalJS("document.querySelectorAll('.sidebar [data-filter]').length");
  check("sidebar no longer duplicates the filter chips", noSideFilters === 0, "found " + noSideFilters);

  // 12) Alternate versions: a downloaded book exposes other md5s to swap to
  await evalJS("document.querySelector('[data-list=\"jeremy\"]').click()");
  await evalJS("document.querySelector('[data-filter=\"done\"]').click()");
  await sleep(40);
  await evalJS("document.querySelector('.book').click()"); // open a done book
  await sleep(40);
  const hasVersions = await evalJS("/Variations/.test(document.getElementById('dBody').textContent) && document.querySelectorAll('[data-dl]').length > 0");
  check("downloaded book lists its md5 variations", hasVersions);
  const swapped = await evalJS("(function(){var b=document.querySelector('[data-dl]'); if(!b) return false; b.click(); return true;})()");
  await sleep(40);
  const reDownloading = await evalJS("document.querySelector('#dBody .conf') !== null");
  check("downloading a variation shows its own progress", swapped && reDownloading);

  // 13) Per-variation: a 'multi' book shows two formats in different states
  await evalJS("document.querySelector('[data-filter=\"all\"]').click()");
  await sleep(40);
  const multiGlyphs = await evalJS(`(function(){
    var rows=[].slice.call(document.querySelectorAll('.book'));
    // find a row with both a done glyph and an active glyph (epub done, pdf downloading)
    return rows.some(function(r){ return r.querySelector('.vg-done') && r.querySelector('.vg-active'); });
  })()`);
  check("a book shows variations in different states (epub✓ pdf⏳)", multiGlyphs);

  // 14) The Preferred-formats rank widget is GONE from the toolbar (it moved
  //     into Settings); the toolbar's old row3 no longer carries a #fmtRank.
  const toolbarHasRank = await evalJS("!!document.querySelector('.toolbar #fmtRank')");
  check("format-rank removed from the toolbar", toolbarHasRank === false);

  // 15) The gear opens the Settings sheet, and the Preferred-formats rank now
  //     lives inside it (and is still reorderable there).
  await evalJS("document.querySelector('[data-action=\"open-settings\"]').click()");
  await sleep(50);
  const settingsOpen = await evalJS("document.getElementById('settingsSheet').classList.contains('open')");
  check("gear opens the Settings sheet", settingsOpen);
  const rankInSettings = await evalJS("!!document.querySelector('#settingsSheet #fmtRank .frank')");
  check("format-rank now lives in Settings", rankInSettings);
  const before = await evalJS("[].map.call(document.querySelectorAll('#fmtRank .frank'), function(e){return e.textContent.replace(/[0-9▲▼]/g,'').trim();}).join(',')");
  await evalJS("document.querySelectorAll('#fmtRank [data-fmtmv$=\":up\"]')[1].click()"); // move pdf (2nd) up
  await sleep(40);
  const after = await evalJS("[].map.call(document.querySelectorAll('#fmtRank .frank'), function(e){return e.textContent.replace(/[0-9▲▼]/g,'').trim();}).join(',')");
  check("preferred formats can be reordered (in Settings)", before !== after && /^PDF,EPUB/.test(after), before + " -> " + after);

  // 16) A per-list setting round-trips through the JS state (keep_top), and an
  //     app setting round-trips too (a download site toggle + the folder field).
  await evalJS("(function(){ var i=document.getElementById('setKeep'); i.value='8'; i.dispatchEvent(new Event('input',{bubbles:true})); })()");
  await evalJS("(function(){ var i=document.getElementById('setOut'); i.value='/tmp/books'; i.dispatchEvent(new Event('input',{bubbles:true})); })()");
  // Enable a second download site (libgen.vg) so the failover chain has 2 entries.
  await evalJS("(function(){ var b=document.querySelector('[data-site-toggle=\"libgen.vg\"]'); if(b) b.click(); })()");
  await sleep(30);
  await evalJS("document.querySelector('[data-action=\"save-settings\"]').click()");
  await sleep(40);
  const roundTrip = await evalJS(`(function(){
    var s=window.__APP__.getSettings(), c=window.__APP__.getAppCfg();
    return { keep: s.keep_top, out: c.out_dir, sites: c.sites.join(','), fmtFirst: window.__APP__.getFormats()[0] };
  })()`);
  check("per-list setting round-trips (keep_top=8)", roundTrip.keep === 8, JSON.stringify(roundTrip));
  check("app setting round-trips (out dir + sites)", roundTrip.out === "/tmp/books" && /libgen\.vg/.test(roundTrip.sites), JSON.stringify(roundTrip));
  check("format-rank order persisted into state (pdf first)", roundTrip.fmtFirst === "pdf", JSON.stringify(roundTrip));

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed.\n`);
  cleanup(failed.length ? 1 : 0);
}

main().catch((e) => { console.error("FATAL:", e.message); cleanup(2); });
