//! Shared-service round trips (issue #29 §4-6, V67 `shared_service_rtt`).
//!
//! One row per HTTP round trip the device makes to a shared service (the Batch relay today):
//! route, milliseconds, HTTP status. **No URL, no batch id, no content** — there is no column for
//! any of them. `shogun metrics` reads the p50/p95 per route from here, in its own block apart from
//! the NFR-SLO rows (those are local latencies; these depend on the network and the server).

use rusqlite::{params, Connection};

use crate::traceability::Route;

/// Append one round trip. `status` is the HTTP status, `0` when no response arrived.
pub fn insert(
    conn: &Connection,
    ts: i64,
    route: Route,
    rtt_ms: i64,
    status: u16,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO shared_service_rtt (ts, route, rtt_ms, status) VALUES (?1, ?2, ?3, ?4)",
        params![
            ts,
            route.as_str(),
            rtt_ms.max(0),
            i64::from(status.min(999))
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// One route's standing over the read window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteLatency {
    pub route: Route,
    pub count: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Round trips answered 429 / 503 (the service said "not now").
    pub throttled: u64,
}

/// Nearest-rank quantile over sorted samples (`q` in 0..=1). Empty → 0.
fn quantile(sorted: &[i64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted.len()) - 1;
    sorted[idx] as f64
}

/// Per-route p50 / p95 for round trips at or after `since_ts`, routes in a stable order. A route
/// with no samples is absent (unmeasured is never reported as zero).
pub fn latencies_since(
    conn: &Connection,
    since_ts: i64,
) -> Result<Vec<RouteLatency>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT route, rtt_ms, status FROM shared_service_rtt WHERE ts >= ?1 ORDER BY route, rtt_ms",
    )?;
    let rows = stmt.query_map(params![since_ts], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out: Vec<RouteLatency> = Vec::new();
    let mut current: Option<(Route, Vec<i64>, u64)> = None;
    let flush = |cur: Option<(Route, Vec<i64>, u64)>, out: &mut Vec<RouteLatency>| {
        if let Some((route, ms, throttled)) = cur {
            out.push(RouteLatency {
                route,
                count: ms.len() as u64,
                p50_ms: quantile(&ms, 0.50),
                p95_ms: quantile(&ms, 0.95),
                throttled,
            });
        }
    };
    for row in rows {
        let (route, ms, status) = row?;
        let Some(route) = Route::parse(&route) else {
            continue;
        };
        let throttled = u64::from(status == 429 || status == 503);
        match current.as_mut() {
            Some((r, samples, t)) if *r == route => {
                samples.push(ms);
                *t += throttled;
            }
            _ => {
                flush(current.take(), &mut out);
                current = Some((route, vec![ms], throttled));
            }
        }
    }
    flush(current, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_is_per_route_and_only_inside_the_window() {
        let conn = crate::open_in_memory().unwrap();
        for (i, ms) in [100, 120, 140, 160, 900].iter().enumerate() {
            insert(&conn, 1_000 + i as i64, Route::BatchRelay, *ms, 200).unwrap();
        }
        insert(&conn, 1_010, Route::BatchRelay, 50, 429).unwrap();
        insert(&conn, 1_020, Route::Billing, 300, 200).unwrap();
        // Outside the window: ignored.
        insert(&conn, 10, Route::BatchRelay, 99_999, 200).unwrap();

        let got = latencies_since(&conn, 1_000).unwrap();
        assert_eq!(got.len(), 2);
        let relay = got.iter().find(|l| l.route == Route::BatchRelay).unwrap();
        assert_eq!(relay.count, 6);
        assert_eq!(relay.p50_ms, 120.0);
        assert_eq!(relay.p95_ms, 900.0);
        assert_eq!(relay.throttled, 1);
        let billing = got.iter().find(|l| l.route == Route::Billing).unwrap();
        assert_eq!((billing.count, billing.p95_ms), (1, 300.0));
    }

    #[test]
    fn no_samples_is_absent_not_zero() {
        let conn = crate::open_in_memory().unwrap();
        assert!(latencies_since(&conn, 0).unwrap().is_empty());
    }

    #[test]
    fn the_table_has_no_place_for_content() {
        let conn = crate::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('shared_service_rtt')")
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(cols, vec!["id", "ts", "route", "rtt_ms", "status"]);
    }
}
