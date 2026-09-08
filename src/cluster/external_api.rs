//! Bounded external-API health sampling, independent of stream monitoring,
//! WebUI refreshes and the control-plane heartbeat connection.

use super::fencing::record_external_api_result;
use super::heartbeat::heartbeat_sleep_duration;
use super::state::cluster_heartbeat_timeout;
use crate::config::Config;
use std::fmt::Display;
use std::future::Future;
use std::time::{Duration, Instant};

fn probe_interval(cfg: &Config) -> Duration {
    let window = Duration::from_secs(
        cfg.cluster
            .thresholds
            .external_api_failure_window_secs
            .max(1),
    );
    let samples = cfg.cluster.thresholds.max_external_api_failures.max(1);
    // Leave room for the threshold to be reached even when the configured
    // heartbeat period is longer than the failure-counting window.
    heartbeat_sleep_duration(cfg)
        .min(window / samples.saturating_add(1))
        .max(Duration::from_millis(100))
}

async fn bounded_check<T, E: Display>(
    request: impl Future<Output = Result<T, E>>,
    timeout: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(timeout, request).await {
        Ok(result) => result.map(|_| ()).map_err(|error| error.to_string()),
        Err(_) => Err(format!(
            "外部 API 健康检查超时（{} ms）",
            timeout.as_millis()
        )),
    }
}

pub(crate) async fn run_external_api_checks() {
    loop {
        let started = Instant::now();
        let cfg = match crate::config::load_config()
            .await
            .map_err(|error| error.to_string())
        {
            Ok(cfg) => cfg,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
        };
        let period = if cfg.cluster.enabled && cfg.bililive.room > 0 {
            let period = probe_interval(&cfg);
            let result = bounded_check(
                crate::plugins::bilibili::get_bili_live_status_once(cfg.bililive.room),
                cluster_heartbeat_timeout(&cfg).min(period),
            )
            .await;
            if let Err(error) = &result {
                tracing::debug!("集群外部 API 健康检查失败: {}", error);
            }
            // A late response for a replaced room/config cannot affect its
            // successor. Only this sampler feeds the window, once per attempt.
            crate::config::with_current_config(&cfg, || {
                record_external_api_result(result.is_ok());
            });
            period
        } else {
            Duration::from_secs(15)
        };
        tokio::time::sleep(period.saturating_sub(started.elapsed())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn sampling_can_reach_threshold_within_the_failure_window() {
        let mut cfg = crate::cluster::tests::test_config("jp", 100);
        for heartbeat in [1, 5, 15, 60, 3_600] {
            cfg.cluster.heartbeat_interval_secs = heartbeat;
            let period = probe_interval(&cfg);
            let timeout = cluster_heartbeat_timeout(&cfg).min(period);
            assert!(timeout <= period);
            assert!(
                period * cfg.cluster.thresholds.max_external_api_failures
                    < Duration::from_secs(cfg.cluster.thresholds.external_api_failure_window_secs)
            );
        }
    }

    #[tokio::test]
    async fn stalled_api_request_is_cancelled_at_the_probe_deadline() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let request = async move {
            let _guard = guard;
            std::future::pending::<Result<(), String>>().await
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            bounded_check(request, Duration::from_millis(20)),
        )
        .await
        .expect("a DNS/API stall must not block the sampler");
        assert!(result.unwrap_err().contains("超时"));
        assert!(dropped.load(Ordering::SeqCst));
        // A reachable offline room is healthy; only API errors count as failures.
        assert!(
            bounded_check(async { Ok::<_, String>(false) }, Duration::from_secs(1))
                .await
                .is_ok()
        );
        assert!(bounded_check(
            async { Err::<(), _>("DNS unavailable") },
            Duration::from_secs(1)
        )
        .await
        .is_err());
    }
}
