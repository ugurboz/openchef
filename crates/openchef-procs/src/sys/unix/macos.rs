//! macOS: kqueue `NOTE_EXIT`, start time and state from `sysctl kern.proc.pid`.

use std::io;
use std::process::Command;

use crate::Error;
use crate::sys::Probe;

/// macOS has no parent-death signal. Children see EOF on stdin when the daemon dies, and
/// the daemon sweeps recorded identities at startup.
pub(super) fn set_parent_death_signal(_cmd: &mut Command) {}

/// Blocks until `pid` has exited, using kqueue `NOTE_EXIT`, and reports whether the exit
/// was observed. The process stays a zombie (pid reserved) until it is reaped. Returns
/// false only if kqueue itself fails; the caller then skips the pre-reap cleanup instead
/// of signalling a process that may still be running.
pub(crate) fn wait_exit_no_reap(pid: u32) -> bool {
    let Ok(ident) = usize::try_from(pid) else {
        return false;
    };
    // SAFETY: kqueue(2) creates a new descriptor and touches no memory.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return false;
    }
    // SAFETY: an all-zero kevent is a valid value; the fields used are set below.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = ident;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: as above.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let mut changes: libc::c_int = 1;
    let observed = loop {
        // SAFETY: `change` and `event` are valid for one element each; a null timeout
        // blocks until an event arrives.
        let n = unsafe {
            libc::kevent(
                kq,
                &raw const change,
                changes,
                &raw mut event,
                1,
                std::ptr::null(),
            )
        };
        if n > 0 {
            if event.flags & libc::EV_ERROR != 0 {
                // Registration failed. ESRCH: only this crate reaps the child, so "no
                // such process" means it already exited and is an unreaped zombie.
                break event.data == libc::ESRCH as isize;
            }
            break true;
        }
        match io::Error::last_os_error().raw_os_error() {
            // The registration was applied; keep waiting without re-adding it.
            Some(libc::EINTR) => changes = 0,
            Some(libc::ESRCH) => break true,
            _ => break false,
        }
    };
    // SAFETY: `kq` is a descriptor this function owns.
    // A failed close of a private kqueue descriptor leaves nothing to clean up.
    let _closed = unsafe { libc::close(kq) };
    observed
}

/// The process start time in microseconds since the Unix epoch, as recorded by the
/// kernel (`kinfo_proc.kp_proc.p_starttime`).
pub(crate) fn start_token(pid: u32) -> Result<u64, Error> {
    match kinfo(pid) {
        Ok(Some(info)) => Ok(info.token),
        Ok(None) => Err(Error::StartToken {
            pid,
            source: io::Error::from_raw_os_error(libc::ESRCH),
        }),
        Err(source) => Err(Error::StartToken { pid, source }),
    }
}

pub(crate) fn probe(pid: u32) -> Result<Option<Probe>, Error> {
    kinfo(pid).map_err(|source| Error::Inspect { pid, source })
}

// Layout of `struct kinfo_proc` (sys/sysctl.h), stable since 10.0 on both arm64 and
// x86_64: `kp_proc` (struct extern_proc) comes first.
const KINFO_PROC_SIZE: usize = 648;
const P_STARTTIME_SEC: usize = 0; // struct timeval: i64 seconds
const P_STARTTIME_USEC: usize = 8; // then i32 microseconds
const P_FLAG: usize = 32; // i32
const P_STAT: usize = 36; // i8
const P_PID: usize = 40; // i32
const SZOMB: u8 = 5; // p_stat: exited, waiting to be reaped
const P_WEXIT: i32 = 0x0000_2000; // p_flag: working on exiting

/// Reads the kernel's record of `pid`. `running` is false for zombies and for processes
/// already inside `exit()`: XNU skips both when signalling a process group.
fn kinfo(pid: u32) -> io::Result<Option<Probe>> {
    let Ok(raw_pid) = libc::c_int::try_from(pid) else {
        return Ok(None);
    };
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        raw_pid,
    ];
    let mut buf = [0_u8; KINFO_PROC_SIZE];
    let mut len = buf.len();
    // SAFETY: `mib` holds 4 valid entries, `buf` is writable for `len` bytes, and `len`
    // is updated by the kernel to the number of bytes written. No new value is set.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // For an unknown pid the kernel writes zero bytes.
    if len == 0 {
        return Ok(None);
    }
    if len < P_PID + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("short kinfo_proc ({len} bytes)"),
        ));
    }
    let read_i32 = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    if read_i32(P_PID) != raw_pid {
        return Ok(None);
    }
    let mut sec = [0_u8; 8];
    sec.copy_from_slice(&buf[P_STARTTIME_SEC..P_STARTTIME_SEC + 8]);
    let (Ok(sec), Ok(usec)) = (
        u64::try_from(i64::from_ne_bytes(sec)),
        u64::try_from(read_i32(P_STARTTIME_USEC)),
    ) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "negative start time",
        ));
    };
    let running = buf[P_STAT] != SZOMB && read_i32(P_FLAG) & P_WEXIT == 0;
    Ok(Some(Probe {
        token: sec * 1_000_000 + usec,
        running,
    }))
}
