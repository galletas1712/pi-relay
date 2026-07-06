use std::hint::black_box;
use std::time::{Duration, Instant};

const SAMPLES: usize = 21;

fn main() {
    assert!(
        std::env::var_os("PI_RELAY_PERF").is_none(),
        "run with PI_RELAY_PERF unset"
    );
    report("o1_hook", 2_000_000, run_o1);
    report("sse_three_hooks", 500_000, run_sse);
    report("reverse_k_1000", 8, run_reverse_k);
}

fn report(name: &str, iterations: usize, run: fn(usize, Hook) -> Duration) {
    let mut overhead = Vec::with_capacity(SAMPLES);
    let mut baseline_ns = Vec::with_capacity(SAMPLES);
    let mut instrumented_ns = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        let (baseline, instrumented) = if sample % 2 == 0 {
            (run(iterations, Hook::None), run(iterations, Hook::Disabled))
        } else {
            let instrumented = run(iterations, Hook::Disabled);
            (run(iterations, Hook::None), instrumented)
        };
        baseline_ns.push(baseline.as_nanos());
        instrumented_ns.push(instrumented.as_nanos());
        overhead.push((instrumented.as_secs_f64() / baseline.as_secs_f64() - 1.0) * 100.0);
    }
    baseline_ns.sort_unstable();
    instrumented_ns.sort_unstable();
    overhead.sort_by(f64::total_cmp);
    println!(
        "perf fixture=disabled_overhead shape={name} samples={SAMPLES} iterations={iterations} baseline_median_ns={} instrumented_median_ns={} overhead_median_percent={:.3} overhead_range_percent={:.3}..{:.3}",
        baseline_ns[SAMPLES / 2],
        instrumented_ns[SAMPLES / 2],
        overhead[SAMPLES / 2],
        overhead[0],
        overhead[SAMPLES - 1],
    );
}

#[derive(Clone, Copy)]
enum Hook {
    None,
    Disabled,
}

fn run_o1(iterations: usize, hook: Hook) -> Duration {
    let started = Instant::now();
    for value in 0..iterations {
        if matches!(hook, Hook::Disabled) {
            agent_perf::active_context_materialized_by(|| black_box(value));
        }
        black_box(value);
    }
    started.elapsed()
}

fn run_sse(iterations: usize, hook: Hook) -> Duration {
    let started = Instant::now();
    for value in 0..iterations {
        if matches!(hook, Hook::Disabled) {
            agent_perf::sse_received(black_box(1));
            agent_perf::sse_scan_windows(black_box(64));
            agent_perf::sse_retained(black_box(value % 4096));
        }
        black_box(value);
    }
    started.elapsed()
}

fn run_reverse_k(iterations: usize, hook: Hook) -> Duration {
    const K: usize = 1_000;
    let started = Instant::now();
    for _ in 0..iterations {
        let mut pending = (0..K).collect::<Vec<_>>();
        for target in (0..K).rev() {
            let position = if matches!(hook, Hook::Disabled) && agent_perf::is_recording() {
                let mut entries = 0;
                let position = pending.iter().position(|value| {
                    entries += 1;
                    value == &target
                });
                agent_perf::action_completion_scan(entries);
                position
            } else {
                pending.iter().position(|value| value == &target)
            };
            black_box(pending.remove(position.expect("target remains pending")));
        }
    }
    started.elapsed()
}
