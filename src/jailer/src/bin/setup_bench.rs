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
//! `setup_isolation` method in env.rs, so this binary can't silently drift
//! from what `jailer`/`landlock-jailer` actually do, without needing to be
//! part of either of them.
//!
//! Two modes, selected by `--mode`:
//!
//!   setup            (default) Runs the chosen isolation backend's setup
//!                     phase exactly once, prints one JSON line of phase
//!                     timings, exits. Never execs --exec-file.
//!
//!   multi-file-open   Runs the same setup phase, then from inside the
//!                     now-restricted process, opens a realistic set of
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
//!   setup-bench --isolation landlock --mode multi-file-open  --id <id> --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir>
//!
//! All flags other than --isolation/--mode are exactly jailer's own flags
//! (this binary builds its parser by extending `jailer::build_arg_parser()`),
//! so --exec-file must point at a real, readable regular file (existing
//! validation requires it) even though it's never exec'd. multi-file-open
//! stages every fixture file it opens, including the /run directory, the
//! metrics FIFO, and the log file *before* isolation is applied, not
//! after: mkfifo() specifically needs Landlock's MakeFifo access right,
//! which jail_root's rule deliberately doesn't grant (real deployments
//! stage the metrics FIFO before the jailed process runs; it never creates
//! one itself), so creating it post-restriction would fail with EACCES.
//! No --api-sock argument is needed for this mode as a result.
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

fn run_multi_file_open_mode(env: Env, isolation: Isolation) -> Result<(), JailerError> {
    // All fixtures are created here, before run_setup_only() -- i.e. before
    // chroot's pivot_root or Landlock's restrict_self() take effect -- and
    // that's not just convenient, it's required. mkfifo() needs Landlock's
    // MakeFifo access right specifically; jail_root's rule in landlock.rs
    // only grants MakeReg (regular files), not MakeFifo, and deliberately
    // so -- real Firecracker deployments have an orchestrator stage the
    // metrics FIFO before the jailed process ever runs; the jailed process
    // itself never creates one. So rather than widen the production
    // ruleset to make a post-restriction mkfifo() work, every fixture is
    // staged up front instead, matching how it actually happens in
    // production. A chroot bind-mount-over-itself preserves nested
    // directories/files the same way it preserves top-level ones, and
    // Landlock's jail_root PathBeneath rule already covers *opening* (not
    // creating) anything under it, so no landlock.rs changes are needed --
    // only the ordering here.
    write_fixture_file(&env.chroot_dir().join("kernel.img"), b"pretend-kernel-image")?;
    write_fixture_file(&env.chroot_dir().join("rootfs.ext4"), b"pretend-rootfs-image")?;
    let run_dir = env.chroot_dir().join("run");
    fs::create_dir_all(&run_dir).map_err(|err| JailerError::CreateDir(run_dir.clone(), err))?;
    make_fifo(&run_dir.join("metrics.fifo"))?;
    write_fixture_file(&run_dir.join("firecracker.log"), b"")?;

    let (timings, base_dir) = env.run_setup_only(isolation)?;

    // Open directory fds ONCE, then openat() with just the filename below --
    // same fix as open-bench's dir_fd/openat design, applied here because
    // this probe had the identical confound: chroot's base_dir is "/" (1
    // component to "/kernel.img"), landlock's base_dir is the real host jail
    // path (5-6 components to walk on every absolute-path open() call). A
    // plain libc::open() on the full path re-walks every intermediate
    // component from "/" each time; openat() from an already-open directory
    // fd only resolves the final component, regardless of how deep that
    // directory's own path happens to be. Without this, landlock's numbers
    // here would conflate "cost of Landlock's mediation" with "cost of a much
    // longer absolute path" -- exactly the confound already fixed once before
    // in open-bench's baseline-vs-chroot comparison; it just didn't get
    // carried over into this probe when it was written.
    let base_dir_c = CString::new(base_dir.to_str().unwrap()).map_err(JailerError::CStringParsing)?;
    let base_dir_fd = unsafe {
        libc::open(
            base_dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if base_dir_fd < 0 {
        return Err(JailerError::FileOpen(base_dir.clone(), std::io::Error::last_os_error()));
    }

    let run_dir = base_dir.join("run");
    let run_dir_c = CString::new(run_dir.to_str().unwrap()).map_err(JailerError::CStringParsing)?;
    let run_dir_fd = unsafe {
        libc::open(
            run_dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if run_dir_fd < 0 {
        return Err(JailerError::FileOpen(run_dir.clone(), std::io::Error::last_os_error()));
    }

    // (dir_fd, filename, display_label), display_label is what shows up in
    // the JSON output, kept as a plain filename now that path depth is no
    // longer part of what's being measured for these four.
    let relative_probes: [(libc::c_int, &str); 4] = [
        (base_dir_fd, "kernel.img"),
        (base_dir_fd, "rootfs.ext4"),
        (run_dir_fd, "metrics.fifo"),
        (run_dir_fd, "firecracker.log"),
    ];

    let mut per_file_ns: Vec<(String, u128)> = Vec::with_capacity(6);

    for (dir_fd, name) in relative_probes {
        let c_name = CString::new(name).map_err(JailerError::CStringParsing)?;
        let t0 = Instant::now();
        // SAFETY: dir_fd is a valid, still-open O_DIRECTORY fd from above;
        // c_name is a valid null-terminated filename within it. O_NONBLOCK
        // matters for metrics.fifo specifically -- a plain blocking open()
        // on a FIFO with no writer connected would hang the benchmark
        // forever; harmless for the regular files, applied uniformly rather
        // than special-cased per path.
        let fd = unsafe {
            libc::openat(
                dir_fd,
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        let elapsed = t0.elapsed().as_nanos();
        if fd < 0 {
            let path_for_error = if dir_fd == base_dir_fd {
                base_dir.join(name)
            } else {
                run_dir.join(name)
            };
            unsafe {
                libc::close(base_dir_fd);
                libc::close(run_dir_fd);
            }
            return Err(JailerError::FileOpen(path_for_error, std::io::Error::last_os_error()));
        }
        unsafe {
            libc::close(fd);
        }
        per_file_ns.push((name.to_string(), elapsed));
    }

    unsafe {
        libc::close(base_dir_fd);
        libc::close(run_dir_fd);
    }

    // /dev/kvm and /dev/net/tun resolve identically under both conditions --
    // chroot mknod's real device nodes at these exact paths inside the jail;
    // Landlock grants the real host device paths directly -- so the same two
    // absolute strings are correct, and identically shallow (2 components),
    // either way. No dir_fd needed here since there's no depth asymmetry to
    // control for.
    for path_str in ["/dev/kvm", "/dev/net/tun"] {
        let c_path = CString::new(path_str).unwrap();
        let t0 = Instant::now();
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        let elapsed = t0.elapsed().as_nanos();
        if fd < 0 {
            return Err(JailerError::FileOpen(
                PathBuf::from(path_str),
                std::io::Error::last_os_error(),
            ));
        }
        unsafe {
            libc::close(fd);
        }
        per_file_ns.push((path_str.to_string(), elapsed));
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
