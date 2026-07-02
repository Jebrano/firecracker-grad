use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use jailer_client::fc_client::FcClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::chown,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};
use tokio::time::sleep;

// ── CLI ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "fc-bench")]
struct Args {
    #[arg(long, value_enum, default_value = "rand-read")]
    mode: BenchMode,

    #[arg(long, default_value = "30")]
    iterations: u32,

    #[arg(long, default_value = "/users/Jubranoo/fc-bench/results.json")]
    output: PathBuf,

    /// Run with Landlock-enabled binary instead of baseline
    #[arg(long)]
    landlock: bool,

    /// Path to the kernel image
    #[arg(long)]
    kernel: PathBuf,

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

    /// Network namespace path (optional)
    #[arg(long)]
    netns: Option<String>,

    /// Daemonize the jailer
    #[arg(long)]
    daemonize: bool,

    /// Fio overrides: "bs=1M:iodepth=1" or "iters=100"
    #[arg(long)]
    bench_params: Option<String>,
}

#[derive(Clone, ValueEnum)]
enum BenchMode {
    RandRead,
    RandWrite,
    SeqWrite,
    Mixed,
    /// Metadata stress — creates/stats/reads/unlinks files on a pre-formatted ext4 disk
    FileOps,
    /// Drop to an interactive shell on the serial console (for debugging)
    Shell,
}

impl BenchMode {
    fn as_str(&self) -> &'static str {
        match self {
            BenchMode::RandRead  => "rand_read",
            BenchMode::RandWrite => "rand_write",
            BenchMode::SeqWrite  => "seq_write",
            BenchMode::Mixed     => "mixed",
            BenchMode::FileOps   => "file_ops",
            BenchMode::Shell     => "shell",
        }
    }

    #[allow(dead_code)]
    fn is_file_ops(&self) -> bool {
        matches!(self, BenchMode::FileOps)
    }

    fn _is_shell(&self) -> bool {
        matches!(self, BenchMode::Shell)
    }
}

// ── Config ────────────────────────────────────────────────────────

struct Config {
    jailer_path: PathBuf,
    fc_binary:   PathBuf,
    kernel:      PathBuf,
    rootfs:      PathBuf,
    bench_disk:  PathBuf,
    /// Pre-formatted ext4 image for file_ops benchmark
    bench_fs:    PathBuf,
    chroot_base: PathBuf,
    jailer_id:   String,
    uid:         u32,
    gid:         u32,
    netns:       Option<String>,
    daemonize:   bool,
    /// Host-visible path to the API socket inside the chroot
    socket:      PathBuf,
    /// Host-visible path to the chroot root (where kernel/rootfs are copied)
    chroot_root: PathBuf,
    serial_out:  PathBuf,
    fc_log:      PathBuf,
}

impl Config {
    /// Return the disk image path for the given benchmark mode
    fn bench_image(&self, is_file_ops: bool) -> &Path {
        if is_file_ops { &self.bench_fs } else { &self.bench_disk }
    }

    fn new(args: &Args) -> Self {
        let base = Path::new("/users/Jubranoo/fc-bench");
        let fc_binary = if args.landlock {
            base.join("firecracker-landlock")
        } else {
            PathBuf::from("/users/Jubranoo/fc-bench/firecracker")
        };

        // Jailer chroot: <chroot-base>/firecracker/<id>/root/
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
            bench_fs:    base.join("bench-fs.ext4"),
            chroot_base: args.chroot_base.clone(),
            jailer_id:   args.jailer_id.clone(),
            uid:         args.uid,
            gid:         args.gid,
            netns:       args.netns.clone(),
            daemonize:   args.daemonize,
            socket:      chroot_root.join("run/firecracker.socket"),
            chroot_root,
            serial_out:  base.join("serial-output.txt"),
            fc_log:      base.join("fc.log"),
        }
    }
}

// ── Results ───────────────────────────────────────────────────────
// With Iteration
#[derive(Debug, Serialize, Deserialize)]
struct BenchResult {
    mode:           String,
    landlock:       bool,
    iteration:      u32,
    total_time_s:   f64,
    fio:            Value,
    fio_logs:       HashMap<String, Vec<FioLogEntry>>,
    instance_info:  Value,
    machine_config: Value,
}

/// A single entry from a fio built-in time-series log.
#[derive(Debug, Serialize, Deserialize)]
struct FioLogEntry {
    timestamp_ms: u64,
    value:        u64,
    /// 0 = read, 1 = write
    direction:    u8,
    /// Only present for latency logs (clat/slat), absent for iops/bw
    block_size:   Option<u64>,
}

// ── Main ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::new(&args);

    let mode_str = match &args.bench_params {
        Some(p) if !p.is_empty() => format!("{}:{}", args.mode.as_str(), p),
        _ => args.mode.as_str().to_string(),
    };

    println!(
        "Running {} x {} | Landlock: {}",
        mode_str,
        args.iterations,
        args.landlock
    );

    let mut all_results: Vec<BenchResult> = Vec::new();

    let result = run_one(&cfg, &mode_str, args.iterations, args.landlock).await?;
    println!(" Completed in {:.2}s", result.total_time_s);

    all_results.push(result);

    let json = serde_json::to_string_pretty(&all_results)?;
    fs::write(&args.output, &json)?;
    println!("\nResults saved to {}", args.output.display());
    print_summary(&all_results);

    Ok(())
}

// ── Single benchmark run ──────────────────────────────────────────
// Will modify it for sysbench later
async fn run_one(
    cfg: &Config,
    mode_str: &str,
    iteration: u32,
    landlock: bool,
) -> Result<BenchResult> {

    // Clean up leftovers from previous run
    let _ = fs::remove_file(&cfg.socket);
    let _ = fs::remove_file(&cfg.serial_out);

    // Wipe previous chroot to avoid "File exists" errors from mknod
    let _ = fs::remove_dir_all(&cfg.chroot_root);

    // Pre-create the chroot root and copy resources into it.
    // The jailer will chroot here; firecracker can only see files inside.
    fs::create_dir_all(&cfg.chroot_root)?;
    copy_into_jail(&cfg.chroot_root, &cfg.kernel, cfg.uid, cfg.gid)?;
    copy_into_jail(&cfg.chroot_root, &cfg.rootfs, cfg.uid, cfg.gid)?;
    let is_file_ops = mode_str.starts_with("file_ops");
    copy_into_jail(&cfg.chroot_root, cfg.bench_image(is_file_ops), cfg.uid, cfg.gid)?;
    println!("Resources copied into chroot: {}", cfg.chroot_root.display());

    // Start the jailer (which execs firecracker inside the sandbox)
    let mut jailer = start_jailer(cfg)?;

    // Shell mode: skip API setup, just wait for guest to poweroff
    if mode_str == "shell" {
        println!("Shell mode: jailer PID {} — attach with:  tail -f {}",
                 jailer.id(), cfg.serial_out.display());
        let status = jailer.wait()?;
        println!("Jailer exited with: {}", status);
        return Ok(BenchResult {
            mode:         "shell".to_string(),
            landlock,
            iteration:     0,
            total_time_s:   0.0,
            fio:            serde_json::json!({"shell": "exited"}),
            fio_logs:       HashMap::new(),
            instance_info:  serde_json::json!({}),
            machine_config: serde_json::json!({}),
        });
    }

    let result = async {
        // Socket appears inside the chroot — visible on host at the full path
        wait_for_socket(&cfg.socket, Duration::from_secs(15), &mut jailer, &cfg.fc_log).await?;

        let client = FcClient::new(cfg.socket.to_str().unwrap());

        // Configure the VM — paths are relative to chroot root
        let boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off benchmark={} ",
            mode_str
        );

        // Resources were copied into chroot root; only filenames needed
        let kernel_name = cfg.kernel.file_name().unwrap().to_str().unwrap();
        let rootfs_name = cfg.rootfs.file_name().unwrap().to_str().unwrap();
        let bench_name  = cfg.bench_image(is_file_ops).file_name().unwrap().to_str().unwrap();

        client.set_boot_source(
            &format!("/{}", kernel_name),
            &boot_args
        ).await?;
        client.add_drive(
            "rootfs",
            &format!("/{}", rootfs_name),
            true,
            false
        ).await?;
        client.add_drive(
            "benchdisk",
            &format!("/{}", bench_name),
            false,
            false
        ).await?;
        client.set_machine_config(1, 512).await?;


        let instance_info = client.instance_info().await?;
        let machine_config = client.machine_config().await?;

        // Boot and time it
        let t_start = Instant::now();
        client.start_instance().await?;

        // Poll for completion marker
        wait_for_results(&cfg.serial_out, Duration::from_secs(900)).await?;
        // give the kernel page cache a moment to flush buffered writes
        let elapsed = t_start.elapsed().as_secs_f64();

        sleep(Duration::from_millis(200)).await;
        // Extract JSON and fio built-in logs from serial output
        let (fio_json, fio_logs) = extract_blocks(&cfg.serial_out)?;

        Ok::<BenchResult, anyhow::Error>(BenchResult {
            mode:         mode_str.to_string(),
            landlock,
            iteration,
            total_time_s: elapsed,
            fio:          fio_json,
            fio_logs,
            instance_info,
            machine_config,
        })
    }.await;



    // Always kill the jailer (which kills firecracker)
    let _ = jailer.kill();
    let _ = jailer.wait();

    result
}


// ── Process management ────────────────────────────────────────────

/// Copy a file into the jail chroot and chown it so firecracker can read it.
fn copy_into_jail(chroot_root: &Path, src: &Path, uid: u32, gid: u32) -> Result<()> {
    let name = src.file_name()
        .ok_or_else(|| anyhow!("No filename in {:?}", src))?;
    let dst = chroot_root.join(name);
    if !dst.exists() {
        println!("  Copying {} -> {}", src.display(), dst.display());
        std::fs::copy(src, &dst)?;
        // Firecracker runs as this uid/gid after the jailer drops privileges
        chown(&dst, Some(uid), Some(gid))?;
    }
    Ok(())
}

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

    if let Some(ref ns) = cfg.netns {
        cmd.args(["--netns", ns]);
    }
    if cfg.daemonize {
        cmd.arg("--daemonize");
    }

    // Everything after -- is forwarded to firecracker.
    // Paths are relative to chroot root.
    cmd.arg("--")
       .args([
           "--api-sock", "run/firecracker.socket",
           "--log-path", "fc.log",
           "--level",    "Info",
       ])
       .stdout(serial_file)     // VM serial console
       .stderr(log_file);       // firecracker log

    let child = cmd.spawn()?;
    println!("Jailer PID: {}  (socket: {})", child.id(), cfg.socket.display());
    Ok(child)
}

// ── Polling helpers ───────────────────────────────────────────────

async fn wait_for_socket(path: &Path, timeout: Duration, child: &mut Child, log_path: &Path) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                let log = fs::read_to_string(log_path).unwrap_or_default();
                let tail: String = log.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                return Err(anyhow!(
                    "Jailer exited early with {}.\nLast 10 lines of jailer log:\n{}",
                    status, tail
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(anyhow!("Failed to check jailer status: {}", e)),
        }
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!("Socket {:?} never appeared after {:?}", path, timeout))
}

async fn wait_for_results(serial_path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    // let mut last_print = Instant::now();

    while Instant::now() < deadline {
        if results_ready(serial_path) {
            return Ok(());
        }

        // Print progress every 10 seconds
        // if last_print.elapsed() >= Duration::from_secs(10) {
        //     let elapsed = deadline - Instant::now();
        //     let last_line = last_serial_line(serial_path);
        //     println!(
        //         "  {}s remaining | {}",
        //         timeout.as_secs() - (timeout.as_secs() - elapsed.as_secs()),
        //         last_line
        //     );
        //     last_print = Instant::now();
        // }

        sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow!("Benchmark timed out"))
}

fn results_ready(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains("===RESULTS_END==="))
        .unwrap_or(false)
}

// fn last_serial_line(path: &Path) -> String {
//     fs::read_to_string(path)
//         .unwrap_or_default()
//         .lines()
//         .last()
//         .unwrap_or("")
//         .to_string()
// }

// ── Result extraction ─────────────────────────────────────────────

/// Parse the multi-block serial output into fio JSON + named log blocks.
fn extract_blocks(serial_path: &Path) -> Result<(Value, HashMap<String, Vec<FioLogEntry>>)> {
    let content = fs::read_to_string(serial_path)?;

    let json_str = extract_json_block(&content)
        .ok_or_else(|| anyhow!("No JSON object found in serial output"))?;

    let fio_json: Value = serde_json::from_str(json_str)
        .with_context(|| format!("Failed to parse fio JSON: '{}'", json_str))?;

    let mut fio_logs: HashMap<String, Vec<FioLogEntry>> = HashMap::new();
    let mut current_tag = String::new();
    let mut current_buf = String::new();
    let mut collecting = false;

    for line in content.lines() {
        if let Some(tag) = line.strip_prefix("===FIO_LOG ").and_then(|s| s.strip_suffix("===")) {
            current_tag = tag.to_string();
            current_buf.clear();
            collecting = true;
            continue;
        }
        if line == "===FIO_LOG_END===" && collecting {
            fio_logs.insert(
                current_tag.clone(),
                parse_fio_log(&current_buf),
            );
            collecting = false;
            continue;
        }
        if collecting {
            current_buf.push_str(line);
            current_buf.push('\n');
        }
    }

    Ok((fio_json, fio_logs))
}

/// Convert raw fio log lines (CSV) into structured entries.
///
/// fio iops/bw logs:    `timestamp_ms, value, direction`
/// fio lat/clat/slat logs: `timestamp_ms, value, direction, block_size`
fn parse_fio_log(raw: &str) -> Vec<FioLogEntry> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let parts: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
            match parts.len() {
                3 => Some(FioLogEntry {
                    timestamp_ms: parts[0].parse().ok()?,
                    value:        parts[1].parse().ok()?,
                    direction:    parts[2].parse().ok()?,
                    block_size:   None,
                }),
                4 => Some(FioLogEntry {
                    timestamp_ms: parts[0].parse().ok()?,
                    value:        parts[1].parse().ok()?,
                    direction:    parts[2].parse().ok()?,
                    block_size:   parts[3].parse().ok(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Strip non-JSON prefix/suffix lines from between the RESULTS markers,
/// returning only the JSON object block.
fn extract_json_block(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')? + 1;
    Some(&raw[start..end])
}

// ── Summary ───────────────────────────────────────────────────────
fn print_summary(results: &[BenchResult]) {
    if results.is_empty() { return; }

    // Print API-verified configuration from the first run (same for all)
    if let Some(first) = results.first() {
        println!("\n=== Firecracker Instance Info ===");
        println!("{}", serde_json::to_string_pretty(&first.instance_info).unwrap_or_default());
        println!("\n=== Firecracker Machine Config ===");
        println!("{}", serde_json::to_string_pretty(&first.machine_config).unwrap_or_default());
    }

    let times: Vec<f64> = results.iter().map(|r| r.total_time_s).collect();
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let variance = times.iter()
        .map(|t| (t - mean).powi(2))
        .sum::<f64>() / times.len() as f64;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_with_prefix_and_suffix_lines() {
        let raw = r#"Running benchmark: mixed with engine: libaio
{
  "fio version" : "fio-3.41",
  "jobs" : [{"jobname":"mixed","read":{"iops":202332}}]
}
===ALL_DONE==="#;
        let json = extract_json_block(raw).expect("should find JSON block");
        let val: Value = serde_json::from_str(json).expect("should parse");
        assert_eq!(val["fio version"], "fio-3.41");
        assert_eq!(val["jobs"][0]["read"]["iops"], 202332);
    }

    #[test]
    fn extracts_json_with_only_prefix_line() {
        let raw = "status line\n{\"x\":1}";
        let json = extract_json_block(raw).expect("should find JSON block");
        assert_eq!(json, "{\"x\":1}");
    }

    #[test]
    fn extracts_json_with_only_suffix_line() {
        let raw = "{\"x\":1}\n===ALL_DONE===";
        let json = extract_json_block(raw).expect("should find JSON block");
        assert_eq!(json, "{\"x\":1}");
    }

    #[test]
    fn returns_none_for_no_braces() {
        assert!(extract_json_block("just some text").is_none());
    }

    #[test]
    fn parses_iops_log_lines() {
        let raw = "123,456,0\n124,789,1\n";
        let entries = parse_fio_log(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].timestamp_ms, 123);
        assert_eq!(entries[0].value, 456);
        assert_eq!(entries[0].direction, 0);
        assert_eq!(entries[1].direction, 1);
    }

    #[test]
    fn parses_lat_log_lines_with_block_size() {
        let raw = "555,1000,0,4096\n556,1500,1,8192\n";
        let entries = parse_fio_log(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].block_size, Some(4096));
        assert_eq!(entries[1].block_size, Some(8192));
    }

    #[test]
    fn parses_fio_log_with_empty_lines() {
        let raw = "\n\n123,456,0\n\n124,789,1\n\n";
        let entries = parse_fio_log(raw);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn parses_fio_log_with_malformed_lines() {
        let raw = "123,456,0\nbad_line\n124,789,1\n";
        let entries = parse_fio_log(raw);
        assert_eq!(entries.len(), 2);
    }
}
