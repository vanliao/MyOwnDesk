//! 客户端网络层——管理与中继服务器的 QUIC 连接。
//!
//! 提供 datagram 和 stream 两种通道的收发能力，供服务模式和 GUI 模式共同使用。
//!
//! # 架构
//!
//! ```text
//! QuicClient
//!   ├── connect(server_addr, device_id, psk) → 建立 QUIC 连接
//!   ├── register() → 发送 Register 消息，完成 HMAC 认证
//!   ├── send_datagram(data) → 发送视频帧（不可靠）
//!   ├── recv_datagram() → 接收视频帧
//!   ├── send_message(msg) → 发送控制消息（可靠）
//!   └── recv_message() → 接收控制消息
//! ```

use myowndesk_protocol as proto;
use prost::Message as _;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

// ============================================================
// Channel 类型别名
// ============================================================

/// KeyFrame 请求信号——网络 task → 编码器 task
/// 收到 `KeyFrameRequest` 时发信号，通知编码器输出 I 帧
pub type KeyFrameSender = mpsc::UnboundedSender<()>;
pub type KeyFrameReceiver = mpsc::UnboundedReceiver<()>;

/// 接收帧 channel（→ Ticket-06 解码器）
pub type IncomingFrameSender = mpsc::UnboundedSender<Vec<u8>>;
pub type IncomingFrameReceiver = mpsc::UnboundedReceiver<Vec<u8>>;

// ============================================================
// QuicClient
// ============================================================

/// 客户端网络层——管理与中继服务器的 QUIC 连接。
pub struct QuicClient {
    /// QUIC 连接实例
    pub connection: quinn::Connection,
    /// 本机设备 ID
    device_id: String,
    /// HMAC 预共享密钥（原始字节）
    pre_shared_key: Vec<u8>,
}

impl QuicClient {
    /// 连接中继服务器并返回 `QuicClient`（尚未 Register）。
    ///
    /// - `server_addr`: 中继服务器地址，如 `"127.0.0.1:21117"`
    /// - `device_id`: 本机设备 ID，用于 Register 消息
    /// - `pre_shared_key`: hex 编码的预共享密钥
    pub async fn connect(
        server_addr: &str,
        device_id: &str,
        pre_shared_key: &str,
    ) -> anyhow::Result<Self> {
        let psk_bytes = hex::decode(pre_shared_key)
            .map_err(|e| anyhow::anyhow!("预共享密钥 hex 解码失败: {}", e))?;

        // 创建跳过证书验证的 TLS 客户端配置
        let tls_config = build_skip_verify_tls_config()?;

        // 创建客户端 QUIC 端点
        let endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;

        // 连接服务器
        let addr: std::net::SocketAddr = server_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("服务器地址格式错误 '{}': {}", server_addr, e))?;

        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            quinn::IdleTimeout::from(quinn::VarInt::from_u32(10_000)),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(3)));

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));
        client_config.transport_config(Arc::new(transport));

        let connection = endpoint
            .connect_with(client_config, addr, "myowndesk")?
            .await?;

        tracing::info!(
            "QUIC 连接已建立: {} => {}",
            connection.remote_address(),
            server_addr
        );

        Ok(Self {
            connection,
            device_id: device_id.to_string(),
            pre_shared_key: psk_bytes,
        })
    }

    /// 注册设备到中继服务器。
    ///
    /// 发送 `Register` 消息（含 HMAC-SHA256 认证令牌），
    /// 等待 `RegisterResponse`，返回在线设备列表。
    pub async fn register(&self) -> anyhow::Result<Vec<String>> {
        let token = compute_hmac(&self.pre_shared_key, &self.device_id);

        let msg = proto::Message {
            r#type: Some(proto::message::Type::Register(proto::Register {
                device_id: self.device_id.clone(),
                auth_token: token,
                protocol_version: 1,
            })),
        };

        // Register 是请求-响应：在同一条 bi-stream 上发请求、收响应
        let resp = self.request_response(&msg).await?;

        match resp.and_then(|m| m.r#type) {
            Some(proto::message::Type::RegisterResponse(r)) => {
                if r.error_code == proto::ErrorCode::Ok as i32 {
                    tracing::info!("注册成功, 在线设备: {:?}", r.online_devices);
                    Ok(r.online_devices)
                } else {
                    Err(anyhow::anyhow!(
                        "注册失败: {} (code {:?})",
                        r.error_message,
                        r.error_code
                    ))
                }
            }
            other => Err(anyhow::anyhow!(
                "注册响应格式错误: {:?}",
                other.map(|t| format!("{:?}", t))
            )),
        }
    }

    /// 发送一条 protobuf 消息并等待响应（请求-响应模式）。
    ///
    /// 在同一条 bi-stream 上发送请求、接收响应。
    /// relay 在收到请求的 stream 上回复响应，而不是打开新 stream。
    ///
    /// 返回 `None` 表示 stream 关闭（对端断开）。
    pub async fn request_response(
        &self,
        msg: &proto::Message,
    ) -> anyhow::Result<Option<proto::Message>> {
        use tokio::io::AsyncReadExt;

        let payload = msg.encode_to_vec();
        let len = (payload.len() as u32).to_le_bytes();

        let (mut send, mut recv) = self.connection.open_bi().await?;
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        send.finish()?;

        // 从同一条 stream 读取响应
        let mut len_buf = [0u8; 4];
        match AsyncReadExt::read_exact(&mut recv, &mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("读取消息失败: {}", e)),
        }

        let msg_len = u32::from_le_bytes(len_buf) as usize;
        if msg_len > 16 * 1024 * 1024 {
            anyhow::bail!("消息长度超过 16MB 上限");
        }

        let mut payload = vec![0u8; msg_len];
        recv.read_exact(&mut payload).await?;

        let msg = proto::Message::decode(payload.as_slice())
            .map_err(|e| anyhow::anyhow!("protobuf 解码失败: {}", e))?;
        Ok(Some(msg))
    }

    /// 发送 datagram（视频帧）。
    ///
    /// datagram 通道：不可靠、无重传、自带边界。
    pub fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        self.connection
            .send_datagram(bytes::Bytes::copy_from_slice(data))
            .map_err(|e| anyhow::anyhow!("发送 datagram 失败: {}", e))
    }

    /// 接收 datagram。
    pub async fn recv_datagram(&self) -> anyhow::Result<bytes::Bytes> {
        self.connection
            .read_datagram()
            .await
            .map_err(|e| anyhow::anyhow!("接收 datagram 失败: {}", e))
    }

    /// 在 stream 上发送 protobuf 消息（可靠）。
    ///
    /// 消息格式：4 字节 LE 长度前缀 + protobuf 编码载荷
    pub async fn send_message(&self, msg: &proto::Message) -> anyhow::Result<()> {
        let payload = msg.encode_to_vec();
        let len = (payload.len() as u32).to_le_bytes();

        let (mut send, _recv) = self.connection.open_bi().await?;
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        send.finish()?;
        Ok(())
    }

    /// 从 stream 接收 protobuf 消息。
    ///
    /// 返回 `None` 表示 stream 已正常关闭（对端关闭连接）。
    pub async fn recv_message(&self) -> anyhow::Result<Option<proto::Message>> {
        let (_send, mut recv) = self.connection.accept_bi().await?;

        // 读 4 字节长度前缀
        let mut len_buf = [0u8; 4];
        // 使用 AsyncReadExt::read_exact（而非 quinn RecvStream 自身的方法），
        // 确保返回 std::io::Error，可以匹配 ErrorKind::UnexpectedEof
        match AsyncReadExt::read_exact(&mut recv, &mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("读取消息失败: {}", e)),
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            anyhow::bail!("消息长度超过 16MB 上限");
        }

        let mut payload = vec![0u8; len];
        recv.read_exact(&mut payload).await?;

        let msg = proto::Message::decode(payload.as_slice())
            .map_err(|e| anyhow::anyhow!("protobuf 解码失败: {}", e))?;
        Ok(Some(msg))
    }
}

// ============================================================
// HMAC 认证
// ============================================================

/// 计算 HMAC-SHA256(预共享密钥, device_id)
/// 与中继服务器 `auth.rs` 中的 `compute_token` 一致。
fn compute_hmac(key: &[u8], device_id: &str) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let tag = ring::hmac::sign(&key, device_id.as_bytes());
    tag.as_ref().to_vec()
}

// ============================================================
// TLS 配置
// ============================================================

/// 构建跳过证书验证的 TLS 客户端配置（接受自签名证书）。
fn build_skip_verify_tls_config() -> anyhow::Result<rustls::ClientConfig> {
    use rustls::client::danger::ServerCertVerifier;

    #[derive(Debug)]
    struct SkipVerification;

    impl ServerCertVerifier for SkipVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA512,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }

    let crypto_provider = rustls::crypto::ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(crypto_provider.into())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS 版本配置失败: {}", e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();

    Ok(config)
}
