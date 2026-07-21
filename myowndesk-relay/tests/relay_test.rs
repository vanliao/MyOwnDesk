//! Ticket-02 集成测试
//!
//! 通过真实 QUIC 连接验证中继服务器的协议行为。
//! 每个测试启动一个本地中继实例（随机端口），模拟客户端发送消息并验证响应。

use myowndesk_protocol::message::Type;
use myowndesk_protocol::*;
use myowndesk_relay::auth;
use myowndesk_relay::relay::{self, RelayState, SharedState};
use prost::Message as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::RwLock;

// ============================================================
// Test helpers
// ============================================================

/// TLS 配置（服务器 + 客户端共用）
struct TlsPair {
    server_config: quinn::ServerConfig,
    client_config: quinn::ClientConfig,
}

/// 测试上下文
struct TestContext {
    addr: SocketAddr,
    tls: TlsPair,
    _state: SharedState,
}

/// 生成自签名证书，返回服务器和客户端 TLS 配置
fn build_tls() -> TlsPair {
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();

    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der =
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der());
    let key = rustls::pki_types::PrivateKeyDer::from(key_der);

    // 服务器
    let server_config =
        quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], key).unwrap();

    // 客户端
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let mut client_config =
        quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(1)));
    client_config.transport_config(Arc::new(transport));

    TlsPair {
        server_config,
        client_config,
    }
}

/// 启动中继服务器（随机端口），返回测试上下文
async fn start_relay(pre_shared_key: Vec<u8>) -> TestContext {
    let tls = build_tls();
    let state: SharedState = Arc::new(RwLock::new(RelayState::new(pre_shared_key)));

    let addr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let endpoint = quinn::Endpoint::server(tls.server_config.clone(), addr).unwrap();
    let local_addr = endpoint.local_addr().unwrap();

    let accept_state = state.clone();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let state = accept_state.clone();
            tokio::spawn(async move {
                let _ = handle_test_connection(incoming, state).await;
            });
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    TestContext {
        addr: local_addr,
        tls,
        _state: state,
    }
}

/// 创建连接到中继的 QUIC 客户端
async fn connect_client(ctx: &TestContext) -> quinn::Connection {
    let mut endpoint = quinn::Endpoint::client(SocketAddr::new(
        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
    ))
    .unwrap();
    endpoint.set_default_client_config(ctx.tls.client_config.clone());

    endpoint
        .connect(ctx.addr, "localhost")
        .unwrap()
        .await
        .unwrap()
}

/// 客户端发送 Register 并获取响应
async fn client_register(conn: &quinn::Connection, device_id: &str, key: &[u8]) -> RegisterResponse {
    let token = auth::compute_token(key, device_id);
    let msg = Message {
        r#type: Some(Type::Register(Register {
            device_id: device_id.to_string(),
            auth_token: token,
            protocol_version: 1,
        })),
    };

    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_frame(&mut send, &msg).await;

    let resp = relay::read_message(&mut recv).await.unwrap().unwrap();
    match resp.r#type {
        Some(Type::RegisterResponse(r)) => r,
        _ => panic!("期望 RegisterResponse"),
    }
}

/// 客户端发送 Pair 并获取响应
async fn client_pair(conn: &quinn::Connection, target: &str) -> PairResponse {
    let msg = Message {
        r#type: Some(Type::Pair(Pair {
            target_device_id: target.to_string(),
        })),
    };
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_frame(&mut send, &msg).await;

    let resp = relay::read_message(&mut recv).await.unwrap().unwrap();
    match resp.r#type {
        Some(Type::PairResponse(r)) => r,
        _ => panic!("期望 PairResponse"),
    }
}

/// 客户端发送 Disconnect
async fn client_disconnect(conn: &quinn::Connection) {
    let msg = Message {
        r#type: Some(Type::Disconnect(Disconnect {
            reason: "测试断开".to_string(),
        })),
    };
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    send_frame(&mut send, &msg).await;
}

/// 在 send stream 上写帧（4 字节 LE 长度 + protobuf 载荷）
async fn send_frame(send: &mut quinn::SendStream, msg: &Message) {
    let payload = msg.encode_to_vec();
    let len_bytes = (payload.len() as u32).to_le_bytes();
    send.write_all(&len_bytes).await.unwrap();
    send.write_all(&payload).await.unwrap();
    send.finish().unwrap();
}

// ============================================================
// 简化的连接处理（测试用，复制 server.rs 核心逻辑）
// ============================================================

async fn handle_test_connection(
    incoming: quinn::Incoming,
    state: SharedState,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    let (mut send, mut recv) = connection.accept_bi().await?;

    let msg = relay::read_message(&mut recv).await?;
    let register = match msg.and_then(|m| match m.r#type {
        Some(Type::Register(reg)) => Some(reg),
        _ => None,
    }) {
        Some(reg) => reg,
        None => return Ok(()),
    };

    let device_id = register.device_id.clone();
    let auth_token = register.auth_token;

    let online_devices = {
        let mut s = state.write().await;
        match s.register(device_id.clone(), connection.clone(), &auth_token).await {
            Ok(devices) => devices,
            Err(error_code) => {
                let resp =
                    relay::build_register_response(error_code, "失败".to_string(), vec![]);
                // 发送错误响应后，等待一小段时间确保客户端收到
                let _ = send_response_on_stream(&mut send, &resp).await;
                // 等待一小段时间确保响应被发送
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                return Ok(());
            }
        }
    };

    let resp = relay::build_register_response(ErrorCode::Ok, String::new(), online_devices);
    send_response_on_stream(&mut send, &resp).await?;

    // Datagram 读取任务
    let dgram_state = state.clone();
    let dgram_device = device_id.clone();
    let dgram_conn = connection.clone();
    tokio::spawn(async move {
        loop {
            match dgram_conn.read_datagram().await {
                Ok(data) => {
                    let _ = dgram_state
                        .read()
                        .await
                        .forward_datagram(&dgram_device, &data)
                        .await;
                }
                Err(_) => break,
            }
        }
    });

    let device = device_id.clone();
    tokio::spawn(async move {
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut r)) => {
                    while let Some(msg) = relay::read_message(&mut r).await.transpose() {
                        match msg {
                            Ok(msg) => match msg.r#type {
                                Some(Type::Pair(pair)) => {
                                    let result = state.write().await.pair(&device, &pair.target_device_id).await;
                                    let (ec, em) = match result {
                                        Ok(()) => (ErrorCode::Ok, String::new()),
                                        Err(ErrorCode::DeviceNotFound) => {
                                            (ErrorCode::DeviceNotFound, "不在线".to_string())
                                        }
                                        Err(ErrorCode::AlreadyPaired) => {
                                            (ErrorCode::AlreadyPaired, "已配对".to_string())
                                        }
                                        Err(e) => (e, "失败".to_string()),
                                    };
                                    // 在同一 stream 上回复 PairResponse
                                    let _ = send_response_on_stream(
                                        &mut send,
                                        &relay::build_pair_response(ec, em),
                                    ).await;
                                    break; // stream 已用于回复，退出内层循环
                                }
                                Some(Type::Disconnect(_)) => {
                                    let _ = state.write().await.disconnect(&device).await;
                                }
                                Some(Type::Pong(_)) => {
                                    state.write().await.heartbeat(&device);
                                }
                                Some(Type::KeyEvent(_) | Type::MouseEvent(_)
                                    | Type::SwitchDisplay(_) | Type::KeyFrameRequest(_)) => {
                                    let encoded = msg.encode_to_vec();
                                    let _ = state.read().await.forward_encoded_msg(&device, &encoded).await;
                                }
                                _ => {}
                            },
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => break,
            }
        }
        state.write().await.remove_device(&device).await;
    });

    Ok(())
}

async fn send_response_on_stream(
    send: &mut quinn::SendStream,
    msg: &Message,
) -> anyhow::Result<()> {
    let payload = msg.encode_to_vec();
    let len_bytes = (payload.len() as u32).to_le_bytes();
    send.write_all(&len_bytes).await?;
    send.write_all(&payload).await?;
    send.finish()?;
    Ok(())
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn test_register_success() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn = connect_client(&ctx).await;
    let resp = client_register(&conn, "van-pc", &key).await;

    assert_eq!(resp.error_code, ErrorCode::Ok as i32);
}

#[tokio::test]
async fn test_register_auth_failed() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let wrong_key = b"wrong_secret_key_32_bytes_long!";
    let ctx = start_relay(key).await;

    let conn = connect_client(&ctx).await;
    let resp = client_register(&conn, "van-pc", wrong_key).await;

    assert_eq!(resp.error_code, ErrorCode::AuthFailed as i32);
}

#[tokio::test]
async fn test_register_duplicate() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn1 = connect_client(&ctx).await;
    let resp = client_register(&conn1, "van-pc", &key).await;
    assert_eq!(resp.error_code, ErrorCode::Ok as i32);

    let conn2 = connect_client(&ctx).await;
    let resp = client_register(&conn2, "van-pc", &key).await;
    assert_eq!(resp.error_code, ErrorCode::Ok as i32);
}

#[tokio::test]
async fn test_pair_success() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn_a = connect_client(&ctx).await;
    let conn_b = connect_client(&ctx).await;

    client_register(&conn_a, "van-pc", &key).await;
    client_register(&conn_b, "van-laptop", &key).await;

    let resp = client_pair(&conn_b, "van-pc").await;
    assert_eq!(resp.error_code, ErrorCode::Ok as i32);
}

#[tokio::test]
async fn test_pair_device_not_found() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn = connect_client(&ctx).await;
    client_register(&conn, "van-pc", &key).await;

    let resp = client_pair(&conn, "nonexistent").await;
    assert_eq!(resp.error_code, ErrorCode::DeviceNotFound as i32);
}

#[tokio::test]
async fn test_pair_already_paired() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn_a = connect_client(&ctx).await;
    let conn_b = connect_client(&ctx).await;
    let conn_c = connect_client(&ctx).await;

    client_register(&conn_a, "van-pc", &key).await;
    client_register(&conn_b, "van-laptop", &key).await;
    client_register(&conn_c, "van-server", &key).await;

    let resp = client_pair(&conn_b, "van-pc").await;
    assert_eq!(resp.error_code, ErrorCode::Ok as i32);

    let resp = client_pair(&conn_b, "van-server").await;
    assert_eq!(resp.error_code, ErrorCode::AlreadyPaired as i32);
}

#[tokio::test]
async fn test_forward_datagram() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn_a = connect_client(&ctx).await;
    let conn_b = connect_client(&ctx).await;

    client_register(&conn_a, "van-pc", &key).await;
    client_register(&conn_b, "van-laptop", &key).await;
    client_pair(&conn_b, "van-pc").await;

    let test_data = b"hello from van-pc";
    conn_a.send_datagram(bytes::Bytes::from_static(test_data)).unwrap();

    let received = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        conn_b.read_datagram(),
    )
    .await
    .expect("超时未收到 datagram")
    .expect("datagram 读取错误");

    assert_eq!(&*received, test_data);
}

#[tokio::test]
async fn test_forward_stream_msg() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn_a = connect_client(&ctx).await;
    let conn_b = connect_client(&ctx).await;

    client_register(&conn_a, "van-pc", &key).await;
    client_register(&conn_b, "van-laptop", &key).await;
    client_pair(&conn_b, "van-pc").await;

    let msg = Message {
        r#type: Some(Type::KeyEvent(KeyEvent {
            key_code: 0x41,
            pressed: true,
        })),
    };
    let (mut send, _recv) = conn_b.open_bi().await.unwrap();
    send_frame(&mut send, &msg).await;

    let (_, mut recv) = conn_a.accept_bi().await.unwrap();
    let received = relay::read_message(&mut recv).await.unwrap().unwrap();

    match received.r#type {
        Some(Type::KeyEvent(e)) => {
            assert_eq!(e.key_code, 0x41);
            assert!(e.pressed);
        }
        _ => panic!("期望 KeyEvent"),
    }
}

#[tokio::test]
async fn test_disconnect_notifies_peer() {
    let key = b"my_secret_key_32_bytes_long!!".to_vec();
    let ctx = start_relay(key.clone()).await;

    let conn_a = connect_client(&ctx).await;
    let conn_b = connect_client(&ctx).await;

    client_register(&conn_a, "van-pc", &key).await;
    client_register(&conn_b, "van-laptop", &key).await;
    client_pair(&conn_b, "van-pc").await;

    client_disconnect(&conn_b).await;

    let (_, mut recv) = conn_a.accept_bi().await.unwrap();
    let received = relay::read_message(&mut recv).await.unwrap().unwrap();

    match received.r#type {
        Some(Type::PeerDisconnected(pd)) => {
            assert!(pd.reason.contains("主动断开"));
        }
        _ => panic!("期望 PeerDisconnected"),
    }
}
