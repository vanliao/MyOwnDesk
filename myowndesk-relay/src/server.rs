//! QUIC 服务器：端点初始化、连接接受、消息路由、心跳管理

use crate::config::RelayConfig;
use crate::relay::{self, SharedState, RelayState};
use myowndesk_protocol::message::Type;
use myowndesk_protocol::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// 启动中继服务器
pub async fn run(config: RelayConfig) -> anyhow::Result<()> {
    let key_bytes = config.key_bytes()?;
    let state: SharedState = Arc::new(RwLock::new(RelayState::new(key_bytes)));

    // 生成自签名证书
    let cert = generate_self_signed_cert()?;

    let mut transport = quinn::TransportConfig::default();
    // 10s idle 超时，快速检测死连接
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::from(quinn::VarInt::from_u32(10_000)),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(3)));

    let mut server_config =
        quinn::ServerConfig::with_single_cert(cert.certs, cert.key)?;
    server_config.transport_config(Arc::new(transport));

    let addr: SocketAddr = config.listen_address.parse()?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    info!("中继服务器已启动，监听 {}", addr);

    let heartbeat_interval = Duration::from_secs(config.heartbeat_interval_secs);
    let heartbeat_timeout = Duration::from_secs(config.heartbeat_timeout_secs);

    // 心跳清理任务
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let stale: Vec<String> = {
                cleanup_state.read().await.stale_devices(heartbeat_timeout)
            };
            for device_id in stale {
                warn!("设备 {} 心跳超时，移除", device_id);
                cleanup_state.write().await.remove_device(&device_id).await;
            }
        }
    });

    // 接受循环
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let heartbeat_interval = heartbeat_interval;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, state, heartbeat_interval).await {
                error!("连接处理失败: {}", e);
            }
        });
    }

    Ok(())
}

/// 处理单个客户端连接
async fn handle_connection(
    incoming: quinn::Incoming,
    state: SharedState,
    heartbeat_interval: Duration,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    let remote_addr = connection.remote_address();
    info!("新连接: {}", remote_addr);

    // 等待客户端打开第一条双向 stream（用于 Register）
    let (mut send, mut recv) = match connection.accept_bi().await {
        Ok(s) => s,
        Err(e) => {
            warn!("{} 未能打开 stream: {}", remote_addr, e);
            return Ok(());
        }
    };

    // ---------- 处理 Register ----------
    let register_msg = match relay::read_message(&mut recv).await? {
        Some(msg) => msg,
        None => {
            warn!("{} 在 Register 前关闭 stream", remote_addr);
            return Ok(());
        }
    };

    let (device_id, auth_token) = match register_msg.r#type {
        Some(Type::Register(reg)) => (reg.device_id.clone(), reg.auth_token),
        _ => {
            // 首条消息不是 Register
            let resp = relay::build_register_response(
                ErrorCode::AuthFailed,
                "请先发送 Register 消息".to_string(),
                vec![],
            );
            let _ = relay::send_message(&connection, &resp).await;
            return Ok(());
        }
    };

    // 验证注册
    let online_devices = {
        let mut s = state.write().await;
        match s.register(device_id.clone(), connection.clone(), &auth_token).await {
            Ok(devices) => devices,
            Err(error_code) => {
                let resp = relay::build_register_response(
                    error_code,
                    match error_code {
                        ErrorCode::AuthFailed => "认证失败".to_string(),
                        _ => "注册失败".to_string(),
                    },
                    vec![],
                );
                let _ = relay::send_message(&connection, &resp).await;
                return Ok(());
            }
        }
    };

    // 发送注册成功响应（在第一条 stream 上回复）
    let resp = relay::build_register_response(
        ErrorCode::Ok,
        String::new(),
        online_devices,
    );
    // 使用已接受的 send stream 回复（而非打开新 stream）
    send_response_on_stream(&mut send, &resp).await?;
    info!("设备 {} 注册成功 (来自 {})", device_id, remote_addr);

    // ---------- 心跳 Ping 任务 ----------
    let _ping_state = state.clone();
    let _ping_device = device_id.clone();
    let ping_conn = connection.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let msg = relay::build_ping(now_ms());
            if relay::send_message(&ping_conn, &msg).await.is_err() {
                break; // 连接已断开，退出心跳
            }
        }
    });

    // ---------- Stream 读取任务 ----------
    let stream_state = state.clone();
    let stream_device = device_id.clone();
    let stream_conn = connection.clone();
    tokio::spawn(async move {
        loop {
            match stream_conn.accept_bi().await {
                Ok((send, mut recv)) => {
                    while let Some(msg) = relay::read_message(&mut recv).await.transpose() {
                        match msg {
                            Ok(msg) => {
                                handle_control_message(
                                    &stream_state,
                                    &stream_device,
                                    msg,
                                    send,
                                )
                                .await;
                                // send was consumed by handle_control_message for Pair/Ping,
                                // need to re-accept for next message
                                break;
                            }
                            Err(e) => {
                                warn!("消息解析失败: {}", e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    // 连接关闭是正常情况
                    if !matches!(
                        e,
                        quinn::ConnectionError::ApplicationClosed { .. }
                            | quinn::ConnectionError::LocallyClosed
                    ) {
                        warn!("接受 stream 失败: {}", e);
                    }
                    break;
                }
            }
        }
        // 连接断开，清理
        info!("设备 {} 离线", stream_device);
        stream_state.write().await.remove_device(&stream_device).await;
    });

    // ---------- Datagram 读取任务 ----------
    let dgram_state = state.clone();
    let dgram_device = device_id.clone();
    tokio::spawn(async move {
        loop {
            match connection.read_datagram().await {
                Ok(data) => {
                    let _ = dgram_state
                        .read()
                        .await
                        .forward_datagram(&dgram_device, &data)
                        .await;
                }
                Err(e) => {
                    if !matches!(
                        e,
                        quinn::ConnectionError::ApplicationClosed { .. }
                            | quinn::ConnectionError::LocallyClosed
                    ) {
                        warn!("datagram 读取失败: {}", e);
                    }
                    break;
                }
            }
        }
    });

    Ok(())
}

/// 分派控制消息（在 stream 上收到的消息）
/// `send` 是收到此消息的 stream 的发送端，用于请求-响应模式
async fn handle_control_message(
    state: &SharedState,
    device_id: &str,
    msg: Message,
    mut send: quinn::SendStream,
) {
    match msg.r#type {
        Some(Type::Pair(pair)) => {
            let result = state
                .write()
                .await
                .pair(device_id, &pair.target_device_id)
                .await;

            let (error_code, error_message) = match result {
                Ok(()) => (ErrorCode::Ok, String::new()),
                Err(ErrorCode::DeviceNotFound) => {
                    (ErrorCode::DeviceNotFound, "目标设备不在线".to_string())
                }
                Err(ErrorCode::AlreadyPaired) => {
                    (ErrorCode::AlreadyPaired, "设备已配对".to_string())
                }
                Err(e) => (e, "配对失败".to_string()),
            };

            let resp = relay::build_pair_response(error_code, error_message);
            let _ = send_response_on_stream(&mut send, &resp).await;
        }

        Some(Type::Disconnect(_)) => {
            let _ = state.write().await.disconnect(device_id).await;
        }

        Some(Type::Pong(_)) => {
            state.write().await.heartbeat(device_id);
        }

        // 转发类消息：编码后转发给对端
        Some(
            Type::KeyEvent(_)
            | Type::MouseEvent(_)
            | Type::SwitchDisplay(_)
            | Type::KeyFrameRequest(_),
        ) => {
            use prost::Message as _;
            let encoded = msg.encode_to_vec();
            let _ = state
                .read()
                .await
                .forward_encoded_msg(device_id, &encoded)
                .await;
        }

        // 客户端发来的 Ping → 回复 Pong
        Some(Type::Ping(ping)) => {
            state.write().await.heartbeat(device_id);
            let pong = Message {
                r#type: Some(Type::Pong(Pong {
                    timestamp_ms: ping.timestamp_ms,
                })),
            };
            let _ = send_response_on_stream(&mut send, &pong).await;
        }

        _ => {
            // 未知消息类型，忽略（向前兼容）
        }
    }
}

/// 在已有的 send stream 上回复消息
async fn send_response_on_stream(
    send: &mut quinn::SendStream,
    msg: &Message,
) -> anyhow::Result<()> {
    use prost::Message as _;
    let payload = msg.encode_to_vec();
    let len_bytes = (payload.len() as u32).to_le_bytes();
    send.write_all(&len_bytes).await?;
    send.write_all(&payload).await?;
    send.finish()?;
    Ok(())
}

// ============================================================
// 证书生成
// ============================================================

struct GeneratedCert {
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

/// 生成自签名证书用于 QUIC TLS
fn generate_self_signed_cert() -> anyhow::Result<GeneratedCert> {
    let key_pair = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(vec!["myowndesk-relay".to_string()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair)?;

    let key_der = key_pair.serialize_der();
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der);

    Ok(GeneratedCert {
        certs: vec![rustls::pki_types::CertificateDer::from(cert.der().to_vec())],
        key: rustls::pki_types::PrivateKeyDer::from(pkcs8),
    })
}

// ============================================================
// 时间工具
// ============================================================

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
