# Ticket-04: H.264 视频编码

## Context

MyOwnDesk 项目已完成 Ticket-01（协议定义）、Ticket-02（中继服务器）、Ticket-03（DXGI 屏幕捕获 + Windows 服务）。Ticket-04 是客户端侧第二个功能模块——从 `CapturedFrame` 管道中取出 D3D11 纹理，通过 FFmpeg 编码为 H.264 NAL 单元，输出到编码帧 channel，供 Ticket-05（客户端网络层）消费。

**依赖关系**：Ticket-04 仅依赖 Ticket-01（协议定义）和 Ticket-03（DXGI 屏幕捕获），是 Ticket-05（客户端网络层）的**直接前置条件**。

**当前代码状态**：
- Ticket-03 已完整实现——`capture.rs` 提供了完整的 `ScreenDuplicator`（DXGI 捕获、纹理复制、Access Lost 恢复）
- `service.rs` 创建了 D3D11 设备、运行捕获循环（专用 std::thread）、建立了 `UnboundedChannel<CapturedFrame>` 通道
- consumer task（tokio::spawn）当前是空的 trace 占位
- `Cargo.toml` 已有 `windows-rs`、`tokio`、`tracing`、`anyhow` 等依赖

---

## 已确认决策（Grilling 结论）

以下决策基于 docs/ 下全部文档（spec.md、tickets.md、需求分析.md、架构技术决策.md、adr/0001-video-frame-fragmentation.md）以及 Grilling 确认得出。

| # | 决策点 | 结论 | 依据 |
|---|--------|------|------|
| 1 | 编码库 | **FFmpeg**（通过 `ffmpeg-next` + `ffmpeg-next-sys`） | tickets.md "ffmpeg-next 初始化 H.264"，Grilling 确认 |
| 2 | 硬件发现策略 | 启动时自动检测：`h264_nvenc` → `h264_qsv` → `h264_amf` → 降级 `libx264` 软编 | tickets.md 自动发现要求，Grilling 确认 |
| 3 | FFmpeg 分发 | **build.rs 自动下载** gyan.dev 的 FFmpeg shared DLLs（Windows x64） | Grilling 确认 |
| 4 | 集成位置 | encoder 跑在 **consumer task（tokio::spawn）** 中，同步阻塞调用 | Grilling 确认 |
| 5 | CPU 回读 | capture 线程完成 D3D11 纹理 → `Vec<u8>`（BGRA 像素），consumer 拿到 CPU 数据直接编码 | Grilling 确认 |
| 6 | 输出格式 | `encode() -> Vec<EncodedFrame>`，一个帧可能产出多个 NAL 单元 | Grilling 确认 |
| 7 | 编码参数 | CBR 15 Mbps、zerolatency tune、ultrafast preset、GOP 60 帧、high profile | tickets.md |
| 8 | Slice mode | 通过 FFmpeg 的 `slice-max-size` 参数配置，目标每个 NAL 单元 ≤ MTU | ADR #1 |
| 9 | 帧类型标记 | `EncodedFrame.frame_type` 标记 KEYFRAME / DELTA，映射到 proto `DataPacket` | tickets.md |
| 10 | 强制关键帧 | 支持从外部触发强制关键帧（KeyFrameRequest 响应） | 网络丢包恢复需求 |
| 11 | 第一版实现 | **先实现 libx264 软编路径**（兼容所有 GPU），硬件编码路径在后续 Ticket 或内部迭代中补充 | Grilling 确认 |
| 12 | BGRA → NV12 转换 | 软编路径用 FFmpeg `swscale` 库转换 | 软编路径的标准做法 |

---

## 架构概览

```
capture 线程 (std::thread)
│
├── AcquireNextFrame → desktop texture
├── CopyResource → owned_texture
├── CopyResource → staging_texture (新)
├── Map → 读取 BGRA 像素到 Vec<u8>      ← CPU 回读
├── CapturedFrame { texture, cpu_buffer, ... }
└── capture_tx.send(frame)
          │
          ▼
consumer task (tokio::spawn)               ← 新增编码器
│
├── rx.recv() → CapturedFrame
├── FfmpegEncoder::encode(&frame)
│   ├── cpu_buffer (BGRA) → swscale → AVFrame (NV12)
│   ├── avcodec_send_frame / avcodec_receive_packet
│   └── → Vec<EncodedFrame>
│
└── encode_tx.send(encoded_frames)         → [Ticket-05 网络层]
```

### 数据流

```
CapturedFrame (D3D11 texture + CPU pixel data)
  │
  ▼
FfmpegEncoder
  ├── cpu_buffer → sws_scale → AVFrame (YUV420P/NV12)
  ├── avcodec_send_frame(encoder_ctx, avframe)
  ├── avcodec_receive_packet(encoder_ctx, packet)
  └── packet → EncodedFrame { nal_units, frame_type, ... }
        │
        ▼
  Vec<EncodedFrame> (一个 frame 可能多个 NAL 单元)
        │
        ▼
  encode_sender  ──►  [Ticket-05 网络层]
```

---

## 与 Ticket-03 的接口变更

### 5.1 `CapturedFrame` 增加 CPU 像素缓冲

```rust
// myowndesk-client/src/capture.rs

pub struct CapturedFrame {
    /// D3D11 纹理（GPU 端，供未来硬编用）
    pub texture: ID3D11Texture2D,
    /// BGRA 像素数据（CPU 端，供软编用） ← 新增
    pub cpu_buffer: Vec<u8>,
    /// 显示器索引
    pub display_index: u32,
    /// 捕获时间戳
    pub timestamp: Instant,
    /// 纹理宽度
    pub width: u32,
    /// 纹理高度
    pub height: u32,
}
```

**说明**：
- `cpu_buffer` 长度为 `width * height * 4` 字节（BGRA 32bpp）
- capture 线程在 `CopyResource` 后立即执行回读
- 两路数据并存：texture 留给未来硬编，cpu_buffer 供给当前软编
- consumer 拿到 cpu_buffer 后直接构造 AVFrame，无需再碰 D3D11

### 5.2 capture 循环增加回读步骤

```
现有流程（Ticket-03）：
  AcquireNextFrame → CopyResource(owned) → ReleaseFrame → tx.send(CapturedFrame)

新流程（Ticket-04）：
  AcquireNextFrame → CopyResource(owned) → CopyResource(staging) → Map(staging)
    → 读 BGRA 像素到 Vec<u8> → Unmap → ReleaseFrame
    → tx.send(CapturedFrame { texture, cpu_buffer, ... })
```

- 新增第二个 staging 纹理（`D3D11_USAGE_STAGING` + `D3D11_CPU_ACCESS_READ`）
- 新增 `CopyResource(owned → staging)` 步骤
- 新增 `Map()` → 读像素 → `Unmap()` 步骤

---

## 与 Ticket-05 的接口约定

### 6.1 编码帧输出通道

```rust
// service.rs 或 future net 模块

pub type EncodeSender = tokio::sync::mpsc::UnboundedSender<EncodedFrame>;
pub type EncodeReceiver = tokio::sync::mpsc::UnboundedReceiver<EncodedFrame>;
```

- `service::run()` 创建 `EncodeSender/EncodeReceiver` 对
- `encode_sender` 传给 encoder consumer task
- `encode_receiver` 预留给 Ticket-05 网络层消费
- channel 选择 `unbounded`：编码器不应因网络慢而被阻塞

### 6.2 `EncodedFrame` 结构体

```rust
/// 编码帧——编码器输出，供网络模块发送（Ticket-05）
pub struct EncodedFrame {
    /// H.264 NAL 单元字节（直接作为 DataPacket.payload）
    pub nal_units: Vec<u8>,
    /// 帧类型：关键帧 / delta 帧（映射到 proto FrameType）
    pub frame_type: FrameType,
    /// 显示器索引
    pub display_index: u32,
    /// 显示时间戳（捕获时记录）
    pub pts: i64,
    /// 帧宽度
    pub width: u32,
    /// 帧高度
    pub height: u32,
}

pub enum FrameType {
    Keyframe = 0,
    Delta = 1,
}
```

**网络层使用时**：
- 每个 `EncodedFrame` 对应一个 `DataPacket` 消息
- `nal_units` → `DataPacket.payload`
- `frame_type` → `DataPacket.frame_type`
- `display_index` → `DataPacket.display_index`

### 6.3 强制关键帧接口

```rust
impl FfmpegEncoder {
    /// 请求下一帧输出为关键帧（网络丢包后由 KeyFrameRequest 触发）
    pub fn request_keyframe(&mut self);
}
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
windows = { version = "0.58", features = [
    "Win32_Graphics_Direct3D",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_Graphics_Gdi",
    "Win32_System_SystemServices",
]}
windows-service = "0.7"
tracing = "0.1"
tracing-subscriber = "0.3"
anyhow = "1"
ffmpeg-next = "7.0"                   # ← 新增：FFmpeg Rust 绑定
ffmpeg-next-sys = "7.0"               # ← 新增：底层 FFI（硬编时需要）
```

### 2. 新增 `myowndesk-client/build.rs` — FFmpeg DLL 自动下载

```rust
// 在构建时自动下载 FFmpeg shared DLLs（Windows x64）
// 从 gyan.dev 下载 release shared build，解压到 vendor/ffmpeg/
// 设置 cargo:rustc-link-search 指向 DLL 目录
```

**行为**：
1. 检查 `vendor/ffmpeg/bin/` 下是否存在 `avcodec-61.dll`
2. 不存在 → 下载 `ffmpeg-release-full-shared.7z`（约 80MB）
3. 解压到 `vendor/ffmpeg/`
4. 设置 `cargo:rustc-link-search=native=vendor/ffmpeg/lib`
5. 设置 `cargo:rustc-link-lib=avcodec`、`avformat`、`avutil`、`swscale`、`avdevice`

**后续运行**：FFmpeg DLLs 需要与 `myowndesk-client.exe` 在同一目录，cargo build 后复制过去。

### 3. 新建 `myowndesk-client/src/encoder.rs` — 编码器模块

#### 3a. 模块结构

```rust
//! H.264 视频编码器模块。
//!
//! 当前实现：基于 `ffmpeg-next` 的 libx264 软件编码（兼容所有 GPU）。
//! 未来扩展：同一 trait 下实现 NVENC / QSV / AMF 硬件编码器。

mod soft;       // libx264 软编实现
// mod hw;      // [未来] 硬件编码器（NVENC/QSV/AMF）

use crate::capture::CapturedFrame;

/// 编码帧类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Keyframe = 0,
    Delta = 1,
}

/// 编码帧——编码器输出，供网络模块发送
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub nal_units: Vec<u8>,
    pub frame_type: FrameType,
    pub display_index: u32,
    pub pts: i64,
    pub width: u32,
    pub height: u32,
}

/// 编码器 trait——支持软编和未来硬编统一接口
pub trait VideoEncoder: Send {
    /// 编码一帧
    /// 返回 Vec<EncodedFrame>，因为一个帧可能产生多个 NAL 单元（slice mode）
    fn encode(&mut self, frame: &CapturedFrame) -> anyhow::Result<Vec<EncodedFrame>>;

    /// 请求强制关键帧
    fn request_keyframe(&mut self);
}

/// 创建最佳可用编码器
///
/// 自动检测：NVENC → QSV → AMF → libx264 降级
/// 当前第一版固定返回软编，检测逻辑后续补充
pub fn create_best_encoder(
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    // 第一版：直接返回软编
    soft::SoftEncoder::new(width, height, fps)
        .map(|e| Box::new(e) as Box<dyn VideoEncoder>)
}
```

#### 3b. `soft.rs` — libx264 软编实现

```rust
//! 基于 `ffmpeg-next` 的 libx264 软件编码器。

use ffmpeg_next::format::Pixel;
use ffmpeg_next::encoder;
use ffmpeg_next::frame;
use ffmpeg_next::packet;
use ffmpeg_next::software::sws;
use ffmpeg_next::util::error::EAGAIN;

/// libx264 软件编码器
pub struct SoftEncoder {
    encoder: encoder::Encoder,
    sws: sws::Context,
    width: u32,
    height: u32,
    frame_count: u64,
    force_keyframe: bool,
}

impl SoftEncoder {
    /// 创建 libx264 软编
    ///
    /// 配置：
    /// - 编码器: libx264
    /// - 码率: CBR 15 Mbps
    /// - preset: ultrafast
    /// - tune: zerolatency
    /// - profile: high
    /// - GOP: 60
    /// - pix_fmt: yuv420p
    /// - slice-max-size: 1200 (MTU 适配)
    pub fn new(width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        // 1. 查找 libx264 编码器
        //    encoder::find_by_name("libx264")

        // 2. 创建编码器
        //    encoder::Encoder::new(codec, Pixel::YUV420P, width, height, fps as f64)

        // 3. 设置编码参数
        //    - bit_rate: 15_000_000 (CBR 15 Mbps)
        //    - max_bit_rate: 15_000_000
        //    - time_base: (1, fps)
        //    - gop_size: 60
        //    - max_b_frames: 0 (zerolatency)
        //    - thread_count: 4 (适度并行)

        // 4. 设置导出参数 (codec-specific)
        //    "preset" → "ultrafast"
        //    "tune" → "zerolatency"
        //    "profile" → "high"
        //    "slice-max-size" → "1200"

        // 5. 打开编码器
        //    encoder.open_with_opts(opts)
        //    注意：ffmpeg-next 的 open_with_opts 接受 HashMap<String, String>

        // 6. 创建 swscale 上下文 (BGRA → YUV420P)
        //    sws::Context::get(
        //        (width, height), Pixel::BGRA,
        //        (width, height), Pixel::YUV420P,
        //        SWS_BILINEAR,
        //    )

        todo!("SoftEncoder::new")
    }
}

impl super::VideoEncoder for SoftEncoder {
    fn encode(&mut self, frame: &CapturedFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        // 1. CPU 像素数据 → AVFrame (BGRA)
        //    let mut av_frame = frame::Frame::new(Pixel::BGRA, self.width, self.height);
        //    av_frame.data_mut(0).copy_from_slice(&frame.cpu_buffer);

        // 2. swscale: BGRA → YUV420P
        //    self.sws.run(&av_frame, &mut yuv_frame)?;

        // 3. 设置帧属性
        //    yuv_frame.set_pts(frame_count as i64);
        //    if self.force_keyframe {
        //        yuv_frame.set_keyframe(true);
        //        self.force_keyframe = false;
        //    }

        // 4. 编码
        //    self.encoder.send_frame(&yuv_frame)?;
        //    loop {
        //        match self.encoder.receive_packet() {
        //            Ok(pkt) => {
        //                // pkt.data → NAL 单元
        //                // pkt.is_key() → 判断关键帧
        //                encoded.push(EncodedFrame { ... })
        //            }
        //            Err(ffmpeg_next::Error::EAGAIN) => break,
        //            Err(e) => return Err(e.into()),
        //        }
        //    }

        // 5. 帧计数递增
        //    self.frame_count += 1;

        // 6. 返回结果
        todo!("SoftEncoder::encode")
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }
}
```

### 4. 修改 `myowndesk-client/src/capture.rs` — 增加 CPU 回读

```rust
/// 新增：staging 纹理用于 CPU 回读
pub struct ScreenDuplicator {
    // ... 现有字段 ...
    /// Staging 纹理（D3D11_USAGE_STAGING + CPU_ACCESS_READ）
    staging_texture: Option<ID3D11Texture2D>,
}

impl ScreenDuplicator {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> anyhow::Result<Self> {
        // ... 现有代码 ...
        // 新增：创建 staging 纹理
        let staging = create_staging_texture(device, width, height)?;
        // 返回时包含 staging_texture
    }

    pub fn acquire_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        // ... 现有代码直到 CopyResource ...

        // 新增：回读 CPU 像素
        // let cpu_buffer = if let Some(ref owned) = self.owned_texture {
        //     if let Some(ref staging) = self.staging_texture {
        //         // 1. CopyResource(owned → staging)
        //         // 2. Map(staging) → read pixels → Vec<u8>
        //         // 3. Unmap
        //         //
        //         // D3D11_MAP_READ | D3D11_MAP_FLAG_DO_NOT_WAIT
        //         //    → 避免阻塞 capture 线程
        //     }
        // };

        // CapturedFrame 新增 cpu_buffer 字段
    }
}

/// 创建 staging 纹理（CPU 可读）
fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,       // ← STAGING
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ,  // ← CPU 读
        MiscFlags: 0,
    };
    // CreateTexture2D(&desc, None, &mut texture)
    todo!()
}
```

### 5. 修改 `myowndesk-client/src/lib.rs`

```rust
pub mod capture;
pub mod encoder;   // ← 新增
pub mod service;
```

### 6. 修改 `myowndesk-client/src/service.rs` — 接入编码器

```rust
use crate::encoder::{self, EncodedFrame, VideoEncoder};

pub async fn run() -> anyhow::Result<()> {
    // ... 现有代码 ...

    let (device, context) = create_d3d11_device()?;
    let mut duplicator = ScreenDuplicator::new(&device, &context)?;

    let (capture_tx, mut capture_rx) = mpsc::unbounded_channel::<CapturedFrame>();
    // 新增：编码帧输出通道 → Ticket-05 网络层
    let (encode_tx, _encode_rx) = mpsc::unbounded_channel::<EncodedFrame>();
    let running = Arc::new(AtomicBool::new(true));

    // 捕获线程（不变）
    let capture_handle = { ... };

    // 编码 task（替换现有 consumer task）
    let encoder_handle = tokio::spawn(async move {
        // 初始化编码器
        let mut encoder = encoder::create_best_encoder(1920, 1080, 60)
            .expect("编码器初始化失败");

        while let Some(frame) = capture_rx.recv().await {
            match encoder.encode(&frame) {
                Ok(encoded_frames) => {
                    for encoded in encoded_frames {
                        if encode_tx.send(encoded).is_err() {
                            // 网络层已关闭，停止编码
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("编码失败: {}", e);
                }
            }
        }
    });

    // ... 等待退出、清理 ...
}
```

### 7. 修改 `myowndesk-client/Cargo.toml` — dev-dependencies

软编开发测试时需要 FFmpeg DLL 在运行目录。在 build.rs 中增加自动复制：

```rust
// build.rs 末尾
fn copy_dlls_to_output() {
    // 将 vendor/ffmpeg/bin/*.dll 复制到 OUT_DIR/../ 或 target/debug/
    // 确保 cargo run 时能找到 DLLs
}
```

---

## 编码流程详解

### 软编流程

```
┌─ capture 线程 ──────────────────────────────────────────┐
│  AcquireNextFrame → CopyResource → Map → Vec<u8> → send │
└──────────────────────────┬──────────────────────────────┘
                           │ CapturedFrame { cpu_buffer, ... }
                           ▼
┌─ consumer task ─────────────────────────────────────────┐
│                                                         │
│  recv() → CapturedFrame                                 │
│    │                                                     │
│    ├─ cpu_buffer → AVFrame(BGRA)                         │
│    ├─ sws_scale → AVFrame(YUV420P)                       │
│    ├─ avcodec_send_frame(avctx, yuv)                     │
│    ├─ avcodec_receive_packet → AVPacket                  │
│    │   ├─ pkt.data → EncodedFrame { nal_units }          │
│    │   ├─ pkt.flags & AV_PKT_FLAG_KEY → frame_type       │
│    │   └─ push to result Vec<EncodedFrame>               │
│    ├─ (loop until EAGAIN or error)                       │
│    └─ return Vec<EncodedFrame>                           │
│                                                         │
│  encode_tx.send(encoded_frames)                          │
└──────────────────────────┬──────────────────────────────┘
                           │ EncodedFrame { nal_units, ... }
                           ▼
                  [Ticket-05: 网络层]
```

### 硬编流程（后续实现）

```
┌─ capture 线程 ───────────────────────────┐
│  AcquireNextFrame → CopyResource → send  │
└─────────────────────┬────────────────────┘
                      │ (仅传 texture, 不回读 CPU)
                      ▼
┌─ 编码线程 ──────────────────────────────────────────┐
│  ID3D11Texture2D → ffmpeg hwupload → AVFrame(hw)   │
│  → avcodec_send_frame(hwenc, hw_frame)              │
│  → avcodec_receive_packet → EncodedFrame            │
└────────────────────────────────────────────────────┘
```

---

## 错误处理矩阵

| 场景 | 错误处理 |
|------|---------|
| FFmpeg DLL 找不到 | build.rs 下载失败时打印清晰错误信息，指导手动下载 |
| 编码器初始化失败（libx264 不支持） | `anyhow::bail!("libx264 编码器不可用，请确保 FFmpeg 编译了 libx264")` |
| swscale 初始化失败 | `anyhow::bail!("swscale 上下文创建失败")` |
| 编码过程中 `EAGAIN` | 正常——编码器需要更多数据，继续下一帧 |
| 编码器 `send_frame` 返回错误 | 记录错误日志，丢弃当前帧，继续下一帧 |
| `Map()` 返回 `DXGI_ERROR_WAS_STILL_DRAWING` | 使用 `D3D11_MAP_FLAG_DO_NOT_WAIT`，跳过该帧回读 |
| 强制关键帧请求 | 设置 `force_keyframe` 标志，下一帧输出 I 帧 |
| Channel 接收端关闭（网络层已关闭） | 编码循环退出，优雅停止 |
| 帧分辨率变化（被控端更改显示设置） | 捕获侧重建 duplicator 时重建编码器（后续 Ticket） |

---

## 文件变更清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 修改 | `myowndesk-client/Cargo.toml` | 添加 `ffmpeg-next`、`ffmpeg-next-sys` 依赖 |
| 新建 | `myowndesk-client/build.rs` | FFmpeg DLL 自动下载 + 链接配置 |
| 新建 | `myowndesk-client/src/encoder.rs` | 编码器模块入口：`VideoEncoder` trait、`EncodedFrame`、`create_best_encoder` |
| 新建 | `myowndesk-client/src/encoder/soft.rs` | `SoftEncoder` libx264 软编实现 |
| 修改 | `myowndesk-client/src/capture.rs` | `CapturedFrame` 增加 `cpu_buffer` 字段；`ScreenDuplicator` 增加 staging 纹理和回读逻辑 |
| 修改 | `myowndesk-client/src/lib.rs` | 声明 `encoder` 模块 |
| 修改 | `myowndesk-client/src/service.rs` | consumer task 替换为编码器接入；创建 `EncodeSender/EncodeReceiver` 通道 |
| 新建 | `vendor/ffmpeg/` | 构建时自动填充（.gitignore 中排除或 LFS 管理） |
| 修改 | `.gitignore` | 添加 `vendor/ffmpeg/bin/`、`vendor/ffmpeg/lib/`（由 build.rs 下载） |

---

## 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| FFmpeg DLL 下载失败（网络问题） | 无法构建 | build.rs 提供详细错误信息和手动下载指引 |
| gyan.dev 的 FFmpeg DLL 版本与 ffmpeg-next 不匹配 | 运行时崩溃 | build.rs 固定下载版本号，与 ffmpeg-next 版本对应 |
| ffmpeg-next 的 `open_with_opts` API 更新 | 编译失败 | pin 住 ffmpeg-next 7.0，API 稳定 |
| CPU 回读在 1080p 60fps 下增加延迟 | 端到端延迟增加 ~1ms | 可接受；后续硬编路径不需要回读 |
| libx264 编码在低端 CPU 上掉帧 | 帧率低于 60fps | 降低预设（superfast）或降低帧率（30fps）作为用户可选项 |
| `Map()` + `D3D11_MAP_FLAG_DO_NOT_WAIT` 频繁返回 `WAS_STILL_DRAWING` | 回读失败，丢帧 | 退化到同步 `Map()`（不加 flag），接受短暂阻塞 |
| 后续 Ticket-05 网络层未就绪 | 编码帧无处发送 | `EncodeReceiver` 在 service.rs 中临时 drop，编码通道关闭时 encoder 优雅退出 |

---

## 依赖关系（Crate 层面）

| Crate | 用途 | 版本 |
|-------|------|------|
| `ffmpeg-next` | FFmpeg Rust 高层绑定（软编、swscale） | 7.0 |
| `ffmpeg-next-sys` | FFmpeg C API 底层 FFI（硬编路径预留） | 7.0 |
| `windows` 0.58 | D3D11 staging 纹理创建、Map/Unmap | 已有 |

---

## 验证

### 编译

```bash
# 第一次构建（自动下载 FFmpeg DLLs）
cargo build -p myowndesk-client

# 验证编码器模块编译
cargo check -p myowndesk-client
```

### 集成测试

按照 spec.md 测试哲学——验证外部可观测行为。编码器的外部行为是：输入 `CapturedFrame` → 输出 `Vec<EncodedFrame>`，NAL 单元可被 FFmpeg 解码。

```rust
// tests/encoder_test.rs

// 1. test_soft_encode_keyframe — 编码第一帧 → 输出为关键帧
// 2. test_soft_encode_delta — 连续编码 → 第二帧为 delta 帧
// 3. test_force_keyframe — request_keyframe() 后 → 输出关键帧
// 4. test_encode_multiple_nal_units — 帧可能分多个 NAL 单元
// 5. test_encoded_frame_roundtrip — 编码 → FFmpeg 解码 → 验证可解码
```

注意：集成测试需要 FFmpeg DLLs 在运行目录。通过 `cargo test` 的 `CARGO_BIN_EXE` 或 build.rs 复制 DLL 到测试目录。

### 手动验证

```bash
# 启动服务（软编编码，当前 console trace 替代网络发送）
cargo run -p myowndesk-client -- --service

# 预期输出：
# [INFO] MyOwnDesk 服务模式启动中...
# [INFO] 屏幕捕获器已初始化: 1920x1080, \\.\DISPLAY1
# [INFO] 编码器已初始化: libx264, 1920x1080, CBR 15Mbps
# [INFO] 服务已启动，按 Ctrl+C 停止
# [INFO] 帧 #1 编码完成: KEYFRAME, 45123 bytes
# [INFO] 帧 #2 编码完成: DELTA, 3124 bytes
# ...
```

---

## 后续扩展路径

| 扩展 | 预计工作量 | 说明 |
|------|-----------|------|
| NVENC 硬件编码 | ~200 行 | 通过 `ffmpeg-next-sys` 调用 h264_nvenc，需要 D3D11 hwdevice_ctx |
| QSV 硬件编码 | ~200 行 | 通过 `ffmpeg-next-sys` 调用 h264_qsv，Intel 集显硬件加速 |
| AMF 硬件编码 | ~200 行 | 通过 `ffmpeg-next-sys` 调用 h264_amf，AMD 独显 |
| 编码器自适应 | — | 根据 CPU 负载动态切换软编/硬编 |
| 动态码率 | — | 根据网络状况调整编码码率 |
