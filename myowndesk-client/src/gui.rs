//! GUI 模式——硬编码直连视频解码与渲染。
//!
//! Ticket-06 MVP：启动后自动连接中继 → Register → Pair 到硬编码目标设备，
//! 接收 H.264 视频流 → 解码 → minifb 窗口渲染。
//!
//! # 架构
//!
//! ```text
//! #[tokio::main]
//!   ├── tokio::spawn → network task (QuicClient)
//!   ├── tokio::spawn_blocking → decoder task (VideoDecoder)
//!   └── minifb 窗口循环（主线程，阻塞）
//!         ├── try_recv: 收状态 + 解码帧
//!         ├── RGB24 → ARGB (0RGB)
//!         └── window.update_with_buffer()
//! ```

use crate::config::ClientConfig;
use crate::decoder::{create_best_decoder, DecodedFrame, OpenH264Decoder};
use crate::net::QuicClient;
use minifb::{Key, Window, WindowOptions};
use myowndesk_protocol as proto;
use prost::Message as _;
use tokio::sync::mpsc;

// ============================================================
// 常量
// ============================================================

/// 硬编码目标设备 ID（Ticket-06 MVP，后续 Ticket-09 替换）。

/// 默认窗口尺寸。
const DEFAULT_WIDTH: usize = 1920;
const DEFAULT_HEIGHT: usize = 1080;

// ============================================================
// 连接状态
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Connecting,
    Registering,
    Paired,
    Receiving,
    Disconnected,
    Error,
}

impl ConnectionState {
    fn label(&self) -> &'static str {
        match self {
            ConnectionState::Connecting => "正在连接中继...",
            ConnectionState::Registering => "正在注册...",
            ConnectionState::Paired => "配对成功，等待视频流...",
            ConnectionState::Receiving => "接收中",
            ConnectionState::Disconnected => "连接已断开",
            ConnectionState::Error => "错误",
        }
    }
}

enum StateUpdate {
    State(ConnectionState),
    Error(String),
}

// ============================================================
// 入口
// ============================================================

pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk GUI 模式启动中...");

    // 1. 加载配置
    let config = ClientConfig::load("client.toml")?;
    let device_id = format!("{}-gui", config.resolve_device_id());
    if config.device.pre_shared_key.is_empty() {
        anyhow::bail!("预共享密钥未配置，请编辑 client.toml 填写 pre_shared_key");
    }
    tracing::info!("设备 ID: {}", device_id);

    // 2. Channel
    let (nal_tx, mut nal_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (rgb_tx, rgb_rx) = mpsc::unbounded_channel::<DecodedFrame>();
    let (state_tx, state_rx) = mpsc::unbounded_channel::<StateUpdate>();

    // 3. 网络 task
    let net_addr = config.server.address.clone();
    let net_psk = config.device.pre_shared_key.clone();
    let target_device = config.device.target_device_id.clone();
    if target_device.is_empty() {
        anyhow::bail!("target_device_id 未配置，请在 client.toml 的 [device] 中添加");
    }
    let _net_task = tokio::spawn(async move {
        if let Err(e) = run_network(&net_addr, &device_id, &net_psk, &target_device, nal_tx, state_tx).await {
            tracing::error!("网络 task 异常退出: {}", e);
        }
        tracing::info!("网络 task 退出");
    });

    // 4. 解码 task
    let _decode_task = tokio::task::spawn_blocking(move || {
        if let Err(e) = run_decoder(&mut nal_rx, rgb_tx) {
            tracing::error!("解码 task 异常退出: {}", e);
        }
        tracing::info!("解码 task 退出");
    });

    // 5. minifb 窗口循环（阻塞直到关闭）
    run_window(rgb_rx, state_rx)?;

    tracing::info!("GUI 模式退出");
    Ok(())
}

// ============================================================
// 网络 task
// ============================================================

async fn run_network(
    server_addr: &str,
    device_id: &str,
    pre_shared_key: &str,
    target_device_id: &str,
    nal_tx: mpsc::UnboundedSender<Vec<u8>>,
    state_tx: mpsc::UnboundedSender<StateUpdate>,
) -> anyhow::Result<()> {
    let _ = state_tx.send(StateUpdate::State(ConnectionState::Connecting));

    let client = match QuicClient::connect(server_addr, device_id, pre_shared_key).await {
        Ok(c) => c,
        Err(e) => {
            let _ = state_tx.send(StateUpdate::Error(format!("连接中继失败: {}", e)));
            return Err(e);
        }
    };

    let _ = state_tx.send(StateUpdate::State(ConnectionState::Registering));

    match client.register().await {
        Ok(devices) => tracing::info!("注册成功, 在线设备: {:?}", devices),
        Err(e) => {
            let _ = state_tx.send(StateUpdate::Error(format!("注册失败: {}", e)));
            return Err(e);
        }
    };

    // 配对（请求-响应模式）
    let pair_msg = proto::Message {
        r#type: Some(proto::message::Type::Pair(proto::Pair {
            target_device_id: target_device_id.to_string(),
        })),
    };
    match client.request_response(&pair_msg).await {
        Ok(Some(msg)) => match msg.r#type {
            Some(proto::message::Type::PairResponse(r)) => {
                if r.error_code == proto::ErrorCode::Ok as i32 {
                    tracing::info!("配对成功");
                    let _ = state_tx.send(StateUpdate::State(ConnectionState::Paired));
                } else {
                    let err = format!("配对失败: {} (code {:?})", r.error_message, r.error_code);
                    let _ = state_tx.send(StateUpdate::Error(err.clone()));
                    anyhow::bail!(err);
                }
            }
            other => {
                let err = format!("配对响应格式错误: {:?}", other);
                let _ = state_tx.send(StateUpdate::Error(err.clone()));
                anyhow::bail!(err);
            }
        },
        Ok(None) => {
            let err = "连接关闭，未收到 PairResponse".to_string();
            let _ = state_tx.send(StateUpdate::Error(err.clone()));
            anyhow::bail!(err);
        }
        Err(e) => {
            let _ = state_tx.send(StateUpdate::Error(format!("Pair 失败: {}", e)));
            return Err(e);
        }
    }

    // 无条件请求关键帧
    let kf_msg = proto::Message {
        r#type: Some(proto::message::Type::KeyFrameRequest(
            proto::KeyFrameRequest { display_index: 0 },
        )),
    };
    if let Err(e) = client.send_message(&kf_msg).await {
        tracing::warn!("KeyFrameRequest 发送失败: {}", e);
    } else {
        tracing::info!("已发送 KeyFrameRequest");
    }

    let _ = state_tx.send(StateUpdate::State(ConnectionState::Receiving));

    let conn = client.connection.clone();

    // datagram 接收 + 帧重组
    let nal_tx2 = nal_tx.clone();
    let state_tx2 = state_tx.clone();
    let dgram_recv = tokio::spawn(async move {
        let mut frame_buf: Vec<u8> = Vec::new();
        let mut current_seq: u32 = 0;
        let mut assembled_count: u64 = 0;
        loop {
            match conn.read_datagram().await {
                Ok(data) => match proto::Message::decode(data.as_ref()) {
                    Ok(msg) => match msg.r#type {
                        Some(proto::message::Type::DataPacket(dp)) => {
                            if dp.frame_seq != current_seq && !frame_buf.is_empty() {
                                assembled_count += 1;
                                if assembled_count % 60 == 1 {
                                    tracing::info!(
                                        "帧重组 #{}: seq={}, {} bytes",
                                        assembled_count,
                                        current_seq,
                                        frame_buf.len()
                                    );
                                }
                                if nal_tx2.send(std::mem::take(&mut frame_buf)).is_err() {
                                    break;
                                }
                            }
                            current_seq = dp.frame_seq;
                            frame_buf.extend_from_slice(&dp.payload);
                        }
                        _ => {}
                    },
                    Err(_) => {}
                },
                Err(e) => {
                    tracing::warn!("datagram 接收失败: {}", e);
                    break;
                }
            }
        }
        // 冲刷最后一帧
        if !frame_buf.is_empty() {
            let _ = nal_tx2.send(frame_buf);
        }
        let _ = state_tx2.send(StateUpdate::State(ConnectionState::Disconnected));
    });

    // stream 接收
    let stream_recv = tokio::spawn(async move {
        loop {
            match client.recv_message().await {
                Ok(Some(msg)) => {
                    use proto::message::Type;
                    match msg.r#type {
                        Some(Type::PeerDisconnected(pd)) => {
                            tracing::warn!("对端已断开: {}", pd.reason);
                            let _ = state_tx.send(StateUpdate::State(ConnectionState::Disconnected));
                            break;
                        }
                        Some(Type::KeyFrameRequest(_)) => {}
                        Some(Type::Ping(ping)) => {
                            // 回复 Pong 维持心跳
                            let pong = proto::Message {
                                r#type: Some(Type::Pong(proto::Pong {
                                    timestamp_ms: ping.timestamp_ms,
                                })),
                            };
                            if let Err(e) = client.send_message(&pong).await {
                                tracing::warn!("发送 Pong 失败: {}", e);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => {
                    let _ = state_tx.send(StateUpdate::State(ConnectionState::Disconnected));
                    break;
                }
                Err(e) => {
                    let _ = state_tx.send(StateUpdate::Error(format!("接收消息失败: {}", e)));
                    break;
                }
            }
        }
    });

    let _ = tokio::join!(dgram_recv, stream_recv);
    Ok(())
}

// ============================================================
// 解码 task
// ============================================================

fn run_decoder(
    nal_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    rgb_tx: mpsc::UnboundedSender<DecodedFrame>,
) -> anyhow::Result<()> {
    let mut decoder = create_best_decoder()?;
    let mut total_frames: u64 = 0;
    let mut error_recovering = false;

    while let Some(nal_units) = nal_rx.blocking_recv() {
        // 错误恢复中：跳过非 IDR 帧直到下一个 keyframe
        if error_recovering {
            if OpenH264Decoder::contains_idr(&nal_units) {
                error_recovering = false;
                decoder = create_best_decoder()?;
                tracing::info!("解码器已恢复（收到新 keyframe）");
            } else {
                continue;
            }
        }

        match decoder.decode(&nal_units) {
            Ok(frames) => {
                for frame in frames {
                    total_frames += 1;
                    if total_frames % 60 == 1 {
                        tracing::info!(
                            "解码帧 #{}: {}x{} ({:?})",
                            total_frames,
                            frame.width,
                            frame.height,
                            frame.frame_type
                        );
                    }
                    if rgb_tx.send(frame).is_err() {
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码帧失败: {}，进入恢复模式", e);
                error_recovering = true;
            }
        }
    }

    match decoder.flush() {
        Ok(frames) => {
            for frame in frames {
                let _ = rgb_tx.send(frame);
            }
        }
        Err(e) => tracing::warn!("解码器 flush 失败: {}", e),
    }

    tracing::info!("解码 task 退出，共 {} 帧", total_frames);
    Ok(())
}

// ============================================================
// minifb 窗口循环
// ============================================================

fn run_window(
    mut rgb_rx: mpsc::UnboundedReceiver<DecodedFrame>,
    mut state_rx: mpsc::UnboundedReceiver<StateUpdate>,
) -> anyhow::Result<()> {
    let mut window = Window::new(
        "MyOwnDesk - 等待连接...",
        DEFAULT_WIDTH,
        DEFAULT_HEIGHT,
        WindowOptions::default(),
    )
    .map_err(|e| anyhow::anyhow!("创建窗口失败: {}", e))?;

    // 限制最大刷新率（避免空转时 100% CPU）
    window.set_target_fps(120);

    let mut current_frame: Option<DecodedFrame> = None;
    let mut _connection_state = ConnectionState::Connecting;
    let mut frame_count: u64 = 0;
    let mut last_frame_size = [0u32, 2];

    // 帧缓冲：32-bit ARGB (0RGB, 即 A 在最高字节)
    let mut buffer: Vec<u32> = vec![0u32; DEFAULT_WIDTH * DEFAULT_HEIGHT];

    while window.is_open() && !window.is_key_down(Key::Escape) {
        // 收集状态更新
        while let Ok(update) = state_rx.try_recv() {
            match update {
                StateUpdate::State(s) => {
                    _connection_state = s;
                    window.set_title(&format!("MyOwnDesk - {}", s.label()));
                }
                StateUpdate::Error(e) => {
                    tracing::error!("{}", e);
                    _connection_state = ConnectionState::Error;
                    window.set_title(&format!("MyOwnDesk - 错误: {}", e));
                }
            }
        }

        // 收集解码帧——只取最后一帧，跳过中间的 ARGB 转换
        {
            let mut latest: Option<DecodedFrame> = None;
            while let Ok(frame) = rgb_rx.try_recv() {
                frame_count += 1;
                latest = Some(frame);
            }
            if let Some(frame) = latest {
                let size_changed = last_frame_size != [frame.width, frame.height];
                last_frame_size = [frame.width, frame.height];
                if size_changed {
                    let new_len = (frame.width * frame.height) as usize;
                    if buffer.len() != new_len {
                        buffer.resize(new_len, 0u32);
                    }
                }
                // 只转换最后一帧
                rgb24_to_argb(&frame.rgb_data, frame.width, frame.height, &mut buffer);
                current_frame = Some(frame);
            }
        }

        // 更新窗口
        if current_frame.is_some() {
            let (w, h) = (
                last_frame_size[0] as usize,
                last_frame_size[1] as usize,
            );
            window
                .update_with_buffer(&buffer, w, h)
                .map_err(|e| anyhow::anyhow!("窗口更新失败: {}", e))?;
        } else {
            // 无帧时更新标题时间
            window.update();
        }

        // 让出 CPU 给 tokio 调度
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    tracing::info!("GUI 窗口关闭，共渲染 {} 帧", frame_count);
    Ok(())
}

// ============================================================
// RGB24 → ARGB (minifb u32 格式)
// ============================================================

/// 将 RGB24 像素转换为 minifb 的 u32 像素格式。
///
/// minifb 在 Windows 上使用 `0RGB` 格式（`A`=0 在最高字节）。
fn rgb24_to_argb(rgb24: &[u8], width: u32, height: u32, buffer: &mut [u32]) {
    let pixel_count = (width * height) as usize;
    for i in 0..pixel_count {
        let src = i * 3;
        // minifb u32 格式: 0xAARRGGBB（小端），0x00RRGGBB
        buffer[i] = ((rgb24[src] as u32) << 16)   // R
            | ((rgb24[src + 1] as u32) << 8)       // G
            | (rgb24[src + 2] as u32);              // B
    }
}
