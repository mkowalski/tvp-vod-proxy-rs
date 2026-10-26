//! Control only the accepted HTTP/1 socket; an owned duplicate prevents fd reuse.
use axum::{extract::connect_info::Connected, serve::IncomingStream};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    sync::Arc,
};
use tokio::net::TcpListener;

#[derive(Clone)]
pub(crate) struct Connection(pub(crate) Option<Arc<OwnedFd>>);

impl Connected<IncomingStream<'_, TcpListener>> for Connection {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self(stream.io().as_fd().try_clone_to_owned().ok().map(Arc::new))
    }
}

impl Connection {
    pub(crate) fn abort(&self) -> io::Result<()> {
        let socket = self
            .0
            .as_ref()
            .ok_or_else(|| io::Error::other("missing connection control"))?;
        let fd = socket.as_raw_fd();
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // Linger zero discards pending output on close; shutdown wakes Hyper so
        // its body and original descriptor cannot remain stuck behind a writer.
        let reset = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of_val(&linger) as libc::socklen_t,
            )
        };
        let error = (reset != 0).then(io::Error::last_os_error);
        if unsafe { libc::shutdown(fd, libc::SHUT_RDWR) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOTCONN) {
                return Err(error);
            }
        }
        error.map_or(Ok(()), Err)
    }
}
