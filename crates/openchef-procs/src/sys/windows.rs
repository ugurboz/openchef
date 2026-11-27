//! Windows: every child runs in a Job Object that kills the whole tree when its last
//! handle closes, so no agent process can outlive OpenChef (§5.1).
//!
//! The child is created suspended, assigned to the job, then resumed, so it never runs
//! outside the job. Windows has no termination signal; graceful stop is stdin EOF.

use std::io;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, GetProcessTimes, INFINITE, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

use super::Probe;
use crate::Error;

pub(crate) const SUPPORTS_TERM_SIGNAL: bool = false;

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Resumes every thread of a process. Not in `windows-sys`; present in ntdll on every
    /// supported Windows version.
    fn NtResumeProcess(process: HANDLE) -> i32;
}

/// The Job Object that forms one process's kill domain.
pub(crate) struct Platform {
    job: HANDLE,
}

// SAFETY: a Job Object handle is a kernel handle; the job APIs used here are thread-safe
// and the handle is only closed in `Drop`, when no other reference exists.
unsafe impl Send for Platform {}
// SAFETY: as above; shared references only call thread-safe job APIs.
unsafe impl Sync for Platform {}

// Every platform exposes the same methods; some need no `self` here.
#[allow(clippy::unused_self)]
impl Platform {
    pub(crate) fn new() -> Result<Self, Error> {
        // SAFETY: null attributes and name create an anonymous job; the call reads no
        // memory of ours.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(os_error("CreateJobObject", 0));
        }
        let platform = Self { job };
        // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a valid value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(u32::MAX);
        // SAFETY: `info` is a valid struct of `size` bytes that outlives the call.
        let ok = unsafe {
            SetInformationJobObject(
                platform.job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        };
        if ok == 0 {
            return Err(os_error("SetInformationJobObject", 0));
        }
        Ok(platform)
    }

    /// Creates the child suspended, in its own console process group.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        let _ = cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    }

    /// Assigns the suspended child to the job, then lets it run.
    pub(crate) fn attach(&self, child: &Child) -> Result<(), Error> {
        let process: HANDLE = child.as_raw_handle();
        // SAFETY: both handles are valid for the duration of the call; `child` owns its
        // process handle and outlives it.
        if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
            return Err(os_error("AssignProcessToJobObject", child.id()));
        }
        // SAFETY: `process` is valid (see above) and has PROCESS_SUSPEND_RESUME access:
        // std creates the child with full access.
        let status = unsafe { NtResumeProcess(process) };
        if status != 0 {
            return Err(Error::Signal {
                pid: child.id(),
                signal: "NtResumeProcess",
                source: io::Error::other(format!("NTSTATUS {status:#x}")),
            });
        }
        Ok(())
    }

    pub(crate) fn signal_term(&self, _pid: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Kills every process in the job.
    pub(crate) fn kill(&self, pid: u32) -> Result<(), Error> {
        // SAFETY: `self.job` stays valid until `Drop`.
        if unsafe { TerminateJobObject(self.job, 1) } == 0 {
            return Err(os_error("TerminateJobObject", pid));
        }
        Ok(())
    }

    /// Kills whatever is left in the job after the leader was reaped. The handle itself
    /// closes in `Drop` (which also kills, through `KILL_ON_JOB_CLOSE`).
    pub(crate) fn release(&self) {
        // SAFETY: as in `kill`. A job that is already empty makes this a no-op; there is
        // nothing to report from the supervisor at this point.
        let _killed = unsafe { TerminateJobObject(self.job, 1) };
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed exactly once, here.
        let _closed = unsafe { CloseHandle(self.job) };
    }
}

fn os_error(call: &'static str, pid: u32) -> Error {
    Error::Signal {
        pid,
        signal: call,
        source: io::Error::last_os_error(),
    }
}

/// A process handle closed on drop.
struct OwnedProcess(HANDLE);

impl OwnedProcess {
    /// Opens `pid` with `access`; `Ok(None)` when no such process exists.
    fn open(pid: u32, access: u32) -> io::Result<Option<Self>> {
        // SAFETY: OpenProcess only reads its arguments.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if !handle.is_null() {
            return Ok(Some(Self(handle)));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed exactly once, here.
        let _closed = unsafe { CloseHandle(self.0) };
    }
}

/// Blocks until `pid` has exited. On Windows the open process handle keeps the pid
/// reserved, so there is no reaping race.
pub(crate) fn wait_exit_no_reap(pid: u32) -> bool {
    let Ok(Some(process)) = OwnedProcess::open(pid, PROCESS_SYNCHRONIZE) else {
        return false;
    };
    // SAFETY: the handle is valid while `process` lives.
    unsafe { WaitForSingleObject(process.0, INFINITE) == WAIT_OBJECT_0 }
}

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// The process creation time (100 ns intervals since 1601) and whether it has exited.
fn times(pid: u32) -> io::Result<Option<(u64, bool)>> {
    let Some(process) = OwnedProcess::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? else {
        return Ok(None);
    };
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle is valid and all four out-pointers point to live FILETIMEs.
    let ok = unsafe {
        GetProcessTimes(
            process.0,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some((filetime(creation), filetime(exit) != 0)))
}

pub(crate) fn start_token(pid: u32) -> Result<u64, Error> {
    match times(pid) {
        Ok(Some((token, _))) => Ok(token),
        Ok(None) => Err(Error::StartToken {
            pid,
            source: io::Error::from(io::ErrorKind::NotFound),
        }),
        Err(source) => Err(Error::StartToken { pid, source }),
    }
}

pub(crate) fn probe(pid: u32) -> Result<Option<Probe>, Error> {
    match times(pid) {
        Ok(found) => Ok(found.map(|(token, exited)| Probe {
            token,
            running: !exited,
        })),
        Err(source) => Err(Error::Inspect { pid, source }),
    }
}

/// Kills a process left behind by an earlier daemon. Its job died with that daemon's
/// handle (`KILL_ON_JOB_CLOSE`), so this only finds a process that escaped it.
pub(crate) fn kill_orphan(pid: u32) -> Result<(), Error> {
    match OwnedProcess::open(pid, PROCESS_TERMINATE) {
        Ok(None) => Ok(()),
        Ok(Some(process)) => {
            // SAFETY: the handle is valid and has PROCESS_TERMINATE access.
            if unsafe { TerminateProcess(process.0, 1) } == 0 {
                Err(os_error("TerminateProcess", pid))
            } else {
                Ok(())
            }
        }
        Err(source) => Err(Error::Signal {
            pid,
            signal: "OpenProcess",
            source,
        }),
    }
}
