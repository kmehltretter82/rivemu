//! Opt-in P17 Linux-to-shell performance evidence.
//!
//! The timing boundary matches the historical P17 fixture: construct a
//! 256 MiB machine with external OpenSBI and no VirtIO devices, then measure
//! from immediately after `Machine::new()` until BusyBox emits its first
//! `~ # ` shell prompt. Enable with `RIVEMU_LINUX_PERF=1`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rivemu_core::size::MIB;
use rivemu_machine::{Machine, MachineConfig};

struct Artifacts {
    bios: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
}

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/linux-test")
}

fn artifacts() -> Artifacts {
    let dir = image_dir();
    Artifacts {
        bios: dir.join("fw_dynamic.bin"),
        kernel: dir.join("Image"),
        initrd: dir.join("rootfs.cpio"),
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be a non-negative integer, got {value:?}")),
        Err(_) => default,
    }
}

fn selected_hart_counts() -> Vec<u32> {
    let raw = std::env::var("RIVEMU_LINUX_PERF_SMPS").unwrap_or_else(|_| "1,4".into());
    let values: Vec<u32> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value.parse::<u32>().unwrap_or_else(|_| {
                panic!("invalid hart count {value:?} in RIVEMU_LINUX_PERF_SMPS")
            })
        })
        .collect();
    assert!(!values.is_empty(), "RIVEMU_LINUX_PERF_SMPS is empty");
    assert!(
        values.iter().all(|&smp| smp == 1 || smp == 4),
        "RIVEMU_LINUX_PERF_SMPS supports only the P17 hart counts 1 and 4"
    );
    values
}

fn boot_to_shell_once(files: &Artifacts, smp: u32, timeout: Duration) -> Duration {
    let mut machine = Machine::new(MachineConfig {
        ram_size: 256 * MIB,
        smp,
        icount: None,
        bios: Some(files.bios.clone()),
        kernel: Some(files.kernel.clone()),
        initrd: Some(files.initrd.clone()),
        dtb: None,
        append: Some("console=ttyS0 rcupdate.rcu_cpu_stall_suppress=1".into()),
        ..Default::default()
    })
    .expect("machine init");

    // P17 deliberately excludes image loading and machine construction.
    let start = Instant::now();
    let mut console = String::new();
    while start.elapsed() < timeout {
        if let Some(event) = machine.run_slice() {
            panic!("guest produced {event:?} before reaching the shell:\n{console}");
        }
        let bytes = machine.uart.take_output();
        if !bytes.is_empty() {
            console.push_str(&String::from_utf8_lossy(&bytes));
            if console.contains("~ # ") {
                return start.elapsed();
            }
            if console.contains("Kernel panic")
                || console.contains("hard LOCKUP")
                || console.contains("rcu_sched detected stalls")
            {
                panic!("guest failed before reaching the shell:\n{console}");
            }
        }
        if machine.all_idle() {
            std::thread::sleep(machine.idle_timeout().min(Duration::from_millis(2)));
        }
    }
    panic!(
        "guest did not reach the shell within {:.2}s:\n{console}",
        timeout.as_secs_f64()
    );
}

/// Times a fixed amount of host work that does not involve the emulator: a
/// chain of dependent integer operations on one thread, like the
/// interpreter's own work. It runs right before and right after every boot,
/// so that a sample taken while the runner was slow (another VM on the host,
/// the thread on a slower core) shows as a slow calibration next to it.
fn calibrate() -> f64 {
    let start = Instant::now();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut acc: u64 = 0;
    for i in 0..60_000_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        acc = acc.wrapping_add(x.rotate_left((i & 63) as u32));
    }
    std::hint::black_box(acc);
    start.elapsed().as_secs_f64()
}

/// One boot with the calibration before and after it, printed as a
/// `P17_CALIB` line next to the `P17_SAMPLE` line.
fn timed_boot(files: &Artifacts, smp: u32, timeout: Duration, phase: &str, index: usize) -> f64 {
    let before = calibrate();
    let seconds = boot_to_shell_once(files, smp, timeout).as_secs_f64();
    let after = calibrate();
    eprintln!("P17_SAMPLE,smp={smp},phase={phase},index={index},seconds={seconds:.6}");
    eprintln!(
        "P17_CALIB,smp={smp},phase={phase},index={index},before={before:.6},after={after:.6}"
    );
    seconds
}

fn median(sorted: &[f64]) -> f64 {
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn nearest_rank_p95(sorted: &[f64]) -> f64 {
    let rank = (0.95 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1)]
}

#[test]
fn p17_linux_to_shell_benchmark() {
    let required = std::env::var_os("RIVEMU_REQUIRE_LINUX_PERF").is_some();
    let enabled = required || std::env::var_os("RIVEMU_LINUX_PERF").is_some();
    if !enabled {
        eprintln!("skipping P17 benchmark (set RIVEMU_LINUX_PERF=1 to run)");
        return;
    }

    let files = artifacts();
    for path in [&files.bios, &files.kernel, &files.initrd] {
        if !path.is_file() {
            assert!(
                !required,
                "missing Linux performance artifact {}",
                path.display()
            );
            eprintln!("skipping P17 benchmark: {} not found", path.display());
            return;
        }
    }

    let warmups = env_usize("RIVEMU_LINUX_PERF_WARMUPS", 2);
    let repetitions = env_usize("RIVEMU_LINUX_PERF_REPETITIONS", 20);
    assert!(repetitions > 0, "RIVEMU_LINUX_PERF_REPETITIONS must be > 0");
    let timeout = Duration::from_secs(env_usize("RIVEMU_LINUX_PERF_TIMEOUT_SECS", 90) as u64);
    let assert_thresholds = std::env::var_os("RIVEMU_LINUX_PERF_ASSERT_THRESHOLDS").is_some();

    for smp in selected_hart_counts() {
        for index in 0..warmups {
            timed_boot(&files, smp, timeout, "warmup", index);
        }

        let mut samples = Vec::with_capacity(repetitions);
        for index in 0..repetitions {
            samples.push(timed_boot(&files, smp, timeout, "recorded", index));
        }

        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        let median_seconds = median(&sorted);
        let p95_seconds = nearest_rank_p95(&sorted);
        eprintln!(
            "P17_RESULT,smp={smp},samples_seconds={samples:?},median_seconds={median_seconds:.6},p95_seconds={p95_seconds:.6}"
        );

        if assert_thresholds {
            let threshold = if smp == 1 { 8.0 } else { 10.0 };
            assert!(
                median_seconds < threshold && p95_seconds < threshold,
                "P17 smp={smp} threshold {threshold:.2}s missed: median={median_seconds:.6}s p95={p95_seconds:.6}s"
            );
        }
    }
}
