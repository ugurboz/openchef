//! Linux: parent-death signal, `waitid(WNOWAIT)`, start time from `/proc`.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use crate::Error;
use crate::sys::Probe;

/// Asks the kernel to SIGTERM the child when the spawning thread dies. The spawning
/// thread is the process's supervisor thread, which lives as long as the child.
pub(super) fn set_parent_death_signal(cmd: &mut Command) {
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and only calls
    // async-signal-safe functions (prctl, getppid); it allocates nothing.
    unsafe {
        let _ = cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died before prctl took effect.
            if u32::try_from(libc::getppid()).ok() != Some(parent) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
}

/// Blocks until `pid` has exited, leaving it a zombie so its pid (and process group id)
/// stay reserved until it is reaped. Reports whether the exit was observed.
pub(crate) fn wait_exit_no_reap(pid: u32) -> bool {
    let id = libc::id_t::from(pid);
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value for waitid to overwrite.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable siginfo_t for the duration of the call.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return true;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

/// The process start time in clock ticks since boot (field 22 of `/proc/<pid>/stat`).
/// Unlike wall-clock times it is immune to clock adjustments.
pub(crate) fn start_token(pid: u32) -> Result<u64, Error> {
    read_stat(pid)
        .map(|(token, _)| token)
        .map_err(|source| Error::StartToken { pid, source })
}

pub(crate) fn probe(pid: u32) -> Result<Option<Probe>, Error> {
    match read_stat(pid) {
        Ok((token, state)) => Ok(Some(Probe {
            token,
            running: state != b'Z' && state != b'X',
        })),
        // ENOENT: no such pid. ESRCH: it vanished between open and read.
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
            Ok(None)
        }
        Err(source) => Err(Error::Inspect { pid, source }),
    }
}

fn read_stat(pid: u32) -> io::Result<(u64, u8)> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_stat(&raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed stat line {raw:?}"),
        )
    })
}

/// Extracts starttime and state from a `/proc/<pid>/stat` line. The command name (field
/// 2) may contain spaces and parentheses, so fields are counted after the last `)`.
pub(crate) fn parse_stat(line: &str) -> Option<(u64, u8)> {
    let end = line.rfind(')')?;
    let fields: Vec<&str> = line.get(end + 1..)?.split_whitespace().collect();
    // fields[0] is field 3 (state); starttime is field 22, i.e. fields[19].
    let state = fields.first().filter(|s| s.len() == 1)?.as_bytes()[0];
    let token = fields.get(19)?.parse().ok()?;
    Some((token, state))
}

#[cfg(test)]
mod tests {
    use super::parse_stat;

    // Fields after the command: state ppid pgrp session tty tpgid flags minflt cminflt
    // majflt cmajflt utime stime cutime cstime priority nice threads itrealvalue starttime
    const TAIL: &str = " S 1 1 1 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";

    #[test]
    fn parse_stat_cases() {
        let zombie = format!("42 (z) Z{}", &TAIL[2..]);
        let ok = [
            ("simple command", format!("42 (sleep){TAIL}"), b'S'),
            (
                "command with spaces and parens",
                format!("42 (my (weird) cmd){TAIL}"),
                b'S',
            ),
            ("zombie", zombie, b'Z'),
        ];
        for (name, line, state) in ok {
            assert_eq!(parse_stat(&line), Some((987_654, state)), "{name}");
        }
        let bad = [
            ("missing paren", format!("42 sleep{TAIL}")),
            ("truncated", "42 (sleep) S 1 1".to_owned()),
            (
                "non-numeric starttime",
                "42 (x) S 1 1 1 0 -1 4 1 0 0 0 1 2 0 0 20 0 1 0 abc".to_owned(),
            ),
        ];
        for (name, line) in bad {
            assert_eq!(parse_stat(&line), None, "{name}");
        }
    }
}
