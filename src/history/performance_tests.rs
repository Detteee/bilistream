//! Opt-in measurements use the production schema, writer and maintenance.

use super::*;

#[test]
#[ignore = "read-only latency measurement against the retained real-cadence fixture"]
fn history_existing_database_query_latency() {
    let path = std::env::var("BILISTREAM_HISTORY_BENCH_DB")
        .expect("set BILISTREAM_HISTORY_BENCH_DB to the real-cadence SQLite file");
    let conn = schema::open_ro(std::path::Path::new(&path)).unwrap();
    let now = T0.div_euclid(DAY) * DAY + 90 * DAY;
    let mut exceeded = Vec::new();
    for name in ["summary", "sessions", "stats", "events"] {
        let mut request = q(now - 30 * DAY, now);
        request.tier = Some("1m".into());
        let request = PeerRequest {
            name: name.into(),
            query: request,
        };
        let mut timings = Vec::new();
        for _ in 0..30 {
            let start = std::time::Instant::now();
            let value = api::local_parts(&conn, &request, "local", now).unwrap();
            assert!(!value.is_null());
            timings.push(start.elapsed());
        }
        timings.sort();
        eprintln!(
            "history endpoint={name} range_days=30 p50_ms={} p95_ms={}",
            timings[15].as_secs_f64() * 1000.0,
            timings[28].as_secs_f64() * 1000.0
        );
        if timings[28] >= std::time::Duration::from_millis(150) {
            exceeded.push(name);
        }
    }
    assert!(
        exceeded.is_empty(),
        "30-day queries exceed the 150ms p95 budget: {exceeded:?}"
    );
}

#[test]
#[ignore = "90-day storage and query acceptance measurement"]
fn history_real_cadence_storage_and_latency() {
    let (mut w, path) = writer("real-cadence");
    let retention = HistoryRetention::default();
    let start = T0.div_euclid(DAY) * DAY;
    let mut worst_maintenance = std::time::Duration::ZERO;
    let mut maintenance_cpu = std::time::Duration::ZERO;
    let fill_start = std::time::Instant::now();
    for day in 0..90 {
        let seq = day as u64 + 1;
        let since = start + day * DAY;
        session(&mut w, seq, since, None);
        for minute in 0..1440 {
            let base = since + minute * 60;
            let mut batch = Vec::with_capacity(18);
            for offset in (0..60).step_by(5) {
                let at = base + offset;
                if offset % 10 == 0 {
                    // Vary values, including f32 fractions and non-equal
                    // extrema, so SQLite's integer compression is realistic.
                    let mean = 3200.0 + (at % 200) as f32 * 0.375;
                    batch.push(ev(
                        at,
                        EventKind::StatsSample {
                            seq,
                            bitrate_kbps: (mean - 42.25, mean, mean + 75.5),
                            fps: Some(29.97),
                            speed: Some(1.001),
                        },
                    ));
                }
                batch.push(ev(
                    at,
                    EventKind::BiliCounts {
                        seq,
                        room: 1,
                        online: Some(100 + (at % 100) as u64),
                        watched: Some((minute * 12 + offset / 5) as u64),
                    },
                ));
            }
            maintain::configure_limits(&w.conn, &retention).unwrap();
            assert!(maintain::admit_write(&w.conn, &retention).unwrap());
            w.commit(&batch).unwrap();
            let tick = std::time::Instant::now();
            maintain::tick(&w.conn, base + 60, &retention).unwrap();
            let elapsed = tick.elapsed();
            worst_maintenance = worst_maintenance.max(elapsed);
            maintenance_cpu += elapsed;
        }
        end(&mut w, seq, since + DAY, EndReason::SourceEnded);
        if day == 13 || day == 89 {
            let size = maintain::space(&w.conn, &retention).unwrap();
            let total = size.db_bytes + size.wal_bytes;
            eprintln!(
                "history day={} db={} wal={} total={} path={}",
                day + 1,
                size.db_bytes,
                size.wal_bytes,
                total,
                path.display()
            );
            assert!(
                total < if day == 13 { 20_000_000 } else { 60_000_000 },
                "history size exceeds the acceptance budget"
            );
        }
    }
    eprintln!(
        "history fill_secs={} maintenance_secs={} worst_tick_ms={}",
        fill_start.elapsed().as_secs_f64(),
        maintenance_cpu.as_secs_f64(),
        worst_maintenance.as_secs_f64() * 1000.0
    );
    let now = start + 90 * DAY;
    let request = q(now - 30 * DAY, now);
    let mut timings = Vec::new();
    for _ in 0..40 {
        let begin = std::time::Instant::now();
        query::summary(&w.conn, &request, "local", now).unwrap();
        timings.push(begin.elapsed());
    }
    timings.sort();
    eprintln!(
        "history summary_30d_p50_ms={} p95_ms={}",
        timings[20].as_secs_f64() * 1000.0,
        timings[38].as_secs_f64() * 1000.0
    );
}
