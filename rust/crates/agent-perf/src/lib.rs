//! Internal, opt-in counters for model-request hot-path measurements.
//!
//! `PI_RELAY_PERF` is read once per process. Disabled hooks return after the
//! cached flag check. Enabled collectors are fixed-size, operation-owned, and
//! contain numeric observations only.

#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static PERF_ENABLED: OnceLock<bool> = OnceLock::new();

tokio::task_local! {
    static CURRENT: Arc<Counters>;
    static PHASE_STATE: Arc<Mutex<PhaseState>>;
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
            provider_failures_persisted,
            sse_received_bytes,
            sse_scan_windows,
            sse_frames,
            sse_peak_retained_bytes,
            provider_request_wait_ns,
            provider_stream_wait_ns,
            provider_metadata_wait_ns,
            request_preparation_ns,
            tool_execution_ns,
            output_persistence_wall_ns,
            coordination_wait_ns,
            classified_wall_ns,
            unclassified_wall_ns,
            total_elapsed_ns,
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
    ModelAction,
    ToolAction,
    ColdActivation,
    TitleSidecar,
    WebSidecar,
    Compaction,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::ModelAction => "model_action",
            Self::ToolAction => "tool_action",
            Self::ColdActivation => "cold_activation",
            Self::TitleSidecar => "title_sidecar",
            Self::WebSidecar => "web_sidecar",
            Self::Compaction => "compaction",
        }
    }
}

/// Exclusive wall-clock phase within one measured operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    ProviderRequestWait,
    ProviderStreamWait,
    ProviderMetadataWait,
    RequestPreparation,
    ToolExecution,
    OutputPersistenceWall,
    CoordinationWait,
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
    started: Instant,
    emit: bool,
    finished: bool,
}

#[derive(Debug, Default)]
struct PhaseState {
    stack: Vec<PhaseFrame>,
}

#[derive(Debug)]
struct PhaseFrame {
    phase: Phase,
    started: Instant,
}

/// RAII owner for an exclusive phase observation.
///
/// Dropping the guard records elapsed time, including during unwinding or
/// cancellation when Rust runs destructors.
#[must_use]
pub struct PhaseGuard {
    active: Option<(Arc<Counters>, Arc<Mutex<PhaseState>>)>,
}

/// Per-response SSE observations published once when the parser exits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SseMetrics {
    pub received_bytes: u64,
    pub scan_windows: u64,
    pub frames: u64,
    pub peak_retained_bytes: u64,
    pub stream_wait_ns: u64,
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
            started: Instant::now(),
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
        CURRENT
            .scope(
                Arc::clone(&self.counters),
                PHASE_STATE.scope(Arc::new(Mutex::new(PhaseState::default())), future),
            )
            .await
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

    /// Record coordination before the task-local collector is installed.
    pub fn coordination_wait(&self, duration: Duration) {
        add_phase_duration(
            &self.counters,
            Phase::CoordinationWait,
            duration_to_nanos(duration),
        );
    }

    /// Consume the operation owner and emit one fixed-shape aggregate.
    pub fn finish(mut self, outcome: Outcome) -> Snapshot {
        let snapshot = self.final_snapshot();
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

    fn final_snapshot(&self) -> Snapshot {
        let total_elapsed_ns = duration_to_nanos(self.started.elapsed());
        self.counters
            .total_elapsed_ns
            .store(total_elapsed_ns, Ordering::Relaxed);
        self.counters.unclassified_wall_ns.store(
            total_elapsed_ns.saturating_sub(load(&self.counters.classified_wall_ns)),
            Ordering::Relaxed,
        );
        self.snapshot()
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        if !self.finished {
            let snapshot = self.final_snapshot();
            if self.emit {
                eprintln!(
                    "perf operation={} outcome={} {snapshot}",
                    self.operation.as_str(),
                    Outcome::Aborted.as_str()
                );
            }
        }
        self.counters.finished.store(true, Ordering::Release);
    }
}

/// Whether a collector is installed on this task.
pub fn is_recording() -> bool {
    if !collector_lookup_enabled() {
        return false;
    }
    CURRENT.try_with(|_| ()).is_ok()
}

pub fn active_context_materialized_by(measure: impl FnOnce() -> usize) {
    if !is_recording() {
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
count_hook!(provider_failure_persisted, provider_failures_persisted);
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

pub fn publish_sse(metrics: SseMetrics) {
    record(|counters| {
        add(&counters.sse_received_bytes, metrics.received_bytes);
        add(&counters.sse_scan_windows, metrics.scan_windows);
        add(&counters.sse_frames, metrics.frames);
        counters
            .sse_peak_retained_bytes
            .fetch_max(metrics.peak_retained_bytes, Ordering::Relaxed);
        add_phase_duration(counters, Phase::ProviderStreamWait, metrics.stream_wait_ns);
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

/// Start an exclusive phase on the current collector.
///
/// A nested phase pauses its parent, then resumes it when the nested guard is
/// dropped. Consequently phase fields do not double-count in
/// `classified_wall_ns`.
pub fn phase(phase: Phase) -> PhaseGuard {
    if !collector_lookup_enabled() {
        return PhaseGuard { active: None };
    }
    let Some(counters) = CURRENT.try_with(Arc::clone).ok() else {
        return PhaseGuard { active: None };
    };
    let Some(state) = PHASE_STATE.try_with(Arc::clone).ok() else {
        return PhaseGuard { active: None };
    };
    let now = Instant::now();
    {
        let mut state_ref = state.lock().expect("perf phase state lock poisoned");
        if let Some(parent) = state_ref.stack.last() {
            add_phase_duration(
                &counters,
                parent.phase,
                duration_to_nanos(now.saturating_duration_since(parent.started)),
            );
        }
        state_ref.stack.push(PhaseFrame {
            phase,
            started: now,
        });
    }
    PhaseGuard {
        active: Some((counters, state)),
    }
}

pub async fn scope_phase<F: Future>(phase_name: Phase, future: F) -> F::Output {
    let _phase = phase(phase_name);
    future.await
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let Some((counters, state)) = self.active.take() else {
            return;
        };
        let now = Instant::now();
        let mut state = state.lock().expect("perf phase state lock poisoned");
        if let Some(frame) = state.stack.pop() {
            add_phase_duration(
                &counters,
                frame.phase,
                duration_to_nanos(now.saturating_duration_since(frame.started)),
            );
        }
        if let Some(parent) = state.stack.last_mut() {
            parent.started = now;
        }
    }
}

pub fn output_sql_statement_for_transition() {
    record(|counters| add(&counters.output_sql_statements, 1));
}

pub fn output_sql_statement() {
    if is_recording() && OUTPUT_PERSISTENCE.try_with(|()| ()).is_ok() {
        record(|counters| add(&counters.output_sql_statements, 1));
    }
}

pub async fn scope_output_persistence<F: Future>(future: F) -> F::Output {
    let future = scope_phase(Phase::OutputPersistenceWall, future);
    if is_recording() {
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
    if !is_recording() {
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
    if !collector_lookup_enabled() {
        return;
    }
    let _ = CURRENT.try_with(|counters| operation(counters));
}

fn collector_lookup_enabled() -> bool {
    if enabled() {
        return true;
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        true
    }
    #[cfg(not(any(test, feature = "test-support")))]
    false
}

fn add_phase_duration(counters: &Counters, phase: Phase, nanos: u64) {
    let counter = match phase {
        Phase::ProviderRequestWait => &counters.provider_request_wait_ns,
        Phase::ProviderStreamWait => &counters.provider_stream_wait_ns,
        Phase::ProviderMetadataWait => &counters.provider_metadata_wait_ns,
        Phase::RequestPreparation => &counters.request_preparation_ns,
        Phase::ToolExecution => &counters.tool_execution_ns,
        Phase::OutputPersistenceWall => &counters.output_persistence_wall_ns,
        Phase::CoordinationWait => &counters.coordination_wait_ns,
    };
    add(counter, nanos);
    add(&counters.classified_wall_ns, nanos);
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
