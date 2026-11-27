//! Spike S2 harness (task card R1.2): confine *this* process with Landlock and seccomp,
//! then exec the target. This is the shape the product would use (`openchef __sandbox`):
//! applying the policy in the helper before `exec` avoids running non-async-signal-safe
//! code in `pre_exec`.
//!
//! Usage:
//!   landlock-spike abi
//!   landlock-spike run [--rw DIR]... [--ro DIR]... [--no-seccomp] -- CMD [ARGS]...

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("landlock-spike only runs on Linux");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, ExitCode};

    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

    pub(super) fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("abi") => {
                println!("landlock_abi={}", kernel_abi());
                ExitCode::SUCCESS
            }
            Some("run") => match run(&args[1..]) {
                Ok(never) => match never {},
                Err(e) => {
                    eprintln!("landlock-spike: {e}");
                    ExitCode::from(125)
                }
            },
            _ => {
                eprintln!("usage: landlock-spike abi | run [--rw DIR]... [--ro DIR]... [--no-seccomp] -- CMD...");
                ExitCode::from(2)
            }
        }
    }

    /// The Landlock ABI version the kernel supports; negative errno if none.
    fn kernel_abi() -> i64 {
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // SAFETY: with a null attr and size 0, the VERSION flag only queries the ABI.
        let rc = unsafe {
            libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0_usize, LANDLOCK_CREATE_RULESET_VERSION)
        };
        if rc < 0 { -i64::from(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) } else { rc }
    }

    enum Never {}

    fn run(args: &[String]) -> Result<Never, String> {
        let mut rw = Vec::new();
        let mut ro = Vec::new();
        let mut seccomp = true;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--rw" => {
                    rw.push(args.get(i + 1).ok_or("--rw needs a dir")?.clone());
                    i += 2;
                }
                "--ro" => {
                    ro.push(args.get(i + 1).ok_or("--ro needs a dir")?.clone());
                    i += 2;
                }
                "--no-seccomp" => {
                    seccomp = false;
                    i += 1;
                }
                "--" => {
                    i += 1;
                    break;
                }
                other => return Err(format!("unknown flag {other}")),
            }
        }
        let cmd = args.get(i..).filter(|c| !c.is_empty()).ok_or("missing command")?;

        let status = landlock(&ro, &rw)?;
        eprintln!("landlock-spike: kernel_abi={} ruleset={status:?}", kernel_abi());
        if seccomp {
            seccomp_filter()?;
            eprintln!("landlock-spike: seccomp=on");
        }
        let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
        Err(format!("exec {}: {err}", cmd[0]))
    }

    fn landlock(ro: &[String], rw: &[String]) -> Result<RulesetStatus, String> {
        // Ask for the newest ABI this crate knows; best effort downgrades on older
        // kernels and the status reports what was really enforced.
        let abi = ABI::V5;
        let status = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| e.to_string())?
            .create()
            .map_err(|e| e.to_string())?
            .add_rules(path_beneath_rules(ro, AccessFs::from_read(abi)))
            .map_err(|e| e.to_string())?
            .add_rules(path_beneath_rules(rw, AccessFs::from_all(abi)))
            .map_err(|e| e.to_string())?
            .restrict_self()
            .map_err(|e| e.to_string())?;
        Ok(status.ruleset)
    }

    /// Denies syscalls an agent never needs and that could weaken the sandbox.
    fn seccomp_filter() -> Result<(), String> {
        let denied = [
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_bpf,
            libc::SYS_kexec_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_perf_event_open,
            libc::SYS_swapon,
            libc::SYS_reboot,
        ];
        let rules: BTreeMap<i64, Vec<SeccompRule>> = denied.into_iter().map(|s| (s, Vec::new())).collect();
        let arch = if cfg!(target_arch = "aarch64") { TargetArch::aarch64 } else { TargetArch::x86_64 };
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(u32::try_from(libc::EPERM).unwrap_or(1)),
            arch,
        )
        .map_err(|e| e.to_string())?;
        let program: BpfProgram = filter.try_into().map_err(|e: seccompiler::BackendError| e.to_string())?;
        seccompiler::apply_filter(&program).map_err(|e| e.to_string())
    }
}
