//! Windows 服务模式入口。
//!
//! 启动 `--service` 时，创建 D3D11 设备、初始化屏幕捕获、
//! 在专用线程中运行 60fps 捕获循环 → 编码 → 网络发送。

use crate::capture::{CapturedFrame, ScreenDuplicator};
use crate::config::ClientConfig;
use crate::encoder::{self, EncodedFrame};
use crate::net::{KeyFrameSender, QuicClient};
use myowndesk_protocol as proto;
use prost::Message as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_SDK_VERSION, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ID3D11Device, ID3D11DeviceContext,
};

/// 编码帧输出通道——供网络层消费
pub type EncodeSender = mpsc::UnboundedSender<EncodedFrame>;
pub type EncodeReceiver = mpsc::UnboundedReceiver<EncodedFrame>;

/// `--service` 入口
pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk 服务模式启动中...");

    // 1. 加载客户端配置
    let config = ClientConfig::load("client.toml")?;
    let device_id = config.resolve_device_id();
    if config.device.pre_shared_key.is_empty() {
        anyhow::bail!("预共享密钥未配置，请编辑 client.toml 填写 pre_shared_key");
    }
    tracing::info!("设备 ID: {}", device_id);

    // 2. 创建 D3D11 设备 + 屏幕捕获
    let (device, context) = create_d3d11_device()?;
    let mut duplicator = ScreenDuplicator::new(&device, &context)?;

    // 3. 创建通道
    let (capture_tx, mut capture_rx) = mpsc::unbounded_channel::<CapturedFrame>();
    let (encode_tx, mut encode_rx) = mpsc::unbounded_channel::<EncodedFrame>();
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel::<()>();
    let running = Arc::new(AtomicBool::new(true));

    // 4. 捕获线程
    let capture_handle = {
        let running = running.clone();
        std::thread::spawn(move || {
            capture_loop(&mut duplicator, capture_tx, running);
        })
    };

    // 5. 编码 task（消费捕获帧 → 编码，监听 keyframe 信号）
    let encoder_handle = tokio::spawn(async move {
        let mut frame_count: u64 = 0;
        let mut encoder = match encoder::create_best_encoder(1920, 1080, 60) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("编码器初始化失败: {}", e);
                return;
            }
        };
        tracing::info!("编码器已就绪");

        loop {
            tokio::select! {
                Some(frame) = capture_rx.recv() => {
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
                Some(_) = keyframe_rx.recv() => {
                    encoder.request_keyframe();
                }
                else => break,
            }
        }
        tracing::info!("编码 task 退出，共 {} 帧", frame_count);
    });

    // 6. 网络 task（连接中继 → 注册 → 发送帧 → 接收控制消息）
    let net_addr = config.server.address.clone();
    let net_psk = config.device.pre_shared_key.clone();
    let network_handle = tokio::spawn(async move {
        let client = match QuicClient::connect(&net_addr, &device_id, &net_psk).await {
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

        // 子 task A：datagram 发送
        let dgram_sender = tokio::spawn(async move {
            while let Some(frame) = encode_rx.recv().await {
                let packet = proto::DataPacket {
                    frame_type: match frame.frame_type {
                        encoder::FrameType::Keyframe => proto::FrameType::Keyframe as i32,
                        encoder::FrameType::Delta => proto::FrameType::Delta as i32,
                    },
                    display_index: frame.display_index,
                    payload: frame.nal_units,
                    ..Default::default()
                };
                let msg = proto::Message {
                    r#type: Some(proto::message::Type::DataPacket(packet)),
                };
                let data = msg.encode_to_vec();
                if conn.send_datagram(bytes::Bytes::copy_from_slice(&data)).is_err() {
                    tracing::warn!("发送 datagram 失败");
                    break;
                }
            }
        });

        // 子 task B：stream 消息接收
        let conn_clone = client.connection.clone();
        let kf_tx = keyframe_tx.clone();
        let stream_receiver = tokio::spawn(async move {
            loop {
                match recv_stream_message(&conn_clone).await {
                    Ok(Some(msg)) => handle_control_message(msg, &kf_tx),
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
    });

    // 7. 等待退出信号
    tracing::info!("服务已启动，按 Ctrl+C 停止");
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(_) => {
            while running.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // 8. 清理
    tracing::info!("正在停止服务...");
    running.store(false, Ordering::SeqCst);

    let _ = capture_handle.join();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), encoder_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), network_handle).await;
    tracing::info!("服务已停止");
    Ok(())
}

/// 从 QUIC 连接读取一条 stream 消息（4 字节 LE 长度前缀 + protobuf）
async fn recv_stream_message(conn: &quinn::Connection) -> anyhow::Result<Option<proto::Message>> {
    let (_send, mut recv) = conn.accept_bi().await?;
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
    Ok(Some(msg))
}

/// 处理从中继接收的控制消息
fn handle_control_message(msg: proto::Message, keyframe_tx: &KeyFrameSender) {
    use proto::message::Type;
    match msg.r#type {
        Some(Type::KeyFrameRequest(_)) => {
            let _ = keyframe_tx.send(());
        }
        Some(Type::PeerDisconnected(pd)) => {
            tracing::warn!("对端已断开: {}", pd.reason);
            // TODO: Ticket-11 触发锁屏
        }
        Some(Type::KeyEvent(_)) | Some(Type::MouseEvent(_)) => {
            tracing::debug!("收到输入事件（暂未处理）");
            // TODO: Ticket-08 输入注入
        }
        Some(Type::Ping(_)) => {
            tracing::debug!("收到 Ping");
        }
        Some(other) => {
            tracing::debug!("未处理消息: {:?}", other);
        }
        None => {}
    }
}

// ============================================================
// D3D11 设备和捕获循环（不变）
// ============================================================

fn create_d3d11_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_1];
    let mut device: Option<ID3D11Device> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = Default::default();
    let mut context: Option<ID3D11DeviceContext> = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        D3D11CreateDevice(
            None, D3D_DRIVER_TYPE_UNKNOWN, None, flags,
            Some(&feature_levels), D3D11_SDK_VERSION,
            Some(&mut device), Some(&mut feature_level), Some(&mut context),
        ).map_err(|e| anyhow::anyhow!("D3D11CreateDevice 失败: {}", e))?;
    }
    let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 设备创建返回空"))?;
    let context = context.ok_or_else(|| anyhow::anyhow!("D3D11 上下文创建返回空"))?;
    tracing::info!("D3D11 设备已创建");
    Ok((device, context))
}

fn capture_loop(
    duplicator: &mut ScreenDuplicator,
    tx: mpsc::UnboundedSender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let frame_interval = std::time::Duration::from_micros(16667);
    let mut consecutive_failures: u32 = 0;
    while running.load(Ordering::SeqCst) {
        let frame_start = std::time::Instant::now();
        match duplicator.acquire_frame(50) {
            Ok(Some(frame)) => {
                consecutive_failures = 0;
                if tx.send(frame).is_err() { break; }
            }
            Ok(None) => {}
            Err(e) => {
                consecutive_failures += 1;
                tracing::error!("捕获失败 (连续{}次): {}", consecutive_failures, e);
                if consecutive_failures > 3 {
                    tracing::warn!("重建 duplicator...");
                    if let Err(e) = duplicator.recreate() {
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
        } else if elapsed > frame_interval * 2 {
            tracing::warn!("捕获帧耗时 {:?}（目标 {:?}）", elapsed, frame_interval);
        }
    }
    tracing::info!("捕获循环退出");
}
