// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Architecturally distinct from setup-bench's modes: those call into
//! `Env`/`setup_isolation` directly and never actually exec Firecracker.
//! This one spawns the real, unmodified `jailer`/`landlock-jailer` binaries
//! as external child processes with a real Firecracker `--exec-file`, and
//! measures wall-clock time from process spawn to the moment Firecracker's
//! API socket becomes connectable, This does NOT boot a guest: no
//! kernel/rootfs is configured, no InstanceStart action is sent,
//! so nothing past Firecracker's own HTTP server startup is measured.
//!
//! It intentionally links against neither `jailer`'s library nor `utils`
//! as it never constructs an `Env`, it just shells out to whichever real
//! binary path you give it, the same way an orchestrator would in
//! production. This is what makes it a fair test of the *actual*
//! `jailer`/`landlock-jailer` binaries rather than a reimplementation.
//!
//! Usage:
//!   fleet-churn-bench \
//!     --jailer-bin /path/to/jailer            (or /path/to/landlock-jailer) \
//!     --condition chroot                      (label only, or "landlock") \
//!     --exec-file /path/to/real/firecracker \
//!     --uid 123 --gid 100 \
//!     --chroot-base-dir /srv/jailer-bench \
//!     --cycles 200 \
//!     --timeout-ms 2000
//!
//! Prints one JSON line per cycle to stdout:
//!   {"condition":"chroot","socket_ready_ns":N}
//! or, on a timeout/early-exit for that cycle:
//!   {"condition":"chroot","error":"..."}
//!
//! Cleans up each VM's jail directory after tearing it down, so repeated
//! runs don't accumulate stale directories. A fresh --id is generated per
//! cycle (this process's pid + cycle counter) to avoid collisions if
//! cleanup from a previous cycle is still racing.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Args {
    jailer_bin: PathBuf,
    condition: String,
    exec_file: PathBuf,
    uid: String,
    gid: String,
    chroot_base_dir: PathBuf,
    cycles: u32,
    timeout_ms: u64,
}

fn usage_error(msg: &str) -> ! {
    eprintln!("{}", msg);
    eprintln!(
        "usage: fleet-churn-bench --jailer-bin <path> --condition <label> \
         --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir> \
         [--cycles N] [--timeout-ms N]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut jailer_bin = None;
    let mut condition = None;
    let mut exec_file = None;
    let mut uid = None;
    let mut gid = None;
    let mut chroot_base_dir = None;
    let mut cycles = 100u32;
    let mut timeout_ms = 2000u64;

    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut next_val = || {
            argv.next()
                .unwrap_or_else(|| usage_error(&format!("missing value for {}", flag)))
        };
        match flag.as_str() {
            "--jailer-bin" => jailer_bin = Some(PathBuf::from(next_val())),
            "--condition" => condition = Some(next_val()),
            "--exec-file" => exec_file = Some(PathBuf::from(next_val())),
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
            other => usage_error(&format!("unknown flag {}", other)),
        }
    }

    Args {
        jailer_bin: jailer_bin.unwrap_or_else(|| usage_error("--jailer-bin required")),
        condition: condition.unwrap_or_else(|| usage_error("--condition required")),
        exec_file: exec_file.unwrap_or_else(|| usage_error("--exec-file required")),
        uid: uid.unwrap_or_else(|| usage_error("--uid required")),
        gid: gid.unwrap_or_else(|| usage_error("--gid required")),
        chroot_base_dir: chroot_base_dir
            .unwrap_or_else(|| usage_error("--chroot-base-dir required")),
        cycles,
        timeout_ms,
    }
}

fn main() {
    let args = parse_args();

    let exec_file_name = args
        .exec_file
        .file_name()
        .unwrap_or_else(|| usage_error("--exec-file has no filename"))
        .to_owned();

    for cycle in 0..args.cycles {
        let id = format!("fleet-{}-{}", std::process::id(), cycle);
        let vm_dir = args.chroot_base_dir.join(&exec_file_name).join(&id);
        let socket_path = vm_dir.join("root").join("run").join("api.sock");

        let t0 = Instant::now();
        let mut child = match spawn_jailer(&args, &id) {
            Ok(c) => c,
            Err(e) => {
                println!(
                    "{{\"condition\":\"{}\",\"error\":\"spawn failed: {}\"}}",
                    args.condition, e
                );
                continue;
            }
        };

        match wait_for_socket(&socket_path, Duration::from_millis(args.timeout_ms), &mut child, t0)
        {
            Ok(elapsed_ns) => {
                println!(
                    "{{\"condition\":\"{}\",\"socket_ready_ns\":{}}}",
                    args.condition, elapsed_ns
                );
            }
            Err(msg) => {
                println!(
                    "{{\"condition\":\"{}\",\"error\":\"{}\"}}",
                    args.condition, msg
                );
            }
        }

        teardown(&mut child);
        let _ = std::fs::remove_dir_all(&vm_dir);
    }
}

fn spawn_jailer(args: &Args, id: &str) -> std::io::Result<Child> {
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Polls for the socket to become connectable. Also checks the child
/// hasn't already exited (crash, arg validation failure, etc.) so a fast
/// failure doesn't spin for the entire timeout window.
fn wait_for_socket(
    socket_path: &Path,
    timeout: Duration,
    child: &mut Child,
    t0: Instant,
) -> Result<u128, String> {
    loop {
        if t0.elapsed() > timeout {
            return Err("timed out waiting for socket".to_string());
        }

        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("jailer exited early with {}", status));
        }

        match UnixStream::connect(socket_path) {
            Ok(_) => return Ok(t0.elapsed().as_nanos()),
            Err(e) => {
                // Covers both "socket file doesn't exist yet" and
                // "exists but nothing's listening yet" -- both just mean
                // "not ready", so retry the same way either way.
                let _ = e;
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }
}

fn teardown(child: &mut Child) {
    // SAFETY: pid is one we own, returned by our own Command::spawn() above.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(200);
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
