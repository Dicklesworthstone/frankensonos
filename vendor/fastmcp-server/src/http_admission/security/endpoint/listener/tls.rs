//! Native HTTPS admission and owned duplex I/O for the secured listener.
//!
//! A TLS handshake owns its socket and connection permit until success or
//! refusal. Timeout polling retains ONE handshake future; it never retries a
//! partially completed handshake or falls back to plaintext. Established TLS
//! enters the same HTTP authentication, dispatch and response owners as TCP.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::{OwnedReadHalf, OwnedWriteHalf, TcpStream};
use asupersync::tls::{TlsAcceptor, TlsStream};
use asupersync::types::Time;
use fastmcp_core::{McpError, McpResult};

use crate::{HTTP_ACCEPT_CANCEL_POLL, HttpListenerShutdown};

/// This listener speaks only HTTP/1.1, and MCP POST effects are not replay-safe.
/// Validate the actual immutable TLS configuration before binding a socket.
pub(super) fn validate_acceptor(acceptor: &TlsAcceptor) -> McpResult<()> {
    let config = acceptor.config();
    if config.alpn_protocols.len() != 1
        || config.alpn_protocols[0].as_slice() != b"http/1.1"
    {
        return Err(McpError::invalid_request(
            "secured HTTPS requires an HTTP/1.1-only TLS acceptor",
        ));
    }
    if config.max_early_data_size != 0 {
        return Err(McpError::invalid_request(
            "secured HTTPS does not accept replayable TLS early data",
        ));
    }
    Ok(())
}

pub(super) async fn accept(
    cx: &Cx,
    shutdown: &HttpListenerShutdown,
    stream: TcpStream,
    acceptor: &TlsAcceptor,
    timeout: Duration,
) -> Option<ConnectionIo> {
    if shutdown.is_requested() || cx.checkpoint().is_err() || cx.timer_driver().is_none() {
        return None;
    }
    let nanos = u64::try_from(timeout.as_nanos()).ok()?;
    let local_deadline = Time::from_nanos(cx.now().as_nanos().checked_add(nanos)?);
    let deadline = cx.budget().deadline.map_or(local_deadline, |caller| caller.min(local_deadline));
    let mut handshake = std::pin::pin!(acceptor.accept(stream));
    loop {
        let now = cx.now();
        if shutdown.is_requested() || cx.checkpoint().is_err() || now >= deadline {
            return None;
        }
        let remaining = Duration::from_nanos(deadline.as_nanos() - now.as_nanos());
        match asupersync::time::timeout(
            now, remaining.min(HTTP_ACCEPT_CANCEL_POLL), handshake.as_mut(),
        ).await {
            Ok(Ok(stream)) => {
                // Do not admit a success that raced cancellation or its deadline.
                // Absence of ALPN retains HTTP/1.1's ordinary TLS default. An
                // acceptor requiring ALPN has already rejected that case itself.
                if shutdown.is_requested() || cx.checkpoint().is_err() || cx.now() >= deadline
                    || stream.alpn_protocol().is_some_and(|protocol| protocol != b"http/1.1")
                {
                    return None;
                }
                return Some(ConnectionIo::Tls(Arc::new(Mutex::new(stream))));
            }
            Ok(Err(_)) => return None,
            Err(_) => {}
        }
    }
}

type SharedTls = Arc<Mutex<TlsStream<TcpStream>>>;

/// Plain TCP keeps its native owned halves. TLS halves must share the TLS
/// record state, not split off the underlying TCP stream and discard encryption.
/// No I/O worker, forwarding socket or private runtime is created.
pub(super) enum ConnectionIo {
    Plain(TcpStream),
    Tls(SharedTls),
}

impl ConnectionIo {
    pub(super) fn into_split(self) -> (ConnectionRead, ConnectionWrite) {
        match self {
            Self::Plain(stream) => {
                let (reader, writer) = stream.into_split();
                (ConnectionRead::Plain(reader), ConnectionWrite::Plain(writer))
            }
            Self::Tls(stream) => (
                ConnectionRead::Tls(Arc::clone(&stream)),
                ConnectionWrite::Tls(stream),
            ),
        }
    }

    /// The connection task retains this handle only until its response owner
    /// has returned, then attempts bounded TLS close_notify. Abandonment drops
    /// it along with the task; there is no detached cleanup or retained socket.
    pub(super) fn tls_close_handle(&self) -> Option<ConnectionWrite> {
        match self {
            Self::Plain(_) => None,
            Self::Tls(stream) => Some(ConnectionWrite::Tls(Arc::clone(stream))),
        }
    }
}

pub(super) enum ConnectionRead {
    Plain(OwnedReadHalf),
    Tls(SharedTls),
}

pub(super) enum ConnectionWrite {
    Plain(OwnedWriteHalf),
    Tls(SharedTls),
}

// The two halves are private to ONE connection task. A lock covers exactly one
// nonblocking poll and is released before Pending: it is never held across an
// await, so a pending peer read cannot block the response writer. Mutex, rather
// than the runtime's RefCell-based borrowing split, keeps that task Send.
// Poisoned TLS state is refused, never recovered and reused for cryptography.
fn poll_tls<T>(
    stream: &SharedTls,
    poll: impl FnOnce(Pin<&mut TlsStream<TcpStream>>) -> Poll<io::Result<T>>,
) -> Poll<io::Result<T>> {
    match stream.lock() {
        Ok(mut stream) => poll(Pin::new(&mut *stream)),
        Err(_) => Poll::Ready(Err(io::Error::other("TLS connection state unavailable"))),
    }
}

impl AsyncRead for ConnectionIo {
    fn poll_read(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_read(cx, buf)),
        }
    }
}

impl AsyncRead for ConnectionRead {
    fn poll_read(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_read(cx, buf)),
        }
    }
}

impl AsyncWrite for ConnectionIo {
    fn poll_write(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_write(cx, buf)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_flush(cx)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_shutdown(cx)),
        }
    }
}

impl AsyncWrite for ConnectionWrite {
    fn poll_write(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_write(cx, buf)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_flush(cx)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => poll_tls(stream, |stream| stream.poll_shutdown(cx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::tls::{CertificateChain, PrivateKey, TlsAcceptorBuilder};

    // TEST ONLY localhost identity, shared with the existing native OAuth
    // fixtures. Inline PEM survives RCH's deliberate *.pem exclusion.
    const LEAF: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIBjjCCATSgAwIBAgICA+owCgYIKoZIzj0EAwIwJzElMCMGA1UEAwwcRmFzdE1D\nUCBPQXV0aCBURVNUIE9OTFkgUm9vdDAeFw0yMDAxMDEwMDAwMDBaFw00OTEyMzEw\nMDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49\nAwEHA0IABPPKylLna9VpWAlpshHBhSsQHNOv3BaEGX4HSBhHiBVel0ce+qfHF15O\n0T63Zlp7TtxlMdEY+rPpgioSFDQVadijYzBhMAwGA1UdEwEB/wQCMAAwLAYDVR0R\nBCUwI4IJbG9jYWxob3N0hwR/AAABhxAAAAAAAAAAAAAAAAAAAAABMBMGA1UdJQQM\nMAoGCCsGAQUFBwMBMA4GA1UdDwEB/wQEAwIHgDAKBggqhkjOPQQDAgNIADBFAiEA\n6qrAr2qp/t6K62T9Et2mUU/zfd4kJb+ekyoAim1yTFcCICb6SdVY2fg15/SXf0vE\nIvYelqtTk8FQInCEcIxvfF3m\n-----END CERTIFICATE-----\n";
    const KEY: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgcCe44IBKhbw+D/s7\nBjDHOOV0g+EoxFno7VJGKhJeer2hRANCAATzyspS52vVaVgJabIRwYUrEBzTr9wW\nhBl+B0gYR4gVXpdHHvqnxxdeTtE+t2Zae07cZTHRGPqz6YIqEhQ0FWnY\n-----END PRIVATE KEY-----\n";

    fn acceptor(protocols: Vec<Vec<u8>>) -> TlsAcceptor {
        TlsAcceptorBuilder::new(
            CertificateChain::from_pem(LEAF).unwrap(), PrivateKey::from_pem(KEY).unwrap(),
        ).alpn_protocols(protocols).build().unwrap()
    }

    #[test]
    fn secured_https_requires_http11_only_before_listening() {
        assert!(validate_acceptor(&acceptor(vec![b"http/1.1".to_vec()])).is_ok());
        for protocols in [
            vec![], vec![b"h2".to_vec()], vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            vec![b"http/1.1".to_vec(), b"http/1.1".to_vec()],
        ] {
            assert!(validate_acceptor(&acceptor(protocols)).is_err());
        }
    }

    #[test]
    fn secured_https_refuses_early_data_even_on_a_raw_acceptor() {
        let baseline = acceptor(vec![b"http/1.1".to_vec()]);
        assert!(validate_acceptor(&baseline).is_ok());
        let mut config = baseline.config().as_ref().clone();
        config.max_early_data_size = 1;
        assert!(validate_acceptor(&TlsAcceptor::new(config)).is_err());
        assert_eq!(baseline.config().max_early_data_size, 0);
    }

    #[test]
    fn secured_https_connection_and_owned_halves_are_send() {
        fn send<T: Send + Unpin>() {}
        send::<ConnectionIo>();
        send::<ConnectionRead>();
        send::<ConnectionWrite>();
    }
}
