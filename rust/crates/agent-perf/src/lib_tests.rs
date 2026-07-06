use super::*;
use std::process::Command;
use std::time::Duration;

#[test]
fn snapshot_format_has_only_fixed_numeric_fields() {
    let line = Snapshot::default().to_string();

    assert!(line.starts_with("active_context_materializations=0 "));
    assert!(line.ends_with("action_completion_entries_scanned=0"));
    assert!(!line.contains('"'));
}

#[test]
fn operation_and_outcome_names_are_fixed_and_exhaustive() {
    assert_eq!(
        [
            Operation::ModelAction.as_str(),
            Operation::ToolAction.as_str(),
            Operation::ColdActivation.as_str(),
            Operation::TitleSidecar.as_str(),
            Operation::WebSidecar.as_str(),
            Operation::Compaction.as_str(),
        ],
        [
            "model_action",
            "tool_action",
            "cold_activation",
            "title_sidecar",
            "web_sidecar",
            "compaction",
        ]
    );
    assert_eq!(
        [
            Outcome::Aborted.as_str(),
            Outcome::Completed.as_str(),
            Outcome::Failed.as_str(),
            Outcome::Panicked.as_str(),
            Outcome::GateBlocked.as_str(),
            Outcome::ClaimLost.as_str(),
            Outcome::HarnessDeferred.as_str(),
        ],
        [
            "aborted",
            "completed",
            "failed",
            "panicked",
            "gate_blocked",
            "claim_lost",
            "harness_deferred",
        ]
    );
}

#[tokio::test]
async fn nested_phases_are_exclusive_and_wall_covers_classified_time() {
    let metrics = Metrics::for_test(Operation::ModelAction);

    metrics
        .scope(async {
            let _preparation = phase(Phase::RequestPreparation);
            tokio::time::sleep(Duration::from_millis(2)).await;
            scope_phase(
                Phase::ProviderRequestWait,
                tokio::time::sleep(Duration::from_millis(2)),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        })
        .await;
    let snapshot = metrics.finish(Outcome::Completed);

    assert!(snapshot.request_preparation_ns > 0);
    assert!(snapshot.provider_request_wait_ns > 0);
    assert_eq!(
        snapshot.classified_wall_ns,
        snapshot
            .request_preparation_ns
            .saturating_add(snapshot.provider_request_wait_ns)
    );
    assert!(snapshot.total_elapsed_ns >= snapshot.classified_wall_ns);
    assert_eq!(
        snapshot.unclassified_wall_ns,
        snapshot
            .total_elapsed_ns
            .saturating_sub(snapshot.classified_wall_ns)
    );
}

#[tokio::test]
async fn dropped_phase_records_time_after_early_error() {
    let metrics = Metrics::for_test(Operation::ModelAction);

    let result: Result<(), ()> = metrics
        .scope(async {
            let _stream = phase(Phase::ProviderStreamWait);
            tokio::time::sleep(Duration::from_millis(1)).await;
            Err(())
        })
        .await;
    assert_eq!(result, Err(()));
    let snapshot = metrics.finish(Outcome::Failed);

    assert!(snapshot.provider_stream_wait_ns > 0);
    assert!(snapshot.total_elapsed_ns >= snapshot.classified_wall_ns);
}

#[tokio::test]
async fn dropped_owner_marks_aborted_collector_finished() {
    let metrics = Metrics::for_test(Operation::ModelAction);
    let observer = metrics.test_observer();
    tokio::time::sleep(Duration::from_millis(1)).await;
    drop(metrics);

    let snapshot = observer.finished_snapshot().await;
    assert!(snapshot.total_elapsed_ns > 0);
    assert_eq!(snapshot.unclassified_wall_ns, snapshot.total_elapsed_ns);
}

#[test]
fn startup_env_probe_subprocess() {
    const CHILD: &str = "PI_RELAY_PERF_ENV_PROBE_CHILD";
    if let Ok(expected) = std::env::var(CHILD) {
        let metrics = Metrics::new_if_enabled(Operation::ModelAction);
        assert_eq!(metrics.is_some(), expected == "enabled");
        if let Some(metrics) = metrics {
            metrics.finish(Outcome::Completed);
        }
        return;
    }

    let executable = std::env::current_exe().expect("current test executable");
    for (expected, enabled) in [("disabled", false), ("enabled", true)] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::startup_env_probe_subprocess")
            .arg("--nocapture")
            .env(CHILD, expected);
        if enabled {
            command.env("PI_RELAY_PERF", "1");
        } else {
            command.env_remove("PI_RELAY_PERF");
        }
        assert!(
            command.status().expect("run environment probe").success(),
            "{expected} subprocess failed"
        );
    }
}

#[tokio::test]
async fn scoped_metrics_aggregate_without_labels_or_content() {
    let metrics = Metrics::for_test(Operation::ModelAction);

    metrics
        .scope(async {
            active_context_materialized(1024);
            logical_model_request_built();
            provider_body_serialized(900);
            provider_body_compressed(300);
            physical_provider_send();
            publish_sse(SseMetrics {
                received_bytes: 20,
                scan_windows: 40,
                frames: 1,
                peak_retained_bytes: 20,
                ..SseMetrics::default()
            });
        })
        .await;

    assert_eq!(
        metrics.snapshot(),
        Snapshot {
            active_context_materializations: 1,
            active_context_materialized_bytes: 1024,
            latest_context_bytes: 1024,
            logical_model_request_builds: 1,
            provider_body_serializations: 1,
            provider_body_serialized_bytes: 900,
            provider_body_compressions: 1,
            provider_body_encoded_bytes: 300,
            physical_provider_sends: 1,
            sse_received_bytes: 20,
            sse_scan_windows: 40,
            sse_frames: 1,
            sse_peak_retained_bytes: 20,
            ..Snapshot::default()
        }
    );
}

#[tokio::test]
async fn back_to_back_model_actions_have_separate_collectors() {
    let first = Metrics::for_test(Operation::ModelAction);
    first
        .scope(async {
            model_attempt();
            physical_provider_send();
        })
        .await;
    let first = first.finish(Outcome::Completed);

    let second = Metrics::for_test(Operation::ModelAction);
    second.scope(async { model_attempt() }).await;
    let second = second.finish(Outcome::Failed);

    assert_eq!(
        (first.model_attempts, first.physical_provider_sends),
        (1, 1)
    );
    assert_eq!(
        (second.model_attempts, second.physical_provider_sends),
        (1, 0)
    );
}

#[tokio::test]
async fn concurrent_sessions_have_separate_collectors() {
    let first = Metrics::for_test(Operation::ModelAction);
    let second = Metrics::for_test(Operation::ModelAction);
    let ((), ()) = tokio::join!(
        first.scope(async {
            model_attempt();
            publish_sse(SseMetrics {
                received_bytes: 10,
                ..SseMetrics::default()
            });
            tokio::task::yield_now().await;
            publish_sse(SseMetrics {
                received_bytes: 20,
                ..SseMetrics::default()
            });
        }),
        second.scope(async {
            model_attempt();
            publish_sse(SseMetrics {
                received_bytes: 7,
                ..SseMetrics::default()
            });
        }),
    );

    assert_eq!(first.snapshot().sse_received_bytes, 30);
    assert_eq!(second.snapshot().sse_received_bytes, 7);
}

#[tokio::test]
async fn copies_use_each_explicit_or_latest_materialized_size() {
    let metrics = Metrics::for_test(Operation::ModelAction);

    metrics
        .scope(async {
            active_context_materialized(100);
            request_copied();
            active_context_materialized(200);
            request_copied();
        })
        .await;

    assert_eq!(
        metrics.snapshot(),
        Snapshot {
            active_context_materializations: 2,
            active_context_materialized_bytes: 300,
            latest_context_bytes: 200,
            request_copies: 2,
            request_copied_bytes: 300,
            ..Snapshot::default()
        }
    );
}
