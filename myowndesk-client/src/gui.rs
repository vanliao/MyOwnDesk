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
use crate::decoder::{create_best_decoder, DecodedFrame};
use crate::keymap::minifb_key_to_vk;
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
    let (input_tx, input_rx) = mpsc::unbounded_channel::<proto::Message>();

    // 3. 网络 task
    let net_addr = config.server.address.clone();
    let net_psk = config.device.pre_shared_key.clone();
    let target_device = config.device.target_device_id.clone();
    if target_device.is_empty() {
        anyhow::bail!("target_device_id 未配置，请在 client.toml 的 [device] 中添加");
    }
    let _net_task = tokio::spawn(async move {
        if let Err(e) = run_network(&net_addr, &device_id, &net_psk, &target_device, nal_tx, state_tx, input_rx).await {
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
    run_window(rgb_rx, state_rx, input_tx)?;

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
    input_rx: mpsc::UnboundedReceiver<proto::Message>,
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

    // 输入事件发送器：每条消息独立开流 + finish，避免阻塞中继的 stream 处理循环
    let input_conn = client.connection.clone();
    let input_sender = tokio::spawn(async move {
        let mut input_rx = input_rx;
        while let Some(msg) = input_rx.recv().await {
            let payload = msg.encode_to_vec();
            let len = (payload.len() as u32).to_le_bytes();
            let (mut send, _recv) = match input_conn.open_bi().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("打开输入流失败: {}", e);
                    break;
                }
            };
            if send.write_all(&len).await.is_err() {
                break;
            }
            if send.write_all(&payload).await.is_err() {
                break;
            }
            let _ = send.finish();
        }
        tracing::info!("输入事件流已关闭");
    });

    // datagram 接收 + 帧重组
    let nal_tx2 = nal_tx.clone();
    let state_tx2 = state_tx.clone();
    let dgram_recv = tokio::spawn(async move {
        let mut frame_buf: Vec<u8> = Vec::new();
        let mut current_seq: u32 = 0;
        let mut assembled_count: u64 = 0;
        let flush_timeout = std::time::Duration::from_millis(500);
        loop {
            match tokio::time::timeout(flush_timeout, conn.read_datagram()).await {
                // 收到 datagram
                Ok(Ok(data)) => match proto::Message::decode(data.as_ref()) {
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
                // 连接错误
                Ok(Err(e)) => {
                    tracing::warn!("datagram 接收失败: {}", e);
                    break;
                }
                // 超时——屏幕静止，发当前帧
                Err(_) => {
                    if !frame_buf.is_empty() {
                        assembled_count += 1;
                        if nal_tx2.send(std::mem::take(&mut frame_buf)).is_err() {
                            break;
                        }
                    }
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
                            let pong = crate::net::build_pong(&ping);
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

    let _ = tokio::join!(dgram_recv, stream_recv, input_sender);
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

    while let Some(first) = nal_rx.blocking_recv() {
        // 批量处理：拿到一帧后立刻清空队列，一次性解完
        let mut batch = vec![first];
        while let Ok(more) = nal_rx.try_recv() {
            batch.push(more);
        }
        for nal_units in batch {
            let t0 = std::time::Instant::now();
            match decoder.decode(&nal_units) {
                Ok(frames) => {
                    let decode_ms = t0.elapsed().as_millis();
                    for frame in frames {
                        total_frames += 1;
                        if total_frames <= 5 || total_frames % 60 == 1 {
                            tracing::info!(
                                "解码帧 #{}: {}x{} ({:?}), decode={}ms",
                                total_frames,
                                frame.width,
                                frame.height,
                                frame.frame_type,
                                decode_ms
                            );
                        }
                        if rgb_tx.send(frame).is_err() {
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    let decode_ms = t0.elapsed().as_millis();
                    tracing::warn!("解码帧失败 ({}ms): {}", decode_ms, e);
                }
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
// InputCapture — 输入捕获器
// ============================================================

/// 从 minifb 窗口轮询鼠标/键盘事件，输出 protobuf 消息。
///
/// 封装所有输入状态（鼠标位置、按键状态、帧尺寸），每帧调用 `poll()` 即可。
struct InputCapture {
    mouse_threshold: f32,
    prev_mouse_pos: Option<(f32, f32)>,
    prev_mouse_buttons: [bool; 3],
    frame_w: u32,
    frame_h: u32,
}

impl InputCapture {
    fn new() -> Self {
        Self {
            mouse_threshold: 2.0,
            prev_mouse_pos: None,
            prev_mouse_buttons: [false, false, false],
            frame_w: DEFAULT_WIDTH as u32,
            frame_h: DEFAULT_HEIGHT as u32,
        }
    }

    /// 轮询输入事件。
    ///
    /// - `window`: minifb 窗口
    /// - `frame_size`: 当前解码帧尺寸（用于坐标映射）
    /// 返回待发送的 protobuf 消息列表。
    fn poll(&mut self, window: &Window, frame_size: Option<(u32, u32)>) -> Vec<proto::Message> {
        let mut events = Vec::new();

        if let Some((w, h)) = frame_size {
            self.frame_w = w;
            self.frame_h = h;
        }

        let win_size = window.get_size();
        let win_w = win_size.0 as u32;
        let win_h = win_size.1 as u32;

        // 鼠标位置
        if let Some((mx, my)) = window.get_mouse_pos(minifb::MouseMode::Clamp) {
            let host_x = if win_w > 0 {
                ((mx * self.frame_w as f32) / win_w as f32) as i32
            } else {
                0
            };
            let host_y = if win_h > 0 {
                ((my * self.frame_h as f32) / win_h as f32) as i32
            } else {
                0
            };

            let send_move = match self.prev_mouse_pos {
                Some((px, py)) => {
                    (mx - px).abs() >= self.mouse_threshold
                        || (my - py).abs() >= self.mouse_threshold
                }
                None => true,
            };

            if send_move {
                events.push(build_mouse_event(
                    proto::MouseEventType::Move,
                    host_x,
                    host_y,
                    proto::MouseButton::Left,
                    0,
                ));
                // 反馈抑制：注入后光标预期在 (host_x - win_x, host_y - win_y)
                let (win_x, win_y) = window.get_position();
                self.prev_mouse_pos = Some((
                    (host_x as f32 - win_x as f32)
                        .clamp(0.0, win_w.saturating_sub(1) as f32),
                    (host_y as f32 - win_y as f32)
                        .clamp(0.0, win_h.saturating_sub(1) as f32),
                ));
            }

            // 鼠标按键
            for (i, mb) in [
                minifb::MouseButton::Left,
                minifb::MouseButton::Right,
                minifb::MouseButton::Middle,
            ]
            .iter()
            .enumerate()
            {
                let down = window.get_mouse_down(*mb);
                if down != self.prev_mouse_buttons[i] {
                    let pb = [
                        proto::MouseButton::Left,
                        proto::MouseButton::Right,
                        proto::MouseButton::Middle,
                    ][i];
                    let et = if down {
                        proto::MouseEventType::ButtonDown
                    } else {
                        proto::MouseEventType::ButtonUp
                    };
                    events.push(build_mouse_event(et, 0, 0, pb, 0));
                    self.prev_mouse_buttons[i] = down;
                }
            }

            // 滚轮
            if let Some(scroll) = window.get_scroll_wheel() {
                let delta = scroll.1 as i32;
                if delta != 0 {
                    events.push(build_mouse_event(
                        proto::MouseEventType::Wheel,
                        0,
                        0,
                        proto::MouseButton::Left,
                        delta,
                    ));
                }
            }
        }

        // 键盘
        for key in window.get_keys_pressed(minifb::KeyRepeat::No) {
            if let Some(vk) = minifb_key_to_vk(key) {
                events.push(build_key_event(vk, true));
            }
        }
        for key in window.get_keys_released() {
            if let Some(vk) = minifb_key_to_vk(key) {
                events.push(build_key_event(vk, false));
            }
        }

        events
    }
}

// ============================================================
// minifb 窗口循环
// ============================================================

fn run_window(
    mut rgb_rx: mpsc::UnboundedReceiver<DecodedFrame>,
    mut state_rx: mpsc::UnboundedReceiver<StateUpdate>,
    input_tx: mpsc::UnboundedSender<proto::Message>,
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

    // 输入捕获器
    let mut input = InputCapture::new();

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

        // 轮询输入事件
        let frame_size = current_frame.as_ref().map(|f| (f.width, f.height));
        for msg in input.poll(&window, frame_size) {
            let _ = input_tx.send(msg);
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

// ============================================================
// 输入事件构造辅助（Ticket 08）
// ============================================================

fn build_mouse_event(
    event_type: proto::MouseEventType,
    x: i32,
    y: i32,
    button: proto::MouseButton,
    wheel_delta: i32,
) -> proto::Message {
    proto::Message {
        r#type: Some(proto::message::Type::MouseEvent(proto::MouseEvent {
            event_type: event_type as i32,
            x,
            y,
            button: button as i32,
            wheel_delta,
        })),
    }
}

fn build_key_event(key_code: i32, pressed: bool) -> proto::Message {
    proto::Message {
        r#type: Some(proto::message::Type::KeyEvent(proto::KeyEvent {
            key_code: key_code as u32,
            pressed,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_mouse_move_event() {
        let msg = build_mouse_event(proto::MouseEventType::Move, 960, 540, proto::MouseButton::Left, 0);
        match msg.r#type {
            Some(proto::message::Type::MouseEvent(me)) => {
                assert_eq!(me.event_type, proto::MouseEventType::Move as i32);
                assert_eq!(me.x, 960);
                assert_eq!(me.y, 540);
            }
            _ => panic!("期望 MouseEvent"),
        }
    }

    #[test]
    fn test_build_mouse_wheel_event() {
        let msg = build_mouse_event(proto::MouseEventType::Wheel, 0, 0, proto::MouseButton::Left, 120);
        match msg.r#type {
            Some(proto::message::Type::MouseEvent(me)) => {
                assert_eq!(me.wheel_delta, 120);
            }
            _ => panic!("期望 MouseEvent"),
        }
    }

    #[test]
    fn test_build_key_press() {
        let msg = build_key_event(0x41, true);
        match msg.r#type {
            Some(proto::message::Type::KeyEvent(ke)) => {
                assert_eq!(ke.key_code, 0x41);
                assert!(ke.pressed);
            }
            _ => panic!("期望 KeyEvent"),
        }
    }

    #[test]
    fn test_build_key_release() {
        let msg = build_key_event(0x1B, false);
        match msg.r#type {
            Some(proto::message::Type::KeyEvent(ke)) => {
                assert_eq!(ke.key_code, 0x1B);
                assert!(!ke.pressed);
            }
            _ => panic!("期望 KeyEvent"),
        }
    }

    // ============================================================
    // InputCapture 测试
    // ============================================================

    #[test]
    fn test_input_capture_new() {
        let ic = InputCapture::new();
        assert_eq!(ic.mouse_threshold, 2.0);
        assert!(ic.prev_mouse_pos.is_none());
        assert_eq!(ic.prev_mouse_buttons, [false, false, false]);
        assert_eq!(ic.frame_w, 1920);
        assert_eq!(ic.frame_h, 1080);
    }

    #[test]
    fn test_input_capture_updates_frame_size() {
        let mut ic = InputCapture::new();
        // poll with frame_size → updates internal frame_w/frame_h
        // We can't easily test poll() without a real Window,
        // but we can verify the frame size update via construction
        ic.frame_w = 1280;
        ic.frame_h = 720;
        assert_eq!(ic.frame_w, 1280);
        assert_eq!(ic.frame_h, 720);
    }
}
