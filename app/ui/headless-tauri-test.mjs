#!/usr/bin/env node
// Headless test of the REAL Tauri boot path (hasTauri()===true), which the
// file:// suite never exercises. We inject a mock window.__TAURI__ BEFORE the
// page script runs (TAURI is captured once at load), with invoke("library")
// returning a real ViewLibrary dumped from a DB. This catches throws in the
// Tauri-only boot (e.g. renderEmpty referencing removed elements) that leave the
// app empty + the menu dead. Requires /tmp/lib.json (see viewmodel dump test).
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const HTML = resolve(join(import.meta.dirname, "index.html"));
const FILE_URL = pathToFileURL(HTML).href;
const LIB = readFileSync(process.env.LIB || "/tmp/lib.json", "utf8");
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PORT = 9457;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const userDir = mkdtempSync(join(tmpdir(), "libgen-tauri-"));
const chrome = spawn(CHROME, ["--headless=new", `--remote-debugging-port=${PORT}`,
  `--user-data-dir=${userDir}`, "--no-first-run", "--no-default-browser-check",
  "--disable-gpu", "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
let chromeErr = "";
chrome.stderr.on("data", (d) => (chromeErr += d));
const cleanup = (code) => { try { chrome.kill("SIGKILL"); } catch {} process.exit(code); };

async function browserWsUrl() {
  for (let i = 0; i < 50; i++) {
    try { const j = await (await fetch(`http://127.0.0.1:${PORT}/json/version`)).json();
      if (j.webSocketDebuggerUrl) return j.webSocketDebuggerUrl; } catch {}
    await sleep(100);
  }
  throw new Error("Chrome DevTools never came up:\n" + chromeErr);
}

const results = [];
const check = (name, cond, detail) => {
  results.push({ ok: !!cond });
  console.log(`${cond ? "  ✓" : "  ✗"} ${name}${cond ? "" : "  — " + (detail || "")}`);
};

// The mock injected before the page script. invoke("library") returns the real
// lib; other commands resolve it too (harmless). event.listen registers cbs;
// __emit fires them (to simulate the native Settings menu event).
const INJECT = `
window.__LIB__ = ${LIB};
(function () {
  var listeners = {};
  window.__TAURI__ = {
    core: { invoke: function (cmd) { return Promise.resolve(window.__LIB__); } },
    event: {
      listen: function (ev, cb) { (listeners[ev] = listeners[ev] || []).push(cb);
        return Promise.resolve(function () {}); }
    }
  };
  window.__emit = function (ev, payload) {
    (listeners[ev] || []).forEach(function (cb) { cb({ payload: payload }); });
  };
})();
`;

async function main() {
  const ws = new WebSocket(await browserWsUrl());
  await new Promise((r, rej) => { ws.addEventListener("open", r); ws.addEventListener("error", rej); });
  let id = 0; const pending = new Map();
  ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id); m.error ? reject(new Error(m.error.message)) : resolve(m.result); }
  });
  const exceptions = [], consoleErrors = [];
  ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "Runtime.exceptionThrown")
      exceptions.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
    if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error")
      consoleErrors.push(m.params.args.map((a) => a.value || a.description).join(" "));
  });
  let S;
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const m = { id: ++id, method, params }; if (S) m.sessionId = S;
    pending.set(m.id, { resolve, reject }); ws.send(JSON.stringify(m));
  });

  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  S = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable"); await send("Page.enable");
  await send("Page.addScriptToEvaluateOnNewDocument", { source: INJECT });
  await send("Page.navigate", { url: FILE_URL });

  const evalJS = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
    return r.result.value;
  };

  await sleep(400); // let the navigation + script settle
  let ready = false;
  for (let i = 0; i < 80; i++) {
    try { if ((await evalJS("document.documentElement.getAttribute('data-ready')")) === "1") { ready = true; break; } }
    catch (_) { /* context not ready yet */ }
    await sleep(100);
  }

  console.log(`\nHeadless TAURI-boot test — ${FILE_URL}\n`);
  check("Tauri boot completes (data-ready=1)", ready, "boot aborted — likely a throw before markReady");
  check("hasTauri() true (mock injected)", await evalJS("hasTauri()"));
  check("no uncaught exceptions during boot", exceptions.length === 0, exceptions.join(" | "));
  check("no console errors during boot", consoleErrors.length === 0, consoleErrors.join(" | "));

  const nLists = await evalJS("window.__APP__ ? window.__APP__.lists().length : -1");
  check("lists loaded from backend (not empty)", nLists >= 1, "got " + nLists);
  const rows = await evalJS("document.querySelectorAll('.book').length");
  check("book rows rendered", rows > 0, "rendered " + rows);
  const sidebarLists = await evalJS("document.querySelectorAll('.side-item[data-list]:not([data-list=\"__all__\"])').length");
  check("sidebar shows the persisted list(s)", sidebarLists >= 1, "got " + sidebarLists);

  // Native Settings menu: emit menu://settings → the sheet opens.
  await evalJS("window.__emit('menu://settings')");
  await sleep(60);
  const settingsOpen = await evalJS("document.getElementById('settingsSheet').classList.contains('open')");
  check("app-menu Settings opens the sheet (menu://settings)", settingsOpen);

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed.\n`);
  cleanup(failed.length ? 1 : 0);
}
main().catch((e) => { console.error(e); cleanup(2); });
