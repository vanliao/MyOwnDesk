//! Windows 服务模式入口。
//!
//! 启动 `--service` 时，创建 D3D11 设备、初始化屏幕捕获、
//! 在专用线程中运行 60fps 捕获循环，通过 channel 输出帧（供 Ticket-04 编码）。

use crate::capture::{CapturedFrame, ScreenDuplicator};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_SDK_VERSION, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ID3D11Device, ID3D11DeviceContext,
};

/// `--service` 入口
pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk 服务模式启动中...");

    let (device, context) = create_d3d11_device()?;
    let mut duplicator = ScreenDuplicator::new(&device, &context)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<CapturedFrame>();
    let running = Arc::new(AtomicBool::new(true));

    // ---- 捕获线程 ----
    let capture_handle = {
        let running = running.clone();
        std::thread::spawn(move || {
            capture_loop(&mut duplicator, tx, running);
        })
    };

    // ---- channel 消费（当前仅 trace，Ticket-04 接入编码器） ----
    let consumer_handle = tokio::spawn(async move {
        let mut frame_count: u64 = 0;
        while let Some(frame) = rx.recv().await {
            frame_count += 1;
            if frame_count % 60 == 1 {
                tracing::info!(
                    "帧 #{} {}x{} 显示索引 {} | channel backlog: {}",
                    frame_count,
                    frame.width,
                    frame.height,
                    frame.display_index,
                    rx.len()
                );
            }
        }
        tracing::info!("帧 channel 已关闭，共收到 {} 帧", frame_count);
    });

    tracing::info!("服务已启动，按 Ctrl+C 停止");

    // ---- 等待退出 ----
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(_) => {
            // SCM 环境下 Ctrl+C 不可用，阻塞等待 running 标志
            while running.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // ---- 清理 ----
    tracing::info!("正在停止服务...");
    running.store(false, Ordering::SeqCst);

    let _ = capture_handle.join();
    // rx 已在 consumer_handle 中被 move，等待 consumer 完成即可
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        consumer_handle,
    )
    .await;

    tracing::info!("服务已停止");
    Ok(())
}

/// 创建 D3D11 设备和即时上下文
fn create_d3d11_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_1];
    let mut device: Option<ID3D11Device> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = Default::default();
    let mut context: Option<ID3D11DeviceContext> = None;

    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

    unsafe {
        D3D11CreateDevice(
            None,                        // pAdapter
            D3D_DRIVER_TYPE_UNKNOWN,     // DriverType
            None,                        // Software
            flags,                       // Flags
            Some(&feature_levels),       // Feature Levels
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .map_err(|e| anyhow::anyhow!("D3D11CreateDevice 失败: {}", e))?;
    }

    let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 设备创建返回空"))?;
    let context = context.ok_or_else(|| anyhow::anyhow!("D3D11 上下文创建返回空"))?;

    tracing::info!("D3D11 设备已创建 (Feature Level: {:?})", feature_level);
    Ok((device, context))
}

/// 捕获循环（运行在专用 std::thread 中）
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
                if tx.send(frame).is_err() {
                    break;
                }
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
