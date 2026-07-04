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
//! Runs the chosen isolation backend's setup phase exactly once, prints one
//! JSON line of phase timings to stdout, and exits -- it never execs
//! `--exec-file`. A fresh process per sample is intentional: see
//! `Env::run_setup_only`'s doc comment for why.
//!
//! Usage:
//!   setup-bench --isolation chroot   --id <id> --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir>
//!   setup-bench --isolation landlock --id <id> --exec-file <path> --uid <uid> --gid <gid> --chroot-base-dir <dir>
//!
//! All flags other than --isolation are exactly jailer's own flags (this
//! binary builds its parser by extending `jailer::build_arg_parser()`), so
//! --exec-file must point at a real, readable regular file (existing
//! validation requires it) even though it's never exec'd.
//!
//! This binary is intended to grow additional subcommands for the other
//! benchmark ideas (fleet-churn boot latency, multi-file jail setup timing,
//! etc.) rather than having each one bolted onto jailer/landlock-jailer
//! individually.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;

use jailer::JailerError;
use jailer::env::{Env, Isolation, PROC_MOUNTS};
use utils::arg_parser::Argument;
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

fn run() -> Result<(), JailerError> {
    // Reuses jailer's own arg parser wholesale (same --id/--exec-file/--uid/
    // --gid/--chroot-base-dir/... validation as production), extended with
    // one flag this binary alone needs.
    let mut arg_parser = jailer::build_arg_parser().arg(
        Argument::new("isolation")
            .required(true)
            .takes_value(true)
            .help("Which isolation backend to benchmark: \"chroot\" or \"landlock\"."),
    );
    arg_parser
        .parse_from_cmdline()
        .map_err(JailerError::ArgumentParsing)?;
    let arguments = arg_parser.arguments();

    if arguments.flag_present("help") {
        println!("setup-bench v{}\n", jailer::JAILER_VERSION);
        println!("{}\n", arg_parser.formatted_help());
        return Ok(());
    }

    let isolation_arg = arguments.single_value("isolation").map(|s| s.to_string());
    let isolation = match isolation_arg.as_deref() {
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

    let timings = env.run_setup_only(isolation)?;
    println!("{}", timings.to_json());
    Ok(())
}
