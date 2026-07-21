# Ticket-02: 中继服务器

## Context

MyOwnDesk 项目已完成 Ticket-01（项目骨架 + Protobuf 协议定义），三个 crate 均可编译。Ticket-02 是中继服务器的完整实现，是整个系统的核心枢纽——所有客户端通过中继发现彼此、建立配对、转发数据。

**依赖关系**：Ticket-02 仅依赖 Ticket-01（协议定义），是 Ticket-05（客户端网络层）的前置条件。

**当前状态**：`myowndesk-relay/src/main.rs` 仅有一行 `println!` + TODO 注释。`Cargo.toml` 仅依赖 `myowndesk-protocol`。

---

## 已确认决策（Grilling 结论）

以下决策基于 docs/ 下全部文档（spec.md、tickets.md、架构技术决策.md、需求分析.md、adr/0001-video-frame-fragmentation.md）交叉验证得出。

| # | 决策点 | 结论 | 依据 |
|---|--------|------|------|
| 1 | QUIC 库 | `quinn` 0.11 | 架构决策 #1 |
| 2 | 异步运行时 | `tokio` (full features) | 架构决策 #1，quinn 依赖 tokio |
| 3 | 模块拆分 | `main.rs` + `config.rs` + `auth.rs` + `server.rs` + `relay.rs` | 需求分析.md Crate 结构章节 |
| 4 | 共享状态 | `Arc<tokio::sync::RwLock<RelayState>>` | 多任务并发读写设备表的自然选择 |
| 5 | 消息帧格式 | 长度前缀（4 字节 LE u32）+ protobuf 载荷 | quinn stream 是字节流，需要帧边界 |
| 6 | Register 重复 | 允许重注册（覆盖旧连接），用于断线重连场景 | spec.md 连接流程，设备重新上线 |
| 7 | 一对多配对 | 不支持。一个设备同时只能配对一个目标 | tickets.md "配对双方连接"（单数） |
| 8 | 未注册发数据 | 丢弃并关闭该 stream，不关闭 QUIC 连接 | 防御性处理，避免干扰已注册设备 |
| 9 | 配置文件 | `relay.toml`，与二进制同目录；支持 `--config` 覆盖路径 | 架构决策 #17 |
| 10 | 密钥生成 | 首次启动若无预共享密钥，自动生成 256-bit 随机密钥，hex 编码打印到 stdout | spec.md §共享密钥认证 |
| 11 | 心跳机制 | 中继每 10s 发 Ping，设备需在 30s 内回复 Pong，超时则清理连接 | tickets.md "心跳 Ping/Pong 保活" |
| 12 | 带宽控制 | Ticket-02 暂不实现，后续 Ticket 再加 | tickets.md 未列入复选框，避免 scope creep |
| 13 | QUIC 通道分工 | 视频帧走 datagram，控制消息走双向 stream | 架构决策 #15 |
| 14 | 流处理模型 | 每个 QUIC 连接上，客户端开一条双向 stream 传控制消息；中继收到 Pair 后配对两设备，后续该 stream 上的转发类消息原样转发到对端 | proto 设计 + 架构决策 #15 |
| 15 | 对端断开通知 | 设备 QUIC 连接断开时，若已配对则向对端发 `PeerDisconnected`，清理配对状态，移除在线表 | tickets.md "Disconnect 消息处理：解绑配对，通知对端" |
| 16 | Datagram 格式 | 直接 protobuf 编码 DataPacket，不加长度前缀（datagram 自带边界） | ADR #1: 单 NAL unit per datagram |
| 17 | Stream 消息类型路由 | Register/Pair/Disconnect/Ping/Pong 由中继处理；KeyEvent/MouseEvent/SwitchDisplay/KeyFrameRequest 转发给对端 | proto 设计 + 架构决策 #15 |

---

## Deliverables

### 1. 修改 `myowndesk-relay/Cargo.toml`

添加 QUIC、认证、配置、日志等依赖。

```toml
[package]
name = "myowndesk-relay"
version = "0.1.0"
edition = "2021"

[dependencies]
myowndesk-protocol = { path = "../myowndesk-protocol" }
quinn = "0.11"
tokio = { version = "1", features = ["full"] }
rustls = "0.23"
ring = "0.17"                # HMAC-SHA256（quinn 已间接依赖，复用）
prost = "0.13"               # Message::encode/decode
bytes = "1"                  # 字节缓冲
toml = "0.8"                 # 配置解析
serde = { version = "1", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = "0.3"
anyhow = "1"                 # 简化错误处理
rand = "0.8"                 # 密钥生成
hex = "0.4"                  # 密钥 hex 编码
constant_time_eq = "0.3"     # 防计时攻击的 token 比较
```

### 2. 新建 `myowndesk-relay/relay.toml`

默认配置文件模板：

```toml
# MyOwnDesk 中继服务器配置
listen_address = "0.0.0.0:21117"

# 预共享密钥（首次启动自动生成，复制到各客户端配置文件）
pre_shared_key = ""

# 心跳间隔（秒）
heartbeat_interval_secs = 10

# 心跳超时（秒），超过此时间未收到 Pong 则断开
heartbeat_timeout_secs = 30
```

### 3. `myowndesk-relay/src/main.rs` — 入口

```rust
use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 解析命令行参数
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "relay.toml".to_string());

    // 加载/创建配置
    let config = myowndesk_relay::config::RelayConfig::load_or_create(&config_path)?;

    info!("预共享密钥: {}", config.pre_shared_key.as_deref().unwrap_or("(自动生成)"));
    info!("监听地址: {}", config.listen_address);

    // 启动服务器
    myowndesk_relay::server::run(config).await
}
```

### 4. `myowndesk-relay/src/config.rs` — 配置管理

```rust
use anyhow::{Context, Result};
use rand::Rng;
use serde::Deserialize;
use std::path::Path;

fn default_heartbeat_interval() -> u64 { 10 }
fn default_heartbeat_timeout() -> u64 { 30 }
fn default_listen_address() -> String { "0.0.0.0:21117".to_string() }

#[derive(Debug, Clone, Deserialize)]
pub struct RelayConfig {
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    pub pre_shared_key: Option<String>,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,
}

impl RelayConfig {
    /// 从 relay.toml 加载，不存在则创建默认配置并生成密钥
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("无法读取配置文件: {}", path.display()))?;
            let mut config: Self = toml::from_str(&content)
                .with_context(|| "配置文件格式错误")?;
            // 若密钥为空，生成并写回
            if config.pre_shared_key.as_deref().map_or(true, |k| k.is_empty()) {
                config.pre_shared_key = Some(Self::generate_key());
                config.save(path)?;
                println!("[relay] 已生成预共享密钥: {}", config.pre_shared_key.as_ref().unwrap());
            }
            Ok(config)
        } else {
            let config = Self {
                listen_address: default_listen_address(),
                pre_shared_key: Some(Self::generate_key()),
                heartbeat_interval_secs: default_heartbeat_interval(),
                heartbeat_timeout_secs: default_heartbeat_timeout(),
            };
            config.save(path)?;
            println!("[relay] 已创建配置文件: {}", path.display());
            println!("[relay] 预共享密钥: {}", config.pre_shared_key.as_ref().unwrap());
            Ok(config)
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 生成 256-bit 随机密钥，hex 编码
    pub fn generate_key() -> String {
        let mut key = [0u8; 32];
        rand::thread_rng().fill(&mut key);
        hex::encode(key)
    }

    /// 获取密钥的原始字节
    pub fn key_bytes(&self) -> Result<Vec<u8>> {
        let key_str = self.pre_shared_key.as_deref().unwrap_or("");
        if key_str.is_empty() {
            anyhow::bail!("预共享密钥未设置");
        }
        hex::decode(key_str).with_context(|| "预共享密钥格式错误，应为 hex 编码")
    }
}
```

### 5. `myowndesk-relay/src/auth.rs` — HMAC 认证

```rust
use constant_time_eq::constant_time_eq;
use ring::hmac;

/// 计算 HMAC-SHA256(预共享密钥, device_id)
pub fn compute_token(key: &[u8], device_id: &str) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&key, device_id.as_bytes());
    tag.as_ref().to_vec()
}

/// 验证 auth_token，使用 constant-time 比较防计时攻击
pub fn verify_token(key: &[u8], device_id: &str, token: &[u8]) -> bool {
    let expected = compute_token(key, device_id);
    constant_time_eq(&expected, token)
}
```

### 6. `myowndesk-relay/src/relay.rs` — 核心状态与逻辑

#### 6a. 数据结构

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// 设备条目
pub struct DeviceEntry {
    pub connection: quinn::Connection,
    pub paired_with: Option<String>,
    pub last_heartbeat: Instant,
}

/// 中继全局状态
pub struct RelayState {
    pub devices: HashMap<String, DeviceEntry>,
    pub pre_shared_key: Vec<u8>,
}

pub type SharedState = Arc<RwLock<RelayState>>;
```

#### 6b. 核心方法

```rust
use myowndesk_protocol::{ErrorCode, Message, PairResponse, RegisterResponse, PeerDisconnected};

impl RelayState {
    pub fn new(pre_shared_key: Vec<u8>) -> Self {
        Self {
            devices: HashMap::new(),
            pre_shared_key,
        }
    }

    /// 注册设备
    pub async fn register(
        &mut self,
        device_id: String,
        conn: quinn::Connection,
        auth_token: &[u8],
    ) -> Result<Vec<String>, ErrorCode> {
        // 验证 HMAC
        if !crate::auth::verify_token(&self.pre_shared_key, &device_id, auth_token) {
            return Err(ErrorCode::AuthFailed);
        }

        // 若 device_id 已存在（重连），通知旧对端并清理
        if let Some(old_entry) = self.devices.remove(&device_id) {
            if let Some(peer_id) = &old_entry.paired_with {
                // 通知旧对端
                if let Some(peer) = self.devices.get(peer_id) {
                    let msg = Message {
                        r#type: Some(myowndesk_protocol::message::Type::PeerDisconnected(
                            PeerDisconnected {
                                reason: "对端重新上线".to_string(),
                            },
                        )),
                    };
                    let _ = send_message_on_stream(&peer.connection, &msg).await;
                    // 清理对端的配对状态
                    if let Some(peer_entry) = self.devices.get_mut(peer_id) {
                        peer_entry.paired_with = None;
                    }
                }
            }
        }

        // 收集在线设备列表
        let online_devices: Vec<String> = self.devices.keys().cloned().collect();

        // 注册新连接
        self.devices.insert(device_id, DeviceEntry {
            connection: conn,
            paired_with: None,
            last_heartbeat: Instant::now(),
        });

        Ok(online_devices)
    }

    /// 发起配对
    pub async fn pair(
        &mut self,
        from_device: &str,
        target_device: &str,
    ) -> Result<(), ErrorCode> {
        // 不能自配对
        if from_device == target_device {
            return Err(ErrorCode::DeviceNotFound);
        }

        // 检查请求方是否已配对
        if let Some(entry) = self.devices.get(from_device) {
            if entry.paired_with.is_some() {
                return Err(ErrorCode::AlreadyPaired);
            }
        } else {
            return Err(ErrorCode::DeviceNotFound);
        }

        // 检查目标是否存在且未配对
        let target_entry = self.devices.get(target_device)
            .ok_or(ErrorCode::DeviceNotFound)?;
        if target_entry.paired_with.is_some() {
            return Err(ErrorCode::AlreadyPaired);
        }

        // 建立双向配对
        if let Some(entry) = self.devices.get_mut(from_device) {
            entry.paired_with = Some(target_device.to_string());
        }
        if let Some(entry) = self.devices.get_mut(target_device) {
            entry.paired_with = Some(from_device.to_string());
        }

        Ok(())
    }

    /// 断开配对（由 device_id 主动发起）
    pub async fn disconnect(&mut self, device_id: &str) -> Result<(), ErrorCode> {
        let peer_id = {
            let entry = self.devices.get(device_id).ok_or(ErrorCode::DeviceNotFound)?;
            entry.paired_with.clone()
        };

        // 通知对端
        if let Some(peer_id) = &peer_id {
            if let Some(peer) = self.devices.get(peer_id) {
                let msg = Message {
                    r#type: Some(myowndesk_protocol::message::Type::PeerDisconnected(
                        PeerDisconnected {
                            reason: "对端主动断开".to_string(),
                        },
                    )),
                };
                let _ = send_message_on_stream(&peer.connection, &msg).await;
                // 清理对端配对状态
                if let Some(entry) = self.devices.get_mut(peer_id) {
                    entry.paired_with = None;
                }
            }
        }

        // 清理本端配对状态
        if let Some(entry) = self.devices.get_mut(device_id) {
            entry.paired_with = None;
        }

        Ok(())
    }

    /// 移除设备（QUIC 连接断开时）
    pub async fn remove_device(&mut self, device_id: &str) {
        // 先通知对端
        if let Some(entry) = self.devices.get(device_id) {
            if let Some(peer_id) = &entry.paired_with {
                if let Some(peer) = self.devices.get(peer_id) {
                    let msg = Message {
                        r#type: Some(myowndesk_protocol::message::Type::PeerDisconnected(
                            PeerDisconnected {
                                reason: "对端离线".to_string(),
                            },
                        )),
                    };
                    let _ = send_message_on_stream(&peer.connection, &msg).await;
                    if let Some(entry) = self.devices.get_mut(peer_id) {
                        entry.paired_with = None;
                    }
                }
            }
        }
        self.devices.remove(device_id);
    }

    /// 更新心跳
    pub fn heartbeat(&mut self, device_id: &str) {
        if let Some(entry) = self.devices.get_mut(device_id) {
            entry.last_heartbeat = Instant::now();
        }
    }

    /// 获取超时设备列表
    pub fn stale_devices(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        self.devices
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_heartbeat) > timeout)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 转发 datagram 给对端
    pub async fn forward_datagram(&self, from_device: &str, data: &[u8]) -> Result<(), ErrorCode> {
        let peer_conn = {
            let entry = self.devices.get(from_device).ok_or(ErrorCode::DeviceNotFound)?;
            let peer_id = entry.paired_with.as_ref().ok_or(ErrorCode::DeviceNotFound)?;
            let peer = self.devices.get(peer_id).ok_or(ErrorCode::DeviceNotFound)?;
            peer.connection.clone()
        };
        peer_conn.send_datagram(bytes::Bytes::copy_from_slice(data))
            .map_err(|_| ErrorCode::Internal)?;
        Ok(())
    }

    /// 转发 stream 消息给对端
    pub async fn forward_stream_msg(&self, from_device: &str, msg: &Message) -> Result<(), ErrorCode> {
        let peer_conn = {
            let entry = self.devices.get(from_device).ok_or(ErrorCode::DeviceNotFound)?;
            let peer_id = entry.paired_with.as_ref().ok_or(ErrorCode::DeviceNotFound)?;
            let peer = self.devices.get(peer_id).ok_or(ErrorCode::DeviceNotFound)?;
            peer.connection.clone()
        };
        send_message_on_stream(&peer_conn, msg).await
            .map_err(|_| ErrorCode::Internal)?;
        Ok(())
    }
}

/// 在 stream 上发送长度前缀帧 + protobuf 消息
pub async fn send_message_on_stream(
    conn: &quinn::Connection,
    msg: &Message,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use prost::Message as _;
    let payload = msg.encode_to_vec();
    let len = (payload.len() as u32).to_le_bytes();

    let (mut send, _recv) = conn.open_bi().await?;
    send.write_all(&len).await?;
    send.write_all(&payload).await?;
    send.finish()?;
    Ok(())
}

/// 从 stream 读取长度前缀帧 + protobuf 消息
pub async fn read_message_from_stream(
    recv: &mut quinn::RecvStream,
) -> Result<Option<Message>, Box<dyn std::error::Error + Send + Sync>> {
    use prost::Message as _;

    // 读 4 字节长度
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) => {
            // stream 正常关闭
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(None)
            } else {
                Err(Box::new(e))
            };
        }
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    // 安全上限：防止恶意客户端声明巨大长度
    if len > 16 * 1024 * 1024 {
        return Err("消息长度超过 16MB 上限".into());
    }

    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await?;

    let msg = Message::decode(payload.as_slice())?;
    Ok(Some(msg))
}
```

### 7. `myowndesk-relay/src/server.rs` — QUIC 服务器

```rust
use crate::relay::{SharedState, RelayState, read_message_from_stream, send_message_on_stream};
use myowndesk_protocol::*;
use quinn::Endpoint;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

pub async fn run(config: crate::config::RelayConfig) -> anyhow::Result<()> {
    let key_bytes = config.key_bytes()?;
    let state: SharedState = Arc::new(RwLock::new(RelayState::new(key_bytes)));

    // 配置 TLS
    let (certs, key) = generate_self_signed_cert()?;
    let mut server_config = quinn::ServerConfig::with_single_cert(certs, key)?;
    // 允许客户端不验证证书（自签名场景）
    // 后续可通过预共享密钥做 TLS-PSK，Ticket-02 先用自签名证书

    let endpoint = Endpoint::server(server_config, config.listen_address.parse()?)?;
    info!("中继服务器已启动，监听 {}", config.listen_address);

    let heartbeat_interval = Duration::from_secs(config.heartbeat_interval_secs);
    let heartbeat_timeout = Duration::from_secs(config.heartbeat_timeout_secs);

    // 启动心跳清理任务
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let stale: Vec<String> = {
                let s = cleanup_state.read().await;
                s.stale_devices(heartbeat_timeout)
            };
            for device_id in stale {
                warn!("设备 {} 心跳超时，移除", device_id);
                cleanup_state.write().await.remove_device(&device_id).await;
            }
        }
    });

    // 主接受循环
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, state).await {
                error!("连接处理失败: {}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    state: SharedState,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    let remote_addr = connection.remote_address();
    info!("新连接: {}", remote_addr);

    // 等待客户端打开第一条双向 stream（用于 Register）
    let (send, mut recv) = connection.accept_bi().await?;

    // 读取第一条消息——必须是 Register
    let msg = read_message_from_stream(&mut recv).await?
        .ok_or_else(|| anyhow::anyhow!("连接在 Register 前关闭"))?;

    let register = match msg.r#type {
        Some(myowndesk_protocol::message::Type::Register(reg)) => reg,
        _ => {
            // 第一条消息不是 Register，拒绝
            let resp = Message {
                r#type: Some(myowndesk_protocol::message::Type::RegisterResponse(
                    RegisterResponse {
                        error_code: ErrorCode::AuthFailed as i32,
                        error_message: "请先发送 Register 消息".to_string(),
                        online_devices: vec![],
                    },
                )),
            };
            let _ = send_message_on_stream(&connection, &resp).await;
            return Ok(());
        }
    };

    // 验证并注册
    let device_id = register.device_id.clone();
    let auth_token = register.auth_token;

    let online_devices = {
        let mut s = state.write().await;
        match s.register(device_id.clone(), connection.clone(), &auth_token).await {
            Ok(devices) => devices,
            Err(error_code) => {
                let resp = Message {
                    r#type: Some(myowndesk_protocol::message::Type::RegisterResponse(
                        RegisterResponse {
                            error_code: error_code as i32,
                            error_message: match error_code {
                                ErrorCode::AuthFailed => "认证失败".to_string(),
                                _ => "注册失败".to_string(),
                            },
                            online_devices: vec![],
                        },
                    )),
                };
                let _ = send_message_on_stream(&connection, &resp).await;
                return Ok(());
            }
        }
    };

    // 发送注册成功响应
    let resp = Message {
        r#type: Some(myowndesk_protocol::message::Type::RegisterResponse(
            RegisterResponse {
                error_code: ErrorCode::Ok as i32,
                error_message: String::new(),
                online_devices,
            },
        )),
    };
    send_message_on_stream(&connection, &resp).await?;
    info!("设备 {} 注册成功", device_id);

    // 发送心跳 Ping 任务
    let ping_state = state.clone();
    let ping_device = device_id.clone();
    let ping_conn = connection.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let msg = Message {
                r#type: Some(myowndesk_protocol::message::Type::Ping(Ping {
                    timestamp_ms: chrono_now_ms(),
                })),
            };
            if send_message_on_stream(&ping_conn, &msg).await.is_err() {
                break; // 连接已断开
            }
        }
    });

    // 持续读取后续 stream 消息
    let stream_state = state.clone();
    let stream_device = device_id.clone();
    tokio::spawn(async move {
        // 注意：需要持续从连接的 stream 中读取
        // quinn 的 Connection 可以 accept_bi() 持续等待客户端新开的 stream
        loop {
            match connection.accept_bi().await {
                Ok((_send, mut recv)) => {
                    while let Some(msg) = read_message_from_stream(&mut recv).await.transpose() {
                        match msg {
                            Ok(msg) => {
                                handle_control_message(
                                    &stream_state,
                                    &stream_device,
                                    msg,
                                ).await;
                            }
                            Err(e) => {
                                warn!("消息解析失败: {}", e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("接受 stream 失败: {}", e);
                    break;
                }
            }
        }
        // 连接断开，清理
        info!("设备 {} 离线", stream_device);
        stream_state.write().await.remove_device(&stream_device).await;
    });

    // 持续读取 datagram 并转发
    let dgram_state = state.clone();
    let dgram_device = device_id.clone();
    tokio::spawn(async move {
        loop {
            match connection.read_datagram().await {
                Ok(data) => {
                    let _ = dgram_state.read().await
                        .forward_datagram(&dgram_device, &data).await;
                }
                Err(e) => {
                    warn!("datagram 读取失败: {}", e);
                    break;
                }
            }
        }
    });

    Ok(())
}

async fn handle_control_message(
    state: &SharedState,
    device_id: &str,
    msg: Message,
) {
    use myowndesk_protocol::message::Type;

    match msg.r#type {
        Some(Type::Pair(pair)) => {
            let result = state.write().await.pair(device_id, &pair.target_device_id).await;
            let conn = {
                let s = state.read().await;
                s.devices.get(device_id).map(|e| e.connection.clone())
            };
            if let Some(conn) = conn {
                let resp = Message {
                    r#type: Some(Type::PairResponse(PairResponse {
                        error_code: match result {
                            Ok(()) => ErrorCode::Ok as i32,
                            Err(e) => e as i32,
                        },
                        error_message: match result {
                            Ok(()) => String::new(),
                            Err(ErrorCode::DeviceNotFound) => "目标设备不在线".to_string(),
                            Err(ErrorCode::AlreadyPaired) => "设备已配对".to_string(),
                            _ => "配对失败".to_string(),
                        },
                    })),
                };
                let _ = send_message_on_stream(&conn, &resp).await;
            }
        }

        Some(Type::Disconnect(_)) => {
            let _ = state.write().await.disconnect(device_id).await;
        }

        Some(Type::Pong(_)) => {
            state.write().await.heartbeat(device_id);
        }

        // 以下消息转发给对端
        Some(Type::KeyEvent(_))
        | Some(Type::MouseEvent(_))
        | Some(Type::SwitchDisplay(_))
        | Some(Type::KeyFrameRequest(_)) => {
            let _ = state.read().await.forward_stream_msg(device_id, &msg).await;
        }

        _ => {
            // 未知消息类型，忽略（后续协议扩展兼容）
        }
    }
}

fn generate_self_signed_cert() -> anyhow::Result<(Vec<rustls::Certificate>, rustls::PrivateKey)> {
    // 生成自签名证书用于 QUIC TLS
    // 使用 rcgen 或手动构造
    // 注：Ticket-02 可先用 rcgen crate 生成，或在代码中硬编码开发用证书
    todo!("生成自签名证书")
}

fn chrono_now_ms() -> i64 {
    // 注：为避免引入 chrono 依赖，直接用 std::time
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
```

### 8. `myowndesk-relay/src/lib.rs` — Crate 入口

```rust
pub mod auth;
pub mod config;
pub mod relay;
pub mod server;
```

---

## 消息路由规则

中继收到控制消息后的分派逻辑：

| 消息类型 | 中继行为 | 说明 |
|---------|---------|------|
| `Register` | 处理（仅首条消息） | 验证 HMAC → 加入在线表 → 返回在线设备列表 |
| `Pair` | 处理 | 查找目标 → 双向绑定 → 返回 PairResponse |
| `Disconnect` | 处理 | 解绑 → 通知对端 PeerDisconnected |
| `Ping` | 回复 Pong | 中继主动发 Ping；客户端发来的 Ping 回复 Pong |
| `Pong` | 更新心跳时间 | 重置 last_heartbeat |
| `KeyEvent` | 转发给对端 | 原样转发，不解包 |
| `MouseEvent` | 转发给对端 | 原样转发，不解包 |
| `SwitchDisplay` | 转发给对端 | 原样转发 |
| `KeyFrameRequest` | 转发给对端 | 原样转发 |
| `DataPacket` | **不经过 stream**，走 datagram 直接转发 | 只转发 datagram payload，不解码 protobuf |
| `DeviceList` | 中继主动发送 | 设备上下线时向各客户端推送增量列表 |
| `PeerDisconnected` | 中继主动发送 | 通知设备其对端已离线 |

---

## 状态转换图

```
设备生命周期：

  ┌──────────┐  Register(OK)   ┌──────────┐
  │ 未连接    │ ───────────────► │  在线     │
  └──────────┘                  └─────┬────┘
                                      │
                          Pair(OK)    │    Disconnect / QUIC断开
                                      │
                                      ▼
                                ┌──────────┐
                                │  已配对   │
                                └──────────┘

  心跳超时 / QUIC 断开：任何状态 → 移除设备表
```

---

## 错误处理矩阵

| 场景                            | 错误码                | 中继行为                                                              |
| ----------------------------- | ------------------ | ----------------------------------------------------------------- |
| Register 时 `auth_token` 错误    | `AUTH_FAILED`      | 返回 `RegisterResponse{error_code=AUTH_FAILED}`，关闭该 stream，不断开 QUIC |
| Register 时 `device_id` 为空     | `AUTH_FAILED`      | 同上                                                                |
| 首条消息不是 Register               | `AUTH_FAILED`      | 返回 RegisterResponse 提示先注册                                         |
| Pair 时 `target_device_id` 不在线 | `DEVICE_NOT_FOUND` | 返回 `PairResponse{error_code=DEVICE_NOT_FOUND}`                    |
| Pair 时目标已配对                   | `ALREADY_PAIRED`   | 返回 `PairResponse{error_code=ALREADY_PAIRED}`                      |
| Pair 时请求方已配对                  | `ALREADY_PAIRED`   | 返回 `PairResponse{error_code=ALREADY_PAIRED}`                      |
| Pair 时目标是自己                   | `DEVICE_NOT_FOUND` | 返回错误                                                              |
| 已配对设备再次 Register（重连）          | —                  | 覆盖旧连接，通知旧对端 PeerDisconnected                                      |
| 心跳超时                          | —                  | 关闭 QUIC 连接，移除设备，通知对端                                              |
| 转发 datagram 时对端不存在            | —                  | 静默丢弃（视频帧不可靠传输）                                                    |

---

## 验证

### 编译

```bash
cargo build -p myowndesk-relay
cargo check -p myowndesk-relay
```

### 集成测试 `myowndesk-relay/tests/relay_test.rs`

按照 spec.md 测试哲学——通过真实 QUIC 连接进行协议层测试。在测试中启动本地中继实例（随机端口），模拟客户端发送 protobuf 消息并验证响应。

```rust
// 1. test_register_success — 正确 HMAC token → RegisterResponse(OK)
// 2. test_register_auth_failed — 错误 HMAC token → RegisterResponse(AUTH_FAILED)
// 3. test_register_duplicate — 同 device_id 两次注册 → 第二次覆盖第一次
// 4. test_pair_success — A 注册 → B 注册 → B Pair(target=A) → PairResponse(OK)
// 5. test_pair_device_not_found — Pair 不在线的设备 → PairResponse(DEVICE_NOT_FOUND)
// 6. test_pair_already_paired — 已配对设备再次 Pair → PairResponse(ALREADY_PAIRED)
// 7. test_forward_datagram — A↔B 配对后，A 发 datagram → B 收到相同数据
// 8. test_forward_stream_msg — A↔B 配对后，A 发 KeyEvent → B 收到相同 KeyEvent
// 9. test_disconnect_notifies_peer — A 发 Disconnect → B 收到 PeerDisconnected
// 10. test_heartbeat — 中继定时发 Ping，设备回复 Pong 维持在线
```

### 手动验证

```bash
cargo run -p myowndesk-relay
# 预期输出：
# [relay] 已创建配置文件: relay.toml
# [relay] 预共享密钥: a1b2c3d4e5f6...（32 bytes hex）
# [relay] 监听地址: 0.0.0.0:21117
# [relay] 中继服务器已启动
```

---

## 文件变更清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 修改 | `myowndesk-relay/Cargo.toml` | 添加 quinn、tokio、rustls、ring、prost、toml、serde、tracing、anyhow、rand、hex、constant_time_eq 等依赖；添加 `[[test]]` 或 `[dev-dependencies]` |
| 新建 | `myowndesk-relay/src/lib.rs` | Crate 入口，声明模块 |
| 修改 | `myowndesk-relay/src/main.rs` | 替换 TODO 为完整入口（配置加载 + 服务器启动） |
| 新建 | `myowndesk-relay/src/config.rs` | RelayConfig 结构体、TOML 加载/创建、密钥生成 |
| 新建 | `myowndesk-relay/src/auth.rs` | HMAC-SHA256 计算与 constant-time 验证 |
| 新建 | `myowndesk-relay/src/relay.rs` | RelayState、DeviceEntry、register/pair/disconnect/forward 方法 |
| 新建 | `myowndesk-relay/src/server.rs` | QUIC endpoint、连接处理、消息路由、心跳管理 |
| 新建 | `myowndesk-relay/relay.toml` | 默认配置文件模板 |
| 新建 | `myowndesk-relay/tests/relay_test.rs` | 10 个集成测试用例 |

---

## 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| quinn 自签名证书配置复杂 | 首次启动失败 | 使用 `rcgen` crate 动态生成自签名证书；后续 Ticket 可升级为 TLS-PSK |
| 测试中客户端未实现 | 无法端到端测试 | 集成测试中内联最小 QUIC 客户端（quinn::Endpoint::client），仅发/收消息 |
| RwLock 竞争 | 3-5 设备场景无影响，但架构上不够优雅 | 后续可替换为 `dashmap` 或 actor 模式 |
| 中继不解密数据的原则 | 后续加 E2E 加密时需确保 datagram payload 不透传解析 | 当前只转发 datagram bytes（不 decode 为 DataPacket），已满足"不解密"要求 |
| `chrono_now_ms()` 手工实现 | 不够优雅 | 仅一处使用，避免引入 chrono 依赖；后续可统一用 `std::time` |

---

## 依赖关系（Crate 层面）

| Crate | 用途 |
|-------|------|
| `quinn` 0.11 | QUIC 传输层（server endpoint、connection、stream、datagram） |
| `tokio` 1 | 异步运行时（quinn 底层依赖） |
| `rustls` 0.23 | TLS（quinn 底层依赖，需显式引入配证书） |
| `ring` 0.17 | HMAC-SHA256 认证 |
| `prost` 0.13 | Protobuf Message trait（encode/decode） |
| `bytes` 1 | 零拷贝字节缓冲 |
| `toml` 0.8 + `serde` 1 | 配置文件解析 |
| `tracing` 0.1 + `tracing-subscriber` 0.3 | 结构化日志 |
| `anyhow` 1 | 错误传播（main + config） |
| `rand` 0.8 | 密钥生成（CSPRNG） |
| `hex` 0.4 | 密钥 hex 编码/解码 |
| `constant_time_eq` 0.3 | 防计时攻击的 token 比较 |
| `rcgen` (可能) | 自签名证书生成 |
