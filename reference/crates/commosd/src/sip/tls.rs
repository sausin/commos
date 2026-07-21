//! SIP-over-TLS ingress (Volume 7; SIPS, RFC 3261) — feature-gated behind `tls`.
//!
//! Encrypting the signalling channel protects the SDES SRTP keys ([`super::sdes`]) and every
//! header — who is calling whom — from a passive observer, so it pairs naturally with the SRTP
//! media encryption. TLS is a stream transport: messages have no datagram boundaries and a reply
//! must return on the same connection, so this listener frames the byte stream with
//! [`StreamFramer`] and answers through a [`Responder::Stream`]. The request-handling logic is
//! unchanged — [`SipServer::handle`] serves a TLS-framed message exactly as a UDP datagram.
//!
//! **Crypto backend.** This uses rustls with the **ring** provider, selected explicitly (never
//! rustls' `aws-lc-rs` default, which needs cmake/C/NASM). Like the `s3` feature, `tls` therefore
//! introduces ring's C/asm into a `--features tls` build only; the default binary stays pure-Rust
//! and cross-compiles clean. When the pure-Rust rustls-rustcrypto provider stabilises, swapping it
//! in is a one-line provider change.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use super::server::SipServer;
use super::transport::{Frame, Responder, StreamFramer};

/// How many bytes to read from the TLS stream per syscall (a SIP message is small; this only
/// bounds the read chunk, not the message size — the framer enforces that).
const READ_CHUNK: usize = 4096;
/// Depth of the per-connection writer queue — provisional/final responses, MWI, bridge answers.
const WRITER_QUEUE: usize = 32;

/// Build a rustls [`ServerConfig`] from a PEM certificate chain and private key, using the ring
/// crypto provider explicitly. Errors if the PEM is malformed or carries no key.
pub fn load_server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<ServerConfig> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("SIP TLS certificate PEM contained no certificates");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])?
        .ok_or_else(|| anyhow::anyhow!("SIP TLS key PEM contained no private key"))?;

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(config)
}

/// Bind `bind` and serve SIP over TLS forever, dispatching each framed message through `server`.
/// Returns only on a fatal listener error.
pub async fn run_tls(
    server: Arc<SipServer>,
    bind: SocketAddr,
    config: Arc<ServerConfig>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let acceptor = TlsAcceptor::from(config);
    tracing::info!(addr = %bind, "SIP signalling ingress listening (TLS)");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "SIP/TLS accept error; continuing");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let server = server.clone();
        // One task per connection: handshake, then read/frame/dispatch until the peer closes.
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!(error = %e, %peer, "SIP/TLS handshake failed");
                    return;
                }
            };
            if let Err(e) = serve_connection(server, tls, peer).await {
                tracing::debug!(error = %e, %peer, "SIP/TLS connection ended");
            }
        });
    }
}

/// Serve one accepted TLS connection: a writer task drains the reply queue onto the socket while
/// the read loop feeds the framer and dispatches each complete message.
async fn serve_connection(
    server: Arc<SipServer>,
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    peer: SocketAddr,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(WRITER_QUEUE);

    // Single writer task so concurrent replies (100 Trying, a bridge's 200 OK, an MWI NOTIFY)
    // serialise onto the one connection.
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let mut framer = StreamFramer::new();
    let mut buf = [0u8; READ_CHUNK];
    'read: loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break; // peer closed the connection
        }
        framer.push(&buf[..n]);
        loop {
            match framer.next_message() {
                Frame::Message(m) => {
                    let responder = Responder::Stream { tx: tx.clone(), peer };
                    if let Err(e) = server.handle(&responder, &m).await {
                        tracing::debug!(error = %e, %peer, "dropping SIP/TLS message");
                    }
                }
                Frame::Incomplete => break,
                Frame::Overflow => {
                    tracing::debug!(%peer, "SIP/TLS message exceeded the size limit; closing");
                    break 'read;
                }
            }
        }
    }

    drop(tx); // signal the writer task to finish
    let _ = writer_task.await;
    Ok(())
}
