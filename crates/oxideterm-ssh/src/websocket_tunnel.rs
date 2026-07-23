use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::{
    io::{
        AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, WriteHalf,
        duplex,
    },
    net::TcpStream,
    sync::{Mutex, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async, connect_async_tls_with_config,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use url::Url;
use zeroize::Zeroizing;

use crate::SshTransportError;

const BRIDGE_BUFFER_BYTES: usize = 256 * 1024;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 1024 * 1024;

type WssSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 精确钉扎的 TLS 验证器只接受接入串携带的服务端证书，不依赖公网入口的主机名匹配。
struct PinnedCertificateVerifier {
    certificate: Vec<u8>,
    roots: rustls::RootCertStore,
    signature_algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedCertificateVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedCertificateVerifier")
            .field("certificate", &"[pinned certificate]")
            .finish()
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() != self.certificate.as_slice() {
            return Err(rustls::Error::General(
                "Agent 服务端证书与接入串钉扎证书不一致".to_string(),
            ));
        }
        let certificate = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &certificate,
            &self.roots,
            intermediates,
            now,
            self.signature_algorithms.all,
        )?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.signature_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.signature_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.signature_algorithms.supported_schemes()
    }
}

/// 为 HTTPS 与 WSS 共用的 Agent 证书创建精确钉扎 TLS 配置。
pub(crate) fn pinned_agent_tls_config(
    certificate: Vec<u8>,
) -> Result<rustls::ClientConfig, SshTransportError> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(certificate.clone()))
        .map_err(|_| {
            SshTransportError::ConnectionFailed("Nova Agent 钉扎证书格式无效".to_string())
        })?;
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let verifier = PinnedCertificateVerifier {
        certificate,
        roots,
        signature_algorithms: provider.signature_verification_algorithms,
    };
    // 显式指定算法提供方，避免依赖树同时启用 ring 与 AWS-LC 时预检线程发生 panic。
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|_| {
            SshTransportError::ConnectionFailed("Nova Agent TLS 协议配置无效".to_string())
        })?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(config)
}

/// SSH-over-WSS 的运行时连接参数。
///
/// 中文说明: 授权值仅存在于活动连接配置，不能进入保存连接、调试输出或连接池标识。
#[derive(Clone, PartialEq, Eq)]
pub struct WebSocketSshTunnel {
    endpoint: String,
    authorization: Zeroizing<String>,
    pinned_certificate: Option<Vec<u8>>,
}

impl WebSocketSshTunnel {
    pub fn new(
        endpoint: impl Into<String>,
        authorization: Zeroizing<String>,
    ) -> Result<Self, SshTransportError> {
        let endpoint = normalize_endpoint(endpoint.into())?;
        if authorization.trim().is_empty() {
            return Err(SshTransportError::ConnectionFailed(
                "SSH-over-WSS authorization is empty".to_string(),
            ));
        }
        Ok(Self {
            endpoint,
            authorization,
            pinned_certificate: None,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn with_pinned_certificate(mut self, certificate: Option<Vec<u8>>) -> Self {
        self.pinned_certificate = certificate;
        self
    }

    pub(crate) fn connection_key_suffix(&self) -> String {
        let digest = Sha256::digest(self.endpoint.as_bytes());
        format!("|websocket-tunnel={digest:x}")
    }

    pub async fn connect(&self) -> Result<WssSshStream, SshTransportError> {
        let mut request = self
            .endpoint
            .clone()
            .into_client_request()
            .map_err(|error| {
                SshTransportError::ConnectionFailed(format!(
                    "invalid SSH-over-WSS request: {error}"
                ))
            })?;
        // 中文说明: HTTP Header 内的临时副本仅用于本次握手，原始授权值始终由 Zeroizing 持有。
        let mut header_bytes = Zeroizing::new(Vec::with_capacity(self.authorization.len() + 7));
        header_bytes.extend_from_slice(b"Bearer ");
        header_bytes.extend_from_slice(self.authorization.as_bytes());
        let header_value = HeaderValue::from_bytes(header_bytes.as_slice()).map_err(|error| {
            SshTransportError::ConnectionFailed(format!(
                "invalid SSH-over-WSS authorization: {error}"
            ))
        })?;
        request.headers_mut().insert(AUTHORIZATION, header_value);

        let connector = self
            .pinned_certificate
            .as_ref()
            .map(|certificate| pinned_agent_tls_config(certificate.clone()))
            .transpose()?
            .map(|config| Connector::Rustls(Arc::new(config)));
        let (socket, _) = if let Some(connector) = connector {
            connect_async_tls_with_config(request, None, false, Some(connector)).await
        } else {
            connect_async(request).await
        }
        .map_err(|error| {
            SshTransportError::ConnectionFailed(format!("SSH-over-WSS connection failed: {error}"))
        })?;
        Ok(WssSshStream::new(socket))
    }
}

impl fmt::Debug for WebSocketSshTunnel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketSshTunnel")
            .field("endpoint", &self.endpoint)
            .field("authorization", &"[redacted secret]")
            .finish()
    }
}

/// 供 russh 消费的连续 SSH 字节流。
///
/// 中文说明: WebSocket 帧边界仅在桥接任务内部存在，SSH 层始终看到连续的全双工字节流。
pub struct WssSshStream {
    stream: DuplexStream,
    _lifetime: Arc<WssBridgeLifetime>,
}

impl WssSshStream {
    fn new(socket: WssSocket) -> Self {
        let (stream, bridge_stream) = duplex(BRIDGE_BUFFER_BYTES);
        let (socket_write, socket_read) = socket.split();
        let (bridge_read, bridge_write) = tokio::io::split(bridge_stream);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let reader = tokio::spawn(copy_websocket_to_ssh(
            socket_read,
            bridge_write,
            shutdown_rx.clone(),
            shutdown_tx.clone(),
        ));
        let writer = tokio::spawn(copy_ssh_to_websocket(
            bridge_read,
            socket_write,
            shutdown_rx,
            shutdown_tx.clone(),
        ));
        let lifetime = Arc::new(WssBridgeLifetime {
            shutdown: shutdown_tx,
            workers: Mutex::new(vec![reader, writer]),
        });
        Self {
            stream,
            _lifetime: lifetime,
        }
    }
}

impl AsyncRead for WssSshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for WssSshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct WssBridgeLifetime {
    shutdown: watch::Sender<bool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for WssBridgeLifetime {
    fn drop(&mut self) {
        // 中文说明: russh 丢弃底层流时必须同步取消桥接任务，避免后台连接脱离节点生命周期。
        let _ = self.shutdown.send(true);
        if let Ok(mut workers) = self.workers.try_lock() {
            for worker in workers.drain(..) {
                worker.abort();
            }
        }
    }
}

async fn copy_websocket_to_ssh(
    mut socket: futures_util::stream::SplitStream<WssSocket>,
    mut ssh: WriteHalf<DuplexStream>,
    mut shutdown: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            message = socket.next() => {
                let Some(message) = message else {
                    break;
                };
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Binary(bytes) => {
                        if bytes.len() > MAX_WEBSOCKET_MESSAGE_BYTES || ssh.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Text(_) => break,
                }
            }
        }
    }
    let _ = ssh.shutdown().await;
    let _ = shutdown_tx.send(true);
}

async fn copy_ssh_to_websocket(
    mut ssh: tokio::io::ReadHalf<DuplexStream>,
    mut socket: futures_util::stream::SplitSink<WssSocket, Message>,
    mut shutdown: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut buffer = vec![0; 32 * 1024];
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            read = ssh.read(&mut buffer) => {
                let Ok(read) = read else {
                    break;
                };
                if read == 0 || socket.send(Message::Binary(buffer[..read].to_vec().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = socket.close().await;
    let _ = shutdown_tx.send(true);
}

fn normalize_endpoint(endpoint: String) -> Result<String, SshTransportError> {
    let url = Url::parse(endpoint.trim()).map_err(|error| {
        SshTransportError::ConnectionFailed(format!("invalid SSH-over-WSS endpoint: {error}"))
    })?;
    if !matches!(url.scheme(), "ws" | "wss")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SshTransportError::ConnectionFailed(
            "SSH-over-WSS endpoint must be an absolute ws/wss URL without embedded credentials or query data"
                .to_string(),
        ));
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_rejects_secret_bearing_url_parts() {
        assert!(
            WebSocketSshTunnel::new(
                "wss://agent.example/ws/ssh?token=secret",
                Zeroizing::new("session".to_string()),
            )
            .is_err()
        );
        assert!(
            WebSocketSshTunnel::new(
                "https://agent.example/ws/ssh",
                Zeroizing::new("session".to_string()),
            )
            .is_err()
        );
    }

    #[test]
    fn endpoint_pool_key_redacts_authorization() {
        let tunnel = WebSocketSshTunnel::new(
            "wss://agent.example/ws/ssh",
            Zeroizing::new("session-secret".to_string()),
        )
        .unwrap();
        assert!(!format!("{tunnel:?}").contains("session-secret"));
        assert!(!tunnel.connection_key_suffix().contains("session-secret"));
    }
}
