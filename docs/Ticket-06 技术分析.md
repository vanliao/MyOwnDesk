# Ticket-06: 视频解码与渲染

## Context

MyOwnDesk 项目已完成 Ticket-01 至 Ticket-05。Ticket-06 是控制端 GUI 模式的核心能力——从 QUIC datagram 接收 H.264 NAL 单元，通过 ffmpeg-next 软解为 RGB 帧，上传 D3D11 纹理后在 egui 窗口渲染。

**依赖关系**：Ticket-06 仅依赖 Ticket-01（协议定义），与 Ticket-05（客户端网络层）**并行**。但实际集成验证需要 Ticket-05 的 `QuicClient`（已完成）。

**当前代码状态**：
- Ticket-05 已完整实现——`net.rs` 提供 `QuicClient`（connect / register / send_datagram / recv_datagram / send_message / recv_message）
- `main.rs` GUI 分支是空的 `println!("[gui] GUI 模式启动中...")`，待 T06 填充
- `Cargo.toml` 没有 `ffmpeg-next`、`egui`、`egui-wgpu`、`winit`、`wgpu` 依赖
- `lib.rs` 未声明 decoder / gui 模块

---

## 已确认决策（Grilling 结论）

以下决策基于 docs/ 下全部文档（spec.md、tickets.md、需求分析.md、架构技术决策.md）以及 Grilling 确认得出。

| # | 决策点 | 结论 | 依据 |
|---|--------|------|------|
| 1 | 解码库 | **ffmpeg-next**（先试，如依赖获取失败则退到 Windows Media Foundation） | spec.md ADR #4/#11；Grilling Q1 确认 |
| 2 | GUI 模式 MVP | **硬编码直连**——启动后自动连接中继 → Register → Pair 到硬编码目标设备 → 收流解码渲染，无需用户交互 | Grilling Q2 确认 |
| 3 | egui 后端 | **egui-wgpu**——egui 官方 wgpu 后端，wgpu 在 Windows 上底层用 D3D11，纹理交互最自然 | ADR #8/#9；Grilling Q3 确认 |
| 4 | 解码器 trait | **对标编码器**——`VideoDecoder` trait + `create_best_decoder()` 工厂，当前唯一实现 `FfmpegDecoder`，未来可加 `Dxva2Decoder` | 与 T04 编码器架构一致；Grilling Q4 确认 |
| 5 | 事件循环集成 | **tokio 为主 + winit 为辅**——`#[tokio::main]` 跑 winit 事件循环，datagram 接收用 `tokio::spawn`，解码用 `tokio::spawn_blocking` | Grilling Q5 确认 |
| 6 | 首次关键帧 | **无条件请求**——GUI Pair 成功后立即通过 stream 发一次 `KeyFrameRequest`，不检测首帧类型，确保最快拿到 I 帧 | Grilling Q6 确认 |
| 7 | 初始化帧等待 | Pair 成功后 → 发送 KeyFrameRequest → 丢弃所有 delta 帧直到首帧关键帧到达 → 解码器初始化解码上下文 → 开始渲染 | 解码器需要 IDR 才能初始化解码上下文 |
| 8 | 硬编码目标 | 硬编码在代码常量中（`TARGET_DEVICE_ID`），后续 T09 替换为 UI 选择 | MVP 验证策略 |
| 9 | NAL 格式 | **Annex B**（带 0x00 0x00 0x00 0x01 起始码），与 openh264 编码器输出格式一致 | 编码器输出格式决定 |
| 10 | 像素格式 | 解码器输出 RGB24（3 字节/像素，无 alpha），wgpu 纹理用 `RGBA8Unorm`（4 字节/像素），上传前补 alpha=255 | wgpu 常用纹理格式 |

---

## 架构概览

### 进程架构（GUI 模式）

```
myowndesk-client.exe（默认，无参数）
  │
  │  #[tokio::main]
  │
  ├── tokio::spawn → network task
  │   ├── QuicClient::connect(server_addr, device_id, psk)
  │   ├── register() → RegisterResponse
  │   ├── send Pair(TARGET_DEVICE_ID) → 等待 PairResponse
  │   ├── send KeyFrameRequest（无条件）
  │   │
  │   ├── datagram recv loop:
  │   │   └── recv_datagram()
  │   │       → protobuf decode Message
  │   │       → match DataPacket
  │   │       → nal_tx.send(DataPacket.payload)    ──► decoder task
  │   │
  │   └── stream recv loop:
  │       └── recv_message()
  │           → handle PeerDisconnected / KeyFrameRequest / etc.
  │
  ├── tokio::spawn_blocking → decoder task
  │   ├── nal_rx.recv()
  │   ├── decoder.decode(&nal_units)
  │   │   ├── 首帧非 IDR → 丢弃（等待 KeyFrameRequest 响应）
  │   │   ├── 首帧 IDR → 初始化解码上下文
  │   │   └── YUV420P → RGB24 (swscale)
  │   └── rgb_tx.send(DecodedFrame)              ──► render side
  │
  └── main task → winit event loop
      ├── Event::RedrawRequested
      │   ├── 收集 rgb_rx 中的所有 DecodedFrame
      │   ├── 取最新一帧（丢弃中间帧，避免延迟累积）
      │   ├── RGB24 → RGBA（补 alpha=255）
      │   ├── queue.write_texture() 上传到 wgpu 纹理
      │   └── egui::Image 贴图渲染
      │
      └── Event::AboutToWait
          └── 极短 sleep（~1ms），让 tokio 有机会调度
```

### 数据流

```
中继服务器
  │  QUIC datagram (protobuf DataPacket)
  ▼
network task (tokio)
  │  nal_tx (mpsc::unbounded_channel)
  ▼
decoder task (tokio::spawn_blocking)
  │  rgb_tx (mpsc::unbounded_channel)
  ▼
main task (winit event loop)
  │  queue.write_texture()
  ▼
wgpu Texture → egui-wgpu renderer → swapchain → 屏幕
```

### Channel 拓扑

```
QuicClient::recv_datagram()
      │
      ▼
nal_tx: UnboundedSender<Vec<u8>>          ← network task 生产
nal_rx: UnboundedReceiver<Vec<u8>>        ← decoder task 消费
      │
      ▼
rgb_tx: UnboundedSender<DecodedFrame>     ← decoder task 生产
rgb_rx: UnboundedReceiver<DecodedFrame>   ← winit main loop 消费
```

---

## 接口契约

### Ticket-05 → Ticket-06 接口（已有，复用）

```rust
// net.rs (已有)
pub struct QuicClient {
    pub connection: quinn::Connection,
    // ...
}

impl QuicClient {
    pub async fn connect(server_addr: &str, device_id: &str, pre_shared_key: &str) -> anyhow::Result<Self>;
    pub async fn register(&self) -> anyhow::Result<Vec<String>>;
    pub fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()>;
    pub async fn recv_datagram(&self) -> anyhow::Result<bytes::Bytes>;
    pub async fn send_message(&self, msg: &proto::Message) -> anyhow::Result<()>;
    pub async fn recv_message(&self) -> anyhow::Result<Option<proto::Message>>;
}
```

### decoder 模块对外接口（新增）

```rust
// decoder.rs (新增)

/// 解码后的帧
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// RGB24 像素数据 (width × height × 3 bytes)
    pub rgb_data: Vec<u8>,
    /// 帧宽度
    pub width: u32,
    /// 帧高度
    pub height: u32,
    /// 显示器索引
    pub display_index: u32,
    /// 帧类型（关键帧 / delta 帧）
    pub frame_type: FrameType,
}

/// 解码器 trait——支持软解和未来硬解的统一接口
pub trait VideoDecoder: Send {
    /// 解码 NAL 单元，返回解码后的 RGB 帧。
    ///
    /// 返回空 Vec 表示：
    /// - NAL 数据不足以产出一帧（解码器内部缓冲）
    /// - 等待首帧关键帧（解码上下文未初始化时丢弃 delta 帧）
    fn decode(&mut self, nal_units: &[u8]) -> anyhow::Result<Vec<DecodedFrame>>;

    /// 冲刷解码器缓冲区，返回剩余的帧。
    fn flush(&mut self) -> anyhow::Result<Vec<DecodedFrame>>;
}

/// 创建最佳可用解码器（当前返回 FfmpegDecoder）
pub fn create_best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>>;
```

### EncodedFrame 帧类型（复用 T04）

```rust
// encoder.rs (已有)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Keyframe = 0,
    Delta = 1,
}
```

---

## Deliverables

### 1. 修改 `myowndesk-client/Cargo.toml` — 添加依赖

```toml
# ffmpeg 解码
ffmpeg-next = "7.1"

# GUI
egui = "0.31"
egui-wgpu = "0.31"
eframe = "0.31"          # egui 框架（窗口 + wgpu 集成），可选：直接手写 winit + wgpu
wgpu = "24"
winit = "0.30"

# 已有（复用）
tokio = { version = "1", features = ["full"] }
anyhow = "1"
tracing = "0.1"
bytes = "1"
prost = "0.13"
```

> **注**：T04 放弃 ffmpeg-next 的原因是 `ffmpeg-next-sys` 无法从镜像获取。T06 先尝试 ffmpeg-next；如果同样的原因失败，改用 Windows Media Foundation（`windows-rs` 已有依赖中）。

### 2. 新建 `myowndesk-client/src/decoder.rs` — 解码器模块

#### 2a. 数据类型

```rust
use crate::encoder::FrameType;

/// 解码后的帧（RGB 像素 + 元数据）
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub rgb_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub display_index: u32,
    pub frame_type: FrameType,
}
```

#### 2b. VideoDecoder trait

```rust
/// 视频解码器 trait——支持软解和未来硬解的统一接口。
pub trait VideoDecoder: Send {
    fn decode(&mut self, nal_units: &[u8]) -> anyhow::Result<Vec<DecodedFrame>>;
    fn flush(&mut self) -> anyhow::Result<Vec<DecodedFrame>>;
}
```

#### 2c. FfmpegDecoder（ffmpeg-next 软解实现）

```rust
use ffmpeg_next as ffmpeg;

/// 基于 ffmpeg-next 的 H.264 软件解码器。
pub struct FfmpegDecoder {
    codec: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,   // YUV → RGB
    initialized: bool,   // 是否已收到首个 IDR 并完成初始化
    width: u32,
    height: u32,
}

impl FfmpegDecoder {
    pub fn new() -> anyhow::Result<Self> {
        // 1. 查找 H.264 解码器
        let codec = ffmpeg::decoder::find_by_name("h264")
            .or_else(|| ffmpeg::decoder::find(ffmpeg::codec::Id::H264))
            .ok_or_else(|| anyhow::anyhow!("未找到 H.264 解码器"))?;

        // 2. 创建解码上下文
        let context = ffmpeg::codec::context::Context::new_with_codec(codec);
        let mut decoder = context
            .decoder()
            .open_as(codec)
            .map_err(|e| anyhow::anyhow!("打开 H.264 解码器失败: {}", e))?;

        // 3. scaler 延迟创建——不知道宽高直到收到第一帧
        // ...
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, nal_units: &[u8]) -> anyhow::Result<Vec<DecodedFrame>> {
        // 1. 创建 AVPacket，填充 NAL 数据
        // 2. avcodec_send_packet
        // 3. loop: avcodec_receive_frame → AVFrame (YUV420P)
        // 4. swscale: YUV420P → RGB24
        // 5. 返回 Vec<DecodedFrame>
    }

    fn flush(&mut self) -> anyhow::Result<Vec<DecodedFrame>> {
        // 发送 null packet → 冲刷解码器缓冲区
    }
}
```

#### 2d. 解码器工厂

```rust
pub fn create_best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    let decoder = FfmpegDecoder::new()?;
    Ok(Box::new(decoder))
}
```

### 3. 新建 `myowndesk-client/src/gui.rs` — GUI 模式（winit + egui + wgpu）

#### 3a. 硬编码配置

```rust
/// Ticket-06 MVP：硬编码目标设备 ID。
/// 后续 Ticket-09 替换为 UI 选择。
const TARGET_DEVICE_ID: &str = "van-pc";
```

#### 3b. gui::run() 入口

```rust
/// GUI 模式入口——Ticket-06 硬编码直连版本。
///
/// 流程：
/// 1. 连接中继 → Register → Pair(TARGET_DEVICE_ID)
/// 2. 发 KeyFrameRequest
/// 3. datagram 接收 → NAL → 解码 → RGB → wgpu 纹理 → egui 渲染
pub async fn run() -> anyhow::Result<()> {
    // ...
}
```

#### 3c. 核心结构

```rust
/// GUI 应用状态
struct GuiApp {
    /// 最新解码帧（None = 尚未收到首帧）
    current_frame: Option<DecodedFrame>,
    /// wgpu 纹理（懒创建，收首帧后初始化）
    video_texture: Option<wgpu::Texture>,
    /// 解码帧接收 channel
    rgb_rx: UnboundedReceiver<DecodedFrame>,
    /// 连接状态
    state: ConnectionState,
    /// egui-wgpu renderer
    egui_renderer: egui_wgpu::Renderer,
    /// 最近一条错误消息
    error_message: Option<String>,
}

enum ConnectionState {
    Connecting,
    Registering,
    Paired,
    Receiving,    // 正在接收视频流
    Disconnected,
    Error,
}
```

#### 3d. winit + wgpu 初始化

```rust
// 1. 创建 winit window
let event_loop = winit::event_loop::EventLoop::new()?;
let window = winit::window::WindowBuilder::new()
    .with_title("MyOwnDesk")
    .with_inner_size(winit::dpi::LogicalSize::new(1920, 1080))
    .build(&event_loop)?;

// 2. 创建 wgpu instance + surface + device
let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
    backends: wgpu::Backends::DX12 | wgpu::Backends::DX11,  // 优选用 D3D12，回退 D3D11
    ..Default::default()
});
let surface = instance.create_surface(&window)?;
let adapter = instance
    .request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        ..Default::default()
    })
    .await
    .ok_or_else(|| anyhow::anyhow!("无可用 GPU 适配器"))?;
let (device, queue) = adapter
    .request_device(&wgpu::DeviceDescriptor::default(), None)
    .await?;

// 3. 配置 surface
let surface_config = wgpu::SurfaceConfiguration {
    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
    format: surface.get_capabilities(&adapter).formats[0],
    width: 1920,
    height: 1080,
    present_mode: wgpu::PresentMode::AutoVsync,
    // ...
};
surface.configure(&device, &surface_config);

// 4. 创建 egui-wgpu renderer
let egui_renderer = egui_wgpu::Renderer::new(
    &device,
    surface_config.format,
    None,
    1,
);
```

#### 3e. 事件循环主逻辑

```rust
event_loop.run(move |event, window_target| {
    match event {
        winit::event::Event::RedrawRequested(_) => {
            // a. 收集所有待渲染的帧（只取最新）
            while let Ok(frame) = rgb_rx.try_recv() {
                app.current_frame = Some(frame);
            }

            // b. 如果有新帧 → 上传纹理
            if let Some(ref frame) = app.current_frame {
                if needs_texture_update(&app.video_texture, frame) {
                    app.video_texture = Some(create_or_update_texture(
                        &device, &queue, frame,
                    ));
                }
            }

            // c. egui frame
            let raw_input = egui_winit.handle_event(&window);
            let full_output = egui_ctx.run(raw_input, |ctx| {
                render_ui(ctx, &mut app);
            });

            // d. 渲染
            let screen_descriptor = egui_wgpu::ScreenDescriptor { ... };
            let paint_job = egui_ctx.tessellate(full_output.shapes, ...);
            let frame = surface.get_current_texture()?;
            let view = frame.texture.create_view(...);

            let mut encoder = device.create_command_encoder(...);
            egui_renderer.update_texture(&device, &queue, egui_texture_id, &texture_desc);
            egui_renderer.render(&view, &paint_job, &screen_descriptor, ...);
            queue.submit([encoder.finish()]);
            frame.present();

            window.request_redraw(); // 连续渲染
        }
        winit::event::Event::AboutToWait => {
            // 极短 sleep 让 tokio 调度（避免 100% CPU）
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        winit::event::Event::WindowEvent { ref event, .. } => {
            // 窗口关闭 → 退出
            match event {
                winit::event::WindowEvent::CloseRequested => window_target.exit(),
                _ => {}
            }
        }
        _ => {}
    }
})?;
```

#### 3f. egui UI 布局（MVP）

```rust
fn render_ui(ctx: &egui::Context, app: &mut GuiApp) {
    egui::CentralPanel::default().show(ctx, |ui| {
        // 状态栏
        let state_text = match app.state {
            ConnectionState::Connecting => "正在连接中继...",
            ConnectionState::Registering => "正在注册...",
            ConnectionState::Paired => "配对成功，等待视频流...",
            ConnectionState::Receiving => "接收中",
            ConnectionState::Disconnected => "连接已断开",
            ConnectionState::Error => "错误",
        };
        ui.label(state_text);

        // 错误消息
        if let Some(ref err) = app.error_message {
            ui.colored_label(egui::Color32::RED, err);
        }

        // 远程画面区域
        if let Some(ref _frame) = app.current_frame {
            let available = ui.available_size();
            // 保持宽高比的缩放
            let image = egui::Image::new(egui::ImageSource::Texture(
                egui::load::SizedTexture::new(VIDEO_TEXTURE_ID, [available.x, available.y])
            ));
            ui.add(image);
        } else {
            // 无画面：显示 loading
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.label("等待视频流...");
            });
        }
    });
}
```

### 4. 修改 `myowndesk-client/src/main.rs` — 接入 GUI 模式

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            myowndesk_client::service::run().await?;
        }
        _ => {
            // Ticket-06: GUI 模式——硬编码直连版本
            myowndesk_client::gui::run().await?;
        }
    }
    Ok(())
}
```

### 5. 修改 `myowndesk-client/src/lib.rs` — 声明模块

```rust
pub mod capture;
pub mod config;
pub mod decoder;    // 新增
pub mod encoder;
pub mod gui;        // 新增
pub mod net;
pub mod service;
```

### 6. 可能需要修改 `myowndesk-client/src/net.rs`

检查 `QuicClient` 是否能直接在 GUI 模式的 tokio runtime 上工作——当前设计是共享模块，应无需改动。如果 `send_message`（发 Pair、KeyFrameRequest）需要在 GUI 侧调用，确认 `QuicClient` 的 `connection` 字段是 `pub`（当前已是 `pub`）。

### 7. 可能需要修改 `myowndesk-client/src/encoder.rs`

`FrameType` 枚举被 decoder 复用，确保其为 `pub`（当前已是 `pub`）。考虑将其移到 `lib.rs` 或 `protocol` 层级，避免 decoder 对 encoder 模块的尴尬依赖。但 MVP 先不做这个大改动。

---

## 解码流程详解

### ffmpeg-next 解码管线

```
NAL units (annex B, Vec<u8>)
  │
  ▼
AVPacket { data = nal_units, pts, dts }
  │
  ▼
avcodec_send_packet(ctx, &packet)
  │  return: 0 (成功) / AVERROR(EAGAIN) (需要先取帧) / AVERROR_EOF
  ▼
avcodec_receive_frame(ctx, &frame)   ← loop 直到 EAGAIN
  │  return: 0 (有一帧) / AVERROR(EAGAIN) (没更多帧了) / AVERROR_EOF
  ▼
AVFrame (YUV420P)
  │  format: AV_PIX_FMT_YUV420P
  │  data[0] = Y plane (width × height)
  │  data[1] = U plane (width/2 × height/2)
  │  data[2] = V plane (width/2 × height/2)
  │
  ▼
sws_scale(scaler_ctx)
  │  YUV420P → AV_PIX_FMT_RGB24
  ▼
AVFrame (RGB24)
  │  data[0] = RGB pixels (width × height × 3)
  │  linesize = width × 3
  │
  ▼
copy to Vec<u8> → DecodedFrame
```

### 首帧关键帧等待逻辑

```rust
fn decode(&mut self, nal_units: &[u8]) -> anyhow::Result<Vec<DecodedFrame>> {
    // 检查是否为 IDR（关键帧）
    // H.264 NAL unit header: 第一个字节的 bit[0..=4] = nal_unit_type
    // IDR = 5 (0x65 起始码后第一字节 & 0x1F == 5)
    let is_idr = nal_units.len() > 4
        && (nal_units[4] & 0x1F) == 5;  // 跳过 4 字节起始码

    if !self.initialized && !is_idr {
        // 尚未初始化且当前帧非 IDR → 丢弃
        tracing::debug!("丢弃 delta 帧（等待首帧关键帧）");
        return Ok(Vec::new());
    }

    // 正常解码流程...
}
```

### 纹理上传流程

```
DecodedFrame.rgb_data (RGB24, Vec<u8>)
  │
  ▼
RGB24 → RGBA8 (补 alpha=255)
  │  r, g, b → r, g, b, 255
  │  output: Vec<u8> (width × height × 4)
  ▼
queue.write_texture(
    wgpu::ImageCopyTexture {
        texture: &video_texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
    },
    &rgba_data,
    wgpu::ImageDataLayout {
        offset: 0,
        bytes_per_row: Some(width * 4),
        rows_per_image: Some(height),
    },
    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
)
  │
  ▼
egui-wgpu renderer reads video_texture → draw as egui::Image
```

---

## 错误处理矩阵

| 场景 | 处理 |
|------|------|
| QUIC 连接失败 | `QuicClient::connect()` 返回 Err，`gui::run()` 打印错误后退出，窗口显示错误消息 |
| Register 认证失败 | 同上，记录错误 |
| Pair 目标不在线 | 中继返回 `PairResponse(error_code=DEVICE_NOT_FOUND)`，窗口显示 " 目标设备不在线 " |
| datagram 接收失败 | 网络 task 退出，`nal_tx` 关闭 → decoder task 收到 channel close → 退出 |
| 收到非 DataPacket datagram | 静默丢弃（向前兼容） |
| ffmpeg 解码失败 | 记录错误，丢弃当前 NAL 数据，继续下一帧（解码器内部状态可能需重置） |
| wgpu 纹理创建失败 | 记录错误，跳过该帧，下次 Redraw 重试（通常是 OOM 或设备丢失） |
| D3D11 设备丢失 | wgpu 内部处理 device lost；可能需要重建 swapchain |
| 对端断开（PeerDisconnected） | 切换到 Disconnected 状态，窗口显示 " 连接已断开 " |
| 解码器首帧非 IDR | 丢弃 delta 帧，等待 KeyFrameRequest 触发编码器输出 IDR |
| ffmpeg-next 依赖获取失败 | 切换到方案 B（Windows Media Foundation decoder） |

---

## 文件变更清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 修改 | `myowndesk-client/Cargo.toml` | 添加 `ffmpeg-next`、`egui`、`egui-wgpu`、`wgpu`、`winit` |
| 新建 | `myowndesk-client/src/decoder.rs` | `VideoDecoder` trait + `FfmpegDecoder` + `create_best_decoder()` |
| 新建 | `myowndesk-client/src/gui.rs` | winit + wgpu + egui 窗口，硬编码直连逻辑 |
| 修改 | `myowndesk-client/src/main.rs` | GUI 分支调用 `gui::run()` |
| 修改 | `myowndesk-client/src/lib.rs` | 声明 `decoder`、`gui` 模块 |

---

## 验证

### 编译

```bash
cargo build -p myowndesk-client
cargo check -p myowndesk-client
```

### 手动验证（需要中继服务器 + 被控端运行）

```bash
# 终端 1: 启动中继
cargo run -p myowndesk-relay

# 终端 2: 启动被控端服务（在另一台机器或同一机器）
cargo run -p myowndesk-client -- --service

# 终端 3: 启动 GUI 控制端
cargo run -p myowndesk-client

# 预期：
# 1. 窗口打开，显示 "正在连接中继..."
# 2. → "配对成功，等待视频流..."
# 3. → 收到 I 帧后，窗口显示远程桌面画面
# 4. → 1080P 60fps 连续渲染
# 5. → 被控端 Ctrl+C 停止 → GUI 显示 "连接已断开"
```

### 单元测试（decoder.rs）

```rust
#[cfg(test)]
mod tests {
    // test_create_decoder — create_best_decoder() 返回 Ok
    // test_decode_keyframe — 编码器产出 I 帧 → 解码器输出 RGB 帧
    // test_decode_delta — I 帧后 delta 帧正常解码
    // test_decode_no_init — 无 I 帧时的 delta 帧被丢弃
    // test_flush — flush 返回缓冲帧
    // test_decode_empty — 空 NAL 数据返回 Ok(Vec::new())
}
```

---

## 后续扩展路径

| 扩展 | 说明 |
|------|------|
| DXVA2 硬解 | 新增 `Dxva2Decoder` 实现 `VideoDecoder` trait，解码帧直接输出 D3D11 纹理（零拷贝） |
| 设备列表 UI | Ticket-09 替换硬编码目标设备 ID |
| 流畅度优化 | 渲染侧做帧队列 + 计时器均匀显示，而非简单的 " 取最新帧 " |
| 多分辨率适配 | 被控端分辨率 ≠ 窗口大小时的高质量缩放 |
| 画面黑边处理 | 保持宽高比的 letterbox / pillarbox 显示 |
