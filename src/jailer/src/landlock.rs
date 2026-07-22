// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Landlock-based filesystem isolation for the jailer, replacing `chroot.rs`.
//!
//! Unlike `chroot()`, this never calls `unshare`, `mount`, or `pivot_root`: Landlock
//! rules are attached to the calling thread, survive `execve()`, and are inherited by
//! every thread Firecracker subsequently spawns. That means Firecracker's source needs
//! zero modification, and it also means there is no new mount/root namespace remapping
//! "/" for the exec'd process -- every path Firecracker is given (config file, api
//! socket, snapshot files, ...) must be a real, absolute *host* path that falls beneath
//! one of the rules below. `Env::run()` is responsible for rewriting any argument that
//! used to rely on chroot's path remapping (see `build_api_sock_arg()`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use landlock::{
    Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, ABI,
};

use super::JailerError;

/// Landlock ABI we build the ruleset against. The project targets kernel 6.17, which
/// supports ABI V7 (introduced in Linux 6.15). Everything this jailer actually relies
/// on -- scoped path rules and `IoctlDev` mediation for /dev/kvm -- is available from
/// V5 (kernel 6.10) onward; V6/V7 mainly add scoping and audit-log controls we don't
/// use yet. On older kernels the crate's default best-effort compatibility mode
/// silently downgrades to whatever the running kernel supports, so this constant is
/// safe to bump without breaking older hosts -- it just changes what gets enforced.
const TARGET_ABI: ABI = ABI::V7;

/// A single filesystem rule: grant `access` on the subtree rooted at `path`.
///
/// This is the extension point. Every path the default jailer's strace turns up that
/// isn't already covered gets one more `RuleSpec` in `strace_discovered_rules()` below
/// -- nothing else in this file needs to change.
struct RuleSpec {
    path: PathBuf,
    access: BitFlags<AccessFs>,
    /// If the path doesn't exist on this host, is that a hard failure or a silent
    /// skip? Mirrors the existing tolerant handling of `/dev/urandom` and
    /// `/dev/userfaultfd` in `Env::mknod_and_own_dev` (both are opportunistic
    /// features, not hard requirements for booting a microVM).
    optional: bool,
}

impl RuleSpec {
    fn required(path: impl Into<PathBuf>, access: BitFlags<AccessFs>) -> Self {
        Self {
            path: path.into(),
            access,
            optional: false,
        }
    }

    fn optional(path: impl Into<PathBuf>, access: BitFlags<AccessFs>) -> Self {
        Self {
            path: path.into(),
            access,
            optional: true,
        }
    }
}

/// Paths discovered by strace-ing the default (chroot-based) jailer + Firecracker that
/// aren't already covered by `base_rules()`. This list starts empty -- append one
/// `RuleSpec` per path as the A/B benchmark run turns up an EACCES. This is the one
/// place in the whole module you should need to touch for that.
///
/// Example, once you have a real path to add:
/// ```ignore
/// RuleSpec::optional("/dev/userfaultfd", AccessFs::ReadFile | AccessFs::WriteFile),
/// ```
fn strace_discovered_rules() -> Vec<RuleSpec> {
    vec![]
}

/// Builds the full rule table for a given jail.
///
/// * `jail_root` -- everything Firecracker reads or writes for this VM: config file,
///   snapshot images, vsock UDS, metrics/log pipes, balloon stats socket, etc. Grants a
///   generous but still scoped set of rights over the whole subtree, since we don't
///   know ahead of time every file Firecracker will create in there.
/// * `exec_file` -- the Firecracker binary. It is *not* copied into the jail (that's
///   the whole point of not needing pivot_root), so it needs its own rule on its real
///   host path, separate from `jail_root`.
/// * `api_sock_dir` -- parent directory of the API unix socket, if the jailer is asked
///   to expose one. `MakeSock` belongs on the *parent* directory: `bind()` creates the
///   socket inode there, not at the socket path itself.
fn base_rules(jail_root: &Path, exec_file: &Path, api_sock_dir: Option<&Path>) -> Vec<RuleSpec> {
    let mut rules = vec![
        RuleSpec::required(
            jail_root,
            AccessFs::ReadFile
                | AccessFs::WriteFile
                | AccessFs::Truncate
                | AccessFs::ReadDir
                | AccessFs::MakeReg
                | AccessFs::MakeDir
                | AccessFs::RemoveFile,
        ),
        RuleSpec::required(exec_file, AccessFs::ReadFile | AccessFs::Execute),
        // IoctlDev matters here: declaring `handle_access(AccessFs::from_all(V7))`
        // below tells Landlock to mediate *every* access type it knows about,
        // IoctlDev included, everywhere -- not just on paths we list. From ABI V5
        // onward that means every ioctl() on /dev/kvm (KVM_CREATE_VM, KVM_RUN, ...)
        // is denied by default unless explicitly granted here. Forgetting this line
        // doesn't cause a permissions error at startup -- Firecracker boots and then
        // fails the instant it tries to create a vcpu.
        RuleSpec::required(
            "/dev/kvm",
            AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev,
        ),
        RuleSpec::required(
            "/dev/net/tun",
            AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev,
        ),
        // MMDS v2 token generation. The default jailer also treats a missing
        // /dev/urandom as non-fatal (see the warning in Env::run()); we do the same.
        RuleSpec::optional("/dev/urandom", AccessFs::ReadFile.into()),
        // Snapshot restore via userfaultfd, when the host kernel has it loaded. Worth
        // noting for the thesis: the chroot jailer has to parse /proc/misc at startup
        // to discover this device's dynamically-allocated minor number so it can
        // `mknod` a matching node inside the jail (see
        // `Env::get_userfaultfd_minor_dev_number`). Landlock rules are path-based, not
        // device-node-based, so that entire lookup is unnecessary here -- we just
        // grant access to the real host path directly.
        RuleSpec::optional(
            "/dev/userfaultfd",
            AccessFs::ReadFile | AccessFs::WriteFile,
        ),
    ];

    if let Some(dir) = api_sock_dir {
        rules.push(RuleSpec::required(dir, AccessFs::MakeSock | AccessFs::ReadFile | AccessFs::WriteFile));
    }

    rules.extend(strace_discovered_rules());
    rules
}

/// Applies the Landlock ruleset to the calling thread and enforces it.
///
/// Must be called *before* `uid`/`gid` are dropped (i.e. before `exec_command()`'s
/// `Command::uid()`/`gid()` trigger the actual privilege drop at `execve()` time):
/// Landlock rules are inherited across `execve()` and across the privilege drop, so
/// applying them here is sufficient and there's nothing left to do afterwards. No
/// seccomp filter changes are needed either way -- Firecracker installs its own
/// filters after exec, and Landlock enforcement is orthogonal to (and layers safely
/// underneath) whatever seccomp policy it applies to itself.
///
/// `jail_root` does not need to exist as a `chroot` target anymore -- it's just a
/// regular host directory now. Callers are still expected to `create_dir_all()` it
/// first (same as today), since `PathFd::new()` needs something to open.
/// Nanosecond breakdown of `apply_landlock`'s three internal phases. Always
/// computed -- three extra `Instant::now()` VDSO reads cost tens of ns total
/// against a function whose own cost is orders of magnitude larger -- so
/// there's no separate instrumented/production code path to keep in sync.
/// The `jailer`/`landlock-jailer` binaries ignore this return value; the
/// `setup-bench` binary is the only consumer.
#[derive(Debug)]
pub struct LandlockTimings {
    pub ruleset_create_ns: u128,
    pub add_rules_ns: u128,
    pub restrict_self_ns: u128,
}

pub fn apply_landlock(
    jail_root: &Path,
    exec_file: &Path,
    api_sock_dir: Option<&Path>,
) -> Result<LandlockTimings, JailerError> {
    // Handle every access right up to our target ABI. This is what makes the
    // IoctlDev caveat above apply, and it's also required for forward/backward
    // compatibility per the crate's own guidance: handling the full set now means a
    // future ABI bump won't silently change which actions are denied by default.
    let access_all = AccessFs::from_all(TARGET_ABI);

    let t_ruleset_start = Instant::now();
    let mut ruleset = Ruleset::default()
        .handle_access(access_all)
        .and_then(|r| r.create())
        .map_err(JailerError::LandlockRulesetCreate)?;
    let t_ruleset_created = Instant::now();

    for rule in base_rules(jail_root, exec_file, api_sock_dir) {
        let fd = match PathFd::new(&rule.path) {
            Ok(fd) => fd,
            Err(err) if rule.optional => {
                println!(
                    "Warning! Landlock: skipping optional path {}: {}. Related \
                     functionality may be unavailable.",
                    rule.path.display(),
                    err
                );
                continue;
            }
            Err(err) => {
                return Err(JailerError::LandlockAddRule(
                    rule.path.clone(),
                    err.to_string(),
                ));
            }
        };

        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, rule.access))
            .map_err(|err| JailerError::LandlockAddRule(rule.path.clone(), err.to_string()))?;
    }
    let t_rules_added = Instant::now();

    // restrict_self() performs both the ruleset enforcement (landlock_restrict_self())
    // and, by default, the prctl(PR_SET_NO_NEW_PRIVS) call -- in that fixed order,
    // and both strictly before we return control to `Env::run()`, which will go on
    // to exec Firecracker under the dropped uid/gid.
    let status = ruleset
        .restrict_self()
        .map_err(JailerError::LandlockRestrictSelf)?;
    let t_restricted = Instant::now();

    if !status.no_new_privs {
        return Err(JailerError::NoNewPrivs);
    }

    // Fail closed rather than silently degrade: if we're not fully enforced, some
    // access right we asked for isn't backed by the running kernel, and we'd rather
    // the jailer refuse to start than boot a microVM with a security model weaker
    // than what the caller asked for. On the CloudLab kernel 6.17 target this should
    // always be `FullyEnforced`; a `PartiallyEnforced`/`NotEnforced` result here
    // means the binary is running on an older kernel than intended.
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(JailerError::LandlockNotEnforced(status.ruleset));
    }

    Ok(LandlockTimings {
        ruleset_create_ns: (t_ruleset_created - t_ruleset_start).as_nanos(),
        add_rules_ns: (t_rules_added - t_ruleset_created).as_nanos(),
        restrict_self_ns: (t_restricted - t_rules_added).as_nanos(),
    })
}
