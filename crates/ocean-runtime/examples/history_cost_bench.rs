//! Reproducible agent-loop history-cost benchmark.
//!
//! Measures the real `trim_to_context_window` kernel once per simulated model
//! round, including JSON token estimation, provider-validity filtering, and
//! cloning of the outbound history. No provider/network/runtime noise is mixed
//! into the result.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use ocean_protocol::{AssistantMessage, Content, Message, StopReason, ToolResultMessage, Usage};
use ocean_runtime::agent_loop::trim_to_context_window;
use serde_json::{json, Value};

const HISTORY_SIZES: [usize; 3] = [10, 100, 1_000];
const ROUND_COUNTS: [usize; 3] = [1, 5, 20];
const CONTEXT_WINDOW: u32 = 128_000;
const MAX_OUTPUT_TOKENS: u32 = 8_192;
const REGRESSION_THRESHOLD_PERCENT: u64 = 20;
const REGRESSION_ABSOLUTE_FLOOR_US: u64 = 10;

static COUNTING_ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

// SAFETY: every operation delegates to `System` with the exact pointer/layout
// contract received from the caller. Atomics only observe allocation sizes and
// do not alter allocator behavior.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegated unchanged to the process system allocator.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() && COUNTING_ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegated unchanged to the process system allocator.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() && COUNTING_ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer/layout came from the corresponding system allocation.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: pointer/layout came from System and `new_size` is forwarded.
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() && COUNTING_ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        new_pointer
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct Settings {
    warmup: usize,
    samples: usize,
}

#[derive(Clone, Copy)]
struct Sample {
    nanoseconds: u64,
    allocations: u64,
    allocated_bytes: u64,
    checksum: usize,
}

fn main() {
    if cfg!(debug_assertions) {
        usage("benchmark must run with --release");
    }
    let (settings, output) = parse_args();
    let system_prompt = "s".repeat(4_096);
    let mut results = Vec::new();

    for history_size in HISTORY_SIZES {
        let base = synthetic_history(history_size);
        for rounds in ROUND_COUNTS {
            let round_additions = synthetic_round_additions(rounds);
            for _ in 0..settings.warmup {
                let mut history = base.clone();
                black_box(run_rounds(
                    &mut history,
                    &round_additions,
                    rounds,
                    &system_prompt,
                ));
            }

            let mut samples = Vec::with_capacity(settings.samples);
            for _ in 0..settings.samples {
                let mut history = base.clone();
                samples.push(measure(|| {
                    run_rounds(&mut history, &round_additions, rounds, &system_prompt)
                }));
            }
            results.push(summarize(history_size, rounds, &samples));
        }
    }

    let document = json!({
        "schema_version": 1,
        "benchmark": "ocean-runtime agent-loop history cost",
        "kernel": "agent_loop::trim_to_context_window plus intermediate round history append",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "git_revision": command_output("git", &["rev-parse", "HEAD"]),
        "git_status": command_output("git", &["status", "--short"]),
        "toolchain": command_output("rustc", &["-Vv"]),
        "cargo": command_output("cargo", &["-V"]),
        "build_profile": "release",
        "machine": {
            "uname": command_output("uname", &["-a"]),
            "cpu": cpu_description(),
            "memory": memory_description(),
            "os": os_description(),
        },
        "policy": {
            "warmup_iterations": settings.warmup,
            "samples_per_cell": settings.samples,
            "history_sizes": HISTORY_SIZES,
            "round_counts": ROUND_COUNTS,
            "context_window": CONTEXT_WINDOW,
            "max_output_tokens": MAX_OUTPUT_TOKENS,
            "system_prompt_bytes": system_prompt.len(),
            "allocation_scope": "process-global counting allocator; benchmark is single-threaded; base history construction/clone excluded",
            "meaningful_regression_percent": REGRESSION_THRESHOLD_PERCENT,
            "meaningful_regression_absolute_floor_us": REGRESSION_ABSOLUTE_FLOOR_US,
        },
        "results": results,
    });
    let rendered = serde_json::to_string_pretty(&document).expect("serialize benchmark output");

    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create benchmark output directory");
        }
        std::fs::write(&path, format!("{rendered}\n")).expect("write benchmark output");
        eprintln!("wrote {}", path.display());
    } else {
        println!("{rendered}");
    }
}

fn parse_args() -> (Settings, Option<PathBuf>) {
    let mut settings = Settings {
        warmup: 5,
        samples: 30,
    };
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--warmup" => {
                settings.warmup = parse_positive("--warmup", args.next());
            }
            "--samples" => {
                settings.samples = parse_positive("--samples", args.next());
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next()
                        .unwrap_or_else(|| usage("--output requires a path")),
                ));
            }
            "--help" | "-h" => usage(""),
            _ => usage(&format!("unknown argument: {arg}")),
        }
    }
    (settings, output)
}

fn parse_positive(flag: &str, value: Option<String>) -> usize {
    value
        .unwrap_or_else(|| usage(&format!("{flag} requires an integer")))
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or_else(|| usage(&format!("{flag} must be a positive integer")))
}

fn usage(error: &str) -> ! {
    if !error.is_empty() {
        eprintln!("error: {error}");
    }
    eprintln!(
        "usage: cargo run --release -p ocean-runtime --example history_cost_bench -- \
         [--warmup N] [--samples N] [--output PATH]"
    );
    std::process::exit(if error.is_empty() { 0 } else { 2 });
}

fn synthetic_history(count: usize) -> Vec<Message> {
    (0..count)
        .map(|index| {
            let payload = format!(
                "history-{index:04}: {}",
                "representative agent transcript payload ".repeat(5)
            );
            if index % 2 == 0 {
                Message::Assistant(AssistantMessage {
                    content: vec![Content::text(payload)],
                    api: "benchmark".into(),
                    provider: "benchmark".into(),
                    model: "benchmark".into(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                })
            } else {
                Message::User {
                    content: vec![Content::text(payload)],
                    timestamp: 0,
                }
            }
        })
        .collect()
}

fn synthetic_round_additions(rounds: usize) -> Vec<(Message, Message)> {
    (0..rounds.saturating_sub(1))
        .map(|round| {
            let call_id = format!("bench-call-{round}");
            let assistant = Message::Assistant(AssistantMessage {
                content: vec![Content::ToolCall {
                    id: call_id.clone(),
                    name: "benchmark_noop".into(),
                    arguments: json!({"round": round}),
                }],
                api: "benchmark".into(),
                provider: "benchmark".into(),
                model: "benchmark".into(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            });
            let result = Message::ToolResult(ToolResultMessage {
                tool_call_id: call_id,
                tool_name: "benchmark_noop".into(),
                content: vec![Content::text("ok")],
                is_error: false,
                timestamp: 0,
            });
            (assistant, result)
        })
        .collect()
}

fn run_rounds(
    history: &mut Vec<Message>,
    additions: &[(Message, Message)],
    rounds: usize,
    system_prompt: &str,
) -> usize {
    let mut checksum = 0usize;
    for round in 0..rounds {
        let outbound = trim_to_context_window(
            black_box(history.as_slice()),
            black_box(system_prompt),
            CONTEXT_WINDOW,
            MAX_OUTPUT_TOKENS,
        );
        checksum = checksum.wrapping_add(black_box(outbound.len()));
        if let Some((assistant, result)) = additions.get(round) {
            history.push(assistant.clone());
            history.push(result.clone());
        }
    }
    checksum
}

fn measure(work: impl FnOnce() -> usize) -> Sample {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNTING_ENABLED.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let checksum = black_box(work());
    let nanoseconds = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    COUNTING_ENABLED.store(false, Ordering::SeqCst);
    Sample {
        nanoseconds,
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        checksum,
    }
}

fn summarize(history_size: usize, rounds: usize, samples: &[Sample]) -> Value {
    let mut times: Vec<u64> = samples.iter().map(|sample| sample.nanoseconds).collect();
    let mut allocations: Vec<u64> = samples.iter().map(|sample| sample.allocations).collect();
    let mut bytes: Vec<u64> = samples
        .iter()
        .map(|sample| sample.allocated_bytes)
        .collect();
    times.sort_unstable();
    allocations.sort_unstable();
    bytes.sort_unstable();
    let checksum = samples
        .iter()
        .fold(0usize, |acc, sample| acc.wrapping_add(sample.checksum));
    let median_ns = percentile(&times, 50);
    let p95_ns = percentile(&times, 95);
    let min_ns = times[0];
    let max_ns = times[times.len() - 1];
    json!({
        "history_messages": history_size,
        "rounds": rounds,
        "median_ns": median_ns,
        "median_us": median_ns as f64 / 1_000.0,
        "p95_ns": p95_ns,
        "min_ns": min_ns,
        "max_ns": max_ns,
        "median_ns_per_round": median_ns / rounds as u64,
        "median_allocations": percentile(&allocations, 50),
        "median_allocated_bytes": percentile(&bytes, 50),
        "checksum": checksum,
        "samples_ns_sorted": times,
        "samples_allocations_sorted": allocations,
        "samples_allocated_bytes_sorted": bytes,
    })
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let index = ((sorted.len() * percentile).div_ceil(100)).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn cpu_description() -> String {
    let mac = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if mac != "unknown" && !mac.is_empty() {
        return mac;
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model name\t: "))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn memory_description() -> String {
    let mac = command_output("sysctl", &["-n", "hw.memsize"]);
    if mac != "unknown" && !mac.is_empty() {
        return format!("{mac} bytes");
    }
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("MemTotal:"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn os_description() -> String {
    let mac = command_output("sw_vers", &[]);
    if mac != "unknown" && !mac.is_empty() {
        mac
    } else {
        command_output("uname", &["-srv"])
    }
}
