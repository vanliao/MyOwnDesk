# Ticket-03: DXGI 屏幕捕获 + Windows 服务

## Context

MyOwnDesk 项目已完成 Ticket-01（协议定义）和 Ticket-02（中继服务器）。Ticket-03 是客户端侧第一个功能模块——Windows 服务模式下通过 DXGI Desktop Duplication API 捕获主显示器画面，以 60fps 输出 D3D11 纹理到 channel，等待 Ticket-04 的编码器消费。

**依赖关系**：Ticket-03 仅依赖 Ticket-01（协议定义），是 Ticket-04（H.264 编码）的**直接前置条件**。

**当前状态**：`myowndesk-client` 仅有骨架——`main.rs` 是普通 `fn main()`（非 `#[tokio::main]`），`lib.rs` 为空文件含注释，`Cargo.toml` 仅依赖 `myowndesk-protocol`。无 `capture/`、`service/` 子模块。

---

## 已确认决策（Grilling 结论）

| # | 决策点 | 结论 | 理由 |
|---|--------|------|------|
| 1 | Windows 服务实现 | `windows-service` crate | API 简洁，避免手写 Win32 SCM 代码 |
| 2 | D3D11 设备管理 | Service 层统一创建 `ID3D11Device`，注入给 capture 和后续 encoder | 零拷贝纹理传递，避免 Ticket-04 重构 |
| 3 | Channel 传递内容 | `CapturedFrame` struct（含 texture、display_index、timestamp 等元数据） | encoder 拿完整信息，无需带内通信 |
| 4 | 帧率控制 | `AcquireNextFrame(50ms timeout)` + sleep 到 ~16.6ms | 高刷屏降采样到 60fps，正常屏自然对齐 |
| 5 | 服务生命周期 | 完整 SCM 服务（install/run/stop/uninstall） | tickets.md 明确要求，一步到位 |
| 6 | D3D11 + tokio 桥梁 | 专用 `std::thread` 运行捕获循环，`tokio::sync::mpsc::UnboundedSender` 跨线程 | D3D11 设备绑定线程，不能依赖 `spawn_blocking` |
| 7 | `main.rs` 改造 | `#[tokio::main]` + 模块路由 | 确认 |
| 8 | 纹理输出格式 | BGRA (`DXGI_FORMAT_B8G8R8A8_UNORM`) 直出，**由 Ticket-04 的 FFmpeg 做 GPU 格式转换** | AMD GPU 的 D3D11 VideoProcessor 支持不稳定，FFmpeg 针对每种 GPU 有最优转换路径 |

---

## 与 Ticket-04 的接口约定

Ticket-03 和 Ticket-04 之间有明确的合约边界。以下约定需在 Ticket-04 实现时遵守：

### 3.1 D3D11 设备共享

```
service::run()
  ├── 创建 ID3D11Device + ID3D11DeviceContext  ← 上层统一创建
  ├── ScreenDuplicator::new(&device)            ← 注入给 capture
  └── [Ticket-04] VideoEncoder::new(&device)    ← 注入给 encoder（同一设备）
```

**约束**：
- `ID3D11Device` 在 `service.rs` 中创建，通过引用传给 capture 和 encoder
- encoder 不自行创建设备——使用注入的设备，确保纹理零拷贝共享
- 若 encoder 需要不同设备（如 CUDA），由 encoder 内部自行处理跨设备拷贝

### 3.2 Channel 数据契约

```rust
/// capture → encoder 的帧传输 channel
/// Ticket-03 创建，Ticket-04 消费
pub type FrameSender = tokio::sync::mpsc::UnboundedSender<CapturedFrame>;
pub type FrameReceiver = tokio::sync::mpsc::UnboundedReceiver<CapturedFrame>;
```

**约束**：
- `service::run()` 创建 channel，`tx` 传给 capture 线程，`rx` 留给 encoder（Ticket-04）
- 当前 `rx` 为空消费——Ticket-04 接入后替换为实际编码逻辑
- channel 选择 `unbounded`：捕获线程不应因编码慢而被阻塞，让下游自己控制背压

### 3.3 `CapturedFrame` 结构体

```rust
pub struct CapturedFrame {
    /// D3D11 纹理，格式: DXGI_FORMAT_B8G8R8A8_UNORM
    pub texture: ID3D11Texture2D,
    /// 显示器索引（0 = 主屏）
    pub display_index: u32,
    /// 捕获时间戳（用于编码器计算 PTS）
    pub timestamp: Instant,
    /// 纹理宽度
    pub width: u32,
    /// 纹理高度
    pub height: u32,
}
```

**Ticket-04 消费时的注意事项**：
- 纹理格式为 **BGRA**，NVENC/QSV/AMF 偏好 **NV12**——encoder 必须通过 FFmpeg 做格式转换
- `timestamp` 用于计算编码 PTS，保持音视频同步（后续加音频时用）
- `display_index` 用于构造 `DataPacket.display_index` 字段
- 纹理需在 encoder 用完后再释放——使用引用计数（COM AddRef/Release），capture 线程 `ReleaseFrame` 后 encoder 仍持有引用

### 3.4 纹理生命周期

```
capture 线程                          encoder (tokio task)
  │                                      │
  ├── AcquireNextFrame()                 │
  ├── MapDesktopSurface()                │
  ├── CopyResource(dst_texture)   ──►   │ （dst_texture 的引用通过 channel 传递）
  ├── ReleaseFrame()                     │
  │                                      ├── FFmpeg 使用 dst_texture 编码
  │                                      └── 编码完成后释放纹理
```

**约束**：
- `MapDesktopSurface` 返回的表面在 `ReleaseFrame` 后失效——**必须先复制到自有纹理再 Release**
- 复制使用 `ID3D11DeviceContext::CopyResource`（全量复制，简单可靠）
- Ticket-04 的 encoder 拿到纹理后不立即编码也没问题——自有纹理，不依赖 capture 线程

---

## 架构概览

```
myowndesk-client.exe --service
│
├── main.rs: #[tokio::main] 解析参数，路由到 service
│
└── service::run()
    │
    ├── 注册 SCM 服务控制处理器（start/stop）
    │
    ├── [SCM start 回调]
    │     ├── 创建 ID3D11Device + ID3D11DeviceContext
    │     ├── 创建 ScreenDuplicator(device) → 枚举显示器，选主屏
    │     ├── 创建 UnboundedSender<CapturedFrame> / UnboundedReceiver<CapturedFrame>
    │     │
    │     ├── spawn 专用线程: capture_loop(duplicator, tx, running)
    │     │     └── 每 ~16.6ms:
    │     │           ├── AcquireNextFrame(timeout=50ms) → 桌面纹理
    │     │           ├── CopyResource → 自有纹理
    │     │           ├── ReleaseFrame()
    │     │           ├── tx.send(CapturedFrame { ... })
    │     │           └── sleep 到 16.6ms 边界
    │     │
    │     └── tokio::spawn: 从 rx 消费帧（当前仅 trace 帧序号，Ticket-04 接入编码）
    │
    └── [SCM stop 回调]
          ├── running.store(false)
          ├── capture_thread.join()
          └── 释放 D3D11 资源
```

---

## Deliverables

### 1. 修改 `myowndesk-client/Cargo.toml`

```toml
[package]
name = "myowndesk-client"
version = "0.1.0"
edition = "2021"

[dependencies]
myowndesk-protocol = { path = "../myowndesk-protocol" }
tokio = { version = "1", features = ["full"] }
windows-rs = { version = "0.58", features = [
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_System_SystemServices",
]}
windows-service = "0.7"
tracing = "0.1"
tracing-subscriber = "0.3"
anyhow = "1"
```

### 2. 修改 `myowndesk-client/src/lib.rs`

```rust
pub mod capture;
pub mod service;
```

### 3. 新建 `myowndesk-client/src/capture.rs`

`ScreenDuplicator`、`CapturedFrame`、`FrameType`。

```rust
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_SDK_VERSION, D3D11_CREATE_DEVICE_FLAG,
};
use windows::Win32::Graphics::Dxgi::*;

// ============================================================
// 数据类型
// ============================================================

/// 捕获帧——通过 channel 传递给编码器（Ticket-04）
pub struct CapturedFrame {
    /// D3D11 纹理，格式: DXGI_FORMAT_B8G8R8A8_UNORM
    pub texture: ID3D11Texture2D,
    /// 显示器索引（0 = 主屏）
    pub display_index: u32,
    /// 捕获时间戳
    pub timestamp: Instant,
    /// 纹理宽度
    pub width: u32,
    /// 纹理高度
    pub height: u32,
}

/// DXGI Desktop Duplication 屏幕捕获器
///
/// **线程安全**：不是 Send/Sync——绑定到创建时的 D3D11 线程。
/// 必须在同一 `std::thread` 中完成所有操作。
pub struct ScreenDuplicator {
    duplication: IDXGIOutputDuplication,
    output_desc: DXGI_OUTPUT_DESC,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
}

// ============================================================
// impl ScreenDuplicator
// ============================================================

impl ScreenDuplicator {
    /// 枚举显示器，选择主显示器，创建 Duplication 实例
    ///
    /// - `device`: 注入的 D3D11 设备
    /// - 选择第一个 `IsActive` 且 `AttachedToDesktop` 的显示器
    pub fn new(device: &ID3D11Device) -> anyhow::Result<Self> {
        // 1. 创建 DXGI Factory
        //    CreateDXGIFactory1(&IDXGIFactory1::uuid()) → IDXGIFactory1
        //
        // 2. 枚举 Adapter
        //    factory.EnumAdapters1(0) → IDXGIAdapter1
        //
        // 3. 枚举 Output
        //    adapter.EnumOutputs(0) → IDXGIOutput
        //
        // 4. 获取 Output 描述
        //    output.GetDesc() → DXGI_OUTPUT_DESC {
        //        DeviceName: 如 "\\.\DISPLAY1"
        //        DesktopCoordinates: RECT { left, top, right, bottom }
        //        AttachedToDesktop: bool
        //    }
        //
        // 5. 创建 Duplication
        //    output.DuplicateOutput(device) → IDXGIOutputDuplication
        //
        // 6. 获取 D3D11 context
        //    device.GetImmediateContext() → ID3D11DeviceContext

        todo!("DXGI 初始化")
    }

    /// 获取下一帧
    ///
    /// - `timeout_ms`: 等待超时（毫秒），推荐 50ms
    /// - 返回 `Some(frame)` 表示新帧
    /// - 返回 `None` 表示超时无新帧
    /// - 返回 `Err` 表示 DXGI 错误（需重建 duplicator）
    pub fn acquire_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        // 1. duplication.AcquireNextFrame(timeout_ms)?
        //    - 正常: Ok(())
        //    - 超时: Err(DXGI_ERROR_WAIT_TIMEOUT) → return Ok(None)
        //    - 失去访问: Err(DXGI_ERROR_ACCESS_LOST) → 需要重建 duplicator
        //
        // 2. 获取帧信息
        //    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        //    duplication.GetFrameInfo(&mut frame_info)?
        //
        // 3. 映射桌面表面
        //    let mut mapped_rect = RECT::default();
        //    duplication.MapDesktopSurface(&mut mapped_rect, &mut surface)?
        //
        // 4. surface 转 ID3D11Texture2D
        //    IDXGISurface → QueryInterface → ID3D11Texture2D（桌面纹理）
        //
        // 5. 复制到自有纹理（必须在 ReleaseFrame 前完成）
        //    context.CopyResource(&self.owned_texture, &desktop_texture)
        //
        // 6. 释放帧
        //    duplication.ReleaseFrame()?
        //
        // 7. 构造 CapturedFrame
        //    CapturedFrame {
        //        texture: self.owned_texture.clone(),
        //        display_index: 0,
        //        timestamp: Instant::now(),
        //        width: self.width,
        //        height: self.height,
        //    }

        todo!("帧捕获")
    }

    /// 重建 duplicator（Access Lost 后调用）
    pub fn recreate(&mut self) -> anyhow::Result<()> {
        // 释放旧 duplicator，重新调用 DuplicateOutput
        todo!()
    }
}
```

### 4. 新建 `myowndesk-client/src/service.rs`

Windows 服务生命周期管理、D3D11 设备创建、捕获线程启动。

```rust
use crate::capture::{CapturedFrame, ScreenDuplicator};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// SCM 启动 --service 时的入口
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("MyOwnDesk 服务启动中...");

    // 创建 D3D11 设备（共享给 capture 和未来的 encoder）
    let (device, context) = create_d3d11_device()?;

    // 创建屏幕捕获器
    let mut duplicator = ScreenDuplicator::new(&device)?;

    // 创建帧 channel
    // tx → capture 线程
    // rx → 当前 trace 帧序号，Ticket-04 替换为 encoder 消费
    let (tx, mut rx) = mpsc::unbounded_channel::<CapturedFrame>();

    let running = Arc::new(AtomicBool::new(true));

    // 启动捕获线程
    let capture_handle = {
        let running = running.clone();
        std::thread::spawn(move || {
            capture_loop(&mut duplicator, tx, running);
        })
    };

    // 消费 channel（当前仅 trace，Ticket-04 在此接入 encoder）
    let consumer_handle = tokio::spawn(async move {
        let mut frame_count: u64 = 0;
        while let Some(frame) = rx.recv().await {
            frame_count += 1;
            if frame_count % 60 == 1 {
                // 每 60 帧（~1 秒）打印一次
                tracing::info!(
                    "捕获帧 #{} {}x{} 显示索引 {}",
                    frame_count,
                    frame.width,
                    frame.height,
                    frame.display_index
                );
            }
        }
        tracing::info!("帧 channel 关闭，共收到 {} 帧", frame_count);
    });

    // ============================================================
    // TODO: 接入 windows-service SCM 事件循环
    // 当前简化版：等待 Ctrl+C 退出
    // ============================================================
    tracing::info!("服务已启动，按 Ctrl+C 停止");

    // 等待中断信号
    tokio::signal::ctrl_c().await?;

    // 清理
    tracing::info!("正在停止服务...");
    running.store(false, Ordering::SeqCst);

    // 等待捕获线程退出
    let _ = capture_handle.join();
    // 关闭 channel → consumer 退出
    drop(rx); // 实际上 rx 已在上面 moved，不需要显式 drop
    let _ = consumer_handle.await;

    tracing::info!("服务已停止");
    Ok(())
}

fn create_d3d11_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    // D3D11CreateDevice(
    //     pAdapter: None（默认 adapter）
    //     DriverType: D3D_DRIVER_TYPE_UNKNOWN（自动选择硬件/软件）
    //     Flags: D3D11_CREATE_DEVICE_BGRA_SUPPORT（BGRA 纹理支持）
    //     pFeatureLevel: D3D_FEATURE_LEVEL_11_1
    // )
    todo!()
}

fn capture_loop(
    duplicator: &mut ScreenDuplicator,
    tx: mpsc::UnboundedSender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let frame_interval = std::time::Duration::from_micros(16667); // ~60fps
    let mut consecutive_failures = 0u32;

    while running.load(Ordering::SeqCst) {
        let frame_start = std::time::Instant::now();

        match duplicator.acquire_frame(50) {
            Ok(Some(frame)) => {
                consecutive_failures = 0;
                if tx.send(frame).is_err() {
                    break; // 接收端关闭
                }
            }
            Ok(None) => {
                // 超时无新帧，正常情况（桌面静止时 DXGI 不产生新帧）
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::error!("捕获帧失败 (连续 {} 次): {}", consecutive_failures, e);

                if consecutive_failures > 3 {
                    tracing::warn!("尝试重建 duplicator...");
                    if let Err(e) = duplicator.recreate() {
                        tracing::error!("重建 duplicator 失败: {}", e);
                        break;
                    }
                    consecutive_failures = 0;
                }
            }
        }

        // 帧率控制
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}
```

### 5. 修改 `myowndesk-client/src/main.rs`

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            myowndesk_client::service::run()?;
        }
        Some("--install") => {
            // TODO: SCM 服务注册
            println!("[install] 服务注册功能将在后续实现");
        }
        Some("--uninstall") => {
            // TODO: SCM 服务卸载
            println!("[uninstall] 服务卸载功能将在后续实现");
        }
        _ => {
            println!("[gui] GUI 模式启动中...");
            // TODO: Ticket-06（视频解码 + 渲染）/ Ticket-09（完整 GUI）
        }
    }
    Ok(())
}
```

### 6. 命令行接口

| 命令 | 行为 |
|------|------|
| `myowndesk-client.exe --service` | 以服务模式运行（当前用 Ctrl+C 停止，后续接 SCM） |
| `myowndesk-client.exe --install` | 注册为 Windows 服务（后续实现） |
| `myowndesk-client.exe --uninstall` | 卸载服务（后续实现） |
| `myowndesk-client.exe`（无参数） | GUI 模式（Ticket-06/09） |

---

## DXGI 初始化流程

```
CreateDXGIFactory1()
  → factory.EnumAdapters1(0)                              // 枚举 GPU
    → adapter.EnumOutputs(0)                              // 枚举显示器输出
      → output.GetDesc()                                  // 获取 DXGI_OUTPUT_DESC
        → DeviceName: "\\.\DISPLAY1"
        → DesktopCoordinates: RECT { left, top, right, bottom }
        → AttachedToDesktop: bool
      → output.DuplicateOutput(device)                    // 创建 Duplication
        → IDXGIOutputDuplication
```

## acquire_frame 流程

```
duplication.AcquireNextFrame(50ms)
  ├── 超时 → return Ok(None)
  ├── ACCESS_LOST → return Err（调用方重建 duplicator）
  └── 成功:
        ├── GetFrameInfo() → DXGI_OUTDUPL_FRAME_INFO
        ├── MapDesktopSurface() → IDXGIResource
        ├── QueryInterface → ID3D11Texture2D（桌面表面）
        ├── CopyResource(dst, src) → 复制到自有纹理
        ├── ReleaseFrame()
        └── return Ok(Some(CapturedFrame { ... }))
```

---

## 错误处理矩阵

| 场景 | 错误处理 |
|------|---------|
| 找不到活跃显示器 | `anyhow::bail!("无可用显示器")` |
| `DuplicateOutput` 失败 | 可能已有程序独占 Duplication，返回错误 |
| `AcquireNextFrame` 超时 | 返回 `Ok(None)`，正常进入下一轮 |
| `AcquireNextFrame` → `DXGI_ERROR_ACCESS_LOST` | 模式切换（UAC、全屏），重建 duplicator，最多重试 3 次 |
| 连续失败 > 3 次 | 记录错误日志，退出捕获循环 |
| Channel 接收端关闭 | `tx.send()` 返回 `Err`，优雅退出 |
| Ctrl+C / SCM Stop | 设置 `running = false`，join 线程，释放资源 |

---

## 验证

### 编译

```bash
cargo build -p myowndesk-client
cargo check -p myowndesk-client
```

### 手动验证

```bash
# 启动捕获服务（需管理员权限——DXGI Duplication 需要）
cargo run -p myowndesk-client -- --service

# 预期输出：
# [INFO] MyOwnDesk 服务启动中...
# [INFO] 服务已启动，按 Ctrl+C 停止
# [INFO] 捕获帧 #1 1920x1080 显示索引 0
# [INFO] 捕获帧 #61 1920x1080 显示索引 0
# ...
# Ctrl+C
# [INFO] 正在停止服务...
# [INFO] 服务已停止
```

### 测试边界

按照 spec.md 测试哲学——不 mock DXGI。以下可测：
- `capture.rs`：纯逻辑部分（显示器选择策略、帧率计算）
- `service.rs`：D3D11 设备创建标志位验证、channel 类型检查

---

## 文件变更清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 修改 | `myowndesk-client/Cargo.toml` | 添加 `windows-rs`、`tokio`、`windows-service`、`tracing`、`anyhow` |
| 修改 | `myowndesk-client/src/main.rs` | `#[tokio::main]` + 参数路由 |
| 修改 | `myowndesk-client/src/lib.rs` | 声明 `capture`、`service` 模块 |
| 新建 | `myowndesk-client/src/capture.rs` | `ScreenDuplicator`、`CapturedFrame` |
| 新建 | `myowndesk-client/src/service.rs` | D3D11 设备创建、捕获线程管理、服务生命周期 |

---

## 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| Session 0 下 DXGI 无权访问桌面 | 捕获黑屏 | DXGI 在 GPU 层面操作，不依赖桌面会话；但仍需在目标环境验证 |
| 笔记本双 GPU（集显+独显） | `EnumAdapters` 可能选错 GPU | 选择有 `AttachedToDesktop` output 的 adapter |
| `windows-rs` 0.58 API 覆盖不全 | 部分 DXGI 接口需额外绑定 | 0.58 已覆盖 DXGI 1.2，Duplication API 完整 |
| `windows-service` + tokio 集成 | SCM 回调是同步的，需桥接到异步 | SCM start 回调内创建 `tokio::runtime::Runtime` 阻塞运行 |
| AMD GPU VideoProcessor 不稳定 | 若 capture 做格式转换可能崩溃 | **已规避**：BGRA 直出，转换交给 FFmpeg（Ticket-04） |
