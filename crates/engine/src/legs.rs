//! Shared **leg-tracking** state machine — the single source of truth for the
//! active-downloads panel in *both* frontends (the `kwire` TUI and the Tauri
//! desktop). It models the concurrent **legs** of one download (the primary plus
//! any speculative **hedge** legs the scheduler launches) from the live
//! [`Progress`] event stream, keyed by the backend's authoritative `leg_id`.
//!
//! This is a faithful Rust port of the desktop's former JavaScript reconstruction
//! (`LEGS` / `noteLeg` / `legsFor` / `primaryLeg` in `app/ui/index.html`); the
//! contract is specified in `docs/LEG_LIFECYCLE.md`.
//!
//! ## Identity & lifecycle (see `docs/LEG_LIFECYCLE.md`)
//! - A leg is keyed by `leg_id` (assigned in start order: primary = 0, hedges
//!   1,2,…), **not** by host — so a leg that changes host (failover or cdn
//!   edge-rotation) stays one leg instead of spawning a phantom second line.
//! - Removal is **explicit**: a per-leg [`Progress::LegEnded`] drops just that
//!   leg; a whole-download terminal ([`Progress::Done`]/[`Progress::Failed`]/
//!   [`Progress::Cancelled`]) clears every leg of the md5.
//! - A long **60s TTL** ([`LEG_TTL_MS`]) is a pure backstop against a *lost*
//!   `LegEnded` (the Drop guard's `try_send` can fail on a full channel); it is
//!   never a liveness timer, so an alive-but-silent leg (TTFB/connect) is kept.
//!
//! ## Display rule
//! Among the **live** legs of one md5, the **lowest `leg_id` = primary** (earliest
//! start); the rest are **alt copies**. The headline `%` follows the primary, so a
//! slower hedge can never drag it backward. `is_hedge` is carried for diagnostics
//! but the *displayed* primary/alt split is purely the start-order rule above (a
//! promoted survivor reads as primary).

use std::collections::{BTreeMap, HashMap};

use libgen_core::queue::Progress;

/// Backstop TTL (ms) for a leg with no `LegEnded` and no recent event. Long
/// enough never to false-drop a connecting/TTFB leg; see `docs/LEG_LIFECYCLE.md` §3.
pub const LEG_TTL_MS: u64 = 60_000;

/// Internal per-leg record. Most fields are `Option` so a sparse event (e.g.
/// `FailingOver`, which carries no new host) carries forward the prior value
/// rather than clobbering it — matching the desktop `noteLeg` upsert.
#[derive(Debug, Clone, Default)]
struct Leg {
    leg_id: u64,
    host: Option<String>,
    bytes_done: Option<u64>,
    total_bytes: Option<u64>,
    speed_bps: Option<u64>,
    eta_secs: Option<u64>,
    is_hedge: bool,
    /// `now_ms` of the last keep-alive — the TTL backstop reads this.
    last_seen_ms: u64,
}

impl Leg {
    /// Progress percent (0..=100) from this leg's own bytes/total, or 0 when the
    /// total is unknown.
    fn progress(&self) -> u32 {
        match (self.bytes_done, self.total_bytes) {
            (Some(done), Some(total)) if total > 0 => {
                ((done as f64 / total as f64) * 100.0).round().min(100.0) as u32
            }
            _ => 0,
        }
    }
}

/// Read-side projection of one leg, returned by [`LegTracker::legs_for`] /
/// [`LegTracker::primary`]. `is_alt` is `true` for every non-primary (alt-copy)
/// leg; `progress` is derived from this leg's own bytes/total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegView {
    pub leg_id: u64,
    pub host: Option<String>,
    pub bytes_done: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_secs: Option<u64>,
    pub is_hedge: bool,
    /// `true` for non-primary legs — the UI badges these as "· alt copy".
    pub is_alt: bool,
    pub progress: u32,
}

impl LegView {
    fn from_leg(leg: &Leg, is_alt: bool) -> Self {
        LegView {
            leg_id: leg.leg_id,
            host: leg.host.clone(),
            bytes_done: leg.bytes_done,
            total_bytes: leg.total_bytes,
            speed_bps: leg.speed_bps,
            eta_secs: leg.eta_secs,
            is_hedge: leg.is_hedge,
            is_alt,
            progress: leg.progress(),
        }
    }
}

/// Per-leg fields extracted from a single [`Progress`] event, used to upsert the
/// matching leg. `None` host/bytes/etc. mean "carry forward the prior value".
struct LegUpdate {
    leg_id: u64,
    is_hedge: bool,
    host: Option<String>,
    bytes_done: Option<u64>,
    total_bytes: Option<u64>,
    /// `Some(..)` only for `Bytes` events (the only carrier of speed/eta); `None`
    /// otherwise so non-`Bytes` keep-alives leave speed/eta untouched.
    speed_eta: Option<(Option<u64>, Option<u64>)>,
}

/// Tracks the live legs of every in-flight download, keyed `md5 → leg_id → leg`.
/// Fed [`Progress`] events via [`note`](Self::note); read via
/// [`legs_for`](Self::legs_for) / [`primary`](Self::primary). Holds no clock —
/// the caller supplies a monotonic `now_ms` so tests stay deterministic.
#[derive(Debug, Clone, Default)]
pub struct LegTracker {
    by_md5: HashMap<String, BTreeMap<u64, Leg>>,
}

impl LegTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one [`Progress`] event. `now_ms` is a monotonic millisecond clock
    /// (e.g. `Instant::elapsed`), used only to stamp keep-alives for the TTL
    /// backstop. Mirrors the desktop `noteLeg`.
    pub fn note(&mut self, p: &Progress, now_ms: u64) {
        match p {
            // Whole-download terminals clear every leg of the md5.
            Progress::Done { md5, .. }
            | Progress::Failed { md5, .. }
            | Progress::Cancelled { md5, .. } => {
                self.by_md5.remove(md5);
            }
            // One specific leg ended → remove just it; survivors remain (a lower
            // one ending promotes the next as primary by leg_id order).
            Progress::LegEnded { md5, leg_id } => {
                if let Some(legs) = self.by_md5.get_mut(md5) {
                    legs.remove(leg_id);
                    if legs.is_empty() {
                        self.by_md5.remove(md5);
                    }
                }
            }
            // Non-leg diagnostic — ignored.
            Progress::Note { .. } => {}
            // Every other variant is a per-leg keep-alive carrying leg_id.
            _ => {
                if let (Some(md5), Some(upd)) = (progress_md5(p), leg_update(p)) {
                    self.upsert(md5, upd, now_ms);
                }
            }
        }
    }

    fn upsert(&mut self, md5: &str, upd: LegUpdate, now_ms: u64) {
        let legs = self.by_md5.entry(md5.to_string()).or_default();
        let leg = legs.entry(upd.leg_id).or_default();
        leg.leg_id = upd.leg_id;
        leg.is_hedge = upd.is_hedge;
        if upd.host.is_some() {
            leg.host = upd.host;
        }
        if upd.bytes_done.is_some() {
            leg.bytes_done = upd.bytes_done;
        }
        if upd.total_bytes.is_some() {
            leg.total_bytes = upd.total_bytes;
        }
        // speed/eta only ride on Bytes events; other events leave them as-is.
        // Even on a Bytes event the engine omits them until speed is measurable
        // (the first tick, or just after a resume/failover that restarts the
        // speed tracker) — carry the last known value forward rather than blanking
        // an actively-progressing leg back to "tbd".
        if let Some((speed, eta)) = upd.speed_eta {
            if speed.is_some() {
                leg.speed_bps = speed;
            }
            if eta.is_some() {
                leg.eta_secs = eta;
            }
        }
        leg.last_seen_ms = now_ms;
    }

    /// The live legs of `md5` to display: sorted by `leg_id` (= start order), with
    /// any leg older than [`LEG_TTL_MS`] dropped. The lowest live `leg_id` is the
    /// primary (`is_alt = false`); the rest are alt copies (`is_alt = true`).
    /// Empty when there are no live legs — the caller falls back to its viewmodel.
    pub fn legs_for(&self, md5: &str, now_ms: u64) -> Vec<LegView> {
        let Some(legs) = self.by_md5.get(md5) else {
            return Vec::new();
        };
        // BTreeMap already yields leg_ids ascending.
        legs.values()
            .filter(|leg| is_live(leg, now_ms))
            .enumerate()
            .map(|(i, leg)| LegView::from_leg(leg, i > 0))
            .collect()
    }

    /// The primary leg of `md5` = the live leg with the lowest `leg_id` (earliest
    /// start), or `None` when there are no live legs. The headline `%` follows it.
    pub fn primary(&self, md5: &str, now_ms: u64) -> Option<LegView> {
        let legs = self.by_md5.get(md5)?;
        legs.values()
            .find(|leg| is_live(leg, now_ms))
            .map(|leg| LegView::from_leg(leg, false))
    }

    /// `true` when `md5` has at least one live leg (cheaper than `legs_for`).
    pub fn has_live_legs(&self, md5: &str, now_ms: u64) -> bool {
        self.by_md5
            .get(md5)
            .is_some_and(|legs| legs.values().any(|leg| is_live(leg, now_ms)))
    }
}

fn is_live(leg: &Leg, now_ms: u64) -> bool {
    now_ms.saturating_sub(leg.last_seen_ms) < LEG_TTL_MS
}

/// The md5 of any per-leg [`Progress`] variant (the terminals/`LegEnded`/`Note`
/// are handled before this is called).
fn progress_md5(p: &Progress) -> Option<&str> {
    match p {
        Progress::Resolved { md5, .. }
        | Progress::Resuming { md5, .. }
        | Progress::Bytes { md5, .. }
        | Progress::Stalled { md5, .. }
        | Progress::Retrying { md5, .. }
        | Progress::FailingOver { md5, .. } => Some(md5),
        _ => None,
    }
}

/// Extract the upsert fields from a per-leg [`Progress`] variant. Returns `None`
/// for non-per-leg variants (handled earlier in [`LegTracker::note`]).
fn leg_update(p: &Progress) -> Option<LegUpdate> {
    Some(match p {
        Progress::Resolved {
            leg_id,
            is_hedge,
            host,
            total_bytes,
            ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: Some(host.clone()),
            bytes_done: None,
            total_bytes: *total_bytes,
            speed_eta: None,
        },
        Progress::Resuming {
            leg_id,
            is_hedge,
            host,
            offset,
            ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: Some(host.clone()),
            // `offset` is the bytes already on disk — the resume seed.
            bytes_done: Some(*offset),
            total_bytes: None,
            speed_eta: None,
        },
        Progress::Bytes {
            leg_id,
            is_hedge,
            host,
            bytes_done,
            total_bytes,
            speed_bps,
            eta_secs,
            ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: Some(host.clone()),
            bytes_done: Some(*bytes_done),
            total_bytes: *total_bytes,
            speed_eta: Some((*speed_bps, *eta_secs)),
        },
        Progress::Stalled {
            leg_id,
            is_hedge,
            host,
            bytes_done,
            ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: Some(host.clone()),
            bytes_done: Some(*bytes_done),
            total_bytes: None,
            speed_eta: None,
        },
        Progress::Retrying {
            leg_id,
            is_hedge,
            host,
            ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: Some(host.clone()),
            bytes_done: None,
            total_bytes: None,
            speed_eta: None,
        },
        // FailingOver carries only `from_host` (no destination yet) — keep-alive
        // the same leg_id; the next `Resolved` lands the new host on this leg.
        Progress::FailingOver {
            leg_id, is_hedge, ..
        } => LegUpdate {
            leg_id: *leg_id,
            is_hedge: *is_hedge,
            host: None,
            bytes_done: None,
            total_bytes: None,
            speed_eta: None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MD5: &str = "abc123";

    fn resolved(leg_id: u64, is_hedge: bool, host: &str, total: Option<u64>) -> Progress {
        Progress::Resolved {
            md5: MD5.to_string(),
            leg_id,
            is_hedge,
            host: host.to_string(),
            total_bytes: total,
        }
    }

    fn bytes(leg_id: u64, is_hedge: bool, host: &str, done: u64, total: u64) -> Progress {
        Progress::Bytes {
            md5: MD5.to_string(),
            leg_id,
            is_hedge,
            host: host.to_string(),
            bytes_done: done,
            total_bytes: Some(total),
            speed_bps: Some(1_000),
            eta_secs: Some(10),
        }
    }

    #[test]
    fn cdn_edge_rotation_stays_one_leg() {
        // One leg whose host changes (edge-rotation) must NOT spawn a phantom
        // second line: same leg_id → one leg.
        let mut t = LegTracker::new();
        t.note(&resolved(0, false, "cdn1.booksdl.lc", Some(100)), 0);
        t.note(&bytes(0, false, "cdn1.booksdl.lc", 10, 100), 100);
        t.note(&bytes(0, false, "cdn3.booksdl.lc", 25, 100), 200);
        let legs = t.legs_for(MD5, 300);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].leg_id, 0);
        assert_eq!(legs[0].host.as_deref(), Some("cdn3.booksdl.lc"));
        assert_eq!(legs[0].progress, 25);
        assert!(!legs[0].is_alt);
    }

    #[test]
    fn hedge_makes_two_legs_lowest_id_primary() {
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 30, 100), 0);
        t.note(&bytes(1, true, "host-b", 10, 100), 0);
        let legs = t.legs_for(MD5, 0);
        assert_eq!(legs.len(), 2);
        // Primary = lowest live leg_id.
        assert_eq!(legs[0].leg_id, 0);
        assert!(!legs[0].is_alt);
        assert_eq!(legs[0].host.as_deref(), Some("host-a"));
        // Alt copy.
        assert_eq!(legs[1].leg_id, 1);
        assert!(legs[1].is_alt);
        assert_eq!(legs[1].host.as_deref(), Some("host-b"));
        assert!(legs[1].is_hedge);
        // Headline progress follows the primary, not the hedge.
        assert_eq!(t.primary(MD5, 0).unwrap().progress, 30);
    }

    #[test]
    fn leg_ended_on_primary_promotes_survivor() {
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 30, 100), 0);
        t.note(&bytes(1, true, "host-b", 80, 100), 0);
        // The primary (leg 0) dies; the hedge survives and is promoted.
        t.note(
            &Progress::LegEnded {
                md5: MD5.to_string(),
                leg_id: 0,
            },
            10,
        );
        let legs = t.legs_for(MD5, 10);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].leg_id, 1);
        assert!(!legs[0].is_alt, "promoted survivor reads as primary");
        assert_eq!(t.primary(MD5, 10).unwrap().leg_id, 1);
    }

    #[test]
    fn silent_leg_survives_short_gap_but_dies_after_ttl() {
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 10, 100), 0);
        // Alive-but-silent for 6s (old TTL) — still present.
        assert_eq!(t.legs_for(MD5, 6_000).len(), 1);
        // Past the 60s backstop with no event — dropped.
        assert_eq!(t.legs_for(MD5, LEG_TTL_MS + 1).len(), 0);
        assert!(t.primary(MD5, LEG_TTL_MS + 1).is_none());
    }

    #[test]
    fn keepalive_event_refreshes_ttl() {
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 10, 100), 0);
        // A Stalled keep-alive just before the TTL keeps the leg alive.
        t.note(
            &Progress::Stalled {
                md5: MD5.to_string(),
                leg_id: 0,
                is_hedge: false,
                host: "host-a".to_string(),
                bytes_done: 10,
                speed_bps: Some(0),
            },
            LEG_TTL_MS - 1,
        );
        assert_eq!(t.legs_for(MD5, LEG_TTL_MS + 10).len(), 1);
    }

    #[test]
    fn terminal_clears_all_legs() {
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 30, 100), 0);
        t.note(&bytes(1, true, "host-b", 10, 100), 0);
        t.note(
            &Progress::Done {
                md5: MD5.to_string(),
                host: "host-a".to_string(),
                path: std::path::PathBuf::from("/tmp/x"),
                bytes_written: 100,
            },
            5,
        );
        assert!(t.legs_for(MD5, 5).is_empty());
        assert!(!t.has_live_legs(MD5, 5));
    }

    #[test]
    fn failing_over_keeps_leg_until_next_resolved() {
        let mut t = LegTracker::new();
        t.note(&resolved(0, false, "host-a", Some(100)), 0);
        t.note(&bytes(0, false, "host-a", 40, 100), 100);
        // FailingOver carries no new host — leg kept, host unchanged, %, retained.
        t.note(
            &Progress::FailingOver {
                md5: MD5.to_string(),
                leg_id: 0,
                is_hedge: false,
                from_host: "host-a".to_string(),
                error: "exhausted".to_string(),
            },
            200,
        );
        let legs = t.legs_for(MD5, 200);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].host.as_deref(), Some("host-a"));
        assert_eq!(legs[0].progress, 40);
        // Next Resolved lands the new host on the SAME leg.
        t.note(&resolved(0, false, "host-c", Some(100)), 300);
        assert_eq!(t.legs_for(MD5, 300)[0].host.as_deref(), Some("host-c"));
    }

    #[test]
    fn bytes_tick_without_speed_eta_keeps_last_known() {
        // The engine omits speed/eta on a Bytes tick until measurable (first tick /
        // post-resume / post-failover). Such a tick must NOT blank an actively-
        // progressing leg back to no-speed/no-eta ("tbd").
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 10, 100), 0);
        assert_eq!(t.primary(MD5, 0).unwrap().speed_bps, Some(1_000));
        // A later Bytes tick advances bytes but carries no speed/eta.
        t.note(
            &Progress::Bytes {
                md5: MD5.to_string(),
                leg_id: 0,
                is_hedge: false,
                host: "host-a".to_string(),
                bytes_done: 20,
                total_bytes: Some(100),
                speed_bps: None,
                eta_secs: None,
            },
            10,
        );
        let leg = t.primary(MD5, 10).unwrap();
        assert_eq!(leg.bytes_done, Some(20), "bytes still advance");
        assert_eq!(
            leg.speed_bps,
            Some(1_000),
            "speed carried forward, not blanked"
        );
        assert_eq!(leg.eta_secs, Some(10), "eta carried forward, not blanked");
    }

    #[test]
    fn retrying_carries_forward_speed_and_eta() {
        // Non-Bytes events must not clobber speed/eta (carry-forward).
        let mut t = LegTracker::new();
        t.note(&bytes(0, false, "host-a", 10, 100), 0);
        let before = t.primary(MD5, 0).unwrap();
        assert_eq!(before.speed_bps, Some(1_000));
        t.note(
            &Progress::Retrying {
                md5: MD5.to_string(),
                leg_id: 0,
                is_hedge: false,
                host: "host-a".to_string(),
                attempt: 1,
                backoff: Duration::from_secs(1),
                error: "transient".to_string(),
            },
            10,
        );
        let after = t.primary(MD5, 10).unwrap();
        assert_eq!(after.speed_bps, Some(1_000), "speed carried forward");
        assert_eq!(after.eta_secs, Some(10), "eta carried forward");
    }
}
