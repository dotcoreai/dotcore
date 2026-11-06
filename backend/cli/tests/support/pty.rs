//! Runs a command with a real pseudo-terminal as its controlling terminal and drives it like
//! an operator would: it waits for text on an emulated screen, answers cursor-position
//! queries, and sends keystrokes. Waits and the whole-session view also see text that already
//! scrolled off the screen, so assertions do not depend on how much the command printed. The
//! command's own stdout and stderr stay piped unless a session asks for stderr on the
//! terminal, so tests can prove that guided output reaches only the terminal.

use std::{
    fs::File,
    io::{Read, Write},
    num::NonZeroU16,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use dotcore::testing::{TEST_TIMING_ENVIRONMENT, Timing};

const ROWS: u16 = 24;
const COLS: u16 = 80;
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const KEY_SETTLE: Duration = Duration::from_millis(60);
const SCROLLBACK_ROWS: usize = 200;

/// One scripted interaction with the emulated terminal.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// Waits until the emulated screen shows the text, at any moment since the previous
    /// keystrokes were sent (or since the start), even if it has scrolled off since.
    Wait(&'static str),

    /// Types the bytes into the terminal, as keystrokes.
    Send(&'static [u8]),
}

/// Where the command's stdin comes from. The pty is always its controlling terminal.
#[derive(Clone, Copy, Debug)]
pub enum Stdin {
    /// The pty slave, as an interactive shell would provide.
    Pty,

    /// `/dev/null`, so only `/dev/tty` can deliver keystrokes.
    Null,
}

/// A scripted terminal session.
pub struct Session {
    stdin: Stdin,
    stderr_on_terminal: bool,
    cursor_position_answers: usize,
    steps: Vec<Step>,
}

impl Session {
    /// A session whose terminal answers cursor-position queries like a real one.
    pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            stdin: Stdin::Pty,
            stderr_on_terminal: false,
            cursor_position_answers: usize::MAX,
            steps: steps.into_iter().collect(),
        }
    }

    /// Replaces the command's stdin.
    pub fn with_stdin(mut self, stdin: Stdin) -> Self {
        self.stdin = stdin;
        self
    }

    /// Sends the command's stderr to the terminal, as an interactive shell would, instead of
    /// piping it; `stderr()` is then empty and the text appears on the screen.
    pub fn with_stderr_on_terminal(mut self) -> Self {
        self.stderr_on_terminal = true;
        self
    }

    /// Makes the terminal ignore cursor-position queries, as a minimal terminal would.
    pub fn without_cursor_position_answers(self) -> Self {
        self.with_cursor_position_answers(0)
    }

    /// Makes the terminal answer only the first `count` cursor-position queries.
    pub fn with_cursor_position_answers(mut self, count: usize) -> Self {
        self.cursor_position_answers = count;
        self
    }
}

/// What a finished session left behind, on the process streams and on the terminal.
pub struct PtyRun {
    pub output: Output,
    transcript: Vec<u8>,
    screen: String,
    session: String,
    cursor_hidden: bool,
    termios_restored: bool,
}

impl PtyRun {
    /// Process stdout as text.
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    /// Process stderr as text.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// The exit code, when the process exited normally.
    pub fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    /// The final emulated screen, for failure messages.
    pub fn screen(&self) -> &str {
        &self.screen
    }

    /// The whole rendered session: the lines that scrolled off the top, up to
    /// `SCROLLBACK_ROWS`, followed by the final screen.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Number of SGR sequences that set a foreground or background color on the terminal.
    pub fn color_sgr_count(&self) -> usize {
        csi_sequences(&self.transcript)
            .into_iter()
            .filter(|(params, final_byte)| {
                *final_byte == b'm'
                    && params.split(';').any(|param| {
                        matches!(
                            param.parse::<u16>(),
                            Ok(30..=38 | 40..=48 | 90..=97 | 100..=107)
                        )
                    })
            })
            .count()
    }

    /// Number of cursor-position queries (`ESC[6n`) the process sent to the terminal.
    pub fn cursor_query_count(&self) -> usize {
        csi_sequences(&self.transcript)
            .into_iter()
            .filter(|(params, final_byte)| params == "6" && *final_byte == b'n')
            .count()
    }

    /// Every DEC private mode the process enabled (`ESC[?<n>h`), such as `1049` for the
    /// alternate screen.
    pub fn enabled_private_modes(&self) -> Vec<u16> {
        csi_sequences(&self.transcript)
            .into_iter()
            .filter(|(params, final_byte)| params.starts_with('?') && *final_byte == b'h')
            .flat_map(|(params, _)| {
                params[1..]
                    .split(';')
                    .filter_map(|mode| mode.parse::<u16>().ok())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Asserts the terminal is usable again: line settings match the start, the cursor is
    /// visible, and neither the alternate screen nor mouse reporting was ever enabled.
    pub fn assert_terminal_restored(&self) {
        assert!(
            self.termios_restored,
            "terminal line settings were left changed; screen:\n{}",
            self.screen
        );
        assert!(
            !self.cursor_hidden,
            "cursor left hidden; screen:\n{}",
            self.screen
        );
        let modes = self.enabled_private_modes();
        for forbidden in [47, 1047, 1049, 1000, 1002, 1003, 1006] {
            assert!(
                !modes.contains(&forbidden),
                "private mode {forbidden} was enabled: {modes:?}"
            );
        }
    }
}

/// Runs the command under the session and returns what it left behind, or `None` when
/// pseudo-terminals are unavailable.
pub fn run_in_pty(mut command: Command, session: Session) -> Option<PtyRun> {
    let (master, slave) = open_pty()?;
    // Line settings are read through the master: macOS revokes the slave for every holder
    // once the session leader exits, while the master keeps reporting the slave's settings.
    let termios_before = termios(master.as_raw_fd());

    // The test timing bounds the wait on a terminal that never answers a cursor query.
    command.env(TEST_TIMING_ENVIRONMENT, Timing::test().to_environment());
    command.stdout(Stdio::piped());
    if session.stderr_on_terminal {
        command.stderr(Stdio::from(slave.try_clone().expect("clone pty slave")));
    } else {
        command.stderr(Stdio::piped());
    }
    let slave_raw = slave.as_raw_fd();
    let controlling_fd = match session.stdin {
        Stdin::Pty => {
            command.stdin(Stdio::from(slave));
            0
        }
        Stdin::Null => {
            command.stdin(Stdio::null());
            slave_raw
        }
    };
    // SAFETY: the closure calls only async-signal-safe session and ioctl operations before
    // exec.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "macos")]
            let request: libc::c_ulong = libc::TIOCSCTTY.into();
            #[cfg(not(target_os = "macos"))]
            let request = libc::TIOCSCTTY;
            if libc::setsid() == -1 || libc::ioctl(controlling_fd, request, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if controlling_fd != 0 {
                libc::close(controlling_fd);
            }
            Ok(())
        });
    }

    let terminal = Arc::new(Mutex::new(Terminal::new()));
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_reader(
        master.try_clone().expect("clone pty master"),
        terminal.clone(),
        transcript.clone(),
        stop.clone(),
        session.cursor_position_answers,
    );

    let child = command.spawn().expect("spawn dotc under a pty");
    let pid = child.id();
    // Streams are collected while the steps run, and the reader keeps draining the master
    // until the child is reaped: macOS does not let a session leader finish exiting while
    // its terminal output is unread.
    let (sender, exit) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });

    let mut keyboard = master;
    if let Err(failure) = run_steps(&mut keyboard, &terminal, &session.steps) {
        kill_and_reap(pid, &exit);
        stop.store(true, Ordering::SeqCst);
        panic!("{failure}");
    }

    let output = match exit.recv_timeout(EXIT_TIMEOUT) {
        Ok(output) => output.expect("wait for dotc"),
        Err(_) => {
            kill_and_reap(pid, &exit);
            stop.store(true, Ordering::SeqCst);
            let session = terminal.lock().unwrap().session();
            panic!("dotc did not exit; session:\n{session}");
        }
    };
    // Let the reader drain the last terminal output before the final state is read.
    thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);
    reader.join().expect("join pty reader");

    let termios_after = termios(keyboard.as_raw_fd());
    let mut terminal = terminal.lock().unwrap();
    let session_text = terminal.session();
    let final_screen = terminal.parser.screen();
    let transcript = transcript.lock().unwrap().clone();
    Some(PtyRun {
        output,
        screen: final_screen.contents(),
        session: session_text,
        cursor_hidden: final_screen.hide_cursor(),
        termios_restored: termios_before.c_lflag == termios_after.c_lflag
            && termios_before.c_iflag == termios_after.c_iflag
            && termios_before.c_oflag == termios_after.c_oflag
            && termios_before.c_cflag == termios_after.c_cflag,
        transcript,
    })
}

/// Opens a `ROWS` by `COLS` pseudo-terminal pair, or returns `None` after printing a skip
/// notice when the environment forbids it.
fn open_pty() -> Option<(File, File)> {
    let mut winsize = libc::winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors on success; the winsize pointer points at
    // a live value; Apple declares the parameter `*mut` while Linux declares it `*const`.
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut winsize,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        ) {
            eprintln!("SKIPPED: pseudo-terminals are unavailable in this environment: {error}");
            return None;
        }
        panic!("open pseudo-terminal: {error}");
    }
    // Keep the master out of the child.
    // SAFETY: master_fd is the descriptor openpty just returned.
    unsafe { libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: successful openpty returned fresh owned descriptors.
    Some(unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) })
}

fn termios(fd: RawFd) -> libc::termios {
    let mut settings = std::mem::MaybeUninit::uninit();
    // SAFETY: the descriptor is open and the out pointer is valid for one termios.
    assert_eq!(unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) }, 0);
    // SAFETY: tcgetattr initialized the value.
    unsafe { settings.assume_init() }
}

/// Feeds terminal output into the emulator and answers up to `cursor_position_answers`
/// `ESC[6n` queries with the emulated cursor.
fn spawn_reader(
    mut master: File,
    terminal: Arc<Mutex<Terminal>>,
    transcript: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    mut cursor_position_answers: usize,
) -> thread::JoinHandle<()> {
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";

    thread::spawn(move || {
        let mut responder = master.try_clone().expect("clone pty master");
        loop {
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, 20) };
            if ready <= 0 || poll.revents & libc::POLLIN == 0 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            let mut buffer = [0u8; 4096];
            let Ok(read) = master.read(&mut buffer) else {
                return;
            };
            if read == 0 {
                return;
            }
            let chunk = &buffer[..read];
            transcript.lock().unwrap().extend_from_slice(chunk);

            let mut terminal = terminal.lock().unwrap();
            let mut rest = chunk;
            while let Some(index) = find(rest, CURSOR_QUERY) {
                let end = index + CURSOR_QUERY.len();
                terminal.process(&rest[..end]);
                if cursor_position_answers > 0 {
                    cursor_position_answers -= 1;
                    let (row, col) = terminal.parser.screen().cursor_position();
                    let _ =
                        responder.write_all(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                rest = &rest[end..];
            }
            terminal.process(rest);
        }
    })
}

/// Plays the steps on the terminal, failing with the session when a wait times out.
fn run_steps(
    keyboard: &mut File,
    terminal: &Arc<Mutex<Terminal>>,
    steps: &[Step],
) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Wait(text) => wait_for(terminal, text)?,
            Step::Send(bytes) => {
                terminal.lock().unwrap().start_step();
                keyboard.write_all(bytes).expect("send keystrokes");
                keyboard.flush().expect("flush keystrokes");
                thread::sleep(KEY_SETTLE);
            }
        }
    }
    Ok(())
}

fn wait_for(terminal: &Arc<Mutex<Terminal>>, text: &str) -> Result<(), String> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let mut terminal = terminal.lock().unwrap();
        if terminal.has_shown(text) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "timed out waiting for {text:?}; session:\n{}",
                terminal.session()
            ));
        }
        drop(terminal);
        thread::sleep(Duration::from_millis(20));
    }
}

/// The emulated terminal, plus every screen it showed since the current step started, so a
/// wait finds text that scrolled off before the wait looked.
struct Terminal {
    parser: vt100::Parser,
    shown: String,
}

impl Terminal {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(
                NonZeroU16::new(ROWS).expect("rows are non-zero"),
                NonZeroU16::new(COLS).expect("columns are non-zero"),
                SCROLLBACK_ROWS,
            ),
            shown: String::new(),
        }
    }

    /// Feeds terminal output, recording the screen before each line feed: a line feed on the
    /// bottom row is what scrolls the top line away.
    fn process(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(index) = rest.iter().skip(1).position(|byte| *byte == b'\n') {
            let (line, next) = rest.split_at(index + 1);
            self.parser.process(line);
            self.record_screen();
            rest = next;
        }
        if !rest.is_empty() {
            self.parser.process(rest);
            self.record_screen();
        }
    }

    /// Starts a step: only the current screen and what is shown from now on count as shown.
    fn start_step(&mut self) {
        self.shown = self.parser.screen().contents();
    }

    fn has_shown(&self, text: &str) -> bool {
        self.shown.contains(text)
    }

    /// Renders the scrolled-off lines followed by the current screen, joining wrapped rows
    /// the way the screen contents do.
    fn session(&mut self) -> String {
        let screen = self.parser.screen_mut();
        let mut session = String::new();
        screen.set_scrollback(usize::MAX);
        while screen.scrollback() > 0 {
            session.extend(screen.rows(0, COLS).next());
            if !screen.row_wrapped(0) {
                session.push('\n');
            }
            let offset = screen.scrollback() - 1;
            screen.set_scrollback(offset);
        }
        session.push_str(&screen.contents());
        session
    }

    fn record_screen(&mut self) {
        // Separates screens so text never matches across two of them.
        self.shown.push('\0');
        self.shown.push_str(&self.parser.screen().contents());
    }
}

/// Kills the child's process group unless the child already exited, and waits for its waiter
/// thread to reap it, so a failed session leaves no process behind.
fn kill_and_reap(pid: u32, exit: &mpsc::Receiver<std::io::Result<Output>>) {
    if exit.try_recv().is_ok() {
        return;
    }
    // The child leads its own session and process group (setsid before exec), so signalling the
    // group also stops helpers that could hold its stdout or stderr open and block the waiter.
    // SAFETY: plain kill of the process group led by the unreaped child this module spawned.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    assert!(
        exit.recv_timeout(EXIT_TIMEOUT).is_ok(),
        "dotc (pid {pid}) was not reaped after SIGKILL"
    );
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Splits terminal output into CSI sequences as (parameter bytes, final byte).
fn csi_sequences(bytes: &[u8]) -> Vec<(String, u8)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == 0x1b && bytes[index + 1] == b'[' {
            let start = index + 2;
            let mut end = start;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end < bytes.len() {
                found.push((
                    String::from_utf8_lossy(&bytes[start..end]).into_owned(),
                    bytes[end],
                ));
            }
            index = end;
        }
        index += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_finds_text_that_scrolled_off_before_it_looked() {
        let mut terminal = Terminal::new();

        terminal.process(numbered_output("hint").as_bytes());

        assert!(!terminal.parser.screen().contents().contains("hint"));
        assert!(terminal.has_shown("hint"));
    }

    #[test]
    fn a_step_only_sees_text_still_on_screen_or_shown_after_it_started() {
        let mut terminal = Terminal::new();
        terminal.process(numbered_output("before").as_bytes());

        terminal.start_step();
        terminal.process(b"after\r\n");

        assert!(!terminal.has_shown("before"));
        assert!(terminal.has_shown("line 40"));
        assert!(terminal.has_shown("after"));
    }

    #[test]
    fn the_session_renders_scrolled_off_lines_before_the_screen() {
        let mut terminal = Terminal::new();

        terminal.process(numbered_output("first").as_bytes());

        let expected = std::iter::once("first".to_owned())
            .chain((1..=40).map(|line| format!("line {line}")))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(terminal.session(), expected);
        assert_eq!(terminal.parser.screen().scrollback(), 0);
    }

    /// A heading followed by 40 numbered lines, enough to scroll it off a 24-row screen.
    fn numbered_output(heading: &str) -> String {
        let mut output = format!("{heading}\r\n");
        for line in 1..=40 {
            output.push_str(&format!("line {line}\r\n"));
        }
        output
    }
}
