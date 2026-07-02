// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared jailer logic used by both the `jailer` (chroot) and `landlock-jailer`
//! (Landlock) binaries in `src/bin/`. Everything except the choice of filesystem
//! isolation backend lives here, so the two binaries stay a controlled A/B pair:
//! identical arg parsing, cgroup setup, resource limits, daemonization, and PID
//! namespace handling, differing only in the `Isolation` variant `Env::run()` is given.

use std::ffi::{CString, NulError, OsString};
use std::fmt::{Debug, Display};
use std::fs::{OpenOptions, Permissions};

use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::{env as p_env, fs, io};

use env::{Env, Isolation, PROC_MOUNTS};
use utils::arg_parser::{ArgParser, Argument, UtilsArgParserError as ParsingError};
use utils::time::{ClockType, get_time_us};
use utils::validators;
use vmm_sys_util::syscall::SyscallReturnCode;

pub mod cgroup;
pub mod chroot;
pub mod env;
// Named `landlock_jail` rather than `landlock` because the latter would collide with
// the `landlock` crate this module itself depends on (see landlock.rs's `use
// landlock::{...}` at the top -- that import needs the external crate name
// unshadowed). The file is still called `landlock.rs`; only the module identifier
// differs from the filename.
#[path = "landlock.rs"]
pub mod landlock_jail;
pub mod resource_limits;

pub const JAILER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum JailerError {
    #[error("Failed to parse arguments: {0}")]
    ArgumentParsing(ParsingError),
    #[error("{}", format!("Failed to canonicalize path {:?}: {}", .0, .1).replace('\"', ""))]
    Canonicalize(PathBuf, io::Error),
    #[error("{}", format!("Failed to inherit cgroups configurations from file {} in path {:?}", .1, .0).replace('\"', ""))]
    CgroupInheritFromParent(PathBuf, String),
    #[error("{1} configurations not found in {0}")]
    CgroupLineNotFound(String, String),
    #[error("Cgroup invalid file: {0}")]
    CgroupInvalidFile(String),
    #[error("Invalid format for cgroups: {0}")]
    CgroupFormat(String),
    #[error("Hierarchy not found: {0}")]
    CgroupHierarchyMissing(String),
    #[error("Controller {0} is unavailable")]
    CgroupControllerUnavailable(String),
    #[error("{0} is an invalid cgroup version specifier")]
    CgroupInvalidVersion(String),
    #[error("Parent cgroup path is invalid. Path should not be absolute or contain '..' or '.'")]
    CgroupInvalidParentPath(),
    #[error(
        "Failed to move process to cgroup ({0}): {1}.\nHint: If you intended to create a child \
         cgroup under {0}, pass any --cgroup parameters."
    )]
    CgroupMove(PathBuf, io::Error),
    #[error("Failed to change owner for {0}: {1}")]
    ChangeFileOwner(PathBuf, io::Error),
    #[error("Failed to chdir into chroot directory: {0}")]
    ChdirNewRoot(io::Error),
    #[error("Failed to change permissions on {0}: {1}")]
    Chmod(PathBuf, io::Error),
    #[error("Failed cloning into a new child process: {0}")]
    Clone(io::Error),
    #[error("Failed to close netns fd: {0}")]
    CloseNetNsFd(io::Error),
    #[error("Failed to close /dev/null fd: {0}")]
    CloseDevNullFd(io::Error),
    #[error("Failed to call close range syscall: {0}")]
    CloseRange(io::Error),
    #[error("{}", format!("Failed to copy {:?} to {:?}: {}", .0, .1, .2).replace('\"', ""))]
    Copy(PathBuf, PathBuf, io::Error),
    #[error("{}", format!("Failed to create directory {:?}: {}", .0, .1).replace('\"', ""))]
    CreateDir(PathBuf, io::Error),
    #[error("Encountered interior \\0 while parsing a string")]
    CStringParsing(NulError),
    #[error("Failed to daemonize: {0}")]
    Daemonize(io::Error),
    #[error("Failed to open directory {0}: {1}")]
    DirOpen(String, String),
    #[error("Failed to duplicate fd: {0}")]
    Dup2(io::Error),
    #[error("Failed to exec into Firecracker: {0}")]
    Exec(io::Error),
    #[error("{}", format!("Failed to extract filename from path {:?}", .0).replace('\"', ""))]
    ExtractFileName(PathBuf),
    #[error("{}", format!("Failed to open file {:?}: {}", .0, .1).replace('\"', ""))]
    FileOpen(PathBuf, io::Error),
    #[error("Failed to decode string from byte array: {0}")]
    FromBytesWithNul(std::ffi::FromBytesWithNulError),
    #[error("Failed to get flags from fd: {0}")]
    GetOldFdFlags(io::Error),
    #[error("Failed to get PID (getpid): {0}")]
    GetPid(io::Error),
    #[error("Failed to get SID (getsid): {0}")]
    GetSid(io::Error),
    #[error("Invalid gid: {0}")]
    Gid(String),
    #[error("Detected hard link at: {0}")]
    HardLink(PathBuf),
    #[error("Invalid instance ID: {0}")]
    InvalidInstanceId(validators::ValidatorError),
    #[error("Failed to add Landlock rule for {0}: {1}")]
    LandlockAddRule(PathBuf, String),
    #[error("Landlock ruleset was not fully enforced (status: {0:?}); refusing to run unsandboxed")]
    LandlockNotEnforced(landlock::RulesetStatus),
    #[error("Failed to restrict self with Landlock ruleset: {0}")]
    LandlockRestrictSelf(landlock::RulesetError),
    #[error("Failed to create Landlock ruleset: {0}")]
    LandlockRulesetCreate(landlock::RulesetError),
    #[error("Cannot get metadata for a file: {0}: {1}")]
    Metadata(PathBuf, io::Error),
    #[error("{}", format!("File {:?} doesn't have a parent", .0).replace('\"', ""))]
    MissingParent(PathBuf),
    #[error("Failed to create the jail root directory before pivoting root: {0}")]
    MkdirOldRoot(io::Error),
    #[error("Failed to create {1} via mknod inside the jail: {0}")]
    MknodDev(io::Error, String),
    #[error("Failed to bind mount the jail root directory: {0}")]
    MountBind(io::Error),
    #[error("Failed to change the propagation type to slave: {0}")]
    MountPropagationSlave(io::Error),
    #[error("{}", format!("{:?} is not a file", .0).replace('\"', ""))]
    NotAFile(PathBuf),
    #[error("{}", format!("{:?} is not a directory", .0).replace('\"', ""))]
    NotADirectory(PathBuf),
    #[error("Failed to confirm PR_SET_NO_NEW_PRIVS was set by Landlock")]
    NoNewPrivs,
    #[error("Failed to open {0}: {1}")]
    Open(PathBuf, io::Error),
    #[error("{}", format!("Failed to parse path {:?} into an OsString", .0).replace('\"', ""))]
    OsStringParsing(PathBuf, OsString),
    #[error("Failed to pivot root: {0}")]
    PivotRoot(io::Error),
    #[error("{}", format!("Failed to read line from {:?}: {}", .0, .1).replace('\"', ""))]
    ReadLine(PathBuf, io::Error),
    #[error("{}", format!("Failed to read file {:?} into a string: {}", .0, .1).replace('\"', ""))]
    ReadToString(PathBuf, io::Error),
    #[error("Regex failed: {0}")]
    RegEx(regex::Error),
    #[error("Invalid resource argument: {0}")]
    ResLimitArgument(String),
    #[error("Invalid format for resources limits: {0}")]
    ResLimitFormat(String),
    #[error("Invalid limit value for resource: {0}: {1}")]
    ResLimitValue(String, String),
    #[error("Failed to remove old jail root directory: {0}")]
    RmOldRootDir(io::Error),
    #[error("Failed to change current directory: {0}")]
    SetCurrentDir(io::Error),
    #[error("Failed to join network namespace: netns: {0}")]
    SetNetNs(io::Error),
    #[error("Failed to set limit for resource: {0}")]
    Setrlimit(String),
    #[error("Failed to daemonize: setsid: {0}")]
    SetSid(io::Error),
    #[error("Invalid uid: {0}")]
    Uid(String),
    #[error("Failed to unmount the old jail root: {0}")]
    UmountOldRoot(io::Error),
    #[error("Unexpected value for the socket listener fd: {0}")]
    UnexpectedListenerFd(i32),
    #[error("Failed to unshare into new mount namespace: {0}")]
    UnshareNewNs(io::Error),
    #[error("Failed to unset the O_CLOEXEC flag on the socket fd: {0}")]
    UnsetCloexec(io::Error),
    #[error("Slice contains invalid UTF-8 data : {0}")]
    UTF8Parsing(std::str::Utf8Error),
    #[error("{}", format!("Failed to write to {:?}: {}", .0, .1).replace('\"', ""))]
    Write(PathBuf, io::Error),
}

const FOLDER_PERMISSIONS: u32 = 0o700;


/// Create an ArgParser object which contains info about the command line argument parser and
/// populate it with the expected arguments and their characteristics.
pub fn build_arg_parser() -> ArgParser<'static> {
    ArgParser::new()
        .arg(
            Argument::new("id")
                .required(true)
                .takes_value(true)
                .help("Jail ID."),
        )
        .arg(
            Argument::new("exec-file")
                .required(true)
                .takes_value(true)
                .help("File path to exec into."),
        )
        .arg(
            Argument::new("uid")
                .required(true)
                .takes_value(true)
                .help("The user identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("gid")
                .required(true)
                .takes_value(true)
                .help("The group identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("chroot-base-dir")
                .takes_value(true)
                .default_value("/srv/jailer")
                .help("The base folder where chroot jails are located."),
        )
        .arg(
            Argument::new("netns")
                .takes_value(true)
                .help("Path to the network namespace this microVM should join."),
        )
        .arg(Argument::new("daemonize").takes_value(false).help(
            "Daemonize the jailer before exec, by invoking setsid(), and redirecting the standard \
             I/O file descriptors to /dev/null.",
        ))
        .arg(
            Argument::new("new-pid-ns")
                .takes_value(false)
                .help("Exec into a new PID namespace."),
        )
        .arg(Argument::new("cgroup").allow_multiple(true).help(
            "Cgroup and value to be set by the jailer. It must follow this format: \
             <cgroup_file>=<value> (e.g cpu.shares=10). This argument can be used multiple times \
             to add multiple cgroups.",
        ))
        .arg(Argument::new("resource-limit").allow_multiple(true).help(
            "Resource limit values to be set by the jailer. It must follow this format: \
             <resource>=<value> (e.g no-file=1024). This argument can be used multiple times to \
             add multiple resource limits. Current available resource values are:\n\t\tfsize: The \
             maximum size in bytes for files created by the process.\n\t\tno-file: Specifies a \
             value one greater than the maximum file descriptor number that can be opened by this \
             process.",
        ))
        .arg(
            Argument::new("cgroup-version")
                .takes_value(true)
                .default_value("1")
                .help("Select the cgroup version used by the jailer."),
        )
        .arg(
            Argument::new("parent-cgroup")
                .takes_value(true)
                .help("Parent cgroup in which the cgroup of this microvm will be placed."),
        )
        .arg(
            Argument::new("version")
                .takes_value(false)
                .help("Print the binary version number."),
        )
}

// It's called writeln_special because we have to use this rather convoluted way of writing
// to special cgroup files, to avoid getting errors. It would be nice to know why that happens :-s
pub fn writeln_special<T, V>(file_path: &T, value: V) -> Result<(), JailerError>
where
    T: AsRef<Path> + Debug,
    V: Display + Debug,
{
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file_path.as_ref())
        .map_err(|err| JailerError::Write(PathBuf::from(file_path.as_ref()), err))?;
    io::Write::write_all(&mut file, format!("{}\n", value).as_bytes())
        .map_err(|err| JailerError::Write(PathBuf::from(file_path.as_ref()), err))
}

pub fn readln_special<T: AsRef<Path> + Debug>(file_path: &T) -> Result<String, JailerError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file_path.as_ref())
        .map_err(|err| JailerError::ReadToString(PathBuf::from(file_path.as_ref()), err))?;
    let mut line = String::new();
    file.read_to_string(&mut line)
        .map_err(|err| JailerError::ReadToString(PathBuf::from(file_path.as_ref()), err))?;

    // Remove the newline character at the end (if any).
    line.pop();

    Ok(line)
}

fn close_fds_by_close_range() -> Result<(), JailerError> {
    // First try using the close_range syscall to close all open FDs in the range of 3..UINT_MAX
    // SAFETY: if the syscall is not available then ENOSYS will be returned
    SyscallReturnCode(unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_UNSHARE,
        )
    })
    .into_empty_result()
    .map_err(JailerError::CloseRange)
}

// Closes all FDs other than 0 (STDIN), 1 (STDOUT) and 2 (STDERR)
fn close_inherited_fds() -> Result<(), JailerError> {
    // We use the close_range syscall which is available on kernels > 5.9.
    close_fds_by_close_range()?;
    Ok(())
}

fn sanitize_process() -> Result<(), JailerError> {
    // First thing to do is make sure we don't keep any inherited FDs
    // other that IN, OUT and ERR.
    close_inherited_fds()?;

    // Cleanup environment variables.
    clean_env_vars();
    Ok(())
}

fn clean_env_vars() {
    // Remove environment variables received from
    // the parent process so there are no leaks
    // inside the jailer environment
    for (key, _) in p_env::vars() {
        // SAFETY: the function is safe to call in a single-threaded program
        unsafe {
            p_env::remove_var(key);
        }
    }
}

/// Turns an [`AsRef<Path>`] into a [`CString`] (c style string).
/// The expect should not fail, since Linux paths only contain valid Unicode chars (do they?),
/// and do not contain null bytes (do they?).
pub fn to_cstring<T: AsRef<Path> + Debug>(path: T) -> Result<CString, JailerError> {
    let path_str = path
        .as_ref()
        .to_path_buf()
        .into_os_string()
        .into_string()
        .map_err(|err| JailerError::OsStringParsing(path.as_ref().to_path_buf(), err))?;
    CString::new(path_str).map_err(JailerError::CStringParsing)
}

/// Single orchestration entrypoint shared by both `src/bin/jailer.rs` and
/// `src/bin/landlock_jailer.rs`. `label` is only used for `--help`/`--version` output,
/// so logs make it obvious which binary produced them; `isolation` picks the
/// filesystem-isolation backend `Env::run()` uses. Everything else -- arg parsing,
/// cgroup/resource-limit setup order, daemonization, PID namespace handling -- is
/// identical between the two binaries by construction.
pub fn run(isolation: Isolation, label: &str) -> Result<(), JailerError> {
    let result = main_exec(isolation, label);
    if let Err(e) = result {
        eprintln!("{}", e);
        Err(e)
    } else {
        Ok(())
    }
}

fn main_exec(isolation: Isolation, label: &str) -> Result<(), JailerError> {
    sanitize_process()
        .unwrap_or_else(|err| panic!("Failed to sanitize the Jailer process: {}", err));

    let mut arg_parser = build_arg_parser();
    arg_parser
        .parse_from_cmdline()
        .map_err(JailerError::ArgumentParsing)?;
    let arguments = arg_parser.arguments();

    if arguments.flag_present("help") {
        println!("{} v{}\n", label, JAILER_VERSION);
        println!("{}\n", arg_parser.formatted_help());
        println!("Any arguments after the -- separator will be supplied to the jailed binary.\n");
        return Ok(());
    }

    if arguments.flag_present("version") {
        println!("{} v{}\n", label, JAILER_VERSION);
        return Ok(());
    }

    Env::new(
        arguments,
        get_time_us(ClockType::Monotonic),
        get_time_us(ClockType::ProcessCpu),
        PROC_MOUNTS,
    )
    .and_then(|env| {
        fs::create_dir_all(env.chroot_dir())
            .map_err(|err| JailerError::CreateDir(env.chroot_dir().to_owned(), err))?;
        fs::set_permissions(env.chroot_dir(), Permissions::from_mode(FOLDER_PERMISSIONS))
            .map_err(|err| JailerError::Chmod(env.chroot_dir().to_owned(), err))?;
        env.run(isolation)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::env;
    // use std::ffi::CStr;
    use std::fs::File;
    use std::os::unix::io::IntoRawFd;

    use vmm_sys_util::rand;

    use super::*;

    fn run_close_fds_test(test_fn: fn() -> Result<(), JailerError>) {
        let n = 100;

        let tmp_dir_path = format!(
            "/tmp/jailer/tests/close_fds/_{}",
            rand::rand_alphanumerics(4).into_string().unwrap()
        );
        fs::create_dir_all(&tmp_dir_path).unwrap();

        let mut fds = Vec::new();
        for i in 0..n {
            let maybe_file = File::create(format!("{}/{}", tmp_dir_path, i));
            fds.push(maybe_file.unwrap().into_raw_fd());
        }

        test_fn().unwrap();

        for fd in fds {
            // SAFETY: Safe because it's a valid fd from a value known to exist.
            let is_open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
            assert!(!is_open);
        }

        fs::remove_dir_all(&tmp_dir_path).unwrap();
    }

    #[test]
    fn test_close_fds_by_close_range() {
        run_close_fds_test(close_fds_by_close_range);
    }

    #[test]
    fn test_sanitize_process() {
        // # Safety
        // This function is safe to call in a single-threaded program.
        unsafe{
            env::set_var("FOO", "bar");
        }
        assert!(env::var("FOO").is_ok());

        sanitize_process().unwrap();

        assert!(env::var("FOO").is_err());
    }

    #[test]
    fn test_to_cstring() {
        let path = Path::new("some_path");
        let cstring_path = to_cstring(path).unwrap();
        assert_eq!(cstring_path, CString::new("some_path").unwrap());
        let path_with_nul = Path::new("some_path_with_nul\0");
        assert_eq!(
            format!("{}", to_cstring(path_with_nul).unwrap_err()),
            "Encountered interior \\0 while parsing a string"
        );
    }
}
