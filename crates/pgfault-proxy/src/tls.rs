//! Wire-level TLS: pgfault must decrypt to observe protocol content, so TLS
//! support means terminating it on both sides, not passing bytes through.
use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::{
    io,
    os::fd::{AsFd, BorrowedFd, RawFd},
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskCx, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A plain or TLS-wrapped connection. The raw fd is captured before any TLS
/// handshake so `reset()` can still force a bare TCP RST underneath it, and
/// a plain connection can be upgraded in place once SSLRequest is accepted.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<dyn Io>, RawFd),
}
struct FdRef(RawFd);
impl AsFd for FdRef {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.0) }
    }
}
impl Stream {
    pub fn plain(tcp: TcpStream) -> Self {
        Stream::Plain(tcp)
    }
    fn fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        match self {
            Stream::Plain(t) => t.as_raw_fd(),
            Stream::Tls(_, fd) => *fd,
        }
    }
    /// Force a raw TCP RST regardless of any TLS layer above it: sets
    /// SO_LINGER(0) on the underlying socket so the eventual close aborts
    /// the connection instead of running a graceful (TLS or TCP) shutdown.
    pub fn reset(&self) -> Result<()> {
        socket2::SockRef::from(&FdRef(self.fd())).set_linger(Some(std::time::Duration::ZERO))?;
        Ok(())
    }
    /// Take ownership of the underlying plain TCP stream to hand it to a TLS
    /// handshake. Panics if TLS is already established (SSLRequest is only
    /// ever valid as the first message on a fresh plaintext connection).
    pub fn into_plain(self) -> TcpStream {
        match self {
            Stream::Plain(t) => t,
            Stream::Tls(..) => unreachable!("SSLRequest after TLS already established"),
        }
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskCx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(t) => Pin::new(t).poll_read(cx, buf),
            Stream::Tls(t, _) => Pin::new(t).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskCx<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(t) => Pin::new(t).poll_write(cx, buf),
            Stream::Tls(t, _) => Pin::new(t).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(t) => Pin::new(t).poll_flush(cx),
            Stream::Tls(t, _) => Pin::new(t).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(t) => Pin::new(t).poll_shutdown(cx),
            Stream::Tls(t, _) => Pin::new(t).poll_shutdown(cx),
        }
    }
}
/// Loaded once at startup and shared across connections.
#[derive(Clone, Default)]
pub struct TlsConfig {
    pub frontend: Option<TlsAcceptor>,
    pub upstream: Option<TlsConnector>,
}
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    rustls_pemfile::certs(&mut data.as_slice())
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing certificates in {}", path.display()))
}
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .with_context(|| format!("parsing private key in {}", path.display()))?
        .context("no private key found")
}
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
pub fn frontend_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    ensure_crypto_provider();
    let certs = load_certs(cert)?;
    let key = load_key(key)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub fn upstream_connector(insecure: bool, extra_ca: Option<&Path>) -> Result<TlsConnector> {
    ensure_crypto_provider();
    let config = if insecure {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(ca) = extra_ca {
            for cert in load_certs(ca)? {
                roots.add(cert).context("adding upstream CA certificate")?;
            }
        }
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(config)))
}
pub fn server_name(host: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(host.to_string())
        .map_err(|_| anyhow::anyhow!("invalid upstream host for TLS SNI: {host}"))
}
pub async fn accept(tcp: TcpStream, acceptor: &TlsAcceptor) -> Result<Stream> {
    use std::os::fd::AsRawFd;
    let fd = tcp.as_raw_fd();
    let tls = acceptor
        .accept(tcp)
        .await
        .context("TLS handshake with client failed")?;
    Ok(Stream::Tls(Box::new(tls), fd))
}
pub async fn connect(
    tcp: TcpStream,
    connector: &TlsConnector,
    name: ServerName<'static>,
) -> Result<Stream> {
    use std::os::fd::AsRawFd;
    let fd = tcp.as_raw_fd();
    let tls = connector
        .connect(name, tcp)
        .await
        .context("TLS handshake with upstream failed")?;
    Ok(Stream::Tls(Box::new(tls), fd))
}
