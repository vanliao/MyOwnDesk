//! Windows 服务模式入口。
//!
//! 启动 `--service` 时，创建 D3D11 设备、初始化屏幕捕获、
//! 在专用线程中运行 60fps 捕获循环 → 编码 → 网络发送。
//!
//! # 架构
//!
//! 三个独立 actor 通过 channel 串联，`run()` 只负责编排：
//!
//! ```text
//! ScreenDuplicator → CaptureLoop [线程]
//!     │ bounded channel (cap 2)
//!     ▼
//! EncodeLoop [tokio task] ──→ EncodedFrame → QUIC datagram
//!     ▲
//!     └── keyframe_tx (来自 relay 的 KeyFrameRequest)
//! ```

use crate::capture::{CapturedFrame, FrameSource, ScreenDuplicator};
use crate::config::ClientConfig;
use crate::encoder::{self, EncodedFrame};
use crate::input::{DesktopInput, InputBackend};
use crate::net::{KeyFrameSender, QuicClient};
use myowndesk_protocol as proto;
use prost::Message as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_SDK_VERSION, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ID3D11Device, ID3D11DeviceContext,
};

// ============================================================
// 通道类型别名
// ============================================================

type CaptureSender = mpsc::Sender<CapturedFrame>;
type CaptureReceiver = mpsc::Receiver<CapturedFrame>;
type EncodeSender = mpsc::UnboundedSender<EncodedFrame>;
type EncodeReceiver = mpsc::UnboundedReceiver<EncodedFrame>;

// ============================================================
// CaptureLoop — 屏幕捕获循环
// ============================================================

/// 管理 DXGI 捕获线程的创建、运行、停止。
struct CaptureLoop {
    handle: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl CaptureLoop {
    /// 启动捕获线程。
    fn start(frame_source: Box<dyn FrameSource>, frame_tx: CaptureSender) -> Self {
        let shutdown = Arc::new(AtomicBool::new(true));
        let mut frame_source = frame_source; // 让闭包捕获，允许在 capture_loop 中可变借用
        let handle = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                capture_loop(&mut *frame_source, frame_tx, shutdown);
            })
        };
        Self {
            handle: Some(handle),
            shutdown,
        }
    }

    /// 发送停止信号并等待线程退出。
    fn stop(&mut self) {
        self.shutdown.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ============================================================
// EncodeLoop — 编码循环
// ============================================================

/// 管理 H.264 编码 task 的创建、运行、停止。
struct EncodeLoop {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl EncodeLoop {
    /// 启动编码 task，消费捕获帧、响应 keyframe 信号。
    fn start(
        capture_rx: CaptureReceiver,
        encode_tx: EncodeSender,
        keyframe_tx: KeyFrameSender,
        keyframe_rx: mpsc::UnboundedReceiver<()>,
    ) -> Self {
        let handle = tokio::spawn(async move {
            encode_task(capture_rx, encode_tx, keyframe_rx).await;
        });
        // keyframe_tx 给外部（network task）注入 KeyFrameRequest 信号
        let _ = keyframe_tx;
        Self {
            handle: Some(handle),
        }
    }

    /// 等待编码 task 完成（带超时）。
    async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
        }
    }
}

// ============================================================
// NetworkLoop — 网络循环
// ============================================================

/// 管理 QUIC 网络 task 的创建、运行、停止。
struct NetworkLoop {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl NetworkLoop {
    /// 启动网络 task：连接中继 → 注册 → 发送帧 → 接收控制消息。
    fn start(
        server_addr: String,
        device_id: String,
        pre_shared_key: String,
        encode_rx: EncodeReceiver,
        keyframe_tx: KeyFrameSender,
    ) -> Self {
        let handle = tokio::spawn(async move {
            network_task(server_addr, device_id, pre_shared_key, encode_rx, keyframe_tx).await;
        });
        Self {
            handle: Some(handle),
        }
    }

    /// 等待网络 task 完成（带超时）。
    async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
        }
    }
}

// ============================================================
// `--service` 入口
// ============================================================

/// 服务模式主入口——编排 capture / encode / network 三个 actor。
pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk 服务模式启动中...");

    // 1. 加载配置
    let config = ClientConfig::load("client.toml")?;
    let device_id = config.resolve_device_id();
    if config.device.pre_shared_key.is_empty() {
        anyhow::bail!("预共享密钥未配置，请编辑 client.toml 填写 pre_shared_key");
    }
    tracing::info!("设备 ID: {}", device_id);

    // 2. 创建 D3D11 设备 + 屏幕捕获
    let (device, context) = create_d3d11_device()?;
    let duplicator = ScreenDuplicator::new(&device, &context)?;

    // 3. 创建通道
    let (capture_tx, capture_rx) = mpsc::channel::<CapturedFrame>(2);
    let (encode_tx, encode_rx) = mpsc::unbounded_channel::<EncodedFrame>();
    let (keyframe_tx, keyframe_rx) = mpsc::unbounded_channel::<()>();

    // 4. 启动三个 actor（通过 Box<dyn FrameSource> 注入）
    let mut capture = CaptureLoop::start(Box::new(duplicator), capture_tx);
    let mut encode = EncodeLoop::start(capture_rx, encode_tx, keyframe_tx.clone(), keyframe_rx);
    let mut network = NetworkLoop::start(
        config.server.address.clone(),
        device_id,
        config.device.pre_shared_key.clone(),
        encode_rx,
        keyframe_tx,
    );

    // 5. 等待退出信号
    tracing::info!("服务已启动，按 Ctrl+C 停止");
    tokio::signal::ctrl_c().await.ok();

    // 6. 停止并等待
    tracing::info!("正在停止服务...");
    capture.stop();
    encode.stop().await;
    network.stop().await;
    tracing::info!("服务已停止");
    Ok(())
}

// ============================================================
// 编码 task
// ============================================================

/// 编码 task：消费捕获帧 → H.264 编码 → 输出到 channel。
///
/// 分两个阶段：
/// 1. 等待配对：收到首次 KeyFrameRequest 前，丢弃所有捕获帧（不浪费 CPU 编码）
/// 2. 正常编码：收到 KeyFrameRequest 后创建编码器开始编码
async fn encode_task(
    mut capture_rx: CaptureReceiver,
    encode_tx: EncodeSender,
    mut keyframe_rx: mpsc::UnboundedReceiver<()>,
) {
    // ---- 阶段 1：等待配对信号 ----
    tracing::info!("编码器等待配对信号...");
    loop {
        tokio::select! {
            Some(_) = keyframe_rx.recv() => {
                tracing::info!("收到配对信号，创建编码器");
                break;
            }
            Some(_) = capture_rx.recv() => {
                // 配对前丢弃捕获帧，不浪费 CPU 编码
            }
            else => {
                tracing::info!("编码 task 退出（未配对通道已关闭）");
                return;
            }
        }
    }

    // ---- 阶段 2：正常编码 ----
    let mut frame_count: u64 = 0;
    let mut encoder = match encoder::create_best_encoder(1920, 1080, 60) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("编码器初始化失败: {}", e);
            return;
        }
    };
    tracing::info!("编码器已就绪");

    // 首次 KeyFrameRequest 已消费，继续监听后续 keyframe 信号
    loop {
        tokio::select! {
            result = capture_rx.recv() => {
                match result {
                    Some(frame) => {
                        frame_count += 1;
                        if frame.cpu_buffer.is_empty() {
                            continue;
                        }
                        match encoder.encode(&frame) {
                            Ok(encoded_frames) => {
                                for ef in encoded_frames {
                                    if encode_tx.send(ef).is_err() {
                                        tracing::info!("编码输出通道已关闭");
                                        return;
                                    }
                                }
                                if frame_count % 60 == 1 {
                                    tracing::info!("帧 #{} 编码完成", frame_count);
                                }
                            }
                            Err(e) => tracing::error!("帧 #{} 编码失败: {}", frame_count, e),
                        }
                    }
                    None => {
                        tracing::info!("捕获通道已关闭，编码 task 退出，共 {} 帧", frame_count);
                        return;
                    }
                }
            }
            Some(_) = keyframe_rx.recv() => {
                encoder.request_keyframe();
            }
        }
    }
}

// ============================================================
// 网络 task
// ============================================================

/// 网络 task：连接中继 → 注册 → 发送编码帧 → 接收控制消息。
async fn network_task(
    server_addr: String,
    device_id: String,
    pre_shared_key: String,
    mut encode_rx: EncodeReceiver,
    keyframe_tx: KeyFrameSender,
) {
    let client = match QuicClient::connect(&server_addr, &device_id, &pre_shared_key).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("连接中继失败: {}", e);
            return;
        }
    };

    match client.register().await {
        Ok(devices) => tracing::info!("注册成功, 在线设备: {:?}", devices),
        Err(e) => {
            tracing::error!("注册失败: {}", e);
            return;
        }
    }

    let conn = client.connection.clone();

    // 创建桌面输入注入实例（Ticket 08）
    let input = DesktopInput::new();

    // 子 task A：datagram 发送
    let dgram_sender = tokio::spawn(async move {
        let mut frame_seq: u32 = 0;
        while let Some(frame) = encode_rx.recv().await {
            let frame_type = frame.frame_type as i32;
            frame_seq += 1;

            let nals: Vec<&[u8]> = split_nal_units(&frame.nal_units)
                .into_iter()
                .filter(|n| !n.is_empty())
                .collect();
            let total = nals.len() as u32;

            for (idx, nal) in nals.into_iter().enumerate() {
                let packet = proto::DataPacket {
                    frame_type,
                    display_index: frame.display_index,
                    payload: nal.to_vec(),
                    frame_seq,
                    fragment_index: idx as u32,
                    fragment_count: total,
                    ..Default::default()
                };
                let msg = proto::Message {
                    r#type: Some(proto::message::Type::DataPacket(packet)),
                };
                let data = msg.encode_to_vec();
                if conn.send_datagram(bytes::Bytes::copy_from_slice(&data)).is_err() {
                    tracing::warn!("发送 datagram 失败");
                    return;
                }
                // 每 20 个 datagram 暂停 1ms，避免 quinn 内部缓冲区溢出丢包
                if idx > 0 && idx % 20 == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
        // encode_rx 已关闭（编码完成），关闭连接以让 stream_receiver 退出
        tracing::info!("编码通道已关闭，关闭 QUIC 连接");
        conn.close(0u32.into(), b"shutdown");
    });

    // 子 task B：stream 消息接收
    let conn_clone = client.connection.clone();
    let kf_tx = keyframe_tx.clone();
    let mut input = input;
    let stream_receiver = tokio::spawn(async move {
        loop {
            match recv_stream_message(&conn_clone).await {
                Ok(Some((send, msg))) => {
                    handle_control_message(&conn_clone, msg, send, &kf_tx, &mut input).await;
                }
                Ok(None) => {
                    tracing::info!("中继连接已关闭");
                    break;
                }
                Err(e) => {
                    tracing::error!("接收消息失败: {}", e);
                    break;
                }
            }
        }
    });

    let _ = tokio::join!(dgram_sender, stream_receiver);
    tracing::info!("网络 task 退出");
}

// ============================================================
// stream 消息收发
// ============================================================

/// 从 QUIC 连接读取一条 stream 消息，返回 (send half, message)。
async fn recv_stream_message(
    conn: &quinn::Connection,
) -> anyhow::Result<Option<(quinn::SendStream, proto::Message)>> {
    let (send, mut recv) = conn.accept_bi().await?;
    let mut len_buf = [0u8; 4];
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
    Ok(Some((send, msg)))
}

/// 处理从中继接收的控制消息。
async fn handle_control_message(
    conn: &quinn::Connection,
    msg: proto::Message,
    _send: quinn::SendStream,
    keyframe_tx: &KeyFrameSender,
    input: &mut DesktopInput,
) {
    use proto::message::Type;
    match msg.r#type {
        Some(Type::KeyFrameRequest(_)) => {
            tracing::info!("收到 KeyFrameRequest，通知编码器");
            let _ = keyframe_tx.send(());
        }
        Some(Type::PeerDisconnected(pd)) => {
            tracing::warn!("对端已断开: {}", pd.reason);
        }
        Some(Type::KeyEvent(ke)) => {
            tracing::debug!("注入键盘: code={}, pressed={}", ke.key_code, ke.pressed);
            if let Err(e) = input.send_key(ke.key_code as i32, ke.pressed, false) {
                tracing::error!("键盘注入失败: {}", e);
            }
        }
        Some(Type::MouseEvent(me)) => {
            let event_type = me.event_type.try_into().unwrap_or(proto::MouseEventType::Move);
            match event_type {
                proto::MouseEventType::Move => {
                    if let Err(e) = input.send_mouse_move(me.x, me.y) {
                        tracing::error!("鼠标移动注入失败: {}", e);
                    }
                }
                proto::MouseEventType::ButtonDown | proto::MouseEventType::ButtonUp => {
                    let pressed = event_type == proto::MouseEventType::ButtonDown;
                    let button = me.button.try_into().unwrap_or(proto::MouseButton::Left);
                    if let Err(e) = input.send_mouse_button(button, pressed) {
                        tracing::error!("鼠标按键注入失败: {}", e);
                    }
                }
                proto::MouseEventType::Wheel => {
                    if let Err(e) = input.send_mouse_wheel(me.wheel_delta) {
                        tracing::error!("鼠标滚轮注入失败: {}", e);
                    }
                }
            }
        }
        Some(Type::Ping(ping)) => {
            let pong = crate::net::build_pong(&ping);
            let payload = pong.encode_to_vec();
            let len = (payload.len() as u32).to_le_bytes();
            if let Ok((mut s, _r)) = conn.open_bi().await {
                let _ = s.write_all(&len).await;
                let _ = s.write_all(&payload).await;
                let _ = s.finish();
            }
        }
        Some(other) => {
            tracing::debug!("未处理消息: {:?}", other);
        }
        None => {}
    }
}

// ============================================================
// D3D11 设备创建
// ============================================================

fn create_d3d11_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_1];
    let mut device: Option<ID3D11Device> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = Default::default();
    let mut context: Option<ID3D11DeviceContext> = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        D3D11CreateDevice(
            None, D3D_DRIVER_TYPE_HARDWARE, None, flags,
            Some(&feature_levels), D3D11_SDK_VERSION,
            Some(&mut device), Some(&mut feature_level), Some(&mut context),
        ).map_err(|e| anyhow::anyhow!("D3D11CreateDevice 失败: {}", e))?;
    }
    let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 设备创建返回空"))?;
    let context = context.ok_or_else(|| anyhow::anyhow!("D3D11 上下文创建返回空"))?;
    tracing::info!("D3D11 设备已创建");
    Ok((device, context))
}

// ============================================================
// 捕获循环
// ============================================================

fn capture_loop(
    frame_source: &mut dyn FrameSource,
    tx: mpsc::Sender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let frame_interval = Duration::from_micros(16667);
    let mut consecutive_failures: u32 = 0;
    while running.load(Ordering::SeqCst) {
        let frame_start = std::time::Instant::now();
        match frame_source.acquire_frame(50) {
            Ok(Some(frame)) => {
                consecutive_failures = 0;
                let _ = tx.try_send(frame);
            }
            Ok(None) => {}
            Err(e) => {
                consecutive_failures += 1;
                tracing::error!("捕获失败 (连续{}次): {}", consecutive_failures, e);
                if consecutive_failures > 3 {
                    tracing::warn!("重建 duplicator...");
                    if let Err(e) = frame_source.recreate() {
                        tracing::error!("重建失败: {}", e);
                        break;
                    }
                    consecutive_failures = 0;
                }
            }
        }
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        } else if elapsed > Duration::from_millis(200) {
            tracing::warn!("捕获帧耗时 {:?}（目标 {:?}）", elapsed, frame_interval);
        }
    }
    tracing::info!("捕获循环退出");
}

// ============================================================
// NAL 单元切分（Annex B 起始码分割）
// ============================================================

/// 将 Annex B 格式的 H.264 数据按起始码切分为一个个 NAL 单元。
fn split_nal_units(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return vec![];
    }

    let mut units = Vec::new();
    let mut start = 0usize;
    let len = data.len();

    for i in 0..len.saturating_sub(3) {
        if i + 3 < len
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
        {
            if start < i && i > 0 {
                units.push(&data[start..i]);
            }
            start = i;
        } else if data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x01
            && (i < 3 || data[i - 1] != 0x00)
        {
            if start < i && i > 0 {
                units.push(&data[start..i]);
            }
            start = i;
        }
    }

    if start < len {
        units.push(&data[start..len]);
    }

    units
}
