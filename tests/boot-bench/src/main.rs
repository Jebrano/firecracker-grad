//! Distinct from fleet-churn-bench (which stops at "API socket
//! connectable," deliberately before any guest boot happens) and from
//! setup-bench (which never execs Firecracker at all). This one actually
//! configures and boots a guest, using Firecracker's own built-in
//! boot-timer device, the same mechanism Firecracker's own
//! `tests/integration_tests/performance/test_boottime.py` uses, rather
//! than inventing a new readiness signal. Firecracker itself measures and
//! reports the number; this harness's job is just to configure the VM,
//! trigger the boot, and parse Firecracker's own log line.
//!
//! ============================================================================
//! REQUIRES A GUEST INIT BINARY THAT SIGNALS THE BOOT-TIMER DEVICE.
//! We can get one from the default devtool build_ci_artifacts rootfs.
//! ============================================================================
//! The `isolation` string ("chroot" or "landlock") decides how
//! kernel/rootfs/snapshot paths are given to
//! Firecracker's API, since chroot and Landlock give Firecracker
//! fundamentally different filesystem views after exec -- chroot's
//! pivot_root remaps "/" to the jail, so paths must be given
//! relative-to-jail (e.g. "/vmlinux"); Landlock does no remapping, so paths
//! must be the real host-absolute location.
//!
//! MECHANISM (matches test_boottime.py):
//!   1. Spawn jailer/landlock-jailer with extra args `--boot-timer
//!      --log-path <file> --level Info` (passed through to Firecracker,
//!      same as fleet-churn-bench's `--api-sock` extra arg).
//!   2. Wait for the API socket (reuses fleet-churn-bench's polling logic).
//!   3. PUT /boot-source with boot_args including `init=/usr/local/bin/init`
//!      (your rootfs must have boot-signal's output copied there).
//!   4. PUT /drives/rootfs.
//!   5. PUT /actions {"action_type":"InstanceStart"}.
//!   6. Poll the log file for the exact line Firecracker's own boot-timer
//!      device writes: "Guest-boot-time = N us M ms, N2 CPU us M2 CPU ms",
//!      using the identical regex from test_boottime.py.
//!   7. Report boot_time_us and cpu_boot_time_us -- Firecracker's own
//!      numbers.
//!   8. If --snapshot: PATCH /vm Paused, time PUT /snapshot/create,
//!      PATCH /vm Resumed (matches the confirmed-current sequence:
//!      snapshot/create is synchronous, the HTTP response only returns
//!      once the snapshot write is actually complete, so timing the
//!      round-trip IS the snapshot latency, no separate completion signal
//!      needed the way boot required one). This measurement is dominated
//!      by actual memory-dump I/O (proportional to guest memory size)
//!      it exists to positively confirm no regression at the layer where
//!      it could matter, not to isolate a microsecond-scale mechanism
//!      difference the way open-bench's --create mode does.
//!   9. Tear down and clean up.
//!
//! Usage (single invocation now measures BOTH conditions, interleaved):
//!   boot-bench \
//!     --jailer-bin-chroot /path/to/jailer \
//!     --jailer-bin-landlock /path/to/landlock-jailer \
//!     --exec-file /path/to/real/firecracker \
//!     --kernel /path/to/vmlinux \
//!     --rootfs /path/to/rootfs.ext4 \
//!     --uid 123 --gid 100 \
//!     --chroot-base-dir /srv/jailer-bench \
//!     --cycles 600 --warmup-cycles 10 --timeout-ms 5000 \
//!     [--snapshot] [--no-drop-caches]
//!
//! Output: one JSON line per MEASURED cycle (warm-up cycles print nothing to
//! stdout, only progress to stderr), e.g.
//!   {"condition":"chroot","cycle":0,"wall_clock_us":812345,
//!    "boot_time_us":83421,"cpu_boot_time_us":79102}
//!   {"condition":"landlock","cycle":0,"wall_clock_us":934120,
//!    "boot_time_us":71203,"cpu_boot_time_us":69884}
//! or, with --snapshot:
//!   {"condition":"chroot","cycle":0,"wall_clock_us":812345,
//!    "boot_time_us":83421,"cpu_boot_time_us":79102,
//!    "pause_ns":812004,"snapshot_create_ns":41220331,"resume_ns":95441}
//! or on failure:
//!   {"condition":"chroot","cycle":0,"wall_clock_us":812345,"error":"..."}
//!
//! `cycle` and `wall_clock_us` are the audit trail: split the single output
//! stream by "condition" (same grep-based extraction as before, e.g.
//! `grep -o '"boot_time_us":[0-9]*' | cut -d: -f2` per condition) and check
//! that `wall_clock_us` interleaves between conditions rather than running
//! as two contiguous blocks, before trusting the run as properly
//! interleaved. Feed the split fields into open-bench analyze as before.

use hyper::{Body, Client, Method, Request};
use hyperlocal::UnixClientExt;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// NOTE ON THIS REVISION: the previous version of this driver took a single
// `--isolation chroot|landlock` and `--jailer-bin` pair, meaning a full
// `--cycles N` run measured only one condition; getting both conditions
// required two separate process invocations writing to two separate files.
// That is a real methodological problem, not just an inconvenience: any
// drift over the course of a run (thermal throttling, frequency scaling
// settling, page-cache/dentry-cache warmth) then differs systematically
// between the two files rather than being controlled for, and can produce a
// "significant" gap between conditions that is actually just an artifact of
// which file was captured first. This revision takes both jailer binaries
// and interleaves chroot/Landlock cycles within a single process run (see
// `main()`), so both conditions experience the same drift.
struct Args {
    jailer_bin_chroot: PathBuf,
    jailer_bin_landlock: PathBuf,
    exec_file: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
    boot_args: String,
    uid: String,
    gid: String,
    chroot_base_dir: PathBuf,
    cycles: u32,
    /// Untimed cycles run per condition before measurement starts, and
    /// discarded. Exists specifically to absorb one-time cold-start costs
    /// (first vDSO touch, first page-cache population of the jailer/
    /// Firecracker binaries, ...) so they don't land in cycle 0 of the
    /// measured data and masquerade as a condition effect.
    warmup_cycles: u32,
    timeout_ms: u64,
    snapshot: bool,
    /// Drop the page cache before every measured cycle (both conditions).
    /// Defaults to on; `--no-drop-caches` exists only for local iteration
    /// where you don't have permission to write /proc/sys/vm/drop_caches --
    /// never disable it for a run whose numbers are going in the thesis.
    drop_caches: bool,
}

const DEFAULT_BOOT_ARGS: &str = "reboot=k panic=1 nomodule 8250.nr_uarts=0 \
    i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce \
    cryptomgr.notests init=/usr/local/bin/init";

fn usage_error(msg: &str) -> ! {
    eprintln!("{}", msg);
    eprintln!(
        "usage: boot-bench --jailer-bin-chroot <path> --jailer-bin-landlock <path> \
         --exec-file <path> --kernel <path> --rootfs <path> \
         --uid <uid> --gid <gid> --chroot-base-dir <dir> [--boot-args <args>] \
         [--cycles N] [--warmup-cycles N] [--timeout-ms N] [--snapshot] \
         [--no-drop-caches]\n\
         \n\
         Each measured cycle runs BOTH conditions back to back (chroot then \
         landlock), and cycles are what get interleaved -- this is what \
         replaces the old two-separate-invocations workflow. Do not run this \
         binary twice with a single-condition flag anymore; that flag no \
         longer exists, specifically to make the non-interleaved workflow \
         impossible to accidentally fall back into."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut jailer_bin_chroot = None;
    let mut jailer_bin_landlock = None;
    let mut exec_file = None;
    let mut kernel = None;
    let mut rootfs = None;
    let mut boot_args = DEFAULT_BOOT_ARGS.to_string();
    let mut uid = None;
    let mut gid = None;
    let mut chroot_base_dir = None;
    let mut cycles = 50u32;
    let mut warmup_cycles = 10u32;
    let mut timeout_ms = 5000u64;
    let mut snapshot = false;
    let mut drop_caches = true;

    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut next_val = || {
            argv.next()
                .unwrap_or_else(|| usage_error(&format!("missing value for {}", flag)))
        };
        match flag.as_str() {
            "--jailer-bin-chroot" => jailer_bin_chroot = Some(PathBuf::from(next_val())),
            "--jailer-bin-landlock" => jailer_bin_landlock = Some(PathBuf::from(next_val())),
            "--exec-file" => exec_file = Some(PathBuf::from(next_val())),
            "--kernel" => kernel = Some(PathBuf::from(next_val())),
            "--rootfs" => rootfs = Some(PathBuf::from(next_val())),
            "--boot-args" => boot_args = next_val(),
            "--uid" => uid = Some(next_val()),
            "--gid" => gid = Some(next_val()),
            "--chroot-base-dir" => chroot_base_dir = Some(PathBuf::from(next_val())),
            "--cycles" => {
                cycles = next_val()
                    .parse()
                    .unwrap_or_else(|_| usage_error("bad --cycles"))
            }
            "--warmup-cycles" => {
                warmup_cycles = next_val()
                    .parse()
                    .unwrap_or_else(|_| usage_error("bad --warmup-cycles"))
            }
            "--timeout-ms" => {
                timeout_ms = next_val()
                    .parse()
                    .unwrap_or_else(|_| usage_error("bad --timeout-ms"))
            }
            "--snapshot" => snapshot = true,
            "--no-drop-caches" => drop_caches = false,
            // Deliberately rejected rather than silently ignored: these are
            // the old single-condition flags. Erroring here (instead of, say,
            // quietly accepting --isolation and doing the wrong thing) is the
            // whole point -- it should be structurally impossible to
            // accidentally fall back to the old non-interleaved workflow.
            "--isolation" | "--condition" | "--jailer-bin" => usage_error(&format!(
                "{} was removed -- use --jailer-bin-chroot and \
                 --jailer-bin-landlock; every run now measures both \
                 conditions interleaved, in one invocation",
                flag
            )),
            other => usage_error(&format!("unknown flag {}", other)),
        }
    }

    Args {
        jailer_bin_chroot: jailer_bin_chroot
            .unwrap_or_else(|| usage_error("--jailer-bin-chroot required")),
        jailer_bin_landlock: jailer_bin_landlock
            .unwrap_or_else(|| usage_error("--jailer-bin-landlock required")),
        exec_file: exec_file.unwrap_or_else(|| usage_error("--exec-file required")),
        kernel: kernel.unwrap_or_else(|| usage_error("--kernel required")),
        rootfs: rootfs.unwrap_or_else(|| usage_error("--rootfs required")),
        boot_args,
        uid: uid.unwrap_or_else(|| usage_error("--uid required")),
        gid: gid.unwrap_or_else(|| usage_error("--gid required")),
        chroot_base_dir: chroot_base_dir
            .unwrap_or_else(|| usage_error("--chroot-base-dir required")),
        cycles,
        warmup_cycles,
        timeout_ms,
        snapshot,
        drop_caches,
    }
}

fn spawn_jailer(
    args: &Args,
    jailer_bin: &Path,
    id: &str,
    log_path: &Path,
) -> std::io::Result<Child> {
    Command::new(jailer_bin)
        .arg("--id")
        .arg(id)
        .arg("--exec-file")
        .arg(&args.exec_file)
        .arg("--uid")
        .arg(&args.uid)
        .arg("--gid")
        .arg(&args.gid)
        .arg("--chroot-base-dir")
        .arg(&args.chroot_base_dir)
        .arg("--")
        .arg("--api-sock")
        .arg("/run/api.sock")
        .arg("--boot-timer")
        .arg("--log-path")
        .arg(log_path)
        .arg("--level")
        .arg("Info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
}

/// Same polling approach as fleet-churn-bench's wait_for_socket: check the
/// child hasn't already exited, otherwise retry on a short interval.
fn wait_for_socket(
    socket_path: &Path,
    timeout: Duration,
    child: &mut Child,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err("timed out waiting for API socket".to_string());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut details = format!("jailer exited early with {}", status);
            if let Some(mut stderr_pipe) = child.stderr.take() {
                use std::io::Read;
                let mut buf = String::new();
                if stderr_pipe.read_to_string(&mut buf).is_ok() && !buf.is_empty() {
                    details.push_str("\njailer stderr: ");
                    details.push_str(buf.trim());
                }
            }
            return Err(details);
        }
        if socket_path.exists() {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
}


async fn send_request(
    client: &Client<hyperlocal::UnixConnector>,
    socket_path: &Path,
    method: Method,
    url_path: &str,
    body: String,
) -> Result<(), String> {
    let uri: hyper::Uri = hyperlocal::Uri::new(socket_path, url_path).into();
    let req = Request::builder()
        .method(method.clone())
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .map_err(|e| format!("building request for {} {} failed: {}", method, url_path, e))?;

    let resp = client
        .request(req)
        .await
        .map_err(|e| format!("request to {} {} failed: {}", method, url_path, e))?;
    let status = resp.status();
    if !status.is_success() {
        let bytes = hyper::body::to_bytes(resp.into_body())
            .await
            .unwrap_or_default();
        return Err(format!(
            "{} {} -> {}: {}",
            method,
            url_path,
            status,
            String::from_utf8_lossy(&bytes)
        ));
    }
    Ok(())
}

async fn put_request(
    client: &Client<hyperlocal::UnixConnector>,
    socket_path: &Path,
    url_path: &str,
    body: String,
) -> Result<(), String> {
    send_request(client, socket_path, Method::PUT, url_path, body).await
}

async fn configure_and_start(
    socket_path: &Path,
    kernel_path_for_api: &str,
    rootfs_path_for_api: &str,
    boot_args: &str,
) -> Result<(), String> {
    let client = Client::unix();

    let boot_source_body = serde_json::json!({
        "kernel_image_path": kernel_path_for_api,
        "boot_args": boot_args,
    })
    .to_string();
    put_request(&client, socket_path, "/boot-source", boot_source_body).await?;

    let drive_body = serde_json::json!({
        "drive_id": "rootfs",
        "path_on_host": rootfs_path_for_api,
        "is_root_device": true,
        "is_read_only": false,
    })
    .to_string();
    put_request(&client, socket_path, "/drives/rootfs", drive_body).await?;

    let start_body = serde_json::json!({"action_type": "InstanceStart"}).to_string();
    put_request(&client, socket_path, "/actions", start_body).await?;

    Ok(())
}

struct SnapshotTimings {
    pause_ns: u128,
    snapshot_create_ns: u128,
    resume_ns: u128,
}

/// Confirmed-current sequence (checked against several independent sources
/// spanning Firecracker versions, all consistent): pause, then
/// PUT /snapshot/create, then resume. snapshot/create is synchronous --
/// the HTTP response only comes back once the snapshot files are fully
/// written -- so bracketing the request with Instant::now() IS the
/// snapshot latency, unlike boot (which needed the boot-timer device
/// because InstanceStart returns before the guest is actually up).
async fn pause_snapshot_resume(
    socket_path: &Path,
    snapshot_path_for_api: &str,
    mem_file_path_for_api: &str,
) -> Result<SnapshotTimings, String> {
    let client = Client::unix();

    let t0 = Instant::now();
    let pause_body = serde_json::json!({"state": "Paused"}).to_string();
    send_request(&client, socket_path, Method::PATCH, "/vm", pause_body).await?;
    let pause_ns = t0.elapsed().as_nanos();

    let snapshot_body = serde_json::json!({
        "snapshot_type": "Full",
        "snapshot_path": snapshot_path_for_api,
        "mem_file_path": mem_file_path_for_api,
    })
    .to_string();
    let t1 = Instant::now();
    put_request(&client, socket_path, "/snapshot/create", snapshot_body).await?;
    let snapshot_create_ns = t1.elapsed().as_nanos();

    let t2 = Instant::now();
    let resume_body = serde_json::json!({"state": "Resumed"}).to_string();
    send_request(&client, socket_path, Method::PATCH, "/vm", resume_body).await?;
    let resume_ns = t2.elapsed().as_nanos();

    Ok(SnapshotTimings {
        pause_ns,
        snapshot_create_ns,
        resume_ns,
    })
}

fn wait_for_boot_time(
    log_path: &Path,
    timeout: Duration,
    child: &mut Child,
) -> Result<(u64, u64), String> {
    let re = Regex::new(r"Guest-boot-time =\s+(\d+) us\s+(\d+) ms,\s+(\d+) CPU us\s+(\d+) CPU ms")
        .unwrap();
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err("timed out waiting for Guest-boot-time log line".to_string());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("firecracker exited early with {}", status));
        }
        if let Ok(contents) = std::fs::read_to_string(log_path) {
            if let Some(caps) = re.captures(&contents) {
                let boot_us: u64 = caps[1].parse().unwrap_or(0);
                let cpu_us: u64 = caps[3].parse().unwrap_or(0);
                return Ok((boot_us, cpu_us));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Drops the page cache before a measured cycle. Requires root and a
/// writable /proc/sys/vm/drop_caches -- both already required elsewhere in
/// this harness (jailer itself needs root for chown/mknod).
///
/// Fails closed, matching this codebase's existing convention for isolation
/// guarantees (see `landlock.rs`'s hard failure on
/// `RulesetStatus::NotEnforced`): if we can't confirm the cache was actually
/// dropped, silently continuing would let stale cache state leak between
/// cycles undetected, defeating the entire reason this call exists. Better
/// to abort the run loudly than to produce numbers with the same silent
/// confound this revision was written to eliminate.
fn drop_caches() -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg("sync && echo 3 > /proc/sys/vm/drop_caches")
        .status()
        .map_err(|e| format!("failed to spawn drop_caches command: {}", e))?;
    if !status.success() {
        return Err(format!(
            "drop_caches command exited with {} -- are we running as root, \
             and is /proc/sys/vm/drop_caches writable?",
            status
        ));
    }
    Ok(())
}

fn teardown(child: &mut Child) {
    // SAFETY: pid is our own child, returned by our own Command::spawn().
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct CycleResult {
    boot_us: u64,
    cpu_us: u64,
    snapshot: Option<SnapshotTimings>,
}

/// Builds the path string to hand Firecracker's API for a file that's
/// been placed at `jail_root.join(fixed_name)` on the host -- resolving
/// differently per condition, per this file's top comment:
/// - chroot: relative-to-jail-root ("/fixed_name"), since pivot_root
///   already remapped "/" to jail_root by the time Firecracker sees it.
/// - landlock: the real host-absolute path, since no remapping happens.
fn api_path_for(isolation: &str, jail_root: &Path, fixed_name: &str) -> String {
    if isolation == "chroot" {
        format!("/{}", fixed_name)
    } else {
        jail_root.join(fixed_name).to_string_lossy().into_owned()
    }
}

fn run_one_cycle(
    args: &Args,
    isolation: &str,
    jailer_bin: &Path,
    id: &str,
    rt: &tokio::runtime::Runtime,
) -> Result<CycleResult, String> {
    let vm_dir = args
        .chroot_base_dir
        .join(args.exec_file.file_name().unwrap())
        .join(id);
    let jail_root = vm_dir.join("root");
    let socket_path = jail_root.join("run").join("api.sock");
    let log_path = jail_root.join("run").join("firecracker.log");

    std::fs::create_dir_all(&jail_root)
        .map_err(|e| format!("failed to pre-create jail dir: {}", e))?;
    // Firecracker requires --log-path's target file to already exist.
    std::fs::create_dir_all(jail_root.join("run"))
        .map_err(|e| format!("failed to pre-create run dir: {}", e))?;
    std::fs::write(&log_path, "").map_err(|e| format!("failed to pre-create log file: {}", e))?;

    // Copy kernel+rootfs into jail_root for BOTH conditions, see this
    // file's top comment for why this must happen uniformly, not just for
    // chroot. Safe to do before spawning jailer: chroot's bind-mount-over-
    // itself preserves whatever's already in jail_root (same precedent as
    // setup-bench's multi-file-open fixture files).
    let kernel_dest = jail_root.join("vmlinux");
    let rootfs_dest = jail_root.join("rootfs.ext4");
    std::fs::copy(&args.kernel, &kernel_dest)
        .map_err(|e| format!("failed to copy kernel into jail: {}", e))?;
    std::fs::copy(&args.rootfs, &rootfs_dest)
        .map_err(|e| format!("failed to copy rootfs into jail: {}", e))?;


    // Chown everything we placed in the jail so Firecracker (running as uid:gid)
    // can access them. Needed for BOTH chroot and Landlock.
    {
        let uid: u32 = args.uid.parse().map_err(|e| format!("bad uid: {}", e))?;
        let gid: u32 = args.gid.parse().map_err(|e| format!("bad gid: {}", e))?;
        for f in [&kernel_dest, &rootfs_dest, &log_path] {
            std::os::unix::fs::chown(f, Some(uid), Some(gid))
                .map_err(|e| format!("failed to chown {}: {}", f.display(), e))?;
        }
    }


    let kernel_path_for_api = api_path_for(isolation, &jail_root, "vmlinux");
    let rootfs_path_for_api = api_path_for(isolation, &jail_root, "rootfs.ext4");

    // Use jail-relative path for chroot, host-absolute for Landlock
    let log_path_for_fc = api_path_for(isolation, &jail_root, "run/firecracker.log");
    let mut child = spawn_jailer(args, jailer_bin, id, Path::new(&log_path_for_fc))
        .map_err(|e| format!("spawn failed: {}", e))?;

    let result = (|| -> Result<CycleResult, String> {
        wait_for_socket(&socket_path, Duration::from_millis(args.timeout_ms), &mut child)?;
        rt.block_on(configure_and_start(
            &socket_path,
            &kernel_path_for_api,
            &rootfs_path_for_api,
            &args.boot_args,
        ))?;
        let (boot_us, cpu_us) =
            wait_for_boot_time(&log_path, Duration::from_millis(args.timeout_ms), &mut child)?;

        let snapshot = if args.snapshot {
            let snapshot_path_for_api = api_path_for(isolation, &jail_root, "snapshot_file");
            let mem_file_path_for_api = api_path_for(isolation, &jail_root, "mem_file");
            Some(rt.block_on(pause_snapshot_resume(
                &socket_path,
                &snapshot_path_for_api,
                &mem_file_path_for_api,
            ))?)
        } else {
            None
        };

        Ok(CycleResult {
            boot_us,
            cpu_us,
            snapshot,
        })
    })();

    teardown(&mut child);
    let _ = std::fs::remove_dir_all(&vm_dir);

    result
}

/// One measured attempt: drop caches (if enabled), run the cycle, print a
/// JSON line carrying `cycle` and `wall_clock_us` alongside the existing
/// fields. Those two fields are the audit trail: they let interleaving be
/// verified directly from the output file after the fact (e.g. checking
/// that wall_clock_us is monotonically increasing and that cycle indices
/// for both conditions are interleaved, not one block then the other)
/// rather than resting on a claim in the methods section.
fn run_measured_cycle(
    args: &Args,
    isolation: &str,
    jailer_bin: &Path,
    cycle: u32,
    program_start: &Instant,
    rt: &tokio::runtime::Runtime,
) {
    if args.drop_caches {
        if let Err(e) = drop_caches() {
            eprintln!(
                "[boot-bench] FATAL at cycle {} ({}): {}",
                cycle, isolation, e
            );
            eprintln!(
                "[boot-bench] aborting rather than silently continuing without \
                 cache-drop guarantees -- partial output above should NOT be \
                 treated as a valid interleaved run"
            );
            std::process::exit(1);
        }
    }

    let wall_clock_us = program_start.elapsed().as_micros();
    let id = format!("boot-{}-{}-{}", std::process::id(), isolation, cycle);

    match run_one_cycle(args, isolation, jailer_bin, &id, rt) {
        Ok(result) => {
            let mut out = format!(
                "{{\"condition\":\"{}\",\"cycle\":{},\"wall_clock_us\":{},\
                 \"boot_time_us\":{},\"cpu_boot_time_us\":{}",
                isolation, cycle, wall_clock_us, result.boot_us, result.cpu_us
            );
            if let Some(snap) = result.snapshot {
                out.push_str(&format!(
                    ",\"pause_ns\":{},\"snapshot_create_ns\":{},\"resume_ns\":{}",
                    snap.pause_ns, snap.snapshot_create_ns, snap.resume_ns
                ));
            }
            out.push('}');
            println!("{}", out);
        }
        Err(e) => {
            println!(
                "{{\"condition\":\"{}\",\"cycle\":{},\"wall_clock_us\":{},\"error\":\"{}\"}}",
                isolation, cycle, wall_clock_us, e
            );
        }
    }
}

fn main() {
    let args = parse_args();
    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    let program_start = Instant::now();

    // Untimed warm-up, both conditions, fully discarded (nothing printed).
    // Absorbs one-time cold-start costs -- first vDSO touch, first
    // page-cache population of the jailer/Firecracker/kernel/rootfs files,
    // any lazy kernel-side initialization -- that would otherwise land in
    // cycle 0 of the measured data and could produce exactly the kind of
    // single-outlier artifact seen in the previous (non-interleaved) run.
    eprintln!(
        "[boot-bench] warm-up: {} cycles per condition (discarded, not printed)",
        args.warmup_cycles
    );
    for w in 0..args.warmup_cycles {
        let id_c = format!("warmup-chroot-{}-{}", std::process::id(), w);
        if let Err(e) = run_one_cycle(&args, "chroot", &args.jailer_bin_chroot, &id_c, &rt) {
            eprintln!("[boot-bench] warm-up cycle {} (chroot) failed: {}", w, e);
        }
        let id_l = format!("warmup-landlock-{}-{}", std::process::id(), w);
        if let Err(e) = run_one_cycle(&args, "landlock", &args.jailer_bin_landlock, &id_l, &rt) {
            eprintln!("[boot-bench] warm-up cycle {} (landlock) failed: {}", w, e);
        }
    }

    eprintln!(
        "[boot-bench] warm-up complete; starting {} interleaved measured cycles \
         (chroot, landlock per cycle)",
        args.cycles
    );

    // Measured, interleaved ABAB. Within a cycle the order is always
    // chroot-then-landlock (deterministic, not randomized), what actually
    // controls for drift is interleaving across cycles rather than running
    // all of one condition and then all of the other, not the order within
    // a single cycle.
    for cycle in 0..args.cycles {
        run_measured_cycle(
            &args,
            "chroot",
            &args.jailer_bin_chroot,
            cycle,
            &program_start,
            &rt,
        );
        run_measured_cycle(
            &args,
            "landlock",
            &args.jailer_bin_landlock,
            cycle,
            &program_start,
            &rt,
        );
    }
}
