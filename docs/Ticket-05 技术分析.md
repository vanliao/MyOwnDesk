# Ticket-05: 客户端网络层

## Context

MyOwnDesk 项目已完成 Ticket-01（协议定义）、Ticket-02（中继服务器）、Ticket-03（DXGI 屏幕捕获）、Ticket-04（H.264 视频编码）。Ticket-05 是客户端网络层——通过 QUIC 连接中继服务器，完成 Register 认证，建立 datagram 和 stream 通道，发送编码帧、接收对端帧和消息。

**依赖关系**：Ticket-05 依赖 Ticket-02（中继服务器）和 Ticket-04（H.264 视频编码），是 Ticket-07（端到端流式传输）的**直接前置条件**。

**当前代码状态**：
- Ticket-02 已完整实现——QUIC 中继服务器支持 Register/Pair/Disconnect/datagram 转发/stream 转发/心跳
- Ticket-04 已完整实现——`encoder.rs` 输出 `EncodedFrame`，`service.rs` 暴露 `EncodeSender/EncodeReceiver` 类型
- `service.rs` 中 `_encode_rx` 当前被 drop（未消费），待 Ticket-05 接入
- 客户端 `Cargo.toml` 已有 `tokio`、`tracing`、`anyhow` 等——但**没有** `quinn`

---

## 已确认决策（Grilling 结论）

| # | 决策点 | 结论 | 理由 |
|---|--------|------|------|
| 1 | KeyFrameRequest 反馈 | **控制信号 channel**——新增 `keyframe_tx/keyframe_rx` channel，网络 task 发信号，编码 task 接收后调 `request_keyframe()` | 无锁，职责清晰，不阻塞编码 |
| 2 | QuicClient 设计 | **共享模块**——单一 `QuicClient` 实现两种模式。服务模式和 GUI 模式只是 channel 接线不同 | 避免后续重复实现网络层 |
| 3 | TLS 证书 | **跳过验证**——`danger_accept_invalid_certs: true` | 个人自用，自签名证书的合理选择 |
| 4 | 集成方式 | **service.rs 内启动网络 task**——QuicClient 作为 `tokio::spawn` task 运行 | 与现有编码 task 架构一致 |
| 5 | 配置来源 | **`client.toml`**——与 `relay.toml` 对应，含 server 地址、设备 ID、预共享密钥 | 一步到位，后续 GUI 模式也可复用 |

---

## 架构概览

### 服务模式 (--service)

```
capture 线程 (std::thread)      encoder task (tokio)         network task (tokio)
      │                               │                           │
      │  tx.send(CapturedFrame)       │                           │
      ├─────────────────────────►     │                           │
      │                               │                           │
      │                     encode(   │                           │
      │                       frame)  │  encode_tx.send(          │
      │                               │    EncodedFrame)          │
      │                               ├─────────────────────►     │
      │                               │                     send_datagram() → QUIC
      │                               │                            │
      │                               │  ← keyframe_rx           │
      │                               │    (force_keyframe)  ◄── keyframe_tx
      │                               │                            │
      │                               │                     recv_stream() ◄── QUIC
      │                               │                     ├── KeyEvent → [Ticket-08]
      │                               │                     ├── MouseEvent → [Ticket-08]
      │                               │                     ├── KeyFrameRequest → keyframe_tx
      │                               │                     ├── Disconnect → handle
      │                               │                     ├── Pair → handle
      │                               │                     └── Ping → reply Pong
```

### GUI 模式（双击）

```
GUI process (tokio main)
      │
      ├── QuicClient::connect() → Register
      ├── User clicks device → Pair
      │
      ├── recv_datagram() → Decoder (Ticket-06) → Render (Ticket-06)
      └── User input → send_control(MouseEvent/KeyEvent/Disconnect)
```

---

## 接口契约

### Ticket-04 → Ticket-05 接口

```rust
// service.rs (已有)
pub type EncodeSender = mpsc::UnboundedSender<EncodedFrame>;
pub type EncodeReceiver = mpsc::UnboundedReceiver<EncodedFrame>;

// encoder.rs (已有)
pub struct EncodedFrame {
    pub nal_units: Vec<u8>,
    pub frame_type: FrameType,     // Keyframe / Delta
    pub display_index: u32,
    pub pts: i64,
    pub width: u32,
    pub height: u32,
}
```

### QuicClient → 控制信号

```rust
// 新增：KeyFrameRequest 控制信号
pub type KeyFrameSender = tokio::sync::mpsc::UnboundedSender<()>;
pub type KeyFrameReceiver = tokio::sync::mpsc::UnboundedReceiver<()>;
```

### Ticket-05 → Ticket-06/07/08 接口（预留）

```rust
// 接收到的视频帧（从 datagram 解析后的 raw H.264 数据）
// 供 Ticket-06 解码消费
pub type FrameReceiver = tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>;

// 接收到的输入事件（从 stream 解析后的 protobuf 消息）
// 供 Ticket-08 输入注入消费
// 暂定义为基础类型
pub enum ReceivedEvent {
    KeyEvent { key_code: u32, pressed: bool },
    MouseEvent { event_type: i32, x: i32, y: i32, button: i32, wheel_delta: i32 },
}
```

---

## Deliverables

### 1. 新增 `myowndesk-client/client.toml`

```toml
# MyOwnDesk 客户端配置

[server]
address = "127.0.0.1:21117"

[device]
id = "van-pc"
pre_shared_key = ""  # 从中继服务器复制
```

### 2. 修改 `myowndesk-client/Cargo.toml` — 添加依赖

```toml
[dependencies]
# ... 已有依赖 ...
quinn = "0.11"
rustls = "0.23"
prost = "0.13"
bytes = "1"
toml = "0.8"
serde = { version = "1", features = ["derive"] }
hex = "0.4"
```

### 3. 新建 `myowndesk-client/src/net.rs` — 网络模块

#### 3a. 数据类型

```rust
use quinn::Connection;
use tokio::sync::mpsc;
use std::sync::Arc;

/// KeyFrame 请求信号（编码器控制）
pub type KeyFrameSender = mpsc::UnboundedSender<()>;
pub type KeyFrameReceiver = mpsc::UnboundedReceiver<()>;

/// 接收帧 channel（→ Ticket-06 解码器）
pub type IncomingFrameSender = mpsc::UnboundedSender<Vec<u8>>;
pub type IncomingFrameReceiver = mpsc::UnboundedReceiver<Vec<u8>>;
```

#### 3b. QuicClient 结构

```rust
/// 客户端网络层——管理与中继服务器的 QUIC 连接。
///
/// 提供 datagram 和 stream 两种通道的收发能力。
pub struct QuicClient {
    connection: quinn::Connection,
    device_id: String,
    pre_shared_key: Vec<u8>,
}

impl QuicClient {
    /// 连接中继服务器
    pub async fn connect(
        server_addr: &str,
        device_id: &str,
        pre_shared_key: &str,
    ) -> anyhow::Result<Self>;

    /// 注册设备（发送 Register 消息）
    pub async fn register(&self) -> anyhow::Result<Vec<String>>;

    /// 发送 datagram（视频帧）
    pub fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()>;

    /// 接收 datagram（视频帧）
    pub async fn recv_datagram(&self) -> anyhow::Result<bytes::Bytes>;

    /// 在 stream 上发送 protobuf 消息
    pub async fn send_message(&self, msg: &myowndesk_protocol::Message) -> anyhow::Result<()>;

    /// 从 stream 接收 protobuf 消息
    pub async fn recv_message(&self) -> anyhow::Result<Option<myowndesk_protocol::Message>>;

    /// 获取设备是否已配对
    pub fn is_paired(&self) -> bool;
}
```

#### 3c. 连接与注册

```rust
impl QuicClient {
    pub async fn connect(
        server_addr: &str,
        device_id: &str,
        pre_shared_key: &str,
    ) -> anyhow::Result<Self> {
        // 1. 创建 QUIC 客户端 endpoint（skip cert verification）
        let mut endpoint = quinn::Endpoint::client(default_socket_addr())?;
        
        // 2. 连接服务器
        let connection = endpoint
            .connect(server_addr.parse()?, "myowndesk")?
            .await?;

        Ok(Self {
            connection,
            device_id: device_id.to_string(),
            pre_shared_key: hex::decode(pre_shared_key)?,
        })
    }

    pub async fn register(&self) -> anyhow::Result<Vec<String>> {
        use myowndesk_protocol::*;
        use prost::Message as _;

        // 计算 HMAC 令牌
        let token = compute_hmac(&self.pre_shared_key, &self.device_id);

        let msg = Message {
            r#type: Some(message::Type::Register(Register {
                device_id: self.device_id.clone(),
                auth_token: token,
                protocol_version: 1,
            })),
        };

        // 发送 Register，等待 RegisterResponse
        self.send_message(&msg).await?;
        let resp = self.recv_message().await?;

        match resp.and_then(|m| m.r#type) {
            Some(message::Type::RegisterResponse(r)) => {
                if r.error_code == ErrorCode::Ok as i32 {
                    Ok(r.online_devices)
                } else {
                    anyhow::bail!("注册失败: {}", r.error_message)
                }
            }
            _ => anyhow::bail!("注册响应格式错误"),
        }
    }
}
```

#### 3d. Datagram 收发

```rust
impl QuicClient {
    /// 发送 datagram（视频帧）
    /// datagram 已经是 protobuf 编码的 DataPacket，无额外长度前缀
    pub fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        self.connection
            .send_datagram(bytes::Bytes::copy_from_slice(data))
            .map_err(|e| anyhow::anyhow!("发送 datagram 失败: {}", e))
    }

    /// 接收 datagram
    pub async fn recv_datagram(&self) -> anyhow::Result<bytes::Bytes> {
        self.connection
            .read_datagram()
            .await
            .map_err(|e| anyhow::anyhow!("接收 datagram 失败: {}", e))
    }
}
```

#### 3e. Stream 消息收发

```rust
impl QuicClient {
    /// 发送 protobuf 消息（4 字节 LE 长度前缀 + 编码数据）
    pub async fn send_message(&self, msg: &myowndesk_protocol::Message) -> anyhow::Result<()> {
        use prost::Message as _;
        let payload = msg.encode_to_vec();
        let len = (payload.len() as u32).to_le_bytes();

        let (mut send, _recv) = self.connection.open_bi().await?;
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        send.finish()?;
        Ok(())
    }

    /// 接收 protobuf 消息
    pub async fn recv_message(&self) -> anyhow::Result<Option<myowndesk_protocol::Message>> {
        use prost::Message as _;
        let (_send, mut recv) = self.connection.accept_bi().await?;

        // 读 4 字节长度前缀
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            anyhow::bail!("消息长度超过 16MB 上限");
        }

        let mut payload = vec![0u8; len];
        recv.read_exact(&mut payload).await?;

        let msg = myowndesk_protocol::Message::decode(payload.as_slice())?;
        Ok(Some(msg))
    }
}
```

### 4. 新建 `myowndesk-client/src/config.rs` — 客户端配置

```rust
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ClientConfig {
    pub server: ServerConfig,
    pub device: DeviceConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub id: String,
    pub pre_shared_key: String,
}

impl ClientConfig {
    /// 从 client.toml 加载
    pub fn load() -> anyhow::Result<Self> {
        let path = std::path::Path::new("client.toml");
        if !path.exists() {
            // 创建默认配置
            let config = Self {
                server: ServerConfig {
                    address: "127.0.0.1:21117".to_string(),
                },
                device: DeviceConfig {
                    id: whoami::hostname(),
                    pre_shared_key: String::new(),
                },
            };
            let content = toml::to_string_pretty(&config)?;
            std::fs::write(path, content)?;
            println!("[config] 已创建默认 client.toml，请填写预共享密钥");
            return Ok(config);
        }
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
```

### 5. 修改 `service.rs` — 集成网络层

```rust
// service.rs (完整结构预览)

pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk 服务模式启动中...");

    // 1. 加载配置
    let config = crate::config::ClientConfig::load()?;

    // 2. 创建 D3D11 设备
    let (device, context) = create_d3d11_device()?;
    let mut duplicator = ScreenDuplicator::new(&device, &context)?;

    // 3. 创建通道
    let (capture_tx, capture_rx) = mpsc::unbounded_channel::<CapturedFrame>();
    let (encode_tx, encode_rx) = mpsc::unbounded_channel::<EncodedFrame>();
    let (keyframe_tx, keyframe_rx) = mpsc::unbounded_channel::<()>();  // 新增
    let running = Arc::new(AtomicBool::new(true));

    // 4. 启动捕获线程（不变）
    let capture_handle = { ... };

    // 5. 启动编码 task（增加 keyframe_rx 监听）
    let encoder_handle = tokio::spawn(async move {
        let mut encoder = encoder::create_best_encoder(1920, 1080, 60)?;
        loop {
            tokio::select! {
                Some(frame) = capture_rx.recv() => {
                    // 编码逻辑...
                }
                Some(_) = keyframe_rx.recv() => {
                    encoder.request_keyframe();
                }
                else => break,
            }
        }
    });

    // 6. 启动网络 task
    let network_handle = tokio::spawn(async move {
        // 连接中继
        let client = QuicClient::connect(
            &config.server.address,
            &config.device.id,
            &config.device.pre_shared_key,
        ).await?;

        // Register
        let online_devices = client.register().await?;
        tracing::info!("注册成功, 在线设备: {:?}", online_devices);

        // 生成 datagram 发送 task
        let send_handle = tokio::spawn({
            let client = client.clone();
            // 可能需要 Arc 或 clone connection
            async move {
                while let Some(frame) = encode_rx.recv().await {
                    // EncodedFrame → DataPacket protobuf → datagram
                    let data_packet = myowndesk_protocol::DataPacket {
                        frame_type: match frame.frame_type {
                            encoder::FrameType::Keyframe => myowndesk_protocol::FrameType::Keyframe as i32,
                            encoder::FrameType::Delta => myowndesk_protocol::FrameType::Delta as i32,
                        },
                        display_index: frame.display_index,
                        payload: frame.nal_units,
                        ..Default::default()
                    };
                    let msg = myowndesk_protocol::Message {
                        r#type: Some(myowndesk_protocol::message::Type::DataPacket(data_packet)),
                    };
                    let encoded = msg.encode_to_vec();
                    if let Err(e) = client.send_datagram(&encoded) {
                        tracing::error!("发送 datagram 失败: {}", e);
                        break;
                    }
                }
            }
        });

        // stream 接收 task（处理控制消息）
        let recv_handle = tokio::spawn(async move {
            loop {
                match client.recv_message().await {
                    Ok(Some(msg)) => handle_control_message(msg, &keyframe_tx),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!("接收消息失败: {}", e);
                        break;
                    }
                }
            }
        });

        // 等待发送或接收 task 退出
        let _ = tokio::join!(send_handle, recv_handle);
        anyhow::Ok(())
    });

    // 7. 等待退出（不变）
    tracing::info!("服务已启动，按 Ctrl+C 停止");
    tokio::signal::ctrl_c().await?;

    // 8. 清理（更新）
    running.store(false, Ordering::SeqCst);
    let _ = capture_handle.join();
    let _ = tokio::time::timeout(Duration::from_secs(3), encoder_handle).await;
    // 网络 task 在 channel 关闭后自动退出
}
```

### 6. 修改 `myowndesk-client/src/lib.rs`

```rust
pub mod capture;
pub mod config;    // 新增
pub mod encoder;
pub mod net;       // 新增
pub mod service;
```

---

## 消息路由规则（客户端侧）

| 消息类型 | 方向 | 动作 |
|---------|------|------|
| `RegisterResponse` | ← 来自中继 | 提取 `online_devices`，后续传给 GUI |
| `PairResponse` | ← 来自中继 | 配对结果处理 |
| `PeerDisconnected` | ← 来自中继 | 对端离线通知，锁屏触发（Ticket-11） |
| `Ping` | ← 来自中继 | 回复 `Pong` |
| `Pong` | → 发送给中继 | 心跳回复（中继自动处理） |
| `KeyFrameRequest` | ← 来自中继（转发） | 通过 `keyframe_tx` 通知编码器输出 I 帧 |
| `KeyEvent` | ← 来自中继（转发） | 暂存，后续 Ticket-08 消费 |
| `MouseEvent` | ← 来自中继（转发） | 暂存，后续 Ticket-08 消费 |
| `SwitchDisplay` | ← 来自中继（转发） | 暂存，后续 Ticket-10 消费 |
| `Disconnect` | → 发送给中继 | 主动断开 |
| `DataPacket` | 双向 datagram | 发送：编码帧；接收：待 Ticket-06 解码 |

---

## 错误处理矩阵

| 场景 | 处理 |
|------|------|
| QUIC 连接失败 | `QuicClient::connect()` 返回 Err，网络 task 退出并记录错误 |
| Register 认证失败（AUTH_FAILED） | 记录致命错误，网络 task 退出 |
| Register 超时 | QUIC 连接超时（`quinn` 内部处理），返回 Err |
| Datagram 发送失败（连接断开） | 记录日志，发送 task 退出 |
| Stream 接收失败（连接断开） | 记录日志，接收 task 退出 |
| Recv 到无法解析的消息 | 静默丢弃（向前兼容） |
| client.toml 不存在 | 自动创建默认配置，提示填写密钥 |
| 心跳超时 | 中继侧处理；客户端连接断开后 net task 退出 |

---

## 文件变更清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 修改 | `myowndesk-client/Cargo.toml` | 添加 `quinn`、`rustls`、`prost`、`bytes`、`toml`、`serde`、`hex`、`whoami` |
| 新建 | `myowndesk-client/client.toml` | 默认客户端配置文件 |
| 新建 | `myowndesk-client/src/config.rs` | `ClientConfig` 结构体 + TOML 加载 |
| 新建 | `myowndesk-client/src/net.rs` | `QuicClient` 结构体 + connect/register/send/recv 方法 |
| 修改 | `myowndesk-client/src/service.rs` | 集成 net task：连接→注册→发送编码帧→接收控制消息 |
| 修改 | `myowndesk-client/src/lib.rs` | 声明 `config`、`net` 模块 |
| 修改 | `myowndesk-client/src/encoder.rs` | 导出 `KeyFrameSender/KeyFrameReceiver` 类型（或定义在 net.rs） |

---

## 验证

### 编译

```bash
cargo build -p myowndesk-client
cargo check -p myowndesk-client
```

### 集成测试

按照 spec.md 测试哲学——通过真实 QUIC 连接测试协议层交互。

```rust
// tests/net_test.rs

// 1. test_connect_and_register — 启动本地中继 → 客户端连接 → Register → 验证成功
// 2. test_send_datagram — Register 后，发送 DataPacket datagram → 中继接收
// 3. test_send_control_message — Register 后，发送 KeyEvent stream → 中继接收
// 4. test_keyframe_request_signal — 模拟收到 KeyFrameRequest → 验证 keyframe_tx 发出信号
```

### 手动验证（需要中继服务器运行）

```bash
# 终端 1: 启动中继
cargo run -p myowndesk-relay

# 终端 2: 启动客户端服务（需先配好 client.toml）
cargo run -p myowndesk-client -- --service

# 预期输出：
# [config] 已创建默认 client.toml，请填写预共享密钥
# → 填写密钥后重新运行：
# [INFO] 服务模式启动中...
# [INFO] QUIC 客户端连接中继成功
# [INFO] 注册成功, 在线设备: []
# [INFO] 服务已启动，按 Ctrl+C 停止
```

---

## 后续扩展路径

| 扩展 | 说明 |
|------|------|
| 重连逻辑 | 连接断开后自动重试（当前退出 task，依赖重连按钮） |
| 带宽统计 | datagram 发送速率统计 |
| 多个 QUIC 连接 | 后续可能同时连接多个中继 |
| NAT 打洞 | QUIC 连接迁移到 P2P 直连 |
