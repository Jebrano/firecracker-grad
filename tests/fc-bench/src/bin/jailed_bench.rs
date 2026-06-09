//! fc-bench runner that invokes the Firecracker jailer before benchmarking.
//!
//! The jailer creates a chroot sandbox, drops privileges, and then execs
//! firecracker inside it. The API socket ends up at:
//!   <chroot-base>/firecracker/<id>/root/run/firecracker.socket
//!
//! After building, copy firecracker + jailer binaries into /mydata/fc-bench/.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fc_bench::fc_client::FcClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};
use tokio::time::sleep;

// ── CLI ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "jailed-bench")]
struct Args {
    // --- benchmark knobs (same as fc-bench) ---
    #[arg(long, default_value = "rand-read")]
    mode: String,

    #[arg(long, default_value = "30")]
    iterations: u32,

    #[arg(long, default_value = "/users/Jubranoo/fc-bench/results.json")]
    output: PathBuf,

    /// Run with Landlock-enabled binary
    #[arg(long)]
    landlock: bool,

    /// Path to the kernel image
    #[arg(long, default_value = "/users/Jubranoo/fc-bench/vmlinux-5.10.245")]
    kernel: PathBuf,

    /// Fio overrides: "bs=1M:iodepth=1" or "iters=100"
    #[arg(long)]
    bench_params: Option<String>,

    // --- jailer knobs ---
    /// Jailer binary path
    #[arg(long, default_value = "/users/Jubranoo/fc-bench/jailer")]
    jailer_path: PathBuf,

    /// Chroot base directory
    #[arg(long, default_value = "/srv/jailer")]
    chroot_base: PathBuf,

    /// Jail instance ID
    #[arg(long, default_value = "fc-bench-0")]
    jailer_id: String,

    /// UID to switch to after setup
    #[arg(long, default_value = "1000")]
    uid: u32,

    /// GID to switch to after setup
    #[arg(long, default_value = "1000")]
    gid: u32,

    /// Daemonize the jailer
    #[arg(long)]
    daemonize: bool,
}

// ── Config ────────────────────────────────────────────────────────

struct Config {
    jailer_path: PathBuf,
    fc_binary:   PathBuf,
    kernel:      PathBuf,
    rootfs:      PathBuf,
    bench_disk:  PathBuf,
    chroot_base: PathBuf,
    jailer_id:   String,
    uid:         u32,
    gid:         u32,
    daemonize:   bool,
    /// Host-visible path to the API socket inside the chroot
    socket:      PathBuf,
    serial_out:  PathBuf,
    fc_log:      PathBuf,
}

impl Config {
    fn new(args: &Args) -> Self {
        let base = Path::new("/users/Jubranoo/fc-bench");
        let fc_binary = base.join(if args.landlock {
            "firecracker-landlock"
        } else {
            "firecracker"
        });

        // Jailer's chroot layout:
        //   <chroot-base>/firecracker/<id>/root/
        let chroot_root = args.chroot_base
            .join("firecracker")
            .join(&args.jailer_id)
            .join("root");

        Self {
            jailer_path: args.jailer_path.clone(),
            fc_binary,
            kernel:      args.kernel.clone(),
            rootfs:      base.join("rootfs-baseline.ext4"),
            bench_disk:  base.join("bench-disk.raw"),
            chroot_base: args.chroot_base.clone(),
            jailer_id:   args.jailer_id.clone(),
            uid:         args.uid,
            gid:         args.gid,
            daemonize:   args.daemonize,
            socket:      chroot_root.join("run/firecracker.socket"),
            serial_out:  base.join("serial-output.txt"),
            fc_log:      base.join("fc.log"),
        }
    }
}

// ── Results ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct BenchResult {
    mode:           String,
    landlock:       bool,
    iteration:      u32,
    total_time_s:   f64,
    fio:            Value,
    instance_info:  Value,
    machine_config: Value,
}

// ── Main ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::new(&args);

    let mode_str = match &args.bench_params {
        Some(p) if !p.is_empty() => format!("{}:{}", args.mode, p),
        _ => args.mode.clone(),
    };

    println!(
        "jailed-bench: {} x {} | Landlock: {} | chroot: {}",
        mode_str, args.iterations, args.landlock, cfg.socket.display()
    );

    let mut all_results: Vec<BenchResult> = Vec::new();

    for i in 1..=args.iterations {
        println!("\n--- Iteration {}/{} ---", i, args.iterations);
        let result = run_one(&cfg, &mode_str, i, args.landlock).await?;
        println!("  Completed in {:.2}s", result.total_time_s);
        all_results.push(result);
        sleep(Duration::from_secs(2)).await;
    }

    let json = serde_json::to_string_pretty(&all_results)?;
    fs::write(&args.output, &json)?;
    println!("\nResults saved to {}", args.output.display());
    print_summary(&all_results);
    Ok(())
}

// ── Single benchmark run ──────────────────────────────────────────

async fn run_one(
    cfg: &Config,
    mode_str: &str,
    iteration: u32,
    landlock: bool,
) -> Result<BenchResult> {
    let _ = fs::remove_file(&cfg.socket);
    let _ = fs::remove_file(&cfg.serial_out);

    // Start the jailer (which execs firecracker inside the sandbox)
    let mut jailer = start_jailer(cfg)?;

    let result = async {
        // Socket appears inside the chroot — visible on host at the full path
        wait_for_socket(&cfg.socket, Duration::from_secs(10)).await?;

        let client = FcClient::new(cfg.socket.to_str().unwrap());

        let boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off benchmark={}",
            mode_str
        );
        client.set_boot_source(cfg.kernel.to_str().unwrap(), &boot_args).await?;
        client.add_drive("rootfs", cfg.rootfs.to_str().unwrap(), true, false).await?;
        client.add_drive("benchdisk", cfg.bench_disk.to_str().unwrap(), false, false).await?;
        client.set_machine_config(1, 512).await?;

        // Query API before boot — zero benchmark impact
        let instance_info = client.instance_info().await?;
        let machine_config = client.machine_config().await?;

        let t_start = Instant::now();
        client.start_instance().await?;
        wait_for_results(&cfg.serial_out, Duration::from_secs(120)).await?;

        let elapsed = t_start.elapsed().as_secs_f64();
        sleep(Duration::from_millis(200)).await;
        let fio_json = extract_results(&cfg.serial_out)?;

        Ok::<BenchResult, anyhow::Error>(BenchResult {
            mode: mode_str.to_string(),
            landlock,
            iteration,
            total_time_s: elapsed,
            fio: fio_json,
            instance_info,
            machine_config,
        })
    }.await;

    let _ = jailer.kill();
    let _ = jailer.wait();
    result
}

// ── Start jailer ──────────────────────────────────────────────────

fn start_jailer(cfg: &Config) -> Result<Child> {
    let serial_file = fs::File::create(&cfg.serial_out)?;
    let log_file    = fs::File::create(&cfg.fc_log)?;

    let mut cmd = Command::new(&cfg.jailer_path);
    cmd.args([
        "--id",              &cfg.jailer_id,
        "--exec-file",       cfg.fc_binary.to_str().unwrap(),
        "--uid",             &cfg.uid.to_string(),
        "--gid",             &cfg.gid.to_string(),
        "--chroot-base-dir", cfg.chroot_base.to_str().unwrap(),
    ]);

    if cfg.daemonize {
        cmd.arg("--daemonize");
    }

    // Everything after -- is forwarded to firecracker.
    // Paths are relative to chroot root, so "run/firecracker.socket" works.
    cmd.arg("--")
       .args([
           "--api-sock", "run/firecracker.socket",
           "--log-path", "fc.log",
           "--level",    "Info",
       ])
       .stdout(serial_file)   // VM serial console
       .stderr(log_file);      // firecracker log

    let child = cmd.spawn()?;
    println!("Jailer PID: {}", child.id());
    Ok(child)
}

// ── Polling helpers ───────────────────────────────────────────────

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!("Socket {:?} never appeared", path))
}

async fn wait_for_results(serial_path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if results_ready(serial_path) {
            return Ok(());
        }
        sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!("Benchmark timed out"))
}

fn results_ready(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains("===RESULTS_END==="))
        .unwrap_or(false)
}

// ── Result extraction ─────────────────────────────────────────────

fn extract_json_block(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')? + 1;
    Some(&raw[start..end])
}

fn extract_results(serial_path: &Path) -> Result<Value> {
    let content = fs::read_to_string(serial_path)?;
    let content = content.trim();
    let json_str = extract_json_block(content)
        .ok_or_else(|| anyhow!("No JSON object found in serial output"))?;
    serde_json::from_str(json_str)
        .with_context(|| format!("Failed to parse JSON. Content: '{}'", json_str))
}

// ── Summary ───────────────────────────────────────────────────────

fn print_summary(results: &[BenchResult]) {
    if results.is_empty() { return; }

    if let Some(first) = results.first() {
        println!("\n=== Firecracker Instance Info ===");
        println!("{}", serde_json::to_string_pretty(&first.instance_info).unwrap_or_default());
        println!("\n=== Firecracker Machine Config ===");
        println!("{}", serde_json::to_string_pretty(&first.machine_config).unwrap_or_default());
    }

    let times: Vec<f64> = results.iter().map(|r| r.total_time_s).collect();
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let variance = times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / times.len() as f64;
    let stddev = variance.sqrt();

    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p99 = sorted[(sorted.len() as f64 * 0.99) as usize];

    println!("\n=== Summary ({} iterations) ===", results.len());
    println!("  Mean:   {:.3}s", mean);
    println!("  Stddev: {:.3}s", stddev);
    println!("  Min:    {:.3}s", sorted[0]);
    println!("  P99:    {:.3}s", p99);
    println!("  Max:    {:.3}s", sorted[sorted.len() - 1]);
}
