//! Rendering a [`Report`] for people (a table) and for programs (JSON).

use super::{Report, Status};
use std::fmt::Write as _;

/// Version of the JSON shape produced by [`Report::to_json`]. Bump it on any
/// incompatible change; adding fields is compatible.
pub const JSON_SCHEMA: u32 = 1;

impl Status {
    /// The table glyph for this status.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Pass => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
            Self::Skip => "-",
        }
    }
}

impl Report {
    /// A human table: one line per check (glyph, title, summary, duration),
    /// indented detail and remedy lines, then the totals.
    #[must_use]
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            let r = &e.result;
            let _ = write!(out, "{} {} — {}", r.status.glyph(), e.title, r.summary);
            if r.status != Status::Skip {
                let _ = write!(out, " ({} ms)", r.duration_ms);
            }
            out.push('\n');
            if let Some(detail) = &r.detail {
                let _ = writeln!(out, "    {detail}");
            }
            if let Some(remedy) = &r.remedy {
                let _ = writeln!(out, "    → {remedy}");
            }
        }
        let c = self.counts();
        let _ = writeln!(
            out,
            "\n{} checks: {} passed, {} warned, {} failed, {} skipped",
            self.entries.len(),
            c.pass,
            c.warn,
            c.fail,
            c.skip
        );
        out
    }

    /// The report as versioned JSON:
    /// `{"schema", "exit_code", "counts", "checks": [{id, title, status,
    /// summary, detail, remedy, evidence, duration_ms}]}`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": JSON_SCHEMA,
            "exit_code": self.exit_code(),
            "counts": self.counts(),
            "checks": self.entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::doctor::{CheckId, CheckResult, Entry, Report};
    use serde_json::json;

    fn sample() -> Report {
        let entry = |id: &'static str, title: &str, result: CheckResult, ms: u64| Entry {
            id: CheckId(id),
            title: title.to_owned(),
            result: CheckResult {
                duration_ms: ms,
                ..result
            },
        };
        Report {
            entries: vec![
                entry(
                    "lan.ssdp",
                    "SSDP discovery",
                    CheckResult::pass("3 players answered").with_evidence(json!({ "players": 3 })),
                    12,
                ),
                entry(
                    "lan.gena_callback",
                    "GENA callback reachable",
                    CheckResult::warn(
                        "only 2 of 3 players reached the callback",
                        "Allow inbound TCP to the callback port in the host firewall.",
                    )
                    .with_detail("unreached: one player"),
                    40,
                ),
                entry(
                    "spotify.render_params.s1",
                    "Spotify render parameters (S1)",
                    CheckResult::fail(
                        "no Spotify favorite in this household",
                        "Add any Spotify track to Sonos Favorites in the S1 app, then re-run.",
                    ),
                    3,
                ),
                entry(
                    "spotify.enqueue.s1",
                    "Spotify enqueue (S1)",
                    CheckResult::skip("skipped because spotify.render_params.s1 failed"),
                    0,
                ),
            ],
        }
    }

    #[test]
    fn table_snapshot() {
        assert_eq!(
            sample().render_table(),
            "\
✓ SSDP discovery — 3 players answered (12 ms)
! GENA callback reachable — only 2 of 3 players reached the callback (40 ms)
    unreached: one player
    → Allow inbound TCP to the callback port in the host firewall.
✗ Spotify render parameters (S1) — no Spotify favorite in this household (3 ms)
    → Add any Spotify track to Sonos Favorites in the S1 app, then re-run.
- Spotify enqueue (S1) — skipped because spotify.render_params.s1 failed

4 checks: 1 passed, 1 warned, 1 failed, 1 skipped
"
        );
    }

    #[test]
    fn json_snapshot() {
        assert_eq!(
            sample().to_json(),
            json!({
                "schema": 1,
                "exit_code": 7,
                "counts": { "pass": 1, "warn": 1, "fail": 1, "skip": 1 },
                "checks": [
                    {
                        "id": "lan.ssdp",
                        "title": "SSDP discovery",
                        "status": "pass",
                        "summary": "3 players answered",
                        "detail": null,
                        "remedy": null,
                        "evidence": { "players": 3 },
                        "duration_ms": 12
                    },
                    {
                        "id": "lan.gena_callback",
                        "title": "GENA callback reachable",
                        "status": "warn",
                        "summary": "only 2 of 3 players reached the callback",
                        "detail": "unreached: one player",
                        "remedy": "Allow inbound TCP to the callback port in the host firewall.",
                        "evidence": null,
                        "duration_ms": 40
                    },
                    {
                        "id": "spotify.render_params.s1",
                        "title": "Spotify render parameters (S1)",
                        "status": "fail",
                        "summary": "no Spotify favorite in this household",
                        "detail": null,
                        "remedy": "Add any Spotify track to Sonos Favorites in the S1 app, then re-run.",
                        "evidence": null,
                        "duration_ms": 3
                    },
                    {
                        "id": "spotify.enqueue.s1",
                        "title": "Spotify enqueue (S1)",
                        "status": "skip",
                        "summary": "skipped because spotify.render_params.s1 failed",
                        "detail": null,
                        "remedy": null,
                        "evidence": null,
                        "duration_ms": 0
                    }
                ]
            })
        );
    }
}
