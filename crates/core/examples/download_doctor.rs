//! Download Doctor — encode the download/retry INVARIANTS the engine is *supposed*
//! to uphold, then check every book's persisted event history + job state against
//! them, flagging any download whose behavior doesn't match the design.
//!
//! Motivation: a real bug (CDN edge rotation never firing when the first hop is the
//! mirror `get.php`, see `download.rs::fetch_with_edge_rotation` lines ~1259-1272)
//! went uncaught because nothing compared *actual* download behavior to the
//! *intended* behavior. This tool is that comparison, and is meant to be maintained:
//! the invariants live in a single named, data-driven table (`INVARIANTS`) so new
//! ones are easy to add.
//!
//! Usage:  cargo run -p libgen-core --example download_doctor -- <db-path>
//!         (point it at a READ-ONLY COPY of the live DB; never the live file.)
//!
//! It is read-only: it opens the store, walks every list → group → book → variation
//! (Candidate with a `job`), reconstructs that variation's slice of the chronicle
//! (`BookRequest::history` filtered by md5), runs each [`Invariant`] over it, and
//! prints OK or the list of VIOLATED invariants with the concrete evidence (states,
//! attempts, hosts, byte counts, timestamps). Counts are summarized at the end,
//! grouped by invariant.

use std::collections::BTreeMap;

use libgen_core::model::{BookEvent, BookRequest, Candidate, DownloadJob, Group, JobState};
use libgen_core::store::Store;

// ---------------------------------------------------------------------------
// The design constants the invariants are derived from. These mirror the real
// values in the engine (cited per-constant) so the doctor checks the SAME
// numbers the scheduler/downloader use. If those change, change these.
// ---------------------------------------------------------------------------

/// `HostLimits::max_attempts` default — the per-host give-up budget.
/// Source: `crates/core/src/queue.rs` `impl Default for HostLimits` (`max_attempts: 4`)
/// and the retry loop `download_on_host` (`let max_attempts = queue.limits.max_attempts`).
const MAX_ATTEMPTS: u32 = 4;

/// Retry backoff ceiling. Source: `queue.rs` `SchedulerBuilder::new`
/// (`max_backoff: Duration::from_secs(200)`) and `backoff_for` (base 1s × 2^n,
/// capped, then FULL JITTER in `[0, ceiling]`). Because of full jitter the *actual*
/// delay is `<= ceiling`, so we only flag a delay that EXCEEDS the cap (+ slack).
const MAX_BACKOFF_SECS: u64 = 200;

/// The shared booksdl CDN edge suffix. A mirror `get.php` 307-redirects to one of
/// `cdn1..cdn6.booksdl.lc`. Source: `download.rs::booksdl_edge_host` /
/// `booksdl_alternate_edges`.
const BOOKSDL_EDGE_SUFFIX: &str = ".booksdl.lc";

// ---------------------------------------------------------------------------
// Per-variation view: a job + the slice of its book's chronicle that pertains to
// it (events tagged with this md5, in time order).
// ---------------------------------------------------------------------------

/// Everything an invariant needs to judge ONE download variation.
struct VariationCtx<'a> {
    /// Human label: "List / Group / … / Title [fmt]".
    label: String,
    md5: &'a str,
    job: &'a DownloadJob,
    /// This variation's chronicle slice (events tagged with `md5`), oldest first.
    events: Vec<&'a BookEvent>,
    /// Whether ANY sibling variation of the same book reached `Done` (so a per-book
    /// "we got a copy" is distinguishable from "this exact variation failed").
    sibling_done: bool,
}

impl VariationCtx<'_> {
    /// Events of a given `kind`, in time order.
    fn of_kind<'b>(&'b self, kind: &str) -> Vec<&'b BookEvent> {
        self.events
            .iter()
            .copied()
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// True if the chronicle ever shows this variation being served from a real
    /// booksdl CDN edge (`cdnN.booksdl.lc`), i.e. rotation/host-recording worked.
    /// Recorded by `orchestrator.rs` on a `downloading` "serving from {host}" event
    /// (and the job.host is set to the edge), driven by `download.rs::record_edge`.
    fn ever_served_from_edge(&self) -> bool {
        self.job
            .host
            .as_deref()
            .is_some_and(|h| h.ends_with(BOOKSDL_EDGE_SUFFIX))
            || self
                .events
                .iter()
                .any(|e| e.kind == "downloading" && e.detail.contains(BOOKSDL_EDGE_SUFFIX))
    }

    /// True if this variation's lane is the libgen.li family (→ booksdl CDN). The
    /// edge-rotation invariants only apply to that lane. We infer it from the hosts
    /// named in the chronicle / job (libgen.li/vg/la or a cdn edge).
    /// Source: `download.rs::LIBGEN_FAMILY_SITES` + `cdn_group` ("booksdl").
    fn is_booksdl_lane(&self) -> bool {
        let host_is_family = |h: &str| {
            let h = h.to_ascii_lowercase();
            h.ends_with(BOOKSDL_EDGE_SUFFIX)
                || h.starts_with("libgen.li")
                || h.starts_with("libgen.vg")
                || h.starts_with("libgen.la")
        };
        self.job.host.as_deref().is_some_and(host_is_family)
            || self.events.iter().any(|e| {
                matches!(e.kind.as_str(), "downloading" | "retry" | "failover")
                    && (e.detail.contains(BOOKSDL_EDGE_SUFFIX)
                        || e.detail.contains("libgen.li")
                        || e.detail.contains("libgen.vg")
                        || e.detail.contains("libgen.la"))
            })
    }
}

/// A single named, checkable design rule. `check` returns `Ok(())` when the
/// variation conforms, or `Err(evidence)` describing the violation with concrete
/// numbers. Add a new rule by appending one entry to [`INVARIANTS`].
struct Invariant {
    /// Stable short id used in the summary grouping.
    name: &'static str,
    /// Where in the engine the rule is derived from (file:line citation).
    derived_from: &'static str,
    /// One-line statement of the intended behavior.
    intent: &'static str,
    /// Returns `Ok` if upheld, else `Err(evidence)`.
    check: fn(&VariationCtx) -> Result<(), String>,
}

// ---------------------------------------------------------------------------
// THE INVARIANTS. Data-driven so new ones are a one-line append.
// ---------------------------------------------------------------------------

const INVARIANTS: &[Invariant] = &[
    // (1) attempts must not exceed max_attempts for a NON-successful job. A `done`
    // job may legitimately exceed the nominal count because the retry loop RESETS
    // the attempt counter when an attempt downloads a meaningful chunk
    // (queue.rs `PROGRESS_RESET_BYTES`), so we exempt Done. A still-stuck job at/over
    // the cap is the infinite-loop anomaly.
    Invariant {
        name: "max-attempts-respected",
        derived_from: "queue.rs HostLimits::default max_attempts=4; download_on_host `if attempt >= max_attempts`",
        intent: "a non-done job's attempts must stay within the per-host max_attempts budget",
        check: |c| {
            if c.job.state == JobState::Done {
                return Ok(());
            }
            if c.job.attempts > MAX_ATTEMPTS {
                Err(format!(
                    "attempts={} exceeds max_attempts={} while state={:?} (retry loop not giving up / spinning)",
                    c.job.attempts, MAX_ATTEMPTS, c.job.state
                ))
            } else {
                Ok(())
            }
        },
    },
    // (2) THE KNOWN BUG: a booksdl download that gives up because every remaining
    // failover target shares the SAME booksdl CDN, while edge rotation across
    // cdn1..cdn6 never recovered it. The terminal `failed` event reads "skipping
    // <mirror> (same CDN 'booksdl' as an already-failed host)" — the failover loop
    // ran out of INDEPENDENT lanes, and `fetch_with_edge_rotation` cannot rotate
    // when the failing hop is the mirror get.php (its first hop), so the cdn edges
    // were never exhausted before giving up. This is exactly how the stuck
    // graphic-novel downloads die.
    Invariant {
        name: "booksdl-gave-up-without-independent-lane",
        derived_from: "download.rs fetch_with_edge_rotation ~1259-1272 (no rotate when first hop is mirror); queue.rs cdn_group/failed_cdns failover",
        intent: "a failed booksdl download must not give up solely because alternates share the same CDN before edge rotation has truly exhausted",
        check: |c| {
            if c.job.state != JobState::Failed {
                return Ok(());
            }
            let terminal = c.of_kind("failed");
            let last = match terminal.last() {
                Some(e) => e,
                None => return Ok(()),
            };
            let d = &last.detail;
            if d.contains("same CDN") || (d.contains("booksdl") && d.contains("already-failed")) {
                Err(format!(
                    "gave up with terminal error {:?} — failover ran out of INDEPENDENT lanes \
                     (all remaining mirrors share the booksdl CDN) while {}/{:?} bytes already on disk; \
                     edge rotation never recovered it",
                    truncate(d, 140),
                    c.job.bytes_done,
                    c.job.total_bytes
                ))
            } else {
                Ok(())
            }
        },
    },
    // (3) A retry should RESUME from the partial, not restart from 0 / not loop
    // forever unable to advance. Two failure modes flagged:
    //   (a) repeated "resuming from 0 MB" while bytes_done > 0  → resume hint lost;
    //   (b) the stuck-partial loop: many retries all erroring "host ignored Range
    //       (HTTP 200) … failing over to preserve it" — the edge won't honor Range,
    //       so the partial can never complete yet the partial is never restarted.
    Invariant {
        name: "resume-progresses",
        derived_from: "download.rs `start = existing` resume + `host ignored Range (HTTP 200)` failover (lines ~1390-1400); queue.rs current_part_len resume",
        intent: "a resumed download must make forward byte progress, not loop on an un-honored Range",
        check: |c| {
            let ignored_range = c
                .events
                .iter()
                .filter(|e| matches!(e.kind.as_str(), "retry" | "failover"))
                .filter(|e| e.detail.contains("ignored Range (HTTP 200)"))
                .count();
            // 3+ such events on a non-done job = a stuck-partial loop (can't resume,
            // can't restart). One-or-two can happen transiently and then succeed.
            if c.job.state != JobState::Done && ignored_range >= 3 {
                return Err(format!(
                    "{} 'host ignored Range (HTTP 200)' failover(s) with a {}-byte partial — \
                     edge won't honor Range so the resume can NEVER complete (stuck-partial loop)",
                    ignored_range, c.job.bytes_done
                ));
            }
            Ok(())
        },
    },
    // (4) The backoff the scheduler SCHEDULED before each retry must never exceed the
    // 200s cap. Each `retry` event records the chosen delay ("after {n}s backoff"),
    // which is `backoff_for` = base 1s ×2^n, capped, then full-jittered into
    // `[0, ceiling]`. The recorded value is therefore always `<= 200s`. We check the
    // STATED backoff rather than the wall-clock gap between events: that gap also
    // includes the next attempt's connect/transfer time (and any user pause), so it
    // is not the sleep and would over-report.
    Invariant {
        name: "backoff-within-cap",
        derived_from: "queue.rs SchedulerBuilder base=1s max=200s; backoff_for (×2^n, capped, full jitter); orchestrator.rs retry detail 'after {n}s backoff'",
        intent: "the scheduled backoff recorded on each retry must not exceed the 200s cap",
        check: |c| {
            for e in c.of_kind("retry") {
                if let Some(secs) = retry_backoff_secs(&e.detail) {
                    if secs > MAX_BACKOFF_SECS {
                        return Err(format!(
                            "a retry recorded a {secs}s backoff, exceeding the {MAX_BACKOFF_SECS}s cap ({:?})",
                            truncate(&e.detail, 100)
                        ));
                    }
                }
            }
            Ok(())
        },
    },
    // (5) A Done job must have a verified md5 AND a recorded output path. The
    // downloader only renames the .part into place after the md5 check passes, and
    // the orchestrator sets md5_verified + output_path on Done.
    Invariant {
        name: "done-verified-and-written",
        derived_from: "download.rs md5 verify before rename (~1475-1490); orchestrator.rs Progress::Done sets md5_verified+output_path",
        intent: "a Done variation must be md5-verified and have an output_path",
        check: |c| {
            if c.job.state != JobState::Done {
                return Ok(());
            }
            let mut problems = Vec::new();
            if !c.job.md5_verified {
                problems.push("md5_verified=false".to_string());
            }
            match &c.job.output_path {
                None => problems.push("no output_path".to_string()),
                Some(p) if p.is_empty() => problems.push("empty output_path".to_string()),
                Some(p) if !std::path::Path::new(p).exists() => {
                    problems.push(format!("output_path missing on disk: {p}"))
                }
                Some(_) => {}
            }
            if problems.is_empty() {
                Ok(())
            } else {
                Err(format!("Done but {}", problems.join(", ")))
            }
        },
    },
    // (6) An IN-FLIGHT / FAILED booksdl variation that transferred a meaningful
    // amount should have a cdnN edge recorded as its serving host — the mirror only
    // 307-redirects; bytes come from cdnN.booksdl.lc, and record_edge re-keys the
    // host onto the edge. (Scoped to non-Done: the orchestrator's Done arm overwrites
    // job.host with the leg's resolved MIRROR host, so a Done job legitimately shows
    // the mirror — it is NOT a reliable post-completion signal.)
    Invariant {
        name: "inflight-serving-host-is-edge",
        derived_from: "download.rs record_edge/current_edge; orchestrator.rs Progress::Bytes 'serving from {edge}' (Done arm resets host to mirror)",
        intent: "while downloading/failed on the booksdl lane with bytes flowing, the recorded host should be a cdnN edge",
        check: |c| {
            if c.job.state == JobState::Done || !c.is_booksdl_lane() {
                return Ok(());
            }
            // Only meaningful once a non-trivial amount transferred.
            const MIN_BYTES: u64 = 512 * 1024;
            if c.job.bytes_done < MIN_BYTES {
                return Ok(());
            }
            if c.ever_served_from_edge() {
                return Ok(());
            }
            Err(format!(
                "transferred {} bytes on the booksdl lane (state={:?}) but the recorded host is {:?} \
                 (a mirror front-door, never a {} edge) — host-recording/rotation gap",
                c.job.bytes_done, c.job.state, c.job.host, BOOKSDL_EDGE_SUFFIX
            ))
        },
    },
    // (7) A book whose rolled-up status reads OK to the user while a requested
    // variation is silently stuck (failed/over-budget) is misleading. The
    // orchestrator's roll_up_status surfaces the book as Done when ANY variation is
    // Done, so a failed sibling can hide. Flag a FAILED variation that has a Done
    // sibling so it isn't lost in the UI.
    Invariant {
        name: "no-hidden-failed-sibling",
        derived_from: "orchestrator.rs roll_up_status (a.done>0 ⇒ Done even with failed siblings)",
        intent: "a failed variation should not be masked by a Done sibling without surfacing",
        check: |c| {
            if c.job.state == JobState::Failed && c.sibling_done {
                Err(format!(
                    "this variation FAILED (bytes {}/{:?}) but a sibling is Done — \
                     the book reads as Done, hiding this stuck variation",
                    c.job.bytes_done, c.job.total_bytes
                ))
            } else {
                Ok(())
            }
        },
    },
    // (8) EDGE ROTATION must actually try SIBLING edges before a booksdl download
    // gives up. The downloader emits a `note` per probed edge ("rotate cdn3 → 500",
    // "rotate cdn1 → timeout", "rotate cdn4 → 206 (won)") whenever the first edge is
    // rejected and rotation fires. A FAILED booksdl variation that recorded only ONE
    // (or zero) distinct `rotate cdnN` note never reached its siblings — the exact
    // blind spot that let the old "first hop is the mirror → rotation can't fire" bug
    // hide. (≥2 distinct edges = rotation genuinely probed alternates.)
    Invariant {
        name: "rotation-attempted-on-edge-failure",
        derived_from: "download.rs fetch_with_edge_rotation probe loop notes ('rotate {edge} → …', ~1380-1420); orchestrator.rs Progress::Note → 'note' history kind",
        intent: "a failed booksdl download must show rotation probing ≥2 distinct cdn edges, not dead-ending on one",
        check: |c| {
            if c.job.state != JobState::Failed || !c.is_booksdl_lane() {
                return Ok(());
            }
            let edges = distinct_rotate_edges(c);
            // No rotation notes at all → either rotation never fired (the old bug) or
            // the lane failed before reaching an edge. Either way the design element
            // (rotate-before-give-up) is unverifiable, so we DON'T flag the zero case
            // here — only the "tried exactly one and quit" fingerprint, which is the
            // distinctive old-bug signature.
            if edges.len() == 1 {
                return Err(format!(
                    "failed booksdl download touched only ONE edge ({}) — edge rotation never \
                     probed siblings before giving up (the cdn-rotation blind-spot bug)",
                    edges.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            Ok(())
        },
    },
    // (9) The 200-IGNORED-RANGE → DOWNLOAD-FROM-SCRATCH restart. When a resume (Range)
    // request comes back 200, the edge ignored the Range and is streaming the whole
    // file from 0; the downloader DROPS the partial and restarts from scratch (it must
    // NOT loop forever trying to "preserve" a partial the edge will never honor). Each
    // such transition emits a paired note: "host ignored Range (HTTP 200)…" IMMEDIATELY
    // FOLLOWED BY "restarting from scratch (dropped N-byte partial)". A history showing
    // the 200 note WITHOUT a following from-scratch note is the OLD stuck-loop
    // fingerprint (the request can never complete).
    Invariant {
        name: "range-ignored-restarts-from-scratch",
        derived_from: "download.rs download_with_client_cancellable 200→start=0 block notes ('host ignored Range (HTTP 200)…' + 'restarting from scratch…', ~1490-1520)",
        intent: "every 'host ignored Range (HTTP 200)' note must be followed by a from-scratch restart note (proves the 200 path restarts, not loops)",
        check: |c| {
            let notes = c.of_kind("note");
            let mut unmatched_200 = 0usize;
            let mut iter = notes.iter().peekable();
            for e in iter {
                if e.detail.contains("host ignored Range (HTTP 200)") {
                    // Look ahead for the from-scratch restart that MUST follow it.
                    let restarted = notes
                        .iter()
                        .skip_while(|x| !std::ptr::eq(**x, *e))
                        .skip(1)
                        .any(|x| x.detail.contains("restarting from scratch"));
                    if !restarted {
                        unmatched_200 += 1;
                    }
                }
            }
            if unmatched_200 > 0 {
                Err(format!(
                    "{unmatched_200} 'host ignored Range (HTTP 200)' note(s) with NO following \
                     'restarting from scratch' note — the 200 path looped instead of restarting \
                     (stuck-partial loop; bytes_done={})",
                    c.job.bytes_done
                ))
            } else {
                Ok(())
            }
        },
    },
];

/// Distinct cdn edges named in this variation's `rotate {edge} → …` diagnostic notes
/// (each emitted by `download.rs::fetch_with_edge_rotation` per probed sibling edge).
/// Evidence of how many INDEPENDENT edges rotation actually tried.
fn distinct_rotate_edges(c: &VariationCtx) -> std::collections::BTreeSet<String> {
    let mut edges = std::collections::BTreeSet::new();
    for e in c.of_kind("note") {
        if let Some(rest) = e.detail.strip_prefix("rotate ") {
            // "cdn3.booksdl.lc → 500" → "cdn3.booksdl.lc"
            let edge = rest.split(" → ").next().unwrap_or(rest).trim();
            if !edge.is_empty() {
                edges.insert(edge.to_string());
            }
        }
    }
    edges
}

/// Parse the SCHEDULED backoff (seconds) out of a `retry` event detail
/// ("retry attempt 3 on libgen.vg after 4s backoff — …"). `None` if absent.
fn retry_backoff_secs(detail: &str) -> Option<u64> {
    let after = detail.split(" after ").nth(1)?;
    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// ---------------------------------------------------------------------------
// Tree walk
// ---------------------------------------------------------------------------

fn walk_group<'a>(prefix: &str, group: &'a Group, out: &mut Vec<(String, &'a BookRequest)>) {
    let here = if prefix.is_empty() {
        group.name.clone()
    } else {
        format!("{prefix} / {}", group.name)
    };
    for book in &group.books {
        out.push((here.clone(), book));
    }
    for sub in &group.subgroups {
        walk_group(&here, sub, out);
    }
}

/// Build the per-variation context for every Candidate that has a `job`.
fn variations_of<'a>(label_prefix: &str, book: &'a BookRequest) -> Vec<VariationCtx<'a>> {
    let title = &book.input.title;
    let any_done = book.candidates.iter().any(|c| {
        c.job
            .as_ref()
            .map(|j| j.state == JobState::Done)
            .unwrap_or(false)
    });
    let mut out = Vec::new();
    for cand in &book.candidates {
        let Some(job) = &cand.job else { continue };
        let fmt = cand
            .extension
            .as_ref()
            .map(|e| e.ext())
            .unwrap_or_else(|| "?".into());
        let events: Vec<&BookEvent> = book
            .history
            .iter()
            .filter(|e| e.md5.as_deref() == Some(cand.md5.as_str()))
            .collect();
        let sibling_done = any_done && job.state != JobState::Done;
        out.push(VariationCtx {
            label: format!("{label_prefix} / {title} [{fmt}]"),
            md5: cand_md5(cand),
            job,
            events,
            sibling_done,
        });
    }
    out
}

fn cand_md5(c: &Candidate) -> &str {
    &c.md5
}

fn main() -> anyhow::Result<()> {
    let db = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p libgen-core --example download_doctor -- <db-path>");
    let store = Store::open(&db)?;
    let lists = store.all_lists()?;

    let mut total_vars = 0usize;
    let mut vars_with_violations = 0usize;
    let mut by_invariant: BTreeMap<&'static str, usize> = BTreeMap::new();
    for inv in INVARIANTS {
        by_invariant.insert(inv.name, 0);
    }

    println!(
        "Download Doctor — checking {} list(s) in {db}\n",
        lists.len()
    );

    for sl in &lists {
        let list_title = &sl.list.title;
        let mut books: Vec<(String, &BookRequest)> = Vec::new();
        for g in &sl.list.groups {
            walk_group(list_title, g, &mut books);
        }
        for (group_label, book) in books {
            for v in variations_of(&group_label, book) {
                total_vars += 1;
                let mut violations: Vec<(&Invariant, String)> = Vec::new();
                for inv in INVARIANTS {
                    if let Err(evidence) = (inv.check)(&v) {
                        violations.push((inv, evidence));
                        *by_invariant.get_mut(inv.name).unwrap() += 1;
                    }
                }
                if violations.is_empty() {
                    continue;
                }
                vars_with_violations += 1;
                println!("✗ {}", v.label);
                println!(
                    "    md5={} state={:?} attempts={} bytes={}/{} verified={} host={:?}",
                    v.md5,
                    v.job.state,
                    v.job.attempts,
                    v.job.bytes_done,
                    v.job
                        .total_bytes
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "?".into()),
                    v.job.md5_verified,
                    v.job.host,
                );
                for (inv, evidence) in &violations {
                    println!("    ⚠ [{}] {}", inv.name, evidence);
                    println!("        intent : {}", inv.intent);
                    println!("        derived: {}", inv.derived_from);
                }
                println!();
            }
        }
    }

    println!("─────────────────────────────────────────────────────");
    println!(
        "Summary: {total_vars} download variation(s); {vars_with_violations} with \u{2265}1 violation."
    );
    println!("Violations by invariant:");
    for inv in INVARIANTS {
        println!("    {:>5}  {}", by_invariant[inv.name], inv.name);
    }
    Ok(())
}
