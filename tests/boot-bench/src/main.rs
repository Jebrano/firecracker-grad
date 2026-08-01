//! boot-bench: real guest boot latency, chroot vs Landlock.
//!
//! Distinct from fleet-churn-bench (which stops at "API socket
//! connectable," deliberately before any guest boot happens) and from
//! setup-bench (which never execs Firecracker at all). This one actually
//! configures and boots a guest, using Firecracker's own built-in
//! boot-timer device -- the same mechanism Firecracker's own
//! `tests/integration_tests/performance/test_boottime.py` uses -- rather
//! than inventing a new readiness signal. Firecracker itself measures and
//! reports the number; this harness's job is just to configure the VM,
//! trigger the boot, and parse Firecracker's own log line.
//!
//! ============================================================================
//! REQUIRES A GUEST INIT BINARY THAT SIGNALS THE BOOT-TIMER DEVICE.
//! See the sibling boot-signal/ crate -- and READ ITS TOP COMMENT. The
//! magic MMIO address in there is an unverified placeholder; confirm it
//! against your own Firecracker source before trusting these numbers.
//! ============================================================================
//!
//! WHY --isolation EXISTS SEPARATELY FROM --condition
//! ----------------------------------------------------
//! --condition is a free-text label used only in the JSON output (you
//! could type anything). --isolation chroot|landlock actually changes
//! behavior: it decides how kernel/rootfs/snapshot paths are given to
//! Firecracker's API. This matters because chroot and Landlock give
//! Firecracker fundamentally different filesystem views after exec --
//! chroot's pivot_root remaps "/" to the jail, so paths must be given
//! relative-to-jail (e.g. "/vmlinux"); Landlock does no remapping, so
//! paths must be the real host-absolute location. An earlier version of
//! this harness passed the same --kernel/--rootfs host path straight
//! through for both conditions, which only actually works for Landlock --
//! under chroot it would silently need that exact host path to *also*
//! exist nested inside the jail, which it won't unless by coincidence.
//! Fixed by copying kernel+rootfs into jail_root once per cycle (same
//! precedent as jailer's own copy_exec_to_chroot, just for two more files)
//! and then passing condition-appropriate path strings for whichever one
//! is actually running -- this happens for BOTH conditions uniformly (not
//! just chroot) so a real, existing landlock rule (jail_root's own broad
//! grant, already covering ReadFile/WriteFile/MakeReg) covers these files
//! with no landlock.rs changes needed, at the cost of not exercising
//! Landlock's no-copy-needed advantage for kernel/rootfs specifically --
//! that's a different, explicitly-labeled experiment if you want to
//! demonstrate it, not something this latency benchmark needs.
//!
//! MECHANISM (matches your test_boottime.py snippet exactly):
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
//!      using the identical regex from your test_boottime.py.
//!   7. Report boot_time_us and cpu_boot_time_us -- Firecracker's own
//!      numbers, not anything measured by this harness -- as one JSON
//!      line.
//!   8. If --snapshot: PATCH /vm Paused, time PUT /snapshot/create,
//!      PATCH /vm Resumed (matches the confirmed-current sequence:
//!      snapshot/create is synchronous -- the HTTP response only returns
//!      once the snapshot write is actually complete, so timing the
//!      round-trip IS the snapshot latency, no separate completion signal
//!      needed the way boot required one). This measurement is dominated
//!      by actual memory-dump I/O (proportional to guest memory size) --
//!      it exists to positively confirm no regression at the layer where
//!      it could matter, not to isolate a microsecond-scale mechanism
//!      difference the way open-bench's --create mode does.
//!   9. Tear down and clean up.
//!
//! Usage:
//!   boot-bench \
//!     --jailer-bin /path/to/jailer            (or landlock-jailer) \
//!     --isolation chroot                      (or landlock -- see above) \
//!     --condition chroot                      (free-text label for output) \
//!     --exec-file /path/to/real/firecracker \
//!     --kernel /path/to/vmlinux \
//!     --rootfs /path/to/rootfs.ext4 \
//!     --uid 123 --gid 100 \
//!     --chroot-base-dir /srv/jailer-bench \
//!     --cycles 50 --timeout-ms 5000 \
//!     [--snapshot]
//!
//! Output: one JSON line per cycle, e.g.
//!   {"condition":"chroot","boot_time_us":83421,"cpu_boot_time_us":79102}
//! or, with --snapshot:
//!   {"condition":"chroot","boot_time_us":83421,"cpu_boot_time_us":79102,
//!    "pause_ns":812004,"snapshot_create_ns":41220331,"resume_ns":95441}
//! or on failure:
//!   {"condition":"chroot","error":"..."}
//!
//! Extract fields the same way as setup-bench/fleet-churn-bench's driver
//! scripts (grep -o '"boot_time_us":[0-9]*' | cut -d: -f2), then feed into
//! open-bench analyze.

use hyper::{Body, Client, Method, Request};
use hyperlocal::UnixClientExt;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Args {
    jailer_bin: PathBuf,
    isolation: String,
    condition: String,
    exec_file: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
    boot_args: String,
    uid: String,
    gid: String,
    chroot_base_dir: PathBuf,
    cycles: u32,
    timeout_ms: u64,
    snapshot: bool,
}

const DEFAULT_BOOT_ARGS: &str = "reboot=k panic=1 nomodule 8250.nr_uarts=0 \
    i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce \
    cryptomgr.notests init=/usr/local/bin/init";

fn usage_error(msg: &str) -> ! {
    eprintln!("{}", msg);
    eprintln!(
        "usage: boot-bench --jailer-bin <path> --isolation <chroot|landlock> \
         --condition <label> --exec-file <path> --kernel <path> --rootfs <path> \
         --uid <uid> --gid <gid> --chroot-base-dir <dir> [--boot-args <args>] \
         [--cycles N] [--timeout-ms N] [--snapshot]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut jailer_bin = None;
    let mut isolation = None;
    let mut condition = None;
    let mut exec_file = None;
    let mut kernel = None;
    let mut rootfs = None;
    let mut boot_args = DEFAULT_BOOT_ARGS.to_string();
    let mut uid = None;
    let mut gid = None;
    let mut chroot_base_dir = None;
    let mut cycles = 50u32;
    let mut timeout_ms = 5000u64;
    let mut snapshot = false;

    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut next_val = || {
            argv.next()
                .unwrap_or_else(|| usage_error(&format!("missing value for {}", flag)))
        };
        match flag.as_str() {
            "--jailer-bin" => jailer_bin = Some(PathBuf::from(next_val())),
            "--isolation" => isolation = Some(next_val()),
            "--condition" => condition = Some(next_val()),
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
            "--timeout-ms" => {
                timeout_ms = next_val()
                    .parse()
                    .unwrap_or_else(|_| usage_error("bad --timeout-ms"))
            }
            "--snapshot" => snapshot = true,
            other => usage_error(&format!("unknown flag {}", other)),
        }
    }

    let isolation = isolation.unwrap_or_else(|| usage_error("--isolation required (chroot|landlock)"));
    if isolation != "chroot" && isolation != "landlock" {
        usage_error(&format!(
            "--isolation must be \"chroot\" or \"landlock\", got {:?}",
            isolation
        ));
    }

    Args {
        jailer_bin: jailer_bin.unwrap_or_else(|| usage_error("--jailer-bin required")),
        isolation,
        condition: condition.unwrap_or_else(|| usage_error("--condition required")),
        exec_file: exec_file.unwrap_or_else(|| usage_error("--exec-file required")),
        kernel: kernel.unwrap_or_else(|| usage_error("--kernel required")),
        rootfs: rootfs.unwrap_or_else(|| usage_error("--rootfs required")),
        boot_args,
        uid: uid.unwrap_or_else(|| usage_error("--uid required")),
        gid: gid.unwrap_or_else(|| usage_error("--gid required")),
        chroot_base_dir: chroot_base_dir
            .unwrap_or_else(|| usage_error("--chroot-base-dir required")),
        cycles,
        timeout_ms,
        snapshot,
    }
}

fn spawn_jailer(args: &Args, id: &str, log_path: &Path) -> std::io::Result<Child> {
    Command::new(&args.jailer_bin)
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
        .stderr(Stdio::null())
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
            return Err(format!("jailer exited early with {}", status));
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

/// Identical regex to your test_boottime.py's timestamp_log_regex. Group 1
/// is boot_time_us, group 3 is boot_time_cpu_us -- same groups your
/// Python's `boot_time_us, _, boot_time_cpu_us, _ = timestamps[0]` picks.
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

fn run_one_cycle(args: &Args, id: &str, rt: &tokio::runtime::Runtime) -> Result<CycleResult, String> {
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

    // We need to chown the log file to the target uid/gid
    let uid: u32 = args.uid.parse().expect("uid must be a u32");
    let gid: u32 = args.gid.parse().expect("gid must be a u32");
    std::os::unix::fs::chown(&log_path, Some(uid), Some(gid))
        .map_err(|e| format!("failed to chown log file: {}", e))?;

    // Copy kernel+rootfs into jail_root for BOTH conditions -- see this
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

    let kernel_path_for_api = api_path_for(&args.isolation, &jail_root, "vmlinux");
    let rootfs_path_for_api = api_path_for(&args.isolation, &jail_root, "rootfs.ext4");

    let mut child = spawn_jailer(args, id, Path::new("/run/firecracker.log"))
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
            let snapshot_path_for_api = api_path_for(&args.isolation, &jail_root, "snapshot_file");
            let mem_file_path_for_api = api_path_for(&args.isolation, &jail_root, "mem_file");
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

fn main() {
    let args = parse_args();
    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");

    for cycle in 0..args.cycles {
        let id = format!("boot-{}-{}", std::process::id(), cycle);
        match run_one_cycle(&args, &id, &rt) {
            Ok(result) => {
                let mut out = format!(
                    "{{\"condition\":\"{}\",\"boot_time_us\":{},\"cpu_boot_time_us\":{}",
                    args.condition, result.boot_us, result.cpu_us
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
                println!("{{\"condition\":\"{}\",\"error\":\"{}\"}}", args.condition, e);
            }
        }
    }
}
