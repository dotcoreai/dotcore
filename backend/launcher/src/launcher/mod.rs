//! Runs the `dotcore-launcher` binary: it detaches from the process that started it, starts the
//! declared executable, and holds it until it exits, recording output and the final status in
//! the workload directory.
//!
//! The starting process gets a success status only once the detached launcher has written its
//! identity record, so a successful start always leaves a launcher that can be found again. A
//! stop signal is forwarded to the held process, followed by a forced kill of its whole process
//! group after the grace period. A watched-lifetime launcher runs the same stop once the watched
//! process exits, or stops at once when that process is already gone. Once the held process has
//! started, every launcher failure still stops it and writes an exit record.

mod log;

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::{Child, Command, ExitCode, Stdio},
    sync::atomic::{AtomicI32, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use dotcore_host_process::{self as process, ExitWatch};

use self::log::LogWriter;
use crate::workload::{
    EXIT_FILE, ExitReason, ExitRecord, ExitStatus, LAUNCH_FILE, LAUNCHER_FILE, LOG_CAP_BYTES,
    LaunchSpec, LauncherRecord, Lifetime, LogStream, PROCESS_FILE, ProcessRecord, SCHEMA_VERSION,
    StoredRecord, WORK_DIRECTORY, unix_time_ns, write_file_atomically,
};

/// Longest wait of the starting process for the detached launcher to record its identity.
// Not a behavior duration: bound on a real workload process reaching readiness.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Signals that ask the launcher to stop the held process.
const STOP_SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// Signals that would terminate the launcher by default and are therefore caught and ignored.
/// They are caught rather than set to ignored, because ignored signals stay ignored in the held
/// process across exec.
const IGNORED_SIGNALS: [libc::c_int; 7] = [
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGVTALRM,
    libc::SIGPROF,
];

/// Write end of the self-pipe that signal handlers report delivered signals to.
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Runs the launcher for the workload directory named by the only argument.
pub fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(dir), None) = (args.next(), args.next()) else {
        eprintln!("usage: dotcore-launcher <workload directory>");
        return ExitCode::from(2);
    };
    match start(Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dotcore-launcher: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Detaches into a new session and returns once the detached launcher recorded its identity.
fn start(dir: &Path) -> Result<()> {
    // Descriptors the starting process leaked without close-on-exec would otherwise stay open
    // for the whole life of the workload.
    close_inherited_descriptors();
    let dir = fs::canonicalize(dir).context("resolve workload directory")?;
    let spec =
        LaunchSpec::decode(&fs::read(dir.join(LAUNCH_FILE)).context("read launch specification")?)
            .context("parse launch specification")?;

    let (ready_read, ready_write) = pipe().context("create readiness pipe")?;

    // Double fork: the intermediate process starts a new session, which leaves the starting
    // process's process group so stopping that group (for example by a process manager) does not
    // reach the launcher; its own child then never leads the session. No threads exist yet, so
    // forking is safe.
    let intermediate = fork().context("fork session leader")?;
    if intermediate == 0 {
        // SAFETY: setsid has no preconditions; a forked child never leads a process group.
        if unsafe { libc::setsid() } < 0 {
            std::process::exit(1);
        }
        match fork() {
            Ok(0) => {
                drop(ready_read);
                std::process::exit(match hold(&dir, &spec, ready_write) {
                    Ok(()) => 0,
                    Err(error) => {
                        // Stderr still reaches the starting process until readiness.
                        eprintln!("dotcore-launcher: {error:#}");
                        LogWriter::open(&dir, LogStream::Stderr, LOG_CAP_BYTES)
                            .note(&format!("{error:#}"));
                        1
                    }
                })
            }
            Ok(_) => std::process::exit(0),
            Err(_) => std::process::exit(1),
        }
    }

    drop(ready_write);
    let mut status = 0;
    // SAFETY: waitpid reaps the intermediate child created above.
    unsafe { libc::waitpid(intermediate, &mut status, 0) };
    wait_ready(ready_read)
}

/// Waits a bounded time for the readiness byte of the detached launcher.
fn wait_ready(ready: OwnedFd) -> Result<()> {
    let mut poll = poll_fd(ready.as_raw_fd());
    let timeout = READY_TIMEOUT.as_millis() as libc::c_int;
    // SAFETY: one valid pollfd is passed.
    let polled = unsafe { libc::poll(&mut poll, 1, timeout) };
    if polled == 0 {
        bail!("launcher did not record its identity within {READY_TIMEOUT:?}");
    }
    let mut byte = [0u8; 1];
    match File::from(ready).read(&mut byte) {
        Ok(1) => Ok(()),
        _ => bail!("launcher stopped before recording its identity"),
    }
}

/// Body of the detached launcher: records its identity, signals readiness, and holds the
/// workload process until it ends.
fn hold(dir: &Path, spec: &LaunchSpec, ready: OwnedFd) -> Result<()> {
    let signals = SignalPipe::install().context("install signal handlers")?;
    let pid = std::process::id();
    let record = LauncherRecord {
        schema_version: SCHEMA_VERSION,
        pid,
        start_token: process::start_token(pid)?.context("read launcher start token")?,
        boot_id: process::boot_id()?,
    };
    write_file_atomically(&dir.join(LAUNCHER_FILE), &record.encode())
        .context("write launcher record")?;

    // Inherited stdio would keep the starting process's output pipes open for as long as the
    // workload runs.
    redirect_stdio_to_null().context("close inherited stdio")?;
    File::from(ready)
        .write_all(b"1")
        .context("signal readiness")?;

    let (status, reason) = run(dir, spec, &signals);
    let exit = ExitRecord {
        status,
        reason,
        time_ms: unix_time_ns() / 1_000_000,
    };
    write_file_atomically(&dir.join(EXIT_FILE), &exit.encode()).context("write exit record")
}

/// Starts the workload process and returns its exit status and the reason it ended. Failures
/// are recorded in the stderr log and end in a stopped process, never in an error.
fn run(dir: &Path, spec: &LaunchSpec, signals: &SignalPipe) -> (Option<ExitStatus>, ExitReason) {
    let mut logs = [
        LogWriter::open(dir, LogStream::Stdout, LOG_CAP_BYTES),
        LogWriter::open(dir, LogStream::Stderr, LOG_CAP_BYTES),
    ];
    let mut watch = None;
    if let Lifetime::Watch { pid, start_token } = spec.lifetime {
        match watch_process(pid, start_token) {
            Ok(Some(registered)) => watch = Some(registered),
            Ok(None) => return (None, ExitReason::WatchedProcessExited),
            Err(error) => {
                logs[1].note(&format!("{error:#}"));
                return (None, ExitReason::LauncherFailed);
            }
        }
    }

    let child = match spawn(dir, spec) {
        Ok(child) => child,
        Err(error) => {
            logs[1].note(&format!("start {}: {error}", spec.executable.display()));
            return (None, ExitReason::SpawnFailed);
        }
    };
    let mut held = Held::new(child, logs, Duration::from_millis(spec.grace_period_ms));
    if let Err(error) = held.record(dir) {
        held.logs[1].note(&format!("{error:#}"));
    }

    match catch_unwind(AssertUnwindSafe(|| held.supervise(signals, watch))) {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            held.logs[1].note(&format!("{error:#}"));
            (held.terminate(), ExitReason::LauncherFailed)
        }
        Err(_) => (held.terminate(), ExitReason::LauncherFailed),
    }
}

/// The started workload process, its output, and its stop state.
struct Held {
    child: Child,
    group: libc::pid_t,
    outputs: [Option<File>; 2],
    logs: [LogWriter; 2],
    grace: Duration,
    status: Option<ExitStatus>,
    stop: Option<(ExitReason, Instant)>,
    killed: bool,
}

impl Held {
    fn new(mut child: Child, logs: [LogWriter; 2], grace: Duration) -> Self {
        let outputs = [
            child
                .stdout
                .take()
                .map(|pipe| File::from(OwnedFd::from(pipe))),
            child
                .stderr
                .take()
                .map(|pipe| File::from(OwnedFd::from(pipe))),
        ];
        Self {
            group: child.id() as libc::pid_t,
            child,
            outputs,
            logs,
            grace,
            status: None,
            stop: None,
            killed: false,
        }
    }

    /// Records the held process's identity so its group can be stopped even without the
    /// launcher.
    fn record(&self, dir: &Path) -> Result<()> {
        let pid = self.child.id();
        // A process that already exited needs no record.
        let Some(start_token) = process::start_token(pid)? else {
            return Ok(());
        };
        let record = ProcessRecord { pid, start_token };
        write_file_atomically(&dir.join(PROCESS_FILE), &record.encode())
            .context("write process record")
    }

    /// Forwards stop requests and output until the process exited and its output is drained.
    fn supervise(
        &mut self,
        signals: &SignalPipe,
        mut watch: Option<ExitWatch>,
    ) -> Result<(Option<ExitStatus>, ExitReason)> {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if self.status.is_none() {
                self.status = self.reap_if_exited()?;
            }
            if self.status.is_some() && self.outputs.iter().all(Option::is_none) {
                break;
            }

            let mut fds = vec![poll_fd(signals.read.as_raw_fd())];
            if let Some(watch) = &watch {
                fds.push(poll_fd(watch.as_raw_fd()));
            }
            for output in self.outputs.iter().flatten() {
                fds.push(poll_fd(output.as_raw_fd()));
            }
            // Once the process has exited, remaining output is drained without waiting for more.
            let timeout = match (self.status, self.stop) {
                (Some(_), _) => 0,
                (None, Some((_, deadline))) if !self.killed => deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128)
                    as libc::c_int,
                _ => -1,
            };
            // SAFETY: fds holds valid pollfd structures for its whole length.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(anyhow::Error::new(error).context("poll launcher events"));
            }
            if self.status.is_some() && ready == 0 {
                break;
            }

            let mut revents = fds.iter().map(|fd| fd.revents);
            let mut stop_reason = None;
            if revents.next() != Some(0)
                && signals
                    .drain()
                    .iter()
                    .any(|signal| STOP_SIGNALS.contains(signal))
            {
                stop_reason = Some(ExitReason::Stopped);
            }
            if watch.is_some() && revents.next() != Some(0) {
                watch = None;
                stop_reason = stop_reason.or(Some(ExitReason::WatchedProcessExited));
            }
            for (output, log) in self.outputs.iter_mut().zip(&mut self.logs) {
                let Some(pipe) = output else {
                    continue;
                };
                if revents.next() == Some(0) {
                    continue;
                }
                let read = pipe.read(&mut buffer).context("read workload output")?;
                if read == 0 {
                    log.finish();
                    *output = None;
                } else {
                    log.write(&buffer[..read]);
                }
            }

            if let Some(reason) = stop_reason
                && self.stop.is_none()
                && self.status.is_none()
            {
                // SAFETY: kill only sends a signal to the held process.
                unsafe { libc::kill(self.group, libc::SIGTERM) };
                self.stop = Some((reason, Instant::now() + self.grace));
            }
            if let Some((_, deadline)) = self.stop
                && !self.killed
                && self.status.is_none()
                && Instant::now() >= deadline
            {
                signal_group(self.group, libc::SIGKILL);
                self.killed = true;
            }
        }

        for log in &mut self.logs {
            log.finish();
        }
        Ok((
            self.status,
            self.stop.map_or(ExitReason::Exited, |(reason, _)| reason),
        ))
    }

    /// Stops the process after a launcher failure: stop signal, grace period, forced kill of its
    /// group, then reap. Output is no longer read, so its pipes are closed first.
    fn terminate(&mut self) -> Option<ExitStatus> {
        self.outputs = [None, None];
        if self.status.is_none() {
            // SAFETY: kill only sends a signal to the held process.
            unsafe { libc::kill(self.group, libc::SIGTERM) };
            let deadline = Instant::now() + self.grace;
            while self.status.is_none() && Instant::now() < deadline {
                self.status = self.reap_if_exited().ok().flatten();
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if self.status.is_none() {
            signal_group(self.group, libc::SIGKILL);
            self.status = self.child.wait().ok().map(exit_status);
        }
        for log in &mut self.logs {
            log.finish();
        }
        self.status
    }

    /// Reaps the process once it exited, after killing what remains of its process group.
    fn reap_if_exited(&mut self) -> Result<Option<ExitStatus>> {
        // SAFETY: an all-zero siginfo is a valid output buffer for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid only inspects the held child; WNOWAIT leaves it unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(anyhow::Error::new(error).context("wait for workload process"));
        }
        if info.si_signo == 0 {
            return Ok(None);
        }
        // The exited leader is not reaped yet, so its group id cannot name another group. The
        // group ends with its leader, like a container ends with its first process.
        signal_group(self.group, libc::SIGKILL);
        let status = self.child.wait().context("reap workload process")?;
        Ok(Some(exit_status(status)))
    }
}

/// Watches a process for exit and returns `None` when it is gone or its token differs.
fn watch_process(pid: u32, start_token: u64) -> Result<Option<ExitWatch>> {
    // Registering before checking the token means a later exit cannot be missed, and a reused
    // PID cannot pass for the watched process.
    let Some(watch) = ExitWatch::register(pid)? else {
        return Ok(None);
    };
    Ok((process::start_token(pid)? == Some(start_token)).then_some(watch))
}

/// Starts the held process in its own process group with only the declared environment, in the
/// workload's working directory.
fn spawn(dir: &Path, spec: &LaunchSpec) -> io::Result<Child> {
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .env_clear()
        .envs(&spec.environment)
        .current_dir(dir.join(WORK_DIRECTORY))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    #[cfg(target_os = "linux")]
    {
        let launcher = std::process::id() as libc::pid_t;
        // SAFETY: the hook only calls async-signal-safe functions.
        unsafe {
            command.pre_exec(move || {
                // The kernel kills the held process if the launcher dies without stopping it.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != launcher {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            })
        };
    }

    command.spawn()
}

/// Self-pipe that turns delivered signals into readable bytes for the event loop.
struct SignalPipe {
    read: OwnedFd,
    _write: OwnedFd,
}

impl SignalPipe {
    fn install() -> io::Result<Self> {
        let (read, write) = pipe()?;
        for fd in [&read, &write] {
            // SAFETY: fcntl on an owned descriptor only changes its status flags.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        SIGNAL_WRITE_FD.store(write.as_raw_fd(), Ordering::SeqCst);

        for signal in STOP_SIGNALS
            .into_iter()
            .chain(IGNORED_SIGNALS)
            .chain([libc::SIGCHLD])
        {
            // SAFETY: the action is fully initialized and the handler is async-signal-safe.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(Self {
            read,
            _write: write,
        })
    }

    /// Returns the signals delivered since the last call.
    fn drain(&self) -> Vec<libc::c_int> {
        let mut signals = [0u8; 64];
        let mut drained = Vec::new();
        loop {
            // SAFETY: the buffer is valid for its length and the descriptor is non-blocking.
            let read = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    signals.as_mut_ptr().cast(),
                    signals.len(),
                )
            };
            if read <= 0 {
                return drained;
            }
            drained.extend(
                signals[..read as usize]
                    .iter()
                    .map(|&signal| libc::c_int::from(signal)),
            );
        }
    }
}

extern "C" fn on_signal(signal: libc::c_int) {
    let byte = signal as u8;
    // SAFETY: errno_location returns this thread's errno, saved and restored around the write so
    // the interrupted code never sees a value the handler changed. write is async-signal-safe; a
    // full pipe drops the byte, which is harmless because pending bytes already wake the loop.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        libc::write(
            SIGNAL_WRITE_FD.load(Ordering::SeqCst),
            (&byte as *const u8).cast(),
            1,
        );
        *errno = saved;
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

fn exit_status(status: std::process::ExitStatus) -> ExitStatus {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitStatus::Code(code),
        (None, Some(signal)) => ExitStatus::Signal(signal),
        (None, None) => ExitStatus::Code(-1),
    }
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill only sends a signal; a group that no longer exists reports ESRCH, ignored.
    unsafe { libc::kill(-group, signal) };
}

fn poll_fd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Closes every descriptor above stderr, best effort.
fn close_inherited_descriptors() {
    #[cfg(target_os = "linux")]
    let listing = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    let listing = "/dev/fd";

    // The listing is collected first so its own descriptor is closed before the loop runs.
    let Ok(entries) = fs::read_dir(listing) else {
        return;
    };
    let fds = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
        .filter(|fd| *fd > 2)
        .collect::<Vec<_>>();
    for fd in fds {
        // SAFETY: nothing in this process owns these descriptors yet; closing an already closed
        // one reports EBADF, ignored.
        unsafe { libc::close(fd) };
    }
}

fn redirect_stdio_to_null() -> io::Result<()> {
    let null = File::options().read(true).write(true).open("/dev/null")?;
    for target in 0..=2 {
        // SAFETY: dup2 replaces a standard descriptor with a valid open descriptor.
        if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn fork() -> io::Result<libc::pid_t> {
    // SAFETY: the launcher forks only while single-threaded.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        pid => Ok(pid),
    }
}

/// Creates a pipe whose ends are closed on exec.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two new descriptors into the array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in fds {
        // SAFETY: fcntl on an owned descriptor only changes its descriptor flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(ends)
}
