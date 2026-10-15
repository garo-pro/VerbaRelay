//! Statistics for the admin command line.
//!
//! Output is plain text in "label: value" form and simple aligned columns, which
//! reads well with a screen reader. `--json` gives machine-readable output.

use rusqlite::params;
use serde::Serialize;

use crate::db::{Db, now};

#[derive(Debug, Default, Serialize)]
pub struct JobStats {
    pub jobs: i64,
    pub done: i64,
    pub failed: i64,
    pub override_rejected: i64,
    pub audio_minutes_total: f64,
    pub audio_minutes_avg: f64,
    pub words_total: i64,
    pub words_avg: f64,
    pub words_per_audio_minute: f64,
    pub queue_ms_avg: f64,
    pub total_ms_avg: f64,
    pub split_jobs: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct PartStats {
    pub attempts: i64,
    pub declined: i64,
    pub offer_timeouts: i64,
    /// Time peers needed to answer an offer.
    pub offer_ms_avg: f64,
    pub transfer_ms_avg: f64,
    pub processing_ms_avg: f64,
    pub first_segment_ms_avg: f64,
    /// Audio length divided by processing time.
    pub realtime_factor_avg: f64,
}

#[derive(Debug, Serialize)]
pub struct PeerStats {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub accelerator: String,
    pub score: Option<f64>,
    pub online: bool,
    pub online_percent: f64,
    pub parts_done: i64,
    pub parts_failed: i64,
    pub offer_ms_avg: f64,
    pub realtime_factor_avg: f64,
    pub audio_minutes: f64,
    pub last_seen: i64,
    pub client_version: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub days: u32,
    pub jobs: JobStats,
    pub parts: PartStats,
    pub peers: Vec<PeerStats>,
}

fn accelerator_of(device_json: Option<&str>) -> String {
    device_json
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .and_then(|v| v.get("accelerator").and_then(|a| a.as_str()).map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

/// Collects all statistics of the last `days` days.
pub fn collect(db: &Db, days: u32) -> anyhow::Result<Report> {
    let end = now();
    let start = end - i64::from(days) * 86_400;

    let jobs = db.with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(status = 'done'), 0),
                    COALESCE(SUM(status = 'failed'), 0),
                    COALESCE(SUM(override_rejected), 0),
                    COALESCE(SUM(audio_ms), 0) / 60000.0,
                    COALESCE(AVG(audio_ms), 0) / 60000.0,
                    COALESCE(SUM(words), 0),
                    COALESCE(AVG(words), 0),
                    COALESCE(AVG(queue_ms), 0),
                    COALESCE(AVG(total_ms), 0),
                    COALESCE(SUM(parts > 1), 0)
             FROM jobs WHERE created_at >= ?1",
            params![start],
            |r| {
                Ok(JobStats {
                    jobs: r.get(0)?,
                    done: r.get(1)?,
                    failed: r.get(2)?,
                    override_rejected: r.get(3)?,
                    audio_minutes_total: r.get(4)?,
                    audio_minutes_avg: r.get(5)?,
                    words_total: r.get(6)?,
                    words_avg: r.get(7)?,
                    words_per_audio_minute: 0.0,
                    queue_ms_avg: r.get(8)?,
                    total_ms_avg: r.get(9)?,
                    split_jobs: r.get(10)?,
                })
            },
        )
    })?;
    let jobs = JobStats {
        words_per_audio_minute: if jobs.audio_minutes_total > 0.0 {
            jobs.words_total as f64 / jobs.audio_minutes_total
        } else {
            0.0
        },
        ..jobs
    };

    let parts = db.with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(status = 'declined'), 0),
                    COALESCE(SUM(status = 'offer_timeout'), 0),
                    COALESCE(AVG(offer_ms), 0),
                    COALESCE(AVG(transfer_ms), 0),
                    COALESCE(AVG(CASE WHEN status = 'done' THEN processing_ms END), 0),
                    COALESCE(AVG(first_segment_ms), 0),
                    COALESCE(AVG(CASE WHEN status = 'done' AND processing_ms > 0 THEN audio_ms * 1.0 / processing_ms END), 0)
             FROM job_parts WHERE created_at >= ?1",
            params![start],
            |r| {
                Ok(PartStats {
                    attempts: r.get(0)?,
                    declined: r.get(1)?,
                    offer_timeouts: r.get(2)?,
                    offer_ms_avg: r.get(3)?,
                    transfer_ms_avg: r.get(4)?,
                    processing_ms_avg: r.get(5)?,
                    first_segment_ms_avg: r.get(6)?,
                    realtime_factor_avg: r.get(7)?,
                })
            },
        )
    })?;

    let mut peers = Vec::new();
    for peer in db.list_peers()? {
        let id = peer.id.to_string();
        let window_start = start.max(peer.created_at);
        let window = (end - window_start).max(1);
        let (online_seconds, online): (i64, bool) = db.with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(MAX(0, MIN(COALESCE(disconnected_at, ?3), ?3) - MAX(connected_at, ?2))), 0),
                        COALESCE(SUM(disconnected_at IS NULL), 0) > 0
                 FROM peer_sessions WHERE peer_id = ?1",
                params![id, window_start, end],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        let (done, failed, offer_ms, rtf, audio_minutes): (i64, i64, f64, f64, f64) = db.with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(status = 'done'), 0),
                        COALESCE(SUM(status = 'failed'), 0),
                        COALESCE(AVG(offer_ms), 0),
                        COALESCE(AVG(CASE WHEN status = 'done' AND processing_ms > 0 THEN audio_ms * 1.0 / processing_ms END), 0),
                        COALESCE(SUM(CASE WHEN status = 'done' THEN audio_ms END), 0) / 60000.0
                 FROM job_parts WHERE peer_id = ?1 AND created_at >= ?2",
                params![id, start],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })?;
        peers.push(PeerStats {
            id,
            name: peer.name,
            backend: peer.backend.unwrap_or_else(|| "-".into()),
            accelerator: accelerator_of(peer.device_json.as_deref()),
            score: peer.score,
            online,
            online_percent: (online_seconds as f64 / window as f64 * 100.0).clamp(0.0, 100.0),
            parts_done: done,
            parts_failed: failed,
            offer_ms_avg: offer_ms,
            realtime_factor_avg: rtf,
            audio_minutes,
            last_seen: peer.last_seen,
            client_version: peer.client_version,
        });
    }
    peers.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)));

    Ok(Report {
        days,
        jobs,
        parts,
        peers,
    })
}

/// Formats a UNIX time as "2026-10-15 21:04 UTC".
pub fn format_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map_or_else(|| "-".into(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

/// Formats milliseconds as seconds with one decimal.
fn secs(ms: f64) -> String {
    format!("{:.1} s", ms / 1000.0)
}

/// Renders the report as plain text.
pub fn render(report: &Report) -> String {
    let j = &report.jobs;
    let p = &report.parts;
    let mut out = String::new();
    out.push_str(&format!("Statistics for the last {} days\n\n", report.days));
    out.push_str("Jobs\n");
    out.push_str(&format!("  Total: {} (done {}, failed {})\n", j.jobs, j.done, j.failed));
    out.push_str(&format!("  Split across several peers: {}\n", j.split_jobs));
    out.push_str(&format!("  Model override rejected: {}\n", j.override_rejected));
    out.push_str(&format!(
        "  Audio transcribed: {:.1} minutes (average {:.1} minutes per job)\n",
        j.audio_minutes_total, j.audio_minutes_avg
    ));
    out.push_str(&format!(
        "  Words: {} (average {:.0} per job, {:.0} per audio minute)\n",
        j.words_total, j.words_avg, j.words_per_audio_minute
    ));
    out.push_str(&format!("  Average wait for a peer: {}\n", secs(j.queue_ms_avg)));
    out.push_str(&format!("  Average total time per job: {}\n\n", secs(j.total_ms_avg)));

    out.push_str("Peer work\n");
    out.push_str(&format!(
        "  Attempts: {} (declined {}, offer timeouts {})\n",
        p.attempts, p.declined, p.offer_timeouts
    ));
    out.push_str(&format!("  Average offer response time: {:.0} ms\n", p.offer_ms_avg));
    out.push_str(&format!("  Average audio transfer time: {:.0} ms\n", p.transfer_ms_avg));
    out.push_str(&format!("  Average processing time: {}\n", secs(p.processing_ms_avg)));
    out.push_str(&format!(
        "  Average time to first segment: {}\n",
        secs(p.first_segment_ms_avg)
    ));
    out.push_str(&format!(
        "  Average speed: {:.1} times real time\n\n",
        p.realtime_factor_avg
    ));

    out.push_str("Peers\n");
    if report.peers.is_empty() {
        out.push_str("  No peers enrolled.\n");
    }
    for peer in &report.peers {
        out.push_str(&format!(
            "  {} ({})\n",
            peer.name,
            if peer.online { "online" } else { "offline" }
        ));
        out.push_str(&format!("    Device: {} via {}\n", peer.accelerator, peer.backend));
        out.push_str(&format!(
            "    Transcription score: {}\n",
            peer.score
                .map_or_else(|| "not measured yet".into(), |s| format!("{s:.1}"))
        ));
        out.push_str(&format!(
            "    Online: {:.1} percent of the period\n",
            peer.online_percent
        ));
        out.push_str(&format!(
            "    Parts done: {}, failed: {}\n",
            peer.parts_done, peer.parts_failed
        ));
        out.push_str(&format!(
            "    Audio: {:.1} minutes, average speed {:.1} times real time\n",
            peer.audio_minutes, peer.realtime_factor_avg
        ));
        out.push_str(&format!(
            "    Average offer response time: {:.0} ms\n",
            peer.offer_ms_avg
        ));
        out.push_str(&format!(
            "    Last seen: {}, client version {}\n",
            format_time(peer.last_seen),
            peer.client_version
        ));
        out.push_str(&format!("    Id: {}\n", peer.id));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{JobRecord, PartRecord};
    use uuid::Uuid;

    #[test]
    fn collects_and_renders() {
        let db = Db::open_in_memory().unwrap();
        let peer = Uuid::new_v4();
        db.insert_peer(peer, "desk", "pk", "h", "0.1.0").unwrap();
        db.update_peer_score(peer, 12.5, "tiny").unwrap();
        let session = db.open_session(peer).unwrap();
        db.close_session(session).unwrap();
        let job = Uuid::new_v4();
        db.record_job(&JobRecord {
            id: job,
            api_token_id: Uuid::new_v4(),
            created_at: now(),
            status: "done",
            model: Some("tiny".into()),
            language: Some("de".into()),
            audio_ms: 120_000,
            words: Some(300),
            parts: 1,
            queue_ms: Some(100),
            total_ms: 9_000,
            override_rejected: false,
            error: None,
        })
        .unwrap();
        db.record_part(&PartRecord {
            id: Uuid::new_v4(),
            job_id: job,
            peer_id: Some(peer),
            status: "done",
            audio_ms: 120_000,
            offer_ms: Some(12),
            transfer_ms: Some(40),
            processing_ms: Some(8_000),
            first_segment_ms: Some(900),
            error: None,
        })
        .unwrap();

        let report = collect(&db, 7).unwrap();
        assert_eq!(report.jobs.done, 1);
        assert!((report.jobs.words_per_audio_minute - 150.0).abs() < 0.01);
        assert!((report.parts.realtime_factor_avg - 15.0).abs() < 0.01);
        assert_eq!(report.peers[0].parts_done, 1);
        let text = render(&report);
        assert!(text.contains("Transcription score: 12.5"));
    }
}
