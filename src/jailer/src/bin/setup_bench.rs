// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Standalone benchmark binary for the jailer/landlock-jailer A/B comparison.
//!
//! This binary is deliberately separate from `jailer` and `landlock-jailer`:
//! neither of those binaries, nor `lib.rs`'s shared CLI surface, carries any
//! benchmark-only code path or flag. `setup-bench` links against the same
//! `jailer` library those two binaries use (`Env`, `Isolation`,
//! `build_arg_parser`) and calls `Env::run_setup_only()`, which shares its
//! entire setup implementation with production `Env::run()` via the private
//! `setup_isolation` method in env.rs -- so this binary can't silently drift
//! from what `jailer`/`landlock-jailer` actually do, without needing to be
//! part of either of them.
//!
//! Two modes, selected by `--mode`:
//!
//!   setup            (default) Runs the chosen isolation backend's setup
//!                     phase exactly once, prints one JSON line of phase
//!                     timings, exits. Never execs --exec-file.
//!
//!   multi-file-open   Runs the same setup phase, then -- from inside the
//!                     now-restricted process -- opens a realistic set of
//!                     files (kernel image, rootfs, metrics FIFO, log,
//!                     /dev/kvm, /dev/net/tun), timing each open
//!                     individually, and prints setup phases plus per-file
//!                     open timings as one JSON line.
//!
//! A fresh process per sample is intentional in both modes: see
//! `Env::run_setup_only`'s doc comment for why.
//!
//! Usage:
//!   setup-bench --isolation chroot   --mode setup            --id <id> --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir>
//!   setup-bench --isolation landlock --mode multi-file-open  --id <id> --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir> -- --api-sock /run/api.sock
//!
//! All flags other than --isolation/--mode are exactly jailer's own flags
//! (this binary builds its parser by extending `jailer::build_arg_parser()`),
//! so --exec-file must point at a real, readable regular file (existing
//! validation requires it) even though it's never exec'd. `multi-file-open`
//! additionally requires `-- --api-sock /run/api.sock` (or any --api-sock
//! value) as an extra arg: that's what makes Landlock's setup create a
//! `/run` directory the same way chroot's FOLDER_HIERARCHY always does, so
//! both conditions have the same directory to hold the metrics FIFO and log
//! file the probe opens.
//!
//! This binary is intended to grow additional modes for the other benchmark
//! ideas (fleet-churn boot latency, etc.) rather than having each one
//! bolted onto jailer/landlock-jailer individually.

use std::ffi::CString;
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use jailer::JailerError;
use jailer::env::{BenchTimings, Env, Isolation, PROC_MOUNTS};
use utils::arg_parser::{ArgParser, Argument};
use utils::time::{ClockType, get_time_us};

// Mirrors lib.rs's private FOLDER_PERMISSIONS -- main_exec() creates the
// jail-root directory with this mode before calling Env::run(); this binary
// has to do the same thing itself since it doesn't go through main_exec().
const FOLDER_PERMISSIONS: u32 = 0o700;

fn main() {
    if let Err(err) = run() {
        eprintln!("{}", err);
        std::process::exit(1);
    }
}

fn build_parser() -> ArgParser<'static> {
    // Reuses jailer's own arg parser wholesale (same --id/--exec-file/--uid/
    // --gid/--chroot-base-dir/... validation as production), extended with
    // the two flags this binary alone needs.
    jailer::build_arg_parser()
        .arg(
            Argument::new("isolation")
                .required(true)
                .takes_value(true)
                .help("Which isolation backend to benchmark: \"chroot\" or \"landlock\"."),
        )
        .arg(
            Argument::new("mode")
                .required(false)
                .takes_value(true)
                .default_value("setup")
                .help(
                    "Which benchmark to run: \"setup\" (isolation setup/teardown phase \
                     timing only, the default) or \"multi-file-open\" (setup, then open \
                     a realistic set of files -- kernel image, rootfs, metrics FIFO, log, \
                     /dev/kvm, /dev/net/tun -- from inside the now-restricted process, \
                     timing each open individually).",
                ),
        )
}

fn run() -> Result<(), JailerError> {
    let mut arg_parser = build_parser();
    arg_parser
        .parse_from_cmdline()
        .map_err(JailerError::ArgumentParsing)?;
    let arguments = arg_parser.arguments();

    if arguments.flag_present("help") {
        println!("setup-bench v{}\n", jailer::JAILER_VERSION);
        println!("{}\n", arg_parser.formatted_help());
        return Ok(());
    }

    let isolation = match arguments.single_value("isolation").map(|s| s.to_string()).as_deref() {
        Some("chroot") => Isolation::Chroot,
        Some("landlock") => Isolation::Landlock,
        other => {
            eprintln!(
                "--isolation must be \"chroot\" or \"landlock\", got {:?}",
                other
            );
            std::process::exit(2);
        }
    };

    let mode = arguments
        .single_value("mode")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "setup".to_string());

    let env = Env::new(
        arguments,
        get_time_us(ClockType::Monotonic),
        get_time_us(ClockType::ProcessCpu),
        PROC_MOUNTS,
    )?;

    // Same jail-root creation step main_exec() does before calling
    // env.run() -- setup_isolation() assumes this directory already exists.
    fs::create_dir_all(env.chroot_dir())
        .map_err(|err| JailerError::CreateDir(env.chroot_dir().to_owned(), err))?;
    fs::set_permissions(env.chroot_dir(), Permissions::from_mode(FOLDER_PERMISSIONS))
        .map_err(|err| JailerError::Chmod(env.chroot_dir().to_owned(), err))?;

    match mode.as_str() {
        "setup" => run_setup_mode(env, isolation),
        "multi-file-open" => run_multi_file_open_mode(env, isolation),
        other => {
            eprintln!(
                "--mode must be \"setup\" or \"multi-file-open\", got {:?}",
                other
            );
            std::process::exit(2);
        }
    }
}

fn run_setup_mode(env: Env, isolation: Isolation) -> Result<(), JailerError> {
    let (timings, _base_dir) = env.run_setup_only(isolation)?;
    println!("{}", timings.to_json());
    Ok(())
}

/// Idea #5: realistic multi-file jail setup. Times a *sequence* of opens
/// against the kind of files a real Firecracker instance actually touches
/// at boot, from inside the process *after* isolation has been applied --
/// same rationale as `run_setup_only`, just extended one step further
/// instead of exiting right after setup.
///
/// Fixture files that must exist *before* `run_setup_only` (before
/// chroot's pivot_root / Landlock's restrict_self) are created here first,
/// directly under `env.chroot_dir()`: a bind-mount-over-itself (chroot)
/// preserves whatever's already there, and Landlock's jail_root
/// `PathBeneath` rule already covers anything under it, so no landlock.rs
/// changes are needed for those two. `/run`, however, is only created
/// *during* setup_isolation itself (chroot's FOLDER_HIERARCHY always
/// creates it; Landlock only creates it when `build_api_sock_arg` sees
/// `--api-sock` among the extra args) -- see this binary's top-level doc
/// comment for why `-- --api-sock ...` is required for this mode. The two
/// fixture files that belong under `/run` are therefore created only
/// *after* `run_setup_only` returns.
fn run_multi_file_open_mode(env: Env, isolation: Isolation) -> Result<(), JailerError> {
    write_fixture_file(&env.chroot_dir().join("kernel.img"), b"pretend-kernel-image")?;
    write_fixture_file(&env.chroot_dir().join("rootfs.ext4"), b"pretend-rootfs-image")?;

    let (timings, base_dir) = env.run_setup_only(isolation)?;

    let run_dir = base_dir.join("run");
    if !run_dir.is_dir() {
        eprintln!(
            "warning: {} doesn't exist -- did you pass `-- --api-sock /run/api.sock`? \
             (required for Landlock; chroot always creates /run regardless)",
            run_dir.display()
        );
    }
    let metrics_fifo = run_dir.join("metrics.fifo");
    let log_file = run_dir.join("firecracker.log");
    make_fifo(&metrics_fifo)?;
    write_fixture_file(&log_file, b"")?;

    // /dev/kvm and /dev/net/tun resolve identically under both conditions --
    // chroot mknod's real device nodes at these exact paths inside the jail;
    // Landlock grants the real host device paths directly -- so the same
    // two absolute strings are correct either way, unlike the other four
    // paths below, which are base_dir-relative and differ per condition.
    let probe_paths: Vec<PathBuf> = vec![
        base_dir.join("kernel.img"),
        base_dir.join("rootfs.ext4"),
        metrics_fifo,
        log_file,
        PathBuf::from("/dev/kvm"),
        PathBuf::from("/dev/net/tun"),
    ];

    let mut per_file_ns: Vec<(String, u128)> = Vec::with_capacity(probe_paths.len());
    for path in &probe_paths {
        let c_path = CString::new(path.to_str().unwrap()).map_err(JailerError::CStringParsing)?;
        let t0 = Instant::now();
        // SAFETY: c_path is a valid null-terminated string. O_NONBLOCK
        // matters for metrics_fifo specifically -- a plain blocking open()
        // on a FIFO with no writer connected would hang the benchmark
        // forever; it's harmless for the regular files and device nodes in
        // this same list, so it's applied uniformly rather than special-
        // cased per path.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        let elapsed = t0.elapsed().as_nanos();
        if fd < 0 {
            return Err(JailerError::FileOpen(
                path.clone(),
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: fd is the descriptor just returned by the open() above.
        unsafe {
            libc::close(fd);
        }
        per_file_ns.push((path.display().to_string(), elapsed));
    }

    println!("{}", multi_file_json(&timings, &per_file_ns));
    Ok(())
}

fn write_fixture_file(path: &Path, contents: &[u8]) -> Result<(), JailerError> {
    let mut f = File::create(path).map_err(|err| JailerError::FileOpen(path.to_path_buf(), err))?;
    f.write_all(contents)
        .map_err(|err| JailerError::Write(path.to_path_buf(), err))
}

fn make_fifo(path: &Path) -> Result<(), JailerError> {
    let c_path = CString::new(path.to_str().unwrap()).map_err(JailerError::CStringParsing)?;
    // SAFETY: c_path is a valid null-terminated string; 0o600 matches the
    // jail's own FOLDER_PERMISSIONS convention.
    let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if ret != 0 {
        return Err(JailerError::FileOpen(
            path.to_path_buf(),
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Hand-rolled JSON, consistent with BenchTimings::to_json's no-serde
/// approach:
/// {"condition":..,"total_setup_ns":N,"phases":{...},"file_opens_ns":{"path":N,...}}
fn multi_file_json(timings: &BenchTimings, per_file_ns: &[(String, u128)]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("{\"condition\":\"");
    out.push_str(timings.condition);
    out.push_str("\",\"total_setup_ns\":");
    out.push_str(&timings.total_setup_ns.to_string());
    out.push_str(",\"phases\":{");
    for (i, (name, ns)) in timings.phases.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(name);
        out.push_str("\":");
        out.push_str(&ns.to_string());
    }
    out.push_str("},\"file_opens_ns\":{");
    for (i, (path, ns)) in per_file_ns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(path);
        out.push_str("\":");
        out.push_str(&ns.to_string());
    }
    out.push_str("}}");
    out
}
