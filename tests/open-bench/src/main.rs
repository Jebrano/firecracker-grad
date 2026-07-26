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
        /// Append to raw_out instead of truncating (for pooling samples
        /// across interleaved cycles into one file).
        #[arg(long, default_value_t = false)]
        raw_append: bool,
        /// Model snapshot-file creation instead of re-opening an existing
        /// file: each iteration creates a FRESH file (O_CREAT|O_EXCL) and
        /// unlinks it immediately after (outside the timed window), rather
        /// than repeatedly opening target_name. target_name is ignored
        /// when this is set. See run_openat_create_loop's doc comment.
        #[arg(long, default_value_t = false)]
        create: bool,
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
        #[arg(long, default_value_t = false)]
        raw_append: bool,
        #[arg(long, default_value_t = false)]
        create: bool,
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
        #[arg(long, default_value_t = false)]
        raw_append: bool,
        #[arg(long, default_value_t = false)]
        create: bool,
    },
    /// Mann-Whitney U test between two raw sample files (one ns value per line).
    /// Uses the normal approximation with tie correction, which is standard
    /// and accurate at the sample sizes this harness produces (thousands+).
    Analyze {
        #[arg(long)]
        a: PathBuf,
        #[arg(long, default_value = "A")]
        a_label: String,
        #[arg(long)]
        b: PathBuf,
        #[arg(long, default_value = "B")]
        b_label: String,
        /// Significance level for the verdict line.
        #[arg(long, default_value_t = 0.05)]
        alpha: f64,
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


/// Models Firecracker's snapshot-file creation specifically -- a NEW file
/// at a path that doesn't exist yet (O_CREAT|O_EXCL) -- rather than the
/// plain-open loop's repeated open of one already-existing file. This
/// exercises Landlock's MakeReg mediation on the *parent directory*, a
/// different rule path from ReadFile on the file itself, and matches
/// Firecracker's actual snapshot semantics (a fresh file per snapshot, not
/// reopening the same name).
///
/// Each iteration gets a fresh filename via a monotonic counter (shared
/// across warmup and timed phases, so names never repeat) and unlinks it
/// immediately after close -- OUTSIDE the timed window. Without the
/// unlink, directory size would grow with iteration count, and on
/// filesystems where lookup/insertion cost scales with directory size,
/// later iterations would pay a different (higher) cost than earlier
/// ones purely from that growth -- an ordering-adjacent confound in the
/// same family as the ones already found in this project (path depth,
/// run ordering), just triggered by iteration count instead of run order.
/// Warmup iterations run the identical create+unlink pattern (discarded)
/// rather than the plain-open loop's warmup, so whatever cache/ruleset
/// warmth exists going into the timed loop matches what the timed loop
/// itself does.
fn run_openat_create_loop(dir_fd: RawFd, iterations: u32, warmup: u32) -> Vec<u128> {
    let mut counter: u64 = 0;

    for _ in 0..warmup {
        let c_name = CString::new(format!("snapshot_{:016x}", counter)).unwrap();
        counter += 1;
        // SAFETY: dir_fd is a valid O_DIRECTORY fd; c_name is a filename
        // never used before in this directory, so O_EXCL cannot spuriously
        // collide with a leftover from an earlier iteration.
        let fd = unsafe {
            libc::openat(
                dir_fd,
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        // SAFETY: c_name is the path just (possibly) created above.
        unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), 0) };
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let c_name = CString::new(format!("snapshot_{:016x}", counter)).unwrap();
        counter += 1;

        let t0 = Instant::now();
        // SAFETY: same as the warmup loop above.
        let fd = unsafe {
            libc::openat(
                dir_fd,
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL | libc::O_CLOEXEC,
                0o600,
            )
        };
        let elapsed = t0.elapsed().as_nanos();
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        // Unlink OUTSIDE the timed window -- see doc comment above.
        unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), 0) };

        samples.push(elapsed);
    }
    samples
}


fn write_raw(path: &Path, samples: &[u128], append: bool) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let mut f = opts.open(path)?;
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

    // Analyze doesn't produce timing samples of its own, so it's handled
    // separately from the three benchmark conditions below.
    if let Mode::Analyze { a, a_label, b, b_label, alpha } = cli.mode {
        run_analyze(&a, &a_label, &b, &b_label, alpha);
        return;
    }

    let (condition, samples, iterations, warmup, raw_out, raw_append) = match cli.mode {
        Mode::Baseline { target_dir, target_name, iterations, warmup, raw_out, raw_append, create } => {
            // Open dir_fd BEFORE any restriction (none here, but kept symmetric).
            let dir_cstr = CString::new(
                target_dir.as_os_str().as_encoded_bytes()
            ).expect("target_dir contains null");
            // SAFETY: valid path, standard flags.
            let dir_fd = unsafe {
                libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            assert!(dir_fd >= 0, "open target_dir failed");

            let samples = if create {
                run_openat_create_loop(dir_fd, iterations, warmup)
            } else {
                run_openat_loop(dir_fd, &target_name, iterations, warmup)
            };
            ("baseline_unrestricted".to_string(), samples, iterations, warmup, raw_out, raw_append)
        }
        Mode::Landlock { target_dir, target_name, allow_root, iterations, warmup, raw_out, raw_append, create } => {
            // Open dir_fd BEFORE landlock restriction.
            let dir_cstr = CString::new(
                target_dir.as_os_str().as_encoded_bytes()
            ).expect("target_dir contains null");
            let dir_fd = unsafe {
                libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            assert!(dir_fd >= 0, "open target_dir failed");

            // Now apply Landlock — dir_fd remains usable (opened pre-restriction).
            // from_all() already grants MakeReg on allow_root, so --create
            // needs no ruleset changes here.
            setup_landlock(&allow_root);

            let samples = if create {
                run_openat_create_loop(dir_fd, iterations, warmup)
            } else {
                run_openat_loop(dir_fd, &target_name, iterations, warmup)
            };
            ("landlock_restricted".to_string(), samples, iterations, warmup, raw_out, raw_append)
        }
        Mode::Chroot { jail_root, target_name, iterations, warmup, raw_out, raw_append, create } => {
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
            let samples = if create {
                run_openat_create_loop(dir_fd, iterations, warmup)
            } else {
                run_openat_loop(dir_fd, &target_name, iterations, warmup)
            };
            ("chroot_restricted".to_string(), samples, iterations, warmup, raw_out, raw_append)
        }
        Mode::Analyze { .. } => unreachable!("handled above"),
    };

    if let Some(path) = &raw_out {
        write_raw(path, &samples, raw_append).expect("failed to write raw samples file");
    }

    let summary = summarize(&condition, samples, iterations, warmup);
    println!("{}", serde_json::to_string(&summary).unwrap());
}

// ---------------------------------------------------------------------
// Mann-Whitney U (Wilcoxon rank-sum), normal approximation with tie
// correction. Valid at the sample sizes this harness produces (n well
// into the thousands); the normal approximation to the U distribution
// is standard practice at that scale and doesn't require an exact-U
// table or an external stats crate.
// ---------------------------------------------------------------------

struct MannWhitneyResult {
    n1: usize,
    n2: usize,
    u1: f64,
    z: f64,
    p_two_tailed: f64,
    /// Vargha-Delaney A: P(a random sample from A > a random sample from
    /// B), with ties counted as half a win. 0.5 = no effect, further from
    /// 0.5 = larger effect. Read as "probability of superiority."
    prob_a_gt_b: f64,
}

fn read_samples(path: &Path) -> Vec<f64> {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse::<f64>().unwrap_or_else(|e| panic!("bad sample line {:?}: {}", l, e)))
        .collect()
}

/// Standard normal CDF via the Abramowitz-Stegun erf approximation
/// (max error ~1.5e-7), avoiding a dependency on an external stats crate.
fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

fn mann_whitney_u(a: &[f64], b: &[f64]) -> MannWhitneyResult {
    let n1 = a.len();
    let n2 = b.len();
    assert!(n1 > 0 && n2 > 0, "both samples must be non-empty");

    let mut combined: Vec<(f64, u8)> = a.iter().map(|&v| (v, 0u8))
        .chain(b.iter().map(|&v| (v, 1u8)))
        .collect();
    combined.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());

    // Assign average rank to tied values.
    let n = combined.len();
    let mut ranks = vec![0.0f64; n];
    let mut i = 0;
    let mut tie_term_sum = 0.0f64; // sum of (t^3 - t) over tie groups, for variance correction
    while i < n {
        let mut j = i + 1;
        while j < n && combined[j].0 == combined[i].0 {
            j += 1;
        }
        let tie_count = (j - i) as f64;
        // Ranks are 1-indexed; average rank across the tied block.
        let avg_rank = ((i + 1) as f64 + j as f64) / 2.0;
        for k in i..j {
            ranks[k] = avg_rank;
        }
        if tie_count > 1.0 {
            tie_term_sum += tie_count.powi(3) - tie_count;
        }
        i = j;
    }

    let r1: f64 = (0..n).filter(|&k| combined[k].1 == 0).map(|k| ranks[k]).sum();

    let n1f = n1 as f64;
    let n2f = n2 as f64;
    let u1 = r1 - n1f * (n1f + 1.0) / 2.0;

    let mean_u = n1f * n2f / 2.0;
    let nf = n1f + n2f;
    let sigma_u = (n1f * n2f / 12.0 * ((nf + 1.0) - tie_term_sum / (nf * (nf - 1.0)))).sqrt();

    // Continuity correction: move U1 half a step toward the mean before
    // standardizing.
    let cc = if u1 > mean_u { -0.5 } else { 0.5 };
    let z = if sigma_u > 0.0 { (u1 - mean_u + cc) / sigma_u } else { 0.0 };
    let p_two_tailed = 2.0 * (1.0 - normal_cdf(z.abs()));

    MannWhitneyResult {
        n1,
        n2,
        u1,
        z,
        p_two_tailed,
        prob_a_gt_b: u1 / (n1f * n2f),
    }
}

fn run_analyze(a_path: &Path, a_label: &str, b_path: &Path, b_label: &str, alpha: f64) {
    let a = read_samples(a_path);
    let b = read_samples(b_path);

    let median = |v: &[f64]| -> f64 {
        let mut s = v.to_vec();
        s.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let n = s.len();
        if n % 2 == 0 {
            (s[n / 2 - 1] + s[n / 2]) / 2.0
        } else {
            s[n / 2]
        }
    };
    let mean = |v: &[f64]| -> f64 { v.iter().sum::<f64>() / v.len() as f64 };

    let result = mann_whitney_u(&a, &b);

    let verdict = if result.p_two_tailed < alpha {
        format!(
            "SIGNIFICANT at alpha={alpha}: {a_label} and {b_label} differ (p={:.2e})",
            result.p_two_tailed
        )
    } else {
        format!(
            "NOT significant at alpha={alpha}: no evidence {a_label} and {b_label} differ (p={:.2e})",
            result.p_two_tailed
        )
    };

    #[derive(Serialize)]
    struct AnalyzeOutput {
        a_label: String,
        b_label: String,
        n_a: usize,
        n_b: usize,
        a_median_ns: f64,
        b_median_ns: f64,
        a_mean_ns: f64,
        b_mean_ns: f64,
        u_statistic: f64,
        z_score: f64,
        p_value_two_tailed: f64,
        prob_a_greater_than_b: f64,
        verdict: String,
    }

    let out = AnalyzeOutput {
        a_label: a_label.to_string(),
        b_label: b_label.to_string(),
        n_a: result.n1,
        n_b: result.n2,
        a_median_ns: median(&a),
        b_median_ns: median(&b),
        a_mean_ns: mean(&a),
        b_mean_ns: mean(&b),
        u_statistic: result.u1,
        z_score: result.z,
        p_value_two_tailed: result.p_two_tailed,
        prob_a_greater_than_b: result.prob_a_gt_b,
        verdict,
    };

    println!("{}", serde_json::to_string(&out).unwrap());
}
