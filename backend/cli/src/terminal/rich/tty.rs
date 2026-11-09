//! Low-level controlling-terminal plumbing for the guided prompts of
//! `dotc site init` and reset: drawing on `/dev/tty`, raw mode, and restoring
//! the terminal however a prompt ends.
//!
//! Nothing here writes to process stdout or stderr. The cursor position is
//! asked for on the terminal itself with the timing policy's limit, which also
//! tells apart terminals that never answer the question, and when a prompt
//! fails or the process panics, its half-drawn rows are erased, raw mode is
//! turned off, and the cursor is shown again before anything else is printed.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    sync::{Mutex, Once, PoisonError},
    time::{Duration, Instant},
};

use dotcore_domain::timing::TerminalTiming;
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

/// The prompt currently holding raw mode, if any, with the top row of its viewport once known.
///
/// Process-wide because the panic hook, which cannot reach the prompt, must restore it.
static ACTIVE_PROMPT: Mutex<Option<Option<u16>>> = Mutex::new(None);

/// Raw mode for the lifetime of one prompt.
///
/// Dropping the guard of an unfinished prompt, on error or while unwinding, erases the prompt's
/// rows; every drop turns raw mode off and shows the cursor.
pub(super) struct RawModeGuard;

impl RawModeGuard {
    /// Enables raw mode on the controlling terminal and arms the panic restore.
    pub(super) fn enable() -> io::Result<Self> {
        install_panic_restore();
        crossterm::terminal::enable_raw_mode()?;
        *active_prompt() = Some(None);
        Ok(Self)
    }

    /// Records the top row of the prompt's viewport, which a failure erases from.
    pub(super) fn track_viewport(&self, top: u16) {
        if let Some(viewport) = active_prompt().as_mut() {
            *viewport = Some(top);
        }
    }

    /// Marks the prompt as finished, so its summary line stays on screen.
    pub(super) fn finish(&self) {
        if let Some(viewport) = active_prompt().as_mut() {
            *viewport = None;
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Asks the terminal for its cursor position once, in raw mode, and fails when it does not
/// answer, since inline prompts cannot find their rows on such a terminal.
///
/// Nothing is drawn, and the terminal is restored before returning.
pub(super) fn probe_cursor_reports(tty: &File, timing: TerminalTiming) -> io::Result<()> {
    let _raw = RawModeGuard::enable()?;
    query_cursor_position(tty, timing.cursor_query_timeout)?;
    Ok(())
}

/// A crossterm backend drawing on the controlling terminal.
///
/// Only the cursor-position query is replaced: crossterm sends it to process stdout, which must
/// carry nothing but the command's result records.
pub(super) struct TtyBackend {
    inner: CrosstermBackend<File>,
    tty: File,
    timing: TerminalTiming,
}

impl TtyBackend {
    /// Creates a backend drawing on `tty`, whose cursor-position queries are bounded by `timing`.
    pub(super) fn new(tty: File, timing: TerminalTiming) -> io::Result<Self> {
        let query = tty.try_clone()?;
        Ok(Self {
            inner: CrosstermBackend::new(tty),
            tty: query,
            timing,
        })
    }

    /// Moves to the start of the line below `row`, scrolling at the bottom of the screen, and
    /// erases everything after it.
    pub(super) fn move_below(&mut self, row: u16) -> io::Result<()> {
        self.inner.set_cursor_position(Position::new(0, row))?;
        Backend::flush(&mut self.inner)?;
        // Raw mode disables output processing, so the line feed needs its carriage return.
        self.tty.write_all(b"\r\n\x1b[J")?;
        self.tty.flush()
    }
}

impl Backend for TtyBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Backend::flush(&mut self.inner)?;
        query_cursor_position(&self.tty, self.timing.cursor_query_timeout)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

fn active_prompt() -> std::sync::MutexGuard<'static, Option<Option<u16>>> {
    ACTIVE_PROMPT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Restores the terminal after the active prompt, erasing it first when it did not finish.
/// Does nothing when no prompt is active.
fn restore_terminal() {
    let Some(viewport) = active_prompt().take() else {
        return;
    };
    let _ = crossterm::terminal::disable_raw_mode();
    let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") else {
        return;
    };
    if let Some(top) = viewport {
        let _ = write!(tty, "\x1b[{};1H\x1b[J", u32::from(top) + 1);
    }
    let _ = tty.write_all(b"\x1b[?25h");
    let _ = tty.flush();
}

/// Restores the terminal before the panic message prints, so it is not garbled by raw mode.
fn install_panic_restore() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

/// Sends `ESC[6n` on the terminal and parses its `ESC[row;columnR` reply; bytes typed before
/// the reply are dropped. Requires raw mode, so the reply is neither echoed nor line-buffered.
/// Fails with `TimedOut` when no reply arrives within `timeout`.
fn query_cursor_position(mut tty: &File, timeout: Duration) -> io::Result<Position> {
    tty.write_all(b"\x1b[6n")?;
    tty.flush()?;
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal did not answer the cursor position query",
            ));
        }
        if !wait_readable(tty, remaining)? {
            continue;
        }
        let mut buffer = [0u8; 64];
        let read = tty.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal closed before answering the cursor position query",
            ));
        }
        reply.extend_from_slice(&buffer[..read]);
        if let Some(position) = parse_cursor_report(&reply) {
            return Ok(position);
        }
    }
}

/// Waits up to `timeout` for input on the terminal and returns whether some arrived; an
/// interrupted wait returns `false` so the caller rechecks its deadline.
///
/// macOS `poll(2)` reports `/dev/tty` as an invalid descriptor at once, so the wait goes
/// through the `filedescriptor` poll, which uses `select(2)` there and `poll(2)` elsewhere.
/// A descriptor reported in error or invalid fails the wait instead of counting as readable,
/// since reading it could block past the timeout.
fn wait_readable(tty: &File, timeout: Duration) -> io::Result<bool> {
    let mut poll = [filedescriptor::pollfd {
        fd: tty.as_raw_fd(),
        events: filedescriptor::POLLIN,
        revents: 0,
    }];
    match filedescriptor::poll(&mut poll, Some(timeout)) {
        Ok(0) => Ok(false),
        Ok(_) if poll[0].revents & filedescriptor::POLLIN != 0 => Ok(true),
        Ok(_) => Err(io::Error::other(format!(
            "terminal is not readable (poll events {:#x})",
            poll[0].revents
        ))),
        Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error))
            if error.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(error) => Err(io::Error::other(error)),
    }
}

/// Parses the last `ESC[row;columnR` report in `bytes` into a zero-based position.
fn parse_cursor_report(bytes: &[u8]) -> Option<Position> {
    let end = bytes.iter().rposition(|byte| *byte == b'R')?;
    let start = bytes[..end].windows(2).rposition(|pair| pair == b"\x1b[")?;
    let body = std::str::from_utf8(&bytes[start + 2..end]).ok()?;
    let (row, column) = body.split_once(';')?;
    let row: u16 = row.parse().ok()?;
    let column: u16 = column.parse().ok()?;
    Some(Position::new(
        column.saturating_sub(1),
        row.saturating_sub(1),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{self, Read},
        os::fd::FromRawFd,
        time::{Duration, Instant},
    };

    use ratatui::layout::Position;

    use super::{parse_cursor_report, query_cursor_position};

    #[test]
    fn cursor_query_times_out_when_the_terminal_never_answers() {
        let Some((mut master, slave)) = open_pty() else {
            return;
        };
        let timeout = Duration::from_millis(300);

        let started = Instant::now();
        let error = query_cursor_position(&slave, timeout).expect_err("no answer");
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed >= timeout && elapsed < Duration::from_secs(2),
            "returned after {elapsed:?}"
        );
        let mut sent = [0u8; 4];
        master.read_exact(&mut sent).expect("read the query");
        assert_eq!(&sent, b"\x1b[6n");
    }

    #[test]
    fn cursor_reports_parse_to_zero_based_positions() {
        assert_eq!(
            parse_cursor_report(b"\x1b[5;12R"),
            Some(Position::new(11, 4))
        );
        assert_eq!(
            parse_cursor_report(b"typed-ahead\x1b[1;1R"),
            Some(Position::new(0, 0))
        );
    }

    #[test]
    fn incomplete_or_malformed_reports_are_not_positions() {
        assert_eq!(parse_cursor_report(b"\x1b[5;1"), None);
        assert_eq!(parse_cursor_report(b"\x1b[5R"), None);
        assert_eq!(parse_cursor_report(b"\x1b[a;bR"), None);
        assert_eq!(parse_cursor_report(b"R"), None);
    }

    /// Opens a pseudo-terminal pair as (master, slave), or prints a skip notice and returns
    /// `None` when the environment provides no pseudo-terminals.
    fn open_pty() -> Option<(File, File)> {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors on success and accepts null settings.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) {
                eprintln!("SKIPPED: pseudo-terminals are unavailable in this environment: {error}");
                return None;
            }
            panic!("open pseudo-terminal: {error}");
        }
        // SAFETY: successful openpty returned fresh owned descriptors.
        Some(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
    }
}
