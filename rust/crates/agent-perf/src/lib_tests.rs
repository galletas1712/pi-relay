use super::*;
use std::process::Command;

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
            Operation::ModelTurn.as_str(),
            Operation::ColdActivation.as_str(),
            Operation::TitleSidecar.as_str(),
            Operation::WebSidecar.as_str(),
            Operation::Compaction.as_str(),
        ],
        [
            "model_turn",
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
async fn dropped_owner_marks_aborted_collector_finished() {
    let metrics = Metrics::for_test(Operation::ModelTurn);
    let observer = metrics.test_observer();
    drop(metrics);

    assert_eq!(observer.finished_snapshot().await, Snapshot::default());
}

#[test]
fn startup_env_probe_subprocess() {
    const CHILD: &str = "PI_RELAY_PERF_ENV_PROBE_CHILD";
    if let Ok(expected) = std::env::var(CHILD) {
        let metrics = Metrics::new_if_enabled(Operation::ModelTurn);
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
    let metrics = Metrics::for_test(Operation::ModelTurn);

    metrics
        .scope(async {
            active_context_materialized(1024);
            logical_model_request_built();
            provider_body_serialized(900);
            provider_body_compressed(300);
            physical_provider_send();
            sse_received(20);
            sse_scan_windows(40);
            sse_frame();
            sse_retained(20);
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
async fn back_to_back_model_turns_have_separate_collectors() {
    let first = Metrics::for_test(Operation::ModelTurn);
    first
        .scope(async {
            model_attempt();
            physical_provider_send();
        })
        .await;
    let first = first.finish(Outcome::Completed);

    let second = Metrics::for_test(Operation::ModelTurn);
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
    let first = Metrics::for_test(Operation::ModelTurn);
    let second = Metrics::for_test(Operation::ModelTurn);
    let ((), ()) = tokio::join!(
        first.scope(async {
            model_attempt();
            sse_received(10);
            tokio::task::yield_now().await;
            sse_received(20);
        }),
        second.scope(async {
            model_attempt();
            sse_received(7);
        }),
    );

    assert_eq!(first.snapshot().sse_received_bytes, 30);
    assert_eq!(second.snapshot().sse_received_bytes, 7);
}

#[tokio::test]
async fn copies_use_each_explicit_or_latest_materialized_size() {
    let metrics = Metrics::for_test(Operation::ModelTurn);

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
