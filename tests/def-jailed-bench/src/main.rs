use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use jailer_client::fc_client::FcClient;
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
}

#[derive(Clone, ValueEnum)]
enum BenchMode {
    RandRead,
    RandWrite,
    SeqWrite,
    Mixed,
}

impl BenchMode {
    fn as_str(&self) -> &'static str {
        match self {
            BenchMode::RandRead  => "rand_read",
            BenchMode::RandWrite => "rand_write",
            BenchMode::SeqWrite  => "seq_write",
            BenchMode::Mixed     => "mixed",
        }
    }
}

// ── Config ────────────────────────────────────────────────────────

struct Config {
    fc_binary:  PathBuf,
    kernel:     PathBuf,
    rootfs:     PathBuf,
    bench_disk: PathBuf,
    socket:     PathBuf,
    serial_out: PathBuf,
    fc_log:     PathBuf,
}

impl Config {
    fn new(landlock: bool, kernel: PathBuf) -> Self {
        let base = Path::new("/users/Jubranoo/fc-bench");
        Self {
            fc_binary:  base.join(if landlock {
                "firecracker-landlock"
            } else {
                "firecracker"
            }),
            kernel,
            rootfs:     base.join("rootfs-baseline.ext4"),
            bench_disk: base.join("bench-disk.raw"),
            socket:     PathBuf::from("/tmp/fc-bench.socket"),
            serial_out: base.join("serial-output.txt"),
            fc_log:     base.join("fc.log"),
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
    instance_info:  Value,
    machine_config: Value,
}

// ── Main ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::new(args.landlock, args.kernel);

    println!(
        "Running {} x {} | Landlock: {}",
        args.mode.as_str(),
        args.iterations,
        args.landlock
    );

    let mut all_results: Vec<BenchResult> = Vec::new();

    let result = run_one(&cfg, &args.mode, args.iterations, args.landlock).await?;
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
    mode: &BenchMode,
    iteration: u32,
    landlock: bool,
) -> Result<BenchResult> {

    // Clean up leftovers from previous run
    let _ = fs::remove_file(&cfg.socket);
    let _ = fs::remove_file(&cfg.serial_out);

    // Start Firecracker
    let mut fc = start_firecracker(cfg)?;

    let result = async {
        wait_for_socket(&cfg.socket, Duration::from_secs(5)).await?;

        let client = FcClient::new(cfg.socket.to_str().unwrap());

        // Configure the VM
        let boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off benchmark={} ",
            mode.as_str()
        );
        client.set_boot_source(
            cfg.kernel.to_str().unwrap(),
            &boot_args
        ).await?;
        client.add_drive(
            "rootfs",
            cfg.rootfs.to_str().unwrap(),
            true,
            false
        ).await?;
        client.add_drive(
            "benchdisk",
            cfg.bench_disk.to_str().unwrap(),
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
        // Extract JSON from serial output
        let fio_json = extract_results(&cfg.serial_out)?;

        Ok::<BenchResult, anyhow::Error>(BenchResult {
            mode:         mode.as_str().to_string(),
            landlock,
            iteration,
            total_time_s: elapsed,
            fio:          fio_json,
            instance_info,
            machine_config,
        })
    }.await;



    // Always kill Firecracker regardless of success or failure
    let _ = fc.kill();
    let _ = fc.wait();

    result
}


// ── Process management ────────────────────────────────────────────

fn start_firecracker(cfg: &Config) -> Result<Child> {
    let serial_file = fs::File::create(&cfg.serial_out)?;
    let log_file    = fs::File::create(&cfg.fc_log)?;

    let child = Command::new(&cfg.fc_binary)
        .args([
            "--api-sock", cfg.socket.to_str().unwrap(),
            "--log-path", cfg.fc_log.to_str().unwrap(),
            "--level",    "Info",
        ])
        .stdout(serial_file)
        .stderr(log_file)
        .spawn()?;

    println!("Firecracker PID: {}", child.id());
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

/// Strip non-JSON prefix/suffix lines (status messages, markers) from between
/// the RESULTS markers, returning only the JSON object block.
fn extract_json_block(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    // walk backward from the end to find the matching closing brace
    let end = raw.rfind('}')? + 1;
    Some(&raw[start..end])
}

fn extract_results(serial_path: &Path) -> Result<Value> {
    let content = fs::read_to_string(serial_path)?;


    let json_str = content.trim();
    let json_str = extract_json_block(json_str)
        .ok_or_else(|| anyhow!("No JSON object found between markers"))?;


    serde_json::from_str(json_str)
        .with_context(|| format!("Failed to parse JSON between markers. Content: '{}'", json_str))
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
}
