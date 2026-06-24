//! Auto-prioritize query/download sites: order a host list by a blend of **live
//! availability** (open-slum.org, see [`crate::slum`]) and **measured quality**
//! (per-host success rate + latency persisted in `site_quality`, see
//! [`crate::store::SiteQuality`]).
//!
//! Pure + deterministic so it unit-tests offline and can be applied at
//! scheduler-/search-client build time. A host is **never dropped** — one SLUM
//! reports down (or that has been failing) just sinks to the back, so the engine
//! still falls over to it if everything better is unreachable (resilient when
//! SLUM itself is stale or down). See docs/DOWNLOAD_SCHEDULING.md §6.

use crate::slum::SlumReport;
use crate::store::SiteQuality;
use std::collections::HashMap;

/// Score one host. Higher is better. Combines (in rough priority order):
///   * live SLUM availability — up + 24h-uptime is the strongest signal; a
///     SLUM-down host is heavily penalized; unknown is neutral,
///   * measured success rate (Laplace-smoothed, 0.5 = no data),
///   * a small latency penalty (EWMA ms),
///   * a tiny position prior so the configured order breaks exact ties.
fn score(
    host: &str,
    idx: usize,
    n: usize,
    slum: Option<&SlumReport>,
    q: Option<&SiteQuality>,
) -> f64 {
    let mut s = 0.0;

    // Live availability (dominant term).
    match slum.and_then(|r| r.site_for_host(host)) {
        Some(site) if site.up => {
            s += 2.0 + site.uptime_24h.unwrap_or(0.0); // up: 2.0..3.0
        }
        Some(_) => s -= 3.0, // SLUM says down → sink (but keep as fallback)
        None => s += 1.0,    // not monitored → neutral-positive
    }

    // Measured reliability: success_rate ∈ [0,1], 0.5 neutral → ±0.5 contribution.
    if let Some(q) = q {
        s += (q.success_rate() - 0.5) * 1.5;
        // Latency: gentle penalty, capped so a slow-but-working host still beats a
        // down one.
        if let Some(ms) = q.ewma_ms {
            s -= (ms / 20_000.0).min(0.4);
        }
    }

    // Position prior (tiebreak only): earlier in the configured order = a hair better.
    if n > 0 {
        s += (n - idx) as f64 / n as f64 * 0.05;
    }
    s
}

/// Return `default_order` reordered best-first by [`score`]. Every input host is
/// present in the output exactly once (nothing is dropped). Stable for equal
/// scores (preserves the configured order).
pub fn order_hosts(
    default_order: &[String],
    slum: Option<&SlumReport>,
    quality: &[SiteQuality],
) -> Vec<String> {
    let qmap: HashMap<&str, &SiteQuality> = quality.iter().map(|q| (q.host.as_str(), q)).collect();
    let n = default_order.len();
    let mut scored: Vec<(usize, f64, &String)> = default_order
        .iter()
        .enumerate()
        .map(|(idx, host)| {
            let q = qmap.get(host.as_str()).copied();
            (idx, score(host, idx, n, slum, q), host)
        })
        .collect();
    // Sort by score DESC; ties keep original order (idx ASC) for stability.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.into_iter().map(|(_, _, h)| h.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slum::{SlumReport, SlumSite};
    use crate::store::SiteQuality;

    fn site(host: &str, up: bool, uptime: f64) -> SlumSite {
        SlumSite {
            host: host.into(),
            url: format!("https://{host}/"),
            name: host.into(),
            group: "G".into(),
            up,
            ping_ms: Some(100),
            uptime_24h: Some(uptime),
        }
    }

    fn quality(host: &str, ok: u64, fail: u64, ewma: Option<f64>) -> SiteQuality {
        SiteQuality {
            host: host.into(),
            successes: ok,
            failures: fail,
            ewma_ms: ewma,
            last_ok: None,
            last_fail: None,
        }
    }

    fn hosts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identity_when_no_signals() {
        // No SLUM, no quality → configured order preserved.
        let order = hosts(&["a", "b", "c"]);
        assert_eq!(order_hosts(&order, None, &[]), order);
    }

    #[test]
    fn slum_down_host_sinks_but_is_kept() {
        let order = hosts(&["dead", "live"]);
        let report = SlumReport {
            sites: vec![site("dead", false, 0.0), site("live", true, 0.99)],
        };
        let out = order_hosts(&order, Some(&report), &[]);
        // The live host is promoted; the dead one is kept as a last-resort fallback.
        assert_eq!(out, hosts(&["live", "dead"]));
    }

    #[test]
    fn measured_success_breaks_between_unmonitored_hosts() {
        // Neither host in SLUM; the one with the better measured record wins.
        let order = hosts(&["flaky", "solid"]);
        let q = vec![
            quality("flaky", 1, 9, Some(500.0)),
            quality("solid", 20, 0, Some(300.0)),
        ];
        let out = order_hosts(&order, None, &q);
        assert_eq!(out, hosts(&["solid", "flaky"]));
    }

    #[test]
    fn live_uptime_orders_among_up_hosts() {
        let order = hosts(&["lo", "hi"]);
        let report = SlumReport {
            sites: vec![site("lo", true, 0.80), site("hi", true, 0.99)],
        };
        let out = order_hosts(&order, Some(&report), &[]);
        assert_eq!(out, hosts(&["hi", "lo"]));
    }

    #[test]
    fn live_beats_unmonitored_beats_down() {
        let order = hosts(&["down", "unknown", "up"]);
        let report = SlumReport {
            sites: vec![site("down", false, 0.0), site("up", true, 0.95)],
            // "unknown" not present → neutral.
        };
        let out = order_hosts(&order, Some(&report), &[]);
        assert_eq!(out, hosts(&["up", "unknown", "down"]));
    }
}
