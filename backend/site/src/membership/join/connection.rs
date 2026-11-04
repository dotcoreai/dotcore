//! The join host's TCP connections: how many may be open at once, when an idle one is closed,
//! and the identity each request carries so a commit can be bound to the connection that
//! received its offer.
//!
//! A connection with no request in progress closes after the idle timeout; a request such as an
//! operator review pauses that timeout. Closing a connection is reported so the host can drop
//! whatever join it held.

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::{Instant, Sleep},
};
use tonic::transport::server::Connected;

/// Accepts connections up to a fixed number and reports each one that closes.
pub(super) struct JoinConnections {
    max_open: usize,
    idle_timeout: Duration,
    next_id: AtomicU64,
    open: AtomicUsize,
    on_close: Box<dyn Fn(u64) + Send + Sync>,
}

impl JoinConnections {
    pub(super) fn new(
        max_open: usize,
        idle_timeout: Duration,
        on_close: impl Fn(u64) + Send + Sync + 'static,
    ) -> Self {
        Self {
            max_open,
            idle_timeout,
            next_id: AtomicU64::new(0),
            open: AtomicUsize::new(0),
            on_close: Box::new(on_close),
        }
    }

    /// Wraps an accepted stream, or refuses it when the limit of open connections is reached.
    /// Refusals and streams that closed before acceptance use an error kind the listener skips.
    pub(super) fn accept(self: &Arc<Self>, stream: TcpStream) -> io::Result<JoinConnectionIo> {
        if self.open.fetch_add(1, Ordering::SeqCst) >= self.max_open {
            self.open.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "too many open join connections",
            ));
        }
        // From here the wrapper owns the open-connection count and gives it back when dropped.
        let peer_address = stream.peer_addr();
        let now = Instant::now();
        let io = JoinConnectionIo {
            info: JoinConnectionInfo {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                peer_address: peer_address
                    .as_ref()
                    .copied()
                    .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
                activity: Arc::new(Mutex::new(ConnectionActivity {
                    requests: 0,
                    idle_since: now,
                })),
            },
            stream,
            last_transfer: now,
            idle: Box::pin(tokio::time::sleep_until(now + self.idle_timeout)),
            connections: Arc::clone(self),
        };
        peer_address
            .map(|_| io)
            .map_err(|source| io::Error::new(io::ErrorKind::ConnectionAborted, source))
    }
}

/// Connection identity tonic attaches to each request.
#[derive(Clone)]
pub(super) struct JoinConnectionInfo {
    pub(super) id: u64,
    pub(super) peer_address: SocketAddr,
    activity: Arc<Mutex<ConnectionActivity>>,
}

impl JoinConnectionInfo {
    /// Pauses the idle timeout until the returned guard drops.
    pub(super) fn begin_request(&self) -> RequestInProgress {
        self.lock_activity().requests += 1;
        RequestInProgress(Arc::clone(&self.activity))
    }

    /// Returns when the connection last became idle, or `None` while a request is running.
    fn idle_since(&self) -> Option<Instant> {
        let activity = self.lock_activity();
        (activity.requests == 0).then_some(activity.idle_since)
    }

    fn lock_activity(&self) -> MutexGuard<'_, ConnectionActivity> {
        self.activity.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps the idle timeout paused while a request, such as an operator review, is running.
pub(super) struct RequestInProgress(Arc<Mutex<ConnectionActivity>>);

impl Drop for RequestInProgress {
    fn drop(&mut self) {
        let mut activity = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        activity.requests -= 1;
        if activity.requests == 0 {
            activity.idle_since = Instant::now();
        }
    }
}

/// One accepted TCP connection with its idle timeout.
pub(super) struct JoinConnectionIo {
    stream: TcpStream,
    info: JoinConnectionInfo,
    last_transfer: Instant,
    idle: Pin<Box<Sleep>>,
    connections: Arc<JoinConnections>,
}

impl JoinConnectionIo {
    /// Polls the idle deadline, counted from the later of the last transfer and the end of the
    /// last request. While a request runs the deadline is not armed.
    fn poll_idle(&mut self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(idle_since) = self.info.idle_since() else {
            return Poll::Pending;
        };
        let deadline = idle_since.max(self.last_transfer) + self.connections.idle_timeout;
        if self.idle.deadline() != deadline {
            self.idle.as_mut().reset(deadline);
        }
        match self.idle.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "join connection was idle too long",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Connected for JoinConnectionIo {
    type ConnectInfo = JoinConnectionInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl AsyncRead for JoinConnectionIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.stream).poll_read(context, buffer) {
            Poll::Ready(result) => {
                this.last_transfer = Instant::now();
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_idle(context),
        }
    }
}

impl AsyncWrite for JoinConnectionIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_write(context, buffer);
        if result.is_ready() {
            this.last_transfer = Instant::now();
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }
}

impl Drop for JoinConnectionIo {
    fn drop(&mut self) {
        self.connections.open.fetch_sub(1, Ordering::SeqCst);
        (self.connections.on_close)(self.info.id);
    }
}

struct ConnectionActivity {
    requests: usize,
    idle_since: Instant,
}
