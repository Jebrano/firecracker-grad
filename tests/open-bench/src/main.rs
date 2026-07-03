//! Standalone open() syscall microbenchmark.
//!
//! Measures the raw cost of the security_file_open / security_path_openat
//! LSM hook chain under three conditions, with no Firecracker and no VM
//! in the loop:
//!
//!   baseline  - no LSM restriction at all (the floor)
//!   landlock  - process is landlock_restrict_self()'d before the loop
//!   chroot    - process is chroot()'d before the loop
//!
//! Each subcommand is meant to be invoked as a *fresh process* (not forked
//! in-process), because chroot() and landlock_restrict_self() are
//! irreversible for the calling process. Use run_bench.sh to drive all
//! three conditions and diff the resulting JSON summaries.
//!
//! NOTE: swap `setup_landlock()` below for your actual landlock-jailer
//! ruleset-construction code (the ABI selection and access-rights set
//! should match exactly what landlock-jailer applies in production, or
//! this benchmark measures a different ruleset than the one in your
//! thesis). This version uses a minimal single-directory rule so the
//! harness is self-contained and compiles standalone.

use clap::{Parser, Subcommand};
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, ABI,
};
use nix::unistd::{chdir, chroot};
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::ffi::CString;
use std::os::unix::io::RawFd;


#[derive(Parser)]
#[command(name = "open-bench")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// No LSM restriction — control group.
    Baseline {
        #[arg(long)]
        target_dir: PathBuf,
        #[arg(long)]
        target_name: String,
        #[arg(long, default_value_t = 10_000)]
        iterations: u32,
        #[arg(long, default_value_t = 1_000)]
        warmup: u32,
        #[arg(long)]
        raw_out: Option<PathBuf>,
    },
    /// landlock_restrict_self() before the loop.
    Landlock {
        #[arg(long)]
        target_dir: PathBuf,
        #[arg(long)]
        target_name: String,
        #[arg(long)]
        allow_root: PathBuf,
        #[arg(long, default_value_t = 10_000)]
        iterations: u32,
        #[arg(long, default_value_t = 1_000)]
        warmup: u32,
        #[arg(long)]
        raw_out: Option<PathBuf>,
    },
    /// chroot() before the loop.
    Chroot {
        #[arg(long)]
        jail_root: PathBuf,
        #[arg(long)]
        target_name: String,
        #[arg(long, default_value_t = 10_000)]
        iterations: u32,
        #[arg(long, default_value_t = 1_000)]
        warmup: u32,
        #[arg(long)]
        raw_out: Option<PathBuf>,
    },
}


#[derive(Serialize)]
struct BenchSummary {
    condition: String,
    iterations: u32,
    warmup: u32,
    min_ns: u128,
    p50_ns: u128,
    p90_ns: u128,
    p99_ns: u128,
    p999_ns: u128,
    max_ns: u128,
    mean_ns: f64,
    stddev_ns: f64,
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summarize(condition: &str, mut samples: Vec<u128>, iterations: u32, warmup: u32) -> BenchSummary {
    samples.sort_unstable();
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<u128>() as f64 / n;
    let variance = samples
        .iter()
        .map(|&x| {
            let d = x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n;

    BenchSummary {
        condition: condition.to_string(),
        iterations,
        warmup,
        min_ns: *samples.first().unwrap_or(&0),
        p50_ns: percentile(&samples, 0.50),
        p90_ns: percentile(&samples, 0.90),
        p99_ns: percentile(&samples, 0.99),
        p999_ns: percentile(&samples, 0.999),
        max_ns: *samples.last().unwrap_or(&0),
        mean_ns: mean,
        stddev_ns: variance.sqrt(),
    }
}

/// Runs the timed open()/close() loop. Warmup iterations happen first and
/// are discarded, so we're measuring steady-state dentry/page-cache-hot
/// cost (i.e. the LSM hook's marginal cost), not cold-cache effects.
/// Open dir_fd BEFORE calling this. All configs use the same fd + name
/// so the VFS dentry walk is identical; only the LSM hooks differ.
fn run_openat_loop(dir_fd: RawFd, name: &str, iterations: u32, warmup: u32) -> Vec<u128> {
    let c_name = CString::new(name).expect("target_name contains null byte");

    // warmup — prime caches
    for _ in 0..warmup {
        // SAFETY: dir_fd is a valid O_DIRECTORY fd, c_name is valid.
        let fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        // SAFETY: same as above.
        let fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        let elapsed = t0.elapsed().as_nanos();
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        samples.push(elapsed);
    }
    samples
}


fn write_raw(path: &Path, samples: &[u128]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    for s in samples {
        writeln!(f, "{}", s)?;
    }
    Ok(())
}

/// Minimal single-rule ruleset for benchmark purposes.
/// Replace with your landlock-jailer ruleset-construction call for anything
/// that needs to match production rule *count*/depth (see idea #5 from the
/// earlier discussion — rule count is a plausible confound here).
fn setup_landlock(allow_root: &Path) -> RulesetStatus {
    // ABI::V3 covers the filesystem-hook path this benchmark exercises
    // (open/read/write mediation). Bump this to match whatever ABI level
    // your landlock-jailer targets on 6.17 once your landlock crate
    // version exposes the higher constant, so the two aren't silently
    // testing different rule sets.
    let abi = ABI::V7;
    let access_all: landlock::BitFlags<AccessFs> = AccessFs::from_all(abi);

    let ruleset = Ruleset::default()
        .handle_access(access_all)
        .expect("failed to configure ruleset access")
        .create()
        .expect("failed to create ruleset")
        .add_rule(PathBeneath::new(
            PathFd::new(allow_root).expect("failed to open allow_root"),
            AccessFs::ReadDir | AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::RemoveFile | AccessFs::MakeReg,
        ))
        .expect("failed to add rule for allow_root")
        .log_new_exec(true).expect("can't log");

    let status = ruleset.restrict_self().expect("restrict_self syscall failed");

    // Matches your existing project rule: NotEnforced is a hard failure,
    // never a silent fallback.
    if status.ruleset != RulesetStatus::FullyEnforced {
        eprintln!("FATAL: landlock ruleset not fully enforced: {:?}", status.ruleset);
        std::process::exit(1);
    }
    status.ruleset
}

fn main() {
    let cli = Cli::parse();

    let (condition, samples, iterations, warmup, raw_out) = match cli.mode {
        Mode::Baseline { target_dir, target_name, iterations, warmup, raw_out } => {
            // Open dir_fd BEFORE any restriction (none here, but kept symmetric).
            let dir_cstr = CString::new(
                target_dir.as_os_str().as_encoded_bytes()
            ).expect("target_dir contains null");
            // SAFETY: valid path, standard flags.
            let dir_fd = unsafe {
                libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            assert!(dir_fd >= 0, "open target_dir failed");

            let samples = run_openat_loop(dir_fd, &target_name, iterations, warmup);
            ("baseline_unrestricted".to_string(), samples, iterations, warmup, raw_out)
        }
        Mode::Landlock { target_dir, target_name, allow_root, iterations, warmup, raw_out } => {
            // Open dir_fd BEFORE landlock restriction.
            let dir_cstr = CString::new(
                target_dir.as_os_str().as_encoded_bytes()
            ).expect("target_dir contains null");
            let dir_fd = unsafe {
                libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            assert!(dir_fd >= 0, "open target_dir failed");

            // Now apply Landlock — dir_fd remains usable (opened pre-restriction).
            setup_landlock(&allow_root);

            let samples = run_openat_loop(dir_fd, &target_name, iterations, warmup);
            ("landlock_restricted".to_string(), samples, iterations, warmup, raw_out)
        }
        Mode::Chroot { jail_root, target_name, iterations, warmup, raw_out } => {
            // Open dir_fd pointing at the jail BEFORE chroot.
            let dir_cstr = CString::new(
                jail_root.as_os_str().as_encoded_bytes()
            ).expect("jail_root contains null");
            let dir_fd = unsafe {
                libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            assert!(dir_fd >= 0, "open jail_root failed");

            chroot(&jail_root).expect("chroot failed - needs CAP_SYS_CHROOT / root");
            chdir("/").expect("chdir(\"/\") after chroot failed");

            // dir_fd is still valid — it was opened before chroot.
            let samples = run_openat_loop(dir_fd, &target_name, iterations, warmup);
            ("chroot_restricted".to_string(), samples, iterations, warmup, raw_out)
        }
    };
    // ... rest unchanged (write_raw, summarize, println)


    if let Some(path) = &raw_out {
        write_raw(path, &samples).expect("failed to write raw samples file");
    }

    let summary = summarize(&condition, samples, iterations, warmup);
    println!("{}", serde_json::to_string(&summary).unwrap());
}
