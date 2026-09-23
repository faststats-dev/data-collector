//! Linux policy inherited by every Chromium/FFmpeg descendant. Fail closed.
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::{Context, ensure};

#[cfg(target_os = "linux")]
pub fn protect_service() -> Result<()> {
    // A same-UID renderer must not read the API server's token via /proc or ptrace.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "disable process dumps"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "enable descendant reaping"
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn protect_service() -> Result<()> {
    anyhow::bail!("isolated rendering requires Linux")
}

#[cfg(target_os = "linux")]
pub fn restrict() -> Result<()> {
    use libc::{sock_filter, sock_fprog};
    fn stmt(code: u16, k: u32) -> sock_filter {
        sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }
    fn eq(k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: 0x15,
            jt,
            jf,
            k,
        }
    }
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    const ALLOW: u32 = 0x7fff0000;
    const DENY: u32 = 0x00050000 | libc::EPERM as u32;
    const KILL: u32 = 0x80000000;
    // seccomp_data: nr at 0, arch at 4, args[0] at 16.
    let mut filter = vec![
        stmt(0x20, 4),
        eq(ARCH, 1, 0),
        stmt(0x06, KILL),
        stmt(0x20, 0),
    ];
    for call in [libc::SYS_socket, libc::SYS_socketpair] {
        filter.extend([
            eq(call as u32, 0, 4),
            stmt(0x20, 16),
            eq(libc::AF_UNIX as u32, 0, 1),
            stmt(0x06, ALLOW),
            stmt(0x06, DENY),
        ]);
    }
    // A descendant cannot escape the process group used by cancellation/cleanup,
    // acquire another namespace, inspect server memory, or bypass this filter via io_uring.
    for call in [
        libc::SYS_setsid,
        libc::SYS_setpgid,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_bpf,
        libc::SYS_mount,
    ] {
        filter.extend([eq(call as u32, 0, 1), stmt(0x06, DENY)]);
    }
    // clone3 points at user memory; force the conventional clone fallback.
    filter.extend([
        eq(libc::SYS_clone3 as u32, 0, 1),
        stmt(0x06, 0x00050000 | libc::ENOSYS as u32),
    ]);
    filter.extend([
        eq(libc::SYS_clone as u32, 0, 3),
        stmt(0x20, 16),
        sock_filter {
            code: 0x45,
            jt: 0,
            jf: 1,
            k: (libc::CLONE_NEWUSER
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWUTS
                | libc::CLONE_NEWCGROUP) as u32,
        },
        stmt(0x06, DENY),
    ]);
    // Reject x32 syscall aliases as well as alternate architecture entrypoints.
    #[cfg(target_arch = "x86_64")]
    filter.extend([
        stmt(0x20, 0),
        sock_filter {
            code: 0x45,
            jt: 0,
            jf: 1,
            k: 0x40000000,
        },
        stmt(0x06, KILL),
    ]);
    filter.push(stmt(0x06, ALLOW));
    let program = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
        "no_new_privs unavailable"
    );
    let result = unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("renderer seccomp unavailable; refusing unsandboxed execution");
    }
    limit(libc::RLIMIT_CORE, 0)?;
    limit(libc::RLIMIT_FSIZE, 128 * 1024 * 1024)?;
    limit(libc::RLIMIT_NOFILE, 512)?;
    limit(libc::RLIMIT_NPROC, 256)?;
    limit(libc::RLIMIT_CPU, replay_render_protocol::JOB_SECONDS)?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn limit(resource: libc::__rlimit_resource_t, maximum: u64) -> Result<()> {
    let value = libc::rlimit {
        rlim_cur: maximum,
        rlim_max: maximum,
    };
    ensure!(
        unsafe { libc::setrlimit(resource, &value) } == 0,
        "cannot set renderer resource limit"
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn restrict() -> Result<()> {
    anyhow::bail!("isolated rendering requires Linux")
}
