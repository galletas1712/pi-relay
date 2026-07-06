//! Internal, opt-in counters for model-request hot-path measurements.
//!
//! `PI_RELAY_PERF` is read once per process. Disabled hooks return after the
//! cached flag check. Enabled collectors are fixed-size, operation-owned, and
//! contain numeric observations only.

#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

static PERF_ENABLED: OnceLock<bool> = OnceLock::new();

tokio::task_local! {
    static CURRENT: Arc<Counters>;
    static OUTPUT_PERSISTENCE: ();
}

macro_rules! metric_fields {
    ($macro:ident) => {
        $macro! {
            active_context_materializations,
            active_context_materialized_bytes,
            latest_context_bytes,
            request_copies,
            request_copied_bytes,
            logical_model_request_builds,
            provider_body_serializations,
            provider_body_serialized_bytes,
            provider_body_compressions,
            provider_body_encoded_bytes,
            compaction_gate_passes,
            accounting_passes,
            logical_count_token_requests,
            physical_count_token_sends,
            model_attempts,
            model_retries,
            physical_provider_sends,
            provider_auth_retries,
            auth_refreshes,
            sse_received_bytes,
            sse_scan_windows,
            sse_frames,
            sse_peak_retained_bytes,
            session_registry_scans,
            session_registry_entries_scanned,
            dispatch_task_registry_scans,
            dispatch_task_registry_entries_scanned,
            lock_wait_ns,
            output_sql_statements,
            output_transactions,
            output_transaction_ns,
            recovery_sql_statements,
            scoped_store_calls,
            cold_rows_loaded,
            cold_bytes_loaded,
            empty_persist_passes,
            empty_dispatch_passes,
            action_completion_scans,
            action_completion_entries_scanned,
        }
    };
}

macro_rules! define_counters {
    ($($field:ident,)*) => {
        #[derive(Debug, Default)]
        struct Counters {
            $($field: AtomicU64,)*
            finished: AtomicBool,
        }
    };
}

metric_fields!(define_counters);

macro_rules! define_snapshot {
    ($($field:ident,)*) => {
        /// Immutable numeric aggregate for one operation.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct Snapshot {
            $(pub $field: u64,)*
        }

        impl Snapshot {
            fn load(counters: &Counters) -> Self {
                Self {
                    $($field: load(&counters.$field),)*
                }
            }
        }

        impl std::fmt::Display for Snapshot {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut first = true;
                $(
                    if !first {
                        formatter.write_str(" ")?;
                    }
                    first = false;
                    write!(formatter, "{}={}", stringify!($field), self.$field)?;
                )*
                let _ = first;
                Ok(())
            }
        }
    };
}

metric_fields!(define_snapshot);

/// The bounded operation that owns a collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    ModelTurn,
    ColdActivation,
    TitleSidecar,
    WebSidecar,
    Compaction,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::ModelTurn => "model_turn",
            Self::ColdActivation => "cold_activation",
            Self::TitleSidecar => "title_sidecar",
            Self::WebSidecar => "web_sidecar",
            Self::Compaction => "compaction",
        }
    }
}

/// Terminal state of a measured operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Aborted,
    Completed,
    Failed,
    Panicked,
    GateBlocked,
    ClaimLost,
    HarnessDeferred,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Panicked => "panicked",
            Self::GateBlocked => "gate_blocked",
            Self::ClaimLost => "claim_lost",
            Self::HarnessDeferred => "harness_deferred",
        }
    }
}

/// Non-cloneable owner for exactly one measured operation.
///
/// Workspace crates may borrow it to install the collector while an operation
/// runs. Finishing consumes the owner, so no later writer can reuse it.
#[derive(Debug)]
pub struct Metrics {
    operation: Operation,
    counters: Arc<Counters>,
    emit: bool,
    finished: bool,
}

/// Read-only handle for deterministic tests after an operation has finished.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct TestObserver(Arc<Counters>);

#[cfg(any(test, feature = "test-support"))]
impl TestObserver {
    pub async fn finished_snapshot(&self) -> Snapshot {
        while !self.0.finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        Snapshot::load(&self.0)
    }
}

impl Metrics {
    /// Allocate a collector only when `PI_RELAY_PERF` was enabled at startup.
    pub fn new_if_enabled(operation: Operation) -> Option<Self> {
        enabled().then(|| Self::new(operation, true))
    }

    fn new(operation: Operation, emit: bool) -> Self {
        Self {
            operation,
            counters: Arc::new(Counters::default()),
            emit,
            finished: false,
        }
    }

    /// Construct a collector for deterministic internal tests and probes.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(operation: Operation) -> Self {
        Self::new(operation, false)
    }

    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_observer(&self) -> TestObserver {
        TestObserver(Arc::clone(&self.counters))
    }

    /// Run one operation with this collector installed on the current task.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(Arc::clone(&self.counters), future).await
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot::load(&self.counters)
    }

    /// Record the synchronous dispatch-task prune before a runner is spawned.
    pub fn dispatch_task_registry_scan(&self, entries: usize) {
        add(&self.counters.dispatch_task_registry_scans, 1);
        add_usize(
            &self.counters.dispatch_task_registry_entries_scanned,
            entries,
        );
    }

    /// Consume the operation owner and emit one fixed-shape aggregate.
    pub fn finish(mut self, outcome: Outcome) -> Snapshot {
        let snapshot = self.snapshot();
        if self.emit {
            eprintln!(
                "perf operation={} outcome={} {snapshot}",
                self.operation.as_str(),
                outcome.as_str()
            );
        }
        self.finished = true;
        self.counters.finished.store(true, Ordering::Release);
        snapshot
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        if self.emit && !self.finished {
            let snapshot = self.snapshot();
            eprintln!(
                "perf operation={} outcome={} {snapshot}",
                self.operation.as_str(),
                Outcome::Aborted.as_str()
            );
        }
        self.counters.finished.store(true, Ordering::Release);
    }
}

/// Whether a collector is installed on this task.
pub fn is_recording() -> bool {
    recording_enabled()
}

pub fn active_context_materialized_by(measure: impl FnOnce() -> usize) {
    if !recording_enabled() {
        return;
    }
    active_context_materialized(measure());
}

pub fn active_context_materialized(bytes: usize) {
    record(|counters| {
        add(&counters.active_context_materializations, 1);
        add_usize(&counters.active_context_materialized_bytes, bytes);
        counters
            .latest_context_bytes
            .store(usize_to_u64(bytes), Ordering::Relaxed);
    });
}

/// Record a context size without claiming that this operation materialized it.
pub fn observe_context(bytes: usize) {
    record(|counters| {
        counters
            .latest_context_bytes
            .store(usize_to_u64(bytes), Ordering::Relaxed);
    });
}

pub fn request_copied() {
    record(|counters| {
        let bytes = load(&counters.latest_context_bytes);
        add(&counters.request_copies, 1);
        add(&counters.request_copied_bytes, bytes);
    });
}

macro_rules! count_hook {
    ($name:ident, $field:ident) => {
        pub fn $name() {
            record(|counters| add(&counters.$field, 1));
        }
    };
}

count_hook!(logical_model_request_built, logical_model_request_builds);
count_hook!(compaction_gate_pass, compaction_gate_passes);
count_hook!(accounting_pass, accounting_passes);
count_hook!(logical_count_token_request, logical_count_token_requests);
count_hook!(physical_count_token_send, physical_count_token_sends);
count_hook!(model_attempt, model_attempts);
count_hook!(model_retry, model_retries);
count_hook!(physical_provider_send, physical_provider_sends);
count_hook!(provider_auth_retry, provider_auth_retries);
count_hook!(auth_refresh, auth_refreshes);
count_hook!(sse_frame, sse_frames);
count_hook!(recovery_sql_statement, recovery_sql_statements);
count_hook!(scoped_store_call, scoped_store_calls);
count_hook!(empty_persist_pass, empty_persist_passes);
count_hook!(empty_dispatch_pass, empty_dispatch_passes);

pub fn provider_body_serialized(bytes: usize) {
    record(|counters| {
        add(&counters.provider_body_serializations, 1);
        add_usize(&counters.provider_body_serialized_bytes, bytes);
    });
}

pub fn provider_body_compressed(encoded_bytes: usize) {
    record(|counters| {
        add(&counters.provider_body_compressions, 1);
        add_usize(&counters.provider_body_encoded_bytes, encoded_bytes);
    });
}

pub fn sse_received(bytes: usize) {
    record(|counters| add_usize(&counters.sse_received_bytes, bytes));
}

pub fn sse_scan_windows(windows: usize) {
    record(|counters| add_usize(&counters.sse_scan_windows, windows));
}

pub fn sse_retained(bytes: usize) {
    record(|counters| {
        counters
            .sse_peak_retained_bytes
            .fetch_max(usize_to_u64(bytes), Ordering::Relaxed);
    });
}

pub fn session_registry_scan(entries: usize) {
    record(|counters| {
        add(&counters.session_registry_scans, 1);
        add_usize(&counters.session_registry_entries_scanned, entries);
    });
}

pub fn dispatch_task_registry_scan(entries: usize) {
    record(|counters| {
        add(&counters.dispatch_task_registry_scans, 1);
        add_usize(&counters.dispatch_task_registry_entries_scanned, entries);
    });
}

pub fn lock_wait(duration: Duration) {
    record(|counters| add(&counters.lock_wait_ns, duration_to_nanos(duration)));
}

pub fn output_sql_statement_for_transition() {
    record(|counters| add(&counters.output_sql_statements, 1));
}

pub fn output_sql_statement() {
    if recording_enabled() && OUTPUT_PERSISTENCE.try_with(|()| ()).is_ok() {
        record(|counters| add(&counters.output_sql_statements, 1));
    }
}

pub async fn scope_output_persistence<F: Future>(future: F) -> F::Output {
    if recording_enabled() {
        OUTPUT_PERSISTENCE.scope((), future).await
    } else {
        future.await
    }
}

pub fn output_transaction_started() {
    record(|counters| add(&counters.output_transactions, 1));
}

pub fn output_transaction_duration(duration: Duration) {
    record(|counters| add(&counters.output_transaction_ns, duration_to_nanos(duration)));
}

pub fn cold_loaded_by(rows: usize, measure: impl FnOnce() -> usize) {
    if !recording_enabled() {
        return;
    }
    cold_loaded(rows, measure());
}

pub fn cold_loaded(rows: usize, bytes: usize) {
    record(|counters| {
        add_usize(&counters.cold_rows_loaded, rows);
        add_usize(&counters.cold_bytes_loaded, bytes);
    });
}

pub fn action_completion_scan(entries: usize) {
    record(|counters| {
        add(&counters.action_completion_scans, 1);
        add_usize(&counters.action_completion_entries_scanned, entries);
    });
}

fn enabled() -> bool {
    *PERF_ENABLED.get_or_init(|| std::env::var_os("PI_RELAY_PERF").is_some())
}

fn record(operation: impl FnOnce(&Counters)) {
    if !recording_enabled() {
        return;
    }
    let _ = CURRENT.try_with(|counters| operation(counters));
}

fn recording_enabled() -> bool {
    if enabled() {
        return CURRENT.try_with(|_| ()).is_ok();
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        CURRENT.try_with(|_| ()).is_ok()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    false
}

fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

fn add_usize(counter: &AtomicU64, value: usize) {
    add(counter, usize_to_u64(value));
}

fn load(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn duration_to_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
