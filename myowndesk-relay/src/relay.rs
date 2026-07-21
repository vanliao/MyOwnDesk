//! 中继核心状态管理：设备表、配对、数据转发、心跳

use crate::auth;
use myowndesk_protocol::*;
use prost::Message as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ============================================================
// 数据结构
// ============================================================

/// 单个在线设备的运行时状态
pub struct DeviceEntry {
    pub connection: quinn::Connection,
    pub paired_with: Option<String>,
    pub last_heartbeat: Instant,
}

/// 中继全局共享状态
pub struct RelayState {
    pub devices: HashMap<String, DeviceEntry>,
    pub pre_shared_key: Vec<u8>,
}

pub type SharedState = Arc<RwLock<RelayState>>;

// ============================================================
// RelayState 实现
// ============================================================

impl RelayState {
    pub fn new(pre_shared_key: Vec<u8>) -> Self {
        Self {
            devices: HashMap::new(),
            pre_shared_key,
        }
    }

    // --------------------------------------------------------
    // Register — 设备注册
    // --------------------------------------------------------

    /// 注册设备。验证 HMAC token，加入在线表。
    /// 若 device_id 已存在（重连），覆盖旧连接并通知旧对端。
    /// 返回当前在线设备 ID 列表。
    pub async fn register(
        &mut self,
        device_id: String,
        conn: quinn::Connection,
        auth_token: &[u8],
    ) -> Result<Vec<String>, ErrorCode> {
        // 验证 HMAC
        if !auth::verify_token(&self.pre_shared_key, &device_id, auth_token) {
            return Err(ErrorCode::AuthFailed);
        }

        // 处理重连：清理旧连接，通知旧对端
        if let Some(old_entry) = self.devices.remove(&device_id) {
            if let Some(peer_id) = old_entry.paired_with.clone() {
                if let Some(peer) = self.devices.get(&peer_id) {
                    let msg = build_peer_disconnected("对端重新上线");
                    let _ = send_message(&peer.connection, &msg).await;
                }
                // 清理旧对端的配对状态
                if let Some(peer_entry) = self.devices.get_mut(&peer_id) {
                    peer_entry.paired_with = None;
                }
            }
        }

        // 收集在线设备 ID 列表
        let online_devices: Vec<String> = self.devices.keys().cloned().collect();

        // 插入新条目
        self.devices.insert(
            device_id,
            DeviceEntry {
                connection: conn,
                paired_with: None,
                last_heartbeat: Instant::now(),
            },
        );

        Ok(online_devices)
    }

    // --------------------------------------------------------
    // Pair — 配对
    // --------------------------------------------------------

    /// 发起配对。检查双方均存在且未配对，建立双向绑定。
    pub async fn pair(&mut self, from_device: &str, target_device: &str) -> Result<(), ErrorCode> {
        // 不能自配对
        if from_device == target_device {
            return Err(ErrorCode::DeviceNotFound);
        }

        // 检查请求方是否存在且未配对
        let from_entry = self
            .devices
            .get(from_device)
            .ok_or(ErrorCode::DeviceNotFound)?;
        if from_entry.paired_with.is_some() {
            return Err(ErrorCode::AlreadyPaired);
        }

        // 检查目标是否存在且未配对
        let target_entry = self
            .devices
            .get(target_device)
            .ok_or(ErrorCode::DeviceNotFound)?;
        if target_entry.paired_with.is_some() {
            return Err(ErrorCode::AlreadyPaired);
        }

        // 建立双向配对
        self.devices
            .get_mut(from_device)
            .unwrap()
            .paired_with = Some(target_device.to_string());
        self.devices
            .get_mut(target_device)
            .unwrap()
            .paired_with = Some(from_device.to_string());

        Ok(())
    }

    // --------------------------------------------------------
    // Disconnect — 断开配对
    // --------------------------------------------------------

    /// device_id 主动断开配对。解绑双方，通知对端。
    pub async fn disconnect(&mut self, device_id: &str) -> Result<(), ErrorCode> {
        let peer_id = {
            let entry = self
                .devices
                .get(device_id)
                .ok_or(ErrorCode::DeviceNotFound)?;
            entry.paired_with.clone()
        };

        // 通知对端
        if let Some(ref peer_id) = peer_id {
            if let Some(peer) = self.devices.get(peer_id) {
                let msg = build_peer_disconnected("对端主动断开");
                let _ = send_message(&peer.connection, &msg).await;
            }
            // 清理对端配对状态
            if let Some(entry) = self.devices.get_mut(peer_id) {
                entry.paired_with = None;
            }
        }

        // 清理本端配对状态
        if let Some(entry) = self.devices.get_mut(device_id) {
            entry.paired_with = None;
        }

        Ok(())
    }

    // --------------------------------------------------------
    // Remove — 移除设备（QUIC 断开 / 心跳超时）
    // --------------------------------------------------------

    /// 移除设备，清理配对关系并通知对端
    pub async fn remove_device(&mut self, device_id: &str) {
        if let Some(entry) = self.devices.get(device_id) {
            if let Some(peer_id) = entry.paired_with.clone() {
                if let Some(peer) = self.devices.get(&peer_id) {
                    let msg = build_peer_disconnected("对端离线");
                    let _ = send_message(&peer.connection, &msg).await;
                }
                if let Some(peer_entry) = self.devices.get_mut(&peer_id) {
                    peer_entry.paired_with = None;
                }
            }
        }
        self.devices.remove(device_id);
    }

    // --------------------------------------------------------
    // Heartbeat — 心跳管理
    // --------------------------------------------------------

    /// 更新设备心跳时间
    pub fn heartbeat(&mut self, device_id: &str) {
        if let Some(entry) = self.devices.get_mut(device_id) {
            entry.last_heartbeat = Instant::now();
        }
    }

    /// 获取心跳超时的设备 ID 列表
    pub fn stale_devices(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        self.devices
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_heartbeat) > timeout)
            .map(|(id, _)| id.clone())
            .collect()
    }

    // --------------------------------------------------------
    // Forward — 数据转发
    // --------------------------------------------------------

    /// 转发 datagram（原样字节）给对端
    pub async fn forward_datagram(
        &self,
        from_device: &str,
        data: &[u8],
    ) -> Result<(), ErrorCode> {
        let peer_conn = self.get_peer_connection(from_device)?;
        peer_conn
            .send_datagram(bytes::Bytes::copy_from_slice(data))
            .map_err(|_| ErrorCode::Internal)?;
        Ok(())
    }

    /// 转发已编码的 protobuf 消息给对端（用于 stream 消息转发）
    pub async fn forward_encoded_msg(
        &self,
        from_device: &str,
        encoded: &[u8],
    ) -> Result<(), ErrorCode> {
        let peer_conn = self.get_peer_connection(from_device)?;
        send_raw(&peer_conn, encoded)
            .await
            .map_err(|_| ErrorCode::Internal)?;
        Ok(())
    }

    /// 获取 from_device 的对端 connection
    fn get_peer_connection(&self, from_device: &str) -> Result<quinn::Connection, ErrorCode> {
        let entry = self
            .devices
            .get(from_device)
            .ok_or(ErrorCode::DeviceNotFound)?;
        let peer_id = entry
            .paired_with
            .as_ref()
            .ok_or(ErrorCode::DeviceNotFound)?;
        let peer_entry = self
            .devices
            .get(peer_id)
            .ok_or(ErrorCode::DeviceNotFound)?;
        Ok(peer_entry.connection.clone())
    }
}

// ============================================================
// 消息构造辅助
// ============================================================

/// 构造 PeerDisconnected 消息
pub fn build_peer_disconnected(reason: &str) -> Message {
    Message {
        r#type: Some(myowndesk_protocol::message::Type::PeerDisconnected(
            PeerDisconnected {
                reason: reason.to_string(),
            },
        )),
    }
}

/// 构造 RegisterResponse 消息
pub fn build_register_response(
    error_code: ErrorCode,
    error_message: String,
    online_devices: Vec<String>,
) -> Message {
    Message {
        r#type: Some(myowndesk_protocol::message::Type::RegisterResponse(
            RegisterResponse {
                error_code: error_code as i32,
                error_message,
                online_devices,
            },
        )),
    }
}

/// 构造 PairResponse 消息
pub fn build_pair_response(error_code: ErrorCode, error_message: String) -> Message {
    Message {
        r#type: Some(myowndesk_protocol::message::Type::PairResponse(
            PairResponse {
                error_code: error_code as i32,
                error_message,
            },
        )),
    }
}

/// 构造 Ping 消息
pub fn build_ping(timestamp_ms: i64) -> Message {
    Message {
        r#type: Some(myowndesk_protocol::message::Type::Ping(Ping { timestamp_ms })),
    }
}

// ============================================================
// Stream 帧读写
// ============================================================

/// 在 QUIC 连接上发送一条 protobuf 消息（打开新 stream，帧格式：4 字节 LE 长度 + 载荷）
pub async fn send_message(
    conn: &quinn::Connection,
    msg: &Message,
) -> anyhow::Result<()> {
    let payload = msg.encode_to_vec();
    send_raw(conn, &payload).await
}

/// 发送已编码的字节（帧格式：4 字节 LE 长度 + 载荷）
pub async fn send_raw(
    conn: &quinn::Connection,
    payload: &[u8],
) -> anyhow::Result<()> {
    let len_bytes = (payload.len() as u32).to_le_bytes();

    let (mut send, _recv) = conn.open_bi().await?;
    send.write_all(&len_bytes).await?;
    send.write_all(payload).await?;
    send.finish()?;
    Ok(())
}

/// 从 stream 读取一条帧（4 字节 LE 长度 + 载荷）并解码为 Message
/// 返回 None 表示 stream 正常关闭
pub async fn read_message(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<Option<Message>> {
    let payload = read_frame(recv).await?;
    match payload {
        Some(data) => {
            let msg = Message::decode(data.as_slice())?;
            Ok(Some(msg))
        }
        None => Ok(None),
    }
}

/// 从 stream 读取一帧的原始字节
/// 返回 None 表示 stream 正常关闭
pub async fn read_frame(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<Option<Vec<u8>>> {
    // 读 4 字节长度前缀
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(quinn::ReadExactError::ReadError(e)) => return Err(e.into()),
    }

    let len = u32::from_le_bytes(len_buf) as usize;

    // 安全上限：16 MB
    const MAX_MSG_LEN: usize = 16 * 1024 * 1024;
    if len > MAX_MSG_LEN {
        return Err(anyhow::anyhow!(
            "消息长度 {} 超过上限 {} 字节",
            len,
            MAX_MSG_LEN
        ));
    }

    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => {
            anyhow::anyhow!("消息被截断：期望 {} 字节", len)
        }
        quinn::ReadExactError::ReadError(e) => e.into(),
    })?;

    Ok(Some(payload))
}
