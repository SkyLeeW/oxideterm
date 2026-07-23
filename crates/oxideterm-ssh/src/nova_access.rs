use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use zeroize::Zeroizing;

use crate::{WebSocketSshTunnel, websocket_tunnel::pinned_agent_tls_config};

/// NovaAgentAuthorization 保存一次连接所需的 Agent 2FA 授权输入。
/// 中文说明: 该类型只存在于运行时配置，2FA 验证码在换取 WSS token 后随对象销毁。
#[derive(Clone, PartialEq, Eq)]
pub struct NovaAgentAuthorization {
    base_url: Url,
    access_id: String,
    two_factor_code: Zeroizing<String>,
    pinned_certificate: Option<Vec<u8>>,
}

impl fmt::Debug for NovaAgentAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NovaAgentAuthorization")
            .field("base_url", &self.base_url)
            .field("access_id", &self.access_id)
            .field("two_factor_code", &"[redacted secret]")
            .finish()
    }
}

impl NovaAgentAuthorization {
    /// new 创建一次性 Agent 2FA 授权输入。
    pub fn new(
        base_url: Url,
        access_id: String,
        two_factor_code: Zeroizing<String>,
        pinned_certificate: Option<Vec<u8>>,
    ) -> Self {
        Self {
            base_url,
            access_id,
            two_factor_code,
            pinned_certificate,
        }
    }

    /// from_persisted 从保存连接的非敏感 Nova 元数据恢复本次运行时授权对象。
    pub fn from_persisted(
        base_url: &str,
        access_id: &str,
        two_factor_code: Zeroizing<String>,
        pinned_certificate: Vec<u8>,
    ) -> Result<Self, NovaSshAccessError> {
        let base_url =
            Url::parse(base_url.trim()).map_err(|_| NovaSshAccessError::InvalidPayload)?;
        if base_url.scheme() != "https"
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(NovaSshAccessError::InsecureAgentUrl);
        }
        if access_id.trim().is_empty() {
            return Err(NovaSshAccessError::MissingField);
        }
        if pinned_certificate.is_empty() {
            return Err(NovaSshAccessError::MissingPinnedCertificate);
        }
        Ok(Self::new(
            base_url,
            access_id.trim().to_string(),
            two_factor_code,
            Some(pinned_certificate),
        ))
    }

    /// request_websocket_tunnel 使用 2FA 换取一次性 WSS bridge token。
    pub async fn request_websocket_tunnel(self) -> Result<WebSocketSshTunnel, NovaSshAccessError> {
        request_websocket_tunnel(
            &self.base_url,
            &self.access_id,
            self.two_factor_code,
            self.pinned_certificate,
        )
        .await
    }

    /// connection_key_suffix 使用不可逆摘要区分 Agent 接入记录，不暴露 2FA。
    pub(crate) fn connection_key_suffix(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest as _;
        hasher.update(self.base_url.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(self.access_id.as_bytes());
        format!("|nova-agent={:x}", hasher.finalize())
    }
}

/// Nova Agent SSH 接入串的公开描述；加密私钥仅在导入期间保留。
pub struct NovaSshAccessBundle {
    pub base_url: Url,
    pub access_id: String,
    pub agent_name: Option<String>,
    pub username: String,
    pub target_port: u16,
    pub private_key: Zeroizing<String>,
    pub key_fingerprint: String,
    pub pinned_cert_sha256: Option<String>,
    pub pinned_cert_der: Option<Vec<u8>>,
}

impl fmt::Debug for NovaSshAccessBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NovaSshAccessBundle")
            .field("base_url", &self.base_url)
            .field("access_id", &self.access_id)
            .field("agent_name", &self.agent_name)
            .field("username", &self.username)
            .field("target_port", &self.target_port)
            .field("private_key", &"[redacted secret]")
            .field("key_fingerprint", &self.key_fingerprint)
            .field("pinned_cert_sha256", &self.pinned_cert_sha256)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawNovaSshAccessBundle {
    version: String,
    base_url: String,
    access_id: String,
    agent_name: Option<String>,
    username: String,
    target_port: u16,
    private_key: String,
    key_fingerprint: String,
    pinned_cert_sha256: Option<String>,
    pinned_cert_der: Option<String>,
}

#[derive(Deserialize)]
struct AgentResponse<T> {
    ret: i32,
    #[serde(default)]
    msg: String,
    data: Option<T>,
}

#[derive(Deserialize)]
struct LoginData {
    token: String,
}

#[derive(Deserialize)]
struct BridgeData {
    uri: String,
    token: String,
}

#[derive(Serialize)]
struct TwoFactorLoginRequest<'a> {
    code: &'a str,
    login_type: &'static str,
}

/// Nova Agent 接入串或授权请求失败时的安全错误；错误内容不会包含私钥、2FA 或 token。
#[derive(Debug, Error)]
pub enum NovaSshAccessError {
    #[error("Nova SSH 接入串不是有效的 Base64URL 数据")]
    InvalidBase64,
    #[error("Nova SSH 接入串格式无效")]
    InvalidPayload,
    #[error("Nova SSH 接入串版本不受支持")]
    UnsupportedVersion,
    #[error("Nova SSH 接入串必须使用 HTTPS Agent 地址")]
    InsecureAgentUrl,
    #[error("Nova SSH 接入串缺少必要字段")]
    MissingField,
    #[error("Nova SSH 接入串缺少 Agent 钉扎证书，请在 Agent 端重新签发接入串")]
    MissingPinnedCertificate,
    #[error("Nova Agent 2FA 登录失败（{endpoint}）：{reason}")]
    TwoFactorLoginFailed { endpoint: String, reason: String },
    #[error("Nova Agent SSH 桥接授权失败：{reason}")]
    BridgeAuthorizationFailed { reason: String },
    #[error("Nova Agent 返回了无效的桥接地址")]
    InvalidBridgeUrl,
    #[error("Nova Agent {stage} 请求失败（{endpoint}）：{reason}")]
    RequestFailed {
        stage: &'static str,
        endpoint: String,
        reason: String,
    },
    #[error("Nova Agent {stage} 返回的响应格式无效（HTTP {status}）")]
    InvalidAgentResponse { stage: &'static str, status: u16 },
}

impl NovaSshAccessBundle {
    /// parse 从 Agent 管理页复制的 Base64URL 接入串中读取连接描述。
    pub fn parse(access_text: &str) -> Result<Self, NovaSshAccessError> {
        let encoded = access_text.trim();
        if encoded.is_empty() {
            return Err(NovaSshAccessError::MissingField);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(encoded))
            .map_err(|_| NovaSshAccessError::InvalidBase64)?;
        let raw: RawNovaSshAccessBundle =
            serde_json::from_slice(&bytes).map_err(|_| NovaSshAccessError::InvalidPayload)?;
        if raw.version != "nova-ssh-v1" {
            return Err(NovaSshAccessError::UnsupportedVersion);
        }
        let base_url =
            Url::parse(raw.base_url.trim()).map_err(|_| NovaSshAccessError::InvalidPayload)?;
        if base_url.scheme() != "https"
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(NovaSshAccessError::InsecureAgentUrl);
        }
        if raw.access_id.trim().is_empty()
            || raw.username.trim().is_empty()
            || raw.target_port == 0
            || raw.private_key.trim().is_empty()
            || raw.key_fingerprint.trim().is_empty()
        {
            return Err(NovaSshAccessError::MissingField);
        }
        if !raw
            .private_key
            .contains("-----BEGIN OPENSSH PRIVATE KEY-----")
        {
            return Err(NovaSshAccessError::InvalidPayload);
        }
        let pinned_cert_sha256 = raw
            .pinned_cert_sha256
            .filter(|fingerprint| !fingerprint.trim().is_empty());
        let pinned_cert_der =
            decode_pinned_certificate(raw.pinned_cert_der, pinned_cert_sha256.as_deref())?;
        if pinned_cert_der.is_none() {
            return Err(NovaSshAccessError::MissingPinnedCertificate);
        }
        Ok(Self {
            base_url,
            access_id: raw.access_id.trim().to_owned(),
            agent_name: raw.agent_name.filter(|name| !name.trim().is_empty()),
            username: raw.username.trim().to_owned(),
            target_port: raw.target_port,
            private_key: Zeroizing::new(raw.private_key),
            key_fingerprint: raw.key_fingerprint.trim().to_owned(),
            pinned_cert_sha256,
            pinned_cert_der,
        })
    }

    /// request_websocket_tunnel 使用一次 Agent 2FA 登录换取仅内存存活的 WSS 桥接 token。
    pub async fn request_websocket_tunnel(
        &self,
        two_factor_code: Zeroizing<String>,
    ) -> Result<WebSocketSshTunnel, NovaSshAccessError> {
        request_websocket_tunnel(
            &self.base_url,
            &self.access_id,
            two_factor_code,
            self.pinned_cert_der.clone(),
        )
        .await
    }
}

/// request_websocket_tunnel 在 Agent 2FA 成功后请求绑定接入记录的一次性 WSS 凭据。
async fn request_websocket_tunnel(
    base_url: &Url,
    access_id: &str,
    two_factor_code: Zeroizing<String>,
    pinned_certificate: Option<Vec<u8>>,
) -> Result<WebSocketSshTunnel, NovaSshAccessError> {
    let certificate = pinned_certificate.ok_or(NovaSshAccessError::MissingPinnedCertificate)?;
    let endpoint = display_agent_endpoint(base_url);
    let tls_config = pinned_agent_tls_config(certificate.clone())
        .map_err(|_| NovaSshAccessError::InvalidPayload)?;
    let client_builder = reqwest::Client::builder()
        .https_only(true)
        .use_preconfigured_tls(tls_config);
    let client = client_builder
        .build()
        .map_err(|_| NovaSshAccessError::RequestFailed {
            stage: "HTTPS 客户端初始化",
            endpoint: endpoint.clone(),
            reason: "TLS 配置无效".to_string(),
        })?;
    let login_url = agent_endpoint(base_url, "admin/login");
    let login_response = client
        .post(login_url)
        .json(&TwoFactorLoginRequest {
            code: two_factor_code.as_str(),
            login_type: "two_factor",
        })
        .send()
        .await
        .map_err(|error| request_failure("2FA 登录", &endpoint, &error))?;
    let login_status = login_response.status();
    let login: AgentResponse<LoginData> =
        login_response
            .json()
            .await
            .map_err(|_| NovaSshAccessError::InvalidAgentResponse {
                stage: "2FA 登录",
                status: login_status.as_u16(),
            })?;
    let session_token = login
        .data
        .filter(|_| login_status.is_success() && login.ret == 200)
        .map(|data| Zeroizing::new(data.token))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| NovaSshAccessError::TwoFactorLoginFailed {
            endpoint: endpoint.clone(),
            reason: agent_failure_reason(login_status, login.ret, &login.msg),
        })?;

    let bridge_url = agent_endpoint(
        base_url,
        &format!("admin/ssh-accesses/{access_id}/bridge-uri"),
    );
    let mut headers = HeaderMap::new();
    let header_value = HeaderValue::from_str(session_token.as_str()).map_err(|_| {
        NovaSshAccessError::BridgeAuthorizationFailed {
            reason: "Agent 返回的会话凭据格式无效".to_string(),
        }
    })?;
    headers.insert("jy-token", header_value);
    let bridge_response = client
        .get(bridge_url)
        .headers(headers)
        .send()
        .await
        .map_err(|error| request_failure("SSH 桥接授权", &endpoint, &error))?;
    let bridge_status = bridge_response.status();
    let bridge: AgentResponse<BridgeData> =
        bridge_response
            .json()
            .await
            .map_err(|_| NovaSshAccessError::InvalidAgentResponse {
                stage: "SSH 桥接授权",
                status: bridge_status.as_u16(),
            })?;
    let bridge_data = bridge
        .data
        .filter(|_| bridge_status.is_success() && bridge.ret == 200)
        .filter(|data| !data.uri.trim().is_empty() && !data.token.trim().is_empty())
        .ok_or_else(|| NovaSshAccessError::BridgeAuthorizationFailed {
            reason: agent_failure_reason(bridge_status, bridge.ret, &bridge.msg),
        })?;
    WebSocketSshTunnel::new(bridge_data.uri, Zeroizing::new(bridge_data.token))
        .map(|tunnel| tunnel.with_pinned_certificate(Some(certificate)))
        .map_err(|_| NovaSshAccessError::InvalidBridgeUrl)
}

/// 将传输层错误归类为用户可处理的公开原因，避免把请求地址或授权数据写入界面。
fn request_failure(
    stage: &'static str,
    endpoint: &str,
    error: &reqwest::Error,
) -> NovaSshAccessError {
    let reason = if error.is_timeout() {
        "连接超时".to_string()
    } else if error.is_connect() {
        "无法连接 Agent，或 TLS 证书校验失败".to_string()
    } else {
        "网络请求未完成".to_string()
    };
    NovaSshAccessError::RequestFailed {
        stage,
        endpoint: endpoint.to_string(),
        reason,
    }
}

/// 连接错误只展示经过接入串校验的 Agent 基础地址，不包含访问记录或任何授权参数。
fn display_agent_endpoint(base_url: &Url) -> String {
    base_url.as_str().trim_end_matches('/').to_string()
}

/// 只采用 Agent 统一响应中的公开 msg 字段，补足 HTTP 与业务状态便于定位服务端问题。
fn agent_failure_reason(status: reqwest::StatusCode, ret: i32, message: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        format!("HTTP {}，业务码 {ret}", status.as_u16())
    } else {
        format!("{message}（HTTP {}，业务码 {ret}）", status.as_u16())
    }
}

fn decode_pinned_certificate(
    value: Option<String>,
    fingerprint: Option<&str>,
) -> Result<Option<Vec<u8>>, NovaSshAccessError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let certificate = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(value.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(value.trim()))
        .map_err(|_| NovaSshAccessError::InvalidPayload)?;
    use sha2::Digest as _;
    let digest = format!("{:X}", sha2::Sha256::digest(&certificate));
    if fingerprint.is_none_or(|expected| expected.replace(':', "").to_uppercase() != digest) {
        return Err(NovaSshAccessError::InvalidPayload);
    }
    Ok(Some(certificate))
}

/// agent_endpoint 在受验证的 Agent 根地址下拼接固定 API 路径。
fn agent_endpoint(base_url: &Url, path: &str) -> Url {
    let mut url = base_url.clone();
    let base_path = base_url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}/{path}"));
    url.set_query(None);
    url.set_fragment(None);
    url
}

#[cfg(test)]
mod tests {
    use super::{AgentResponse, NovaSshAccessBundle, agent_failure_reason};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    fn access_text(base_url: &str) -> String {
        use sha2::Digest as _;

        let certificate = base64::engine::general_purpose::STANDARD_NO_PAD.encode(b"test-cert");
        let fingerprint = format!("{:X}", sha2::Sha256::digest(b"test-cert"));
        URL_SAFE_NO_PAD.encode(format!(
            r#"{{"version":"nova-ssh-v1","base_url":"{base_url}","access_id":"access-1","username":"root","target_port":22,"private_key":"-----BEGIN OPENSSH PRIVATE KEY-----\\nprivate\\n-----END OPENSSH PRIVATE KEY-----","key_fingerprint":"SHA256:test","pinned_cert_sha256":"{fingerprint}","pinned_cert_der":"{certificate}"}}"#
        ))
    }

    #[test]
    fn parses_https_access_bundle_without_exposing_private_key_in_debug() {
        let bundle =
            NovaSshAccessBundle::parse(&access_text("https://agent.example:28443")).unwrap();
        assert_eq!(bundle.username, "root");
        assert_eq!(bundle.target_port, 22);
        assert!(!format!("{bundle:?}").contains("private\\n"));
    }

    #[test]
    fn rejects_insecure_or_secret_bearing_agent_url() {
        assert!(NovaSshAccessBundle::parse(&access_text("http://agent.example")).is_err());
        assert!(
            NovaSshAccessBundle::parse(&access_text("https://user:pass@agent.example")).is_err()
        );
        assert!(NovaSshAccessBundle::parse(&access_text("https://agent.example?token=x")).is_err());
    }

    #[test]
    fn agent_error_reason_includes_public_message_and_status() {
        let response = AgentResponse::<()> {
            ret: 401,
            msg: "验证码错误".to_string(),
            data: None,
        };
        assert_eq!(
            agent_failure_reason(
                reqwest::StatusCode::BAD_REQUEST,
                response.ret,
                &response.msg
            ),
            "验证码错误（HTTP 400，业务码 401）"
        );
    }
}
