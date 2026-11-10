//! The handover hook: the one step of a cutover that okm cannot know (ADR-0056 §6).
//!
//! Reaching the handed position makes this instance the writer, and that is all the log
//! can say. It cannot say where traffic comes from: a gateway in front of two ports on
//! one machine, a load balancer in front of two containers, a DNS weight — every one of
//! those is the deployment's fact, and okm holds no control plane to publish it with.
//!
//! So the last step is a PROGRAM the host names, run once per take-over, by the
//! SUCCESSOR — the instance that just became the writer and is about to serve. Not the
//! drainer: a holder on its way out has nothing to announce, and it may already be gone
//! by the time the switch is possible.
//!
//! Ordering, and why it is this order:
//!
//! 1. every table reaches the handed position,
//! 2. every table is claimed by this instance,
//! 3. the hook runs.
//!
//! Claim first, then announce. The reverse would point traffic at an instance that could
//! still fail to claim, and the failure would be invisible — the gateway would be sending
//! requests to a process that cannot write. With this order a hook failure leaves the
//! store owned by the instance that could not announce: the single-writer invariant holds,
//! nothing is rolled back (a claim is never undone — see [`super::ChangelogStore::take_over`]),
//! and the operator sees a half-done cutover rather than a silent one.
//!
//! What the program is given is in the environment, because that is the one channel a
//! shell script in a container shares with the process that spawned it:
//!
//! | name | value |
//! |---|---|
//! | `OKM_CHANGELOG_ROOT` | the log root, as configured |
//! | `OKM_ROLE` | `successor` (it caught up from another instance) or `resume` (the claim was already ours) |
//! | `OKM_HOLDER` | this instance's holder name |
//! | `OKM_DATA` | this instance's store location, as configured |
//! | `OKM_TABLES` | how many tables this call took over |
//!
//! Nothing about the deployment is added: the port this instance listens on is already in
//! its own environment, and the owner files are readable to anything that can see the log
//! (that is the shared-mount case — a hook can read them itself when it needs a position).
//!
//! The program's own stdout and stderr are inherited: a hook that fails is something a
//! human has to read, and capturing its output into an error string would hide it from the
//! deployment's log.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How often a running hook is checked for exit. A cutover already waits on files; a
/// 25 ms poll costs nothing next to the waits around it.
const POLL: Duration = Duration::from_millis(25);

/// A program to run once the successor is the writer (ADR-0056 §6).
///
/// ```
/// use okm_core::changelog::HandoverHook;
///
/// let hook = HandoverHook::new("/usr/local/bin/switch-traffic").arg("--to-self");
/// # let _ = hook;
/// ```
#[derive(Debug, Clone)]
pub struct HandoverHook {
    program: PathBuf,
    args: Vec<OsString>,
}

impl HandoverHook {
    /// The program. It is started directly (no shell), so anything that needs shell syntax
    /// — pipes, `&&`, `$VAR` — should be a script with a `#!/bin/sh` line rather than an
    /// argument string: the deployment's logic does not belong in okm's argv.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// One argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Several arguments.
    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The program path, for messages.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Run it, with `env` added to the deployment's own environment, and wait up to
    /// `timeout` for it to exit successfully.
    ///
    /// A non-zero exit, a timeout (the child is killed) and a program that cannot be
    /// started are all the same answer to the caller: the cutover was not announced. The
    /// distinction is in the message, not in a different code path — nothing here can be
    /// retried into a different outcome by okm.
    pub(crate) fn run(
        &self,
        env: &[(&str, OsString)],
        timeout: Duration,
    ) -> Result<(), HookOutcome> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (name, value) in env {
            cmd.env(name, value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| HookOutcome::NotStarted(e.to_string()))?;

        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(HookOutcome::Exited(status.to_string())),
                Ok(None) => {}
                Err(e) => return Err(HookOutcome::NotStarted(e.to_string())),
            }
            if Instant::now() >= deadline {
                // The cutover cannot be announced either way; leaving the program running
                // would mean a script that acts after the caller has given up on it.
                let _ = child.kill();
                let _ = child.wait();
                return Err(HookOutcome::TimedOut(timeout));
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Why a hook did not announce the cutover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookOutcome {
    /// Spawn failed, or the wait itself failed.
    NotStarted(String),
    /// The program ran and said no.
    Exited(String),
    /// It was still running when the budget ran out, and was killed.
    TimedOut(Duration),
}

impl std::fmt::Display for HookOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted(why) => write!(f, "could not be started: {why}"),
            Self::Exited(status) => write!(f, "exited with {status}"),
            Self::TimedOut(budget) => {
                write!(f, "did not finish within {budget:?} and was killed")
            }
        }
    }
}
