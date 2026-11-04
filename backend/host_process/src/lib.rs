//! Reads raw facts about processes on this host: a start token that tells a process apart from a
//! later one reusing its PID, the parent PID, the direct children, the memory a process uses, and
//! the boot id that changes on every host boot. It also walks a process's ancestry, for example from
//! a GPU-using child up to the workload process that spawned it.
//!
//! It watches any same-user process for exit through a pollable descriptor (kqueue on macOS, pidfd
//! on Linux). The facts are owner-neutral and unqualified: callers that compare identities across
//! machines add their own node qualification. Every backend component reads processes through this
//! crate, so they all record the same identity for a process.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

mod ancestry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
};

use anyhow::{Context, Result};

pub use ancestry::{ProcessLifetime, process_ancestors, read_process_start_token};

/// Returns the start token of a running process, or `None` when no such process is running.
///
/// The token is opaque, see [`ProcessEntry::start_token`]. A zombie (exited, not yet reaped)
/// counts as not running.
pub fn start_token(process_id: u32) -> Result<Option<u64>> {
    #[cfg(target_os = "linux")]
    return linux_start_token(process_id);

    #[cfg(target_os = "macos")]
    return Ok(MacosProcessTable
        .entry(process_id)?
        .map(|entry| entry.start_token));
}

/// Returns the parent PID of a running process, or `None` when no such process is running.
pub fn parent_process_id(process_id: u32) -> Result<Option<u32>> {
    #[cfg(target_os = "linux")]
    return Ok(LinuxProcessTable
        .entry(process_id)?
        .map(|entry| entry.parent_process_id));

    #[cfg(target_os = "macos")]
    return Ok(MacosProcessTable
        .entry(process_id)?
        .map(|entry| entry.parent_process_id));
}

/// Returns the memory a running process uses in bytes, or `None` when no such process is running.
///
/// On macOS this is the physical footprint the system charges to the process; on Linux it is the
/// resident set size. A process of another user cannot be read and fails.
pub fn memory_bytes(process_id: u32) -> Result<Option<u64>> {
    #[cfg(target_os = "linux")]
    return linux_memory_bytes(process_id);

    #[cfg(target_os = "macos")]
    return macos::phys_footprint(process_id);
}

/// Returns the PIDs of a process's direct children; empty when it has none or is not running.
pub fn child_process_ids(process_id: u32) -> Result<Vec<u32>> {
    #[cfg(target_os = "linux")]
    return linux_child_process_ids(process_id);

    #[cfg(target_os = "macos")]
    return macos::child_process_ids(process_id);
}

/// Returns the identifier of the current host boot, which changes on every boot.
pub fn boot_id() -> Result<String> {
    #[cfg(target_os = "linux")]
    return Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot id")?
        .trim()
        .to_owned());

    #[cfg(target_os = "macos")]
    return macos::boot_session_uuid();
}

/// Pollable descriptor that becomes readable once a watched process exits.
///
/// Register the watch before verifying the process's start token: a process that exits or whose
/// PID is reused after registration is then never missed.
#[derive(Debug)]
pub struct ExitWatch(OwnedFd);

impl ExitWatch {
    /// Watches a process for exit, or returns `None` when no such process is running.
    pub fn register(process_id: u32) -> Result<Option<Self>> {
        let pid = libc::pid_t::try_from(process_id).context("process id out of range")?;

        #[cfg(target_os = "linux")]
        {
            // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(None);
                }
                return Err(anyhow::Error::new(error).context("open process exit watch"));
            }
            // SAFETY: the descriptor was just returned by pidfd_open and is owned by nobody else.
            Ok(Some(Self(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })))
        }

        #[cfg(target_os = "macos")]
        {
            // SAFETY: kqueue takes no arguments and returns a new descriptor or -1.
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(anyhow::Error::new(io::Error::last_os_error())
                    .context("create process exit watch queue"));
            }
            // SAFETY: the descriptor was just returned by kqueue and is owned by nobody else.
            let queue = unsafe { OwnedFd::from_raw_fd(fd) };
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one valid change is registered and no events are requested.
            let result = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(None);
                }
                return Err(anyhow::Error::new(error).context("register process exit watch"));
            }
            Ok(Some(Self(queue)))
        }
    }

    /// Returns whether the watched process has exited, without blocking.
    pub fn has_exited(&self) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd is passed with a zero timeout.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        if ready < 0 {
            return Err(anyhow::Error::new(io::Error::last_os_error()).context("poll process exit"));
        }
        Ok(ready > 0)
    }
}

impl AsFd for ExitWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for ExitWatch {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Parent and lifetime facts for one running host process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    pub parent_process_id: u32,

    /// Opaque value that never changes for one process lifetime, including across exec, while a
    /// later process reusing the PID gets a different one within one boot. Within one boot a
    /// process's token is never smaller than its parent's, so a parent with a larger token than
    /// its child is a reused PID. macOS uses `p_uniqueid`, Linux the start time in clock ticks.
    pub start_token: u64,
}

/// Reads running host process facts so callers can relate processes to each other.
pub trait ProcessTable: Send + Sync {
    /// Returns the process's facts, or `None` when no such process is running.
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>>;
}

/// Process table of the platform this binary runs on.
#[cfg(target_os = "linux")]
pub use LinuxProcessTable as HostProcessTable;

/// Process table of the platform this binary runs on.
#[cfg(target_os = "macos")]
pub use MacosProcessTable as HostProcessTable;

/// Process table backed by Linux `/proc/<pid>/stat`. Zombies keep their entry until reaped.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxProcessTable;

#[cfg(target_os = "linux")]
impl ProcessTable for LinuxProcessTable {
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
        read_process_stat(process_id)?
            .map(|stat| parse_process_stat(&stat))
            .transpose()
    }
}

#[cfg(target_os = "linux")]
fn linux_start_token(process_id: u32) -> Result<Option<u64>> {
    let Some(stat) = read_process_stat(process_id)? else {
        return Ok(None);
    };
    if matches!(process_state(&stat), Some("Z" | "X")) {
        return Ok(None);
    }
    parse_process_stat(&stat).map(|entry| Some(entry.start_token))
}

#[cfg(target_os = "linux")]
fn read_process_stat(process_id: u32) -> Result<Option<String>> {
    match std::fs::read_to_string(format!("/proc/{process_id}/stat")) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if process_vanished(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context("read process stat")),
    }
}

/// Returns the one-letter scheduler state, the first field after the command name.
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "only Linux reads /proc")
)]
fn process_state(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().next()
}

#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "only Linux reads /proc")
)]
fn parse_process_stat(stat: &str) -> Result<ProcessEntry> {
    // The command name is parenthesized and may contain spaces or parentheses, so fields are
    // counted after its last closing parenthesis: state, parent PID, ..., start time.
    let fields = stat
        .rsplit_once(')')
        .context("parse process stat command")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Ok(ProcessEntry {
        parent_process_id: fields
            .get(1)
            .context("process stat has no parent field")?
            .parse()
            .context("parse process parent PID")?,
        start_token: fields
            .get(19)
            .context("process stat has no start-time field")?
            .parse()
            .context("parse process start-time ticks")?,
    })
}

#[cfg(target_os = "linux")]
fn linux_memory_bytes(process_id: u32) -> Result<Option<u64>> {
    let status = match std::fs::read_to_string(format!("/proc/{process_id}/status")) {
        Ok(status) => status,
        Err(error) if process_vanished(&error) => return Ok(None),
        Err(error) => return Err(anyhow::Error::new(error).context("read process status")),
    };
    // Zombies and kernel threads have no resident set line.
    let Some(line) = status.lines().find_map(|line| line.strip_prefix("VmRSS:")) else {
        return Ok(None);
    };
    let kibibytes = line
        .trim()
        .strip_suffix("kB")
        .context("process resident set has no kB unit")?
        .trim()
        .parse::<u64>()
        .context("parse process resident set")?;
    Ok(Some(kibibytes * 1024))
}

#[cfg(target_os = "linux")]
fn linux_child_process_ids(process_id: u32) -> Result<Vec<u32>> {
    let mut children = Vec::new();
    for entry in std::fs::read_dir("/proc").context("list processes")? {
        let entry = entry.context("list processes")?;
        let Some(candidate) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(stat) = read_process_stat(candidate)? else {
            continue;
        };
        if parse_process_stat(&stat)?.parent_process_id == process_id {
            children.push(candidate);
        }
    }
    Ok(children)
}

#[cfg(target_os = "linux")]
fn process_vanished(error: &io::Error) -> bool {
    // A process that exits while its stat file is read reports ESRCH instead of NotFound.
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// Process table backed by macOS `proc_pidinfo`. Zombies have no entry.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosProcessTable;

#[cfg(target_os = "macos")]
impl ProcessTable for MacosProcessTable {
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
        macos::entry(process_id)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{ffi::CStr, io, mem};

    use anyhow::{Context, Result};

    use super::ProcessEntry;

    /// `proc_pidinfo` flavor returning `ProcUniqIdentifierInfo`; private in the public SDK
    /// (`xnu bsd/sys/proc_info.h`) and stable since macOS 10.8.
    const PROC_PIDUNIQIDENTIFIERINFO: libc::c_int = 17;

    #[repr(C)]
    struct ProcUniqIdentifierInfo {
        p_uuid: [u8; 16],
        p_uniqueid: u64,
        p_puniqueid: u64,
        p_idversion: i32,
        p_orig_ppidversion: i32,
        p_reserve2: u64,
        p_reserve3: u64,
    }

    /// `pbsi_status` of an exited process that is not reaped yet (`SZOMB` in `sys/proc.h`).
    const SZOMB: u32 = 5;

    pub(super) fn entry(process_id: u32) -> Result<Option<ProcessEntry>> {
        let Some(start_token) = unique_id(process_id)? else {
            return Ok(None);
        };
        let Some(short) =
            pid_info::<libc::proc_bsdshortinfo>(process_id, libc::PROC_PIDT_SHORTBSDINFO)
                .context("read process parent and status")?
        else {
            return Ok(None);
        };
        // The facts come from two calls, so the unique id is read again: a PID reused in between
        // must not pair one process's token with another's parent.
        if short.pbsi_status == SZOMB || unique_id(process_id)? != Some(start_token) {
            return Ok(None);
        }
        Ok(Some(ProcessEntry {
            parent_process_id: short.pbsi_ppid,
            start_token,
        }))
    }

    pub(super) fn boot_session_uuid() -> Result<String> {
        let mut buffer = [0u8; 64];
        let mut length = buffer.len();
        // SAFETY: the name is NUL-terminated and the buffer length is passed alongside it.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(anyhow::Error::new(io::Error::last_os_error())
                .context("read macOS boot session uuid"));
        }
        Ok(CStr::from_bytes_until_nul(&buffer[..length])
            .context("parse macOS boot session uuid")?
            .to_str()
            .context("parse macOS boot session uuid")?
            .to_owned())
    }

    pub(super) fn phys_footprint(process_id: u32) -> Result<Option<u64>> {
        let Ok(pid) = libc::c_int::try_from(process_id) else {
            return Ok(None);
        };
        let mut info = mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
        // SAFETY: the buffer is a whole `rusage_info_v4`, the structure the V4 flavor writes.
        let result =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
        if result == 0 {
            // SAFETY: the kernel filled the structure.
            return Ok(Some(unsafe { info.assume_init() }.ri_phys_footprint));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(anyhow::Error::new(error).context("read process memory footprint")),
        }
    }

    pub(super) fn child_process_ids(process_id: u32) -> Result<Vec<u32>> {
        let Ok(pid) = libc::pid_t::try_from(process_id) else {
            return Ok(Vec::new());
        };
        let mut capacity = 64;
        loop {
            let mut buffer = vec![0 as libc::pid_t; capacity];
            let size = libc::c_int::try_from(capacity * mem::size_of::<libc::pid_t>())
                .context("child process buffer size")?;
            // SAFETY: the buffer holds exactly `size` bytes and the kernel writes at most that.
            let count = unsafe { libc::proc_listchildpids(pid, buffer.as_mut_ptr().cast(), size) };
            let Ok(count) = usize::try_from(count) else {
                return Err(
                    anyhow::Error::new(io::Error::last_os_error()).context("list child processes")
                );
            };
            // A full buffer may have dropped children, so the list is read again, larger.
            if count < capacity {
                buffer.truncate(count);
                return Ok(buffer
                    .into_iter()
                    .filter_map(|child| u32::try_from(child).ok().filter(|child| *child > 0))
                    .collect());
            }
            capacity *= 4;
        }
    }

    fn unique_id(process_id: u32) -> Result<Option<u64>> {
        Ok(
            pid_info::<ProcUniqIdentifierInfo>(process_id, PROC_PIDUNIQIDENTIFIERINFO)
                .context("read process unique id")?
                .map(|info| info.p_uniqueid),
        )
    }

    fn pid_info<T>(process_id: u32, flavor: libc::c_int) -> io::Result<Option<T>> {
        let Ok(pid) = libc::c_int::try_from(process_id) else {
            return Ok(None);
        };
        let mut info = mem::MaybeUninit::<T>::zeroed();
        let size = mem::size_of::<T>() as libc::c_int;
        // Cleared first: a failing call does not always set errno, and a stale value must not
        // be mistaken for its cause.
        // SAFETY: __error returns this thread's errno location.
        unsafe { *libc::__error() = 0 };
        // SAFETY: the buffer is exactly `size` bytes and the kernel writes at most that many.
        let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
        if written == size {
            // SAFETY: the kernel filled the whole structure.
            return Ok(Some(unsafe { info.assume_init() }));
        }
        if written > 0 {
            return Err(io::Error::other("short process info"));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(0 | libc::ESRCH) => Ok(None),
            _ => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    /// Every authored source of the crate.
    const SOURCES: [(&str, &str); 3] = [
        ("lib.rs", include_str!("lib.rs")),
        ("ancestry.rs", include_str!("ancestry.rs")),
        ("test_support.rs", include_str!("test_support.rs")),
    ];

    #[test]
    fn sources_are_owner_neutral_and_import_no_backend_crate() {
        for (name, source) in SOURCES {
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("production source")
                .to_lowercase();
            for forbidden in [
                ["dot", "core_"].concat(),
                ["eng", "ine"].concat(),
                ["mod", "el"].concat(),
                ["infer", "ence"].concat(),
                ["ser", "ver"].concat(),
                ["ll", "m"].concat(),
                ["lla", "ma"].concat(),
                ["gg", "uf"].concat(),
                ["hug", "ging"].concat(),
            ] {
                assert!(
                    !production.contains(&forbidden),
                    "{name} contains {forbidden}"
                );
            }
        }
    }

    #[test]
    fn current_process_has_a_stable_nonzero_start_token() {
        let process_id = std::process::id();

        let first = start_token(process_id)
            .unwrap()
            .expect("current process token");
        let second = start_token(process_id)
            .unwrap()
            .expect("current process token");

        assert!(first > 0);
        assert_eq!(first, second);
    }

    #[test]
    fn current_process_parent_matches_the_operating_system() {
        assert_eq!(
            parent_process_id(std::process::id()).unwrap(),
            Some(std::os::unix::process::parent_id())
        );
    }

    #[test]
    fn spawned_child_has_a_token_and_this_process_as_parent_until_it_exits() {
        let mut child = sleeping_child();

        let token = start_token(child.id()).unwrap();
        let parent = parent_process_id(child.id()).unwrap();

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(token.is_some());
        assert_eq!(parent, Some(std::process::id()));
        assert_eq!(start_token(child.id()).unwrap(), None);
        assert_eq!(parent_process_id(child.id()).unwrap(), None);
    }

    #[test]
    fn memory_and_children_of_a_running_process_are_read() {
        let mut child = sleeping_child();

        let children = child_process_ids(std::process::id()).unwrap();
        let own = memory_bytes(std::process::id()).unwrap();
        let of_child = memory_bytes(child.id()).unwrap();

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(children.contains(&child.id()), "{children:?}");
        assert!(own.is_some_and(|bytes| bytes > 0));
        assert!(of_child.is_some_and(|bytes| bytes > 0));
        assert!(
            !child_process_ids(std::process::id())
                .unwrap()
                .contains(&child.id())
        );
        assert_eq!(child_process_ids(child.id()).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn missing_process_has_no_facts() {
        // Neither platform hands out PIDs near i32::MAX.
        let missing = i32::MAX as u32;

        assert_eq!(start_token(missing).unwrap(), None);
        assert_eq!(parent_process_id(missing).unwrap(), None);
        assert_eq!(memory_bytes(missing).unwrap(), None);
        assert_eq!(child_process_ids(missing).unwrap(), Vec::<u32>::new());
        assert!(ExitWatch::register(missing).unwrap().is_none());
        assert_eq!(HostProcessTable.entry(missing).unwrap(), None);
        assert_eq!(HostProcessTable.entry(u32::MAX).unwrap(), None);
    }

    #[test]
    fn host_table_entry_matches_the_process_facts_and_orders_child_after_parent() {
        let mut child = sleeping_child();

        let entry = HostProcessTable.entry(child.id()).unwrap();
        let token = start_token(child.id()).unwrap();
        let parent = HostProcessTable.entry(std::process::id()).unwrap();

        child.kill().unwrap();
        child.wait().unwrap();
        let entry = entry.expect("running child entry");
        let parent = parent.expect("current process entry");
        assert_eq!(entry.parent_process_id, std::process::id());
        assert_eq!(Some(entry.start_token), token);
        assert!(entry.start_token >= parent.start_token);
        assert_eq!(HostProcessTable.entry(child.id()).unwrap(), None);
    }

    #[test]
    fn boot_id_is_stable_and_nonempty() {
        let first = boot_id().unwrap();

        assert_eq!(first.len(), 36, "uuid-shaped boot id: {first}");
        assert_eq!(boot_id().unwrap(), first);
    }

    #[test]
    fn exit_watch_fires_when_the_watched_process_exits() {
        let mut child = sleeping_child();
        let watch = ExitWatch::register(child.id())
            .unwrap()
            .expect("running child");

        assert!(!watch.has_exited().unwrap());
        child.kill().unwrap();
        child.wait().unwrap();

        assert!(watch.has_exited().unwrap());
    }

    #[test]
    fn an_exited_unreaped_child_is_not_running() {
        let mut child = sleeping_child();
        let watch = ExitWatch::register(child.id()).unwrap().unwrap();
        child.kill().unwrap();
        while !watch.has_exited().unwrap() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let token = start_token(child.id()).unwrap();

        child.wait().unwrap();
        assert_eq!(token, None);
    }

    #[test]
    fn stat_parsing_skips_parentheses_and_spaces_in_the_command_name() {
        let stat =
            "4242 (vllm (a) b) S 1200 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 3 0 98765 0 0";

        assert_eq!(
            parse_process_stat(stat).unwrap(),
            ProcessEntry {
                parent_process_id: 1200,
                start_token: 98765,
            }
        );
        assert_eq!(process_state(stat), Some("S"));
    }

    fn sleeping_child() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }
}
