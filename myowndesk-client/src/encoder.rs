//! H.264 视频编码器模块。
//!
//! 当前实现：基于 `openh264` 的软件编码（兼容所有 GPU）。
//! 未来扩展：通过 `VideoEncoder` trait 添加 NVENC / QSV / AMF 硬件编码器。
//!
//! # 架构
//!
//! ```text
//! CapturedFrame { cpu_buffer (BGRA), ... }
//!     │
//!     ▼
//! VideoEncoder::encode()
//!     ├── BGRA → YUV420P 转换（手动实现）
//!     ├── YUVBuffer → openh264 编码器
//!     ├── EncodedBitStream → NAL 单元
//!     └── Vec<EncodedFrame>
//!           │
//!           ▼
//!     EncodeSender ──► [Ticket-05 网络层]
//! ```

use crate::capture::CapturedFrame;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod,
                       RateControlMode, UsageType};
use openh264::formats::YUVBuffer;

// ============================================================
// 数据类型
// ============================================================

/// 帧类型——映射到 proto `FrameType` 枚举
///
/// - `Keyframe` → 可独立解码（IDR / I 帧），用于丢包恢复
/// - `Delta` → 依赖前面的关键帧（P 帧）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Keyframe = 0,
    Delta = 1,
}

/// 编码帧——编码器输出，供网络模块发送（Ticket-05）
///
/// 每个 `EncodedFrame` 对应一个 H.264 NAL 单元，
/// 封装为 `DataPacket` 通过 QUIC datagram 传输。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// H.264 NAL 单元字节（直接作为 `DataPacket.payload`）
    pub nal_units: Vec<u8>,
    /// 帧类型（关键帧 / delta 帧）
    pub frame_type: FrameType,
    /// 显示器索引
    pub display_index: u32,
    /// 时间戳（捕获帧序号，毫秒）
    pub pts: i64,
    /// 帧宽度
    pub width: u32,
    /// 帧高度
    pub height: u32,
}

// ============================================================
// VideoEncoder trait
// ============================================================

/// 编码器 trait——支持软编和未来硬编的统一接口。
pub trait VideoEncoder: Send {
    /// 编码一帧。
    ///
    /// 返回 `Vec<EncodedFrame>`，因为一帧可能产生多个 NAL 单元
    ///（SPS/PPS/IDR 初始化信息 + 多个 slice）。
    /// 返回空 Vec 表示该帧被编码器跳过（`FrameType::Skip`）。
    fn encode(&mut self, frame: &CapturedFrame) -> anyhow::Result<Vec<EncodedFrame>>;

    /// 请求强制输出关键帧（IDR）。
    /// 用于丢包恢复（响应 `KeyFrameRequest`）。
    fn request_keyframe(&mut self);
}

// ============================================================
// 编码器工厂
// ============================================================

/// 创建最佳可用编码器。
///
/// 自动发现策略（当前仅软编，未来扩展）：
/// 1. NVENC（NVIDIA 独显）
/// 2. QSV（Intel 集显）
/// 3. AMF（AMD 独显）
/// 4. openh264 软编（所有 GPU——当前实现）
pub fn create_best_encoder(
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    let encoder = OpenH264Encoder::new(width, height, fps)?;
    Ok(Box::new(encoder))
}

// ============================================================
// OpenH264 软编实现
// ============================================================

/// 基于 openh264 的 H.264 软件编码器。
pub struct OpenH264Encoder {
    encoder: Encoder,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    fps: u32,
    force_keyframe: bool,
    frame_count: u64,
}

impl OpenH264Encoder {
    /// 创建 OpenH264 软编。
    ///
    /// 配置参数：
    /// - 码率控制：CBR 15 Mbps
    /// - 帧率：60 fps
    /// - GOP：60 帧（每 60 帧一个 IDR）
    /// - Usage: `ScreenContentRealTime`（屏幕实时内容优化）
    /// - max_slice_len: 1200（适配 MTU，Slice Mode）
    pub fn new(width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(15_000_000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 1))
            .max_slice_len(1200)
            .num_threads(4);

        let encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        ).map_err(|e| anyhow::anyhow!("openh264 编码器初始化失败: {}", e))?;

        tracing::info!(
            "OpenH264 编码器已初始化: {}x{} @ {}fps, CBR 15Mbps, 屏幕实时模式",
            width, height, fps,
        );

        Ok(Self {
            encoder,
            width,
            height,
            fps,
            force_keyframe: false,
            frame_count: 0,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(&mut self, frame: &CapturedFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        self.frame_count += 1;

        // 1. 转换 BGRA → YUV420P
        let yuv = bgra_to_yuv420p(&frame.cpu_buffer, frame.width as usize, frame.height as usize);

        // 2. 如果需要关键帧，在编码前设置标志
        if self.force_keyframe {
            self.encoder.force_intra_frame();
            self.force_keyframe = false;
        }

        // 3. 编码
        let bitstream = self.encoder
            .encode(&yuv)
            .map_err(|e| anyhow::anyhow!("openh264 编码帧 #{} 失败: {}", self.frame_count, e))?;

        let raw_frame_type = bitstream.frame_type();

        // 4. 检查是否被跳过
        if raw_frame_type == openh264::encoder::FrameType::Skip
            || raw_frame_type == openh264::encoder::FrameType::Invalid
        {
            return Ok(Vec::new());
        }

        // 5. 写入编码数据
        let mut encoded_data = Vec::new();
        bitstream.write_vec(&mut encoded_data);

        if encoded_data.is_empty() {
            return Ok(Vec::new());
        }

        let is_keyframe = matches!(
            raw_frame_type,
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );

        let encoded_frame = EncodedFrame {
            frame_type: if is_keyframe { FrameType::Keyframe } else { FrameType::Delta },
            nal_units: encoded_data,
            display_index: frame.display_index,
            pts: (self.frame_count as i64) * 1000 / (self.fps as i64),
            width: frame.width,
            height: frame.height,
        };

        if self.frame_count % 60 == 1 {
            tracing::info!(
                "编码帧 #{}: {} ({} bytes)",
                self.frame_count,
                if is_keyframe { "KEYFRAME" } else { "DELTA" },
                encoded_frame.nal_units.len(),
            );
        }

        Ok(vec![encoded_frame])
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
        tracing::debug!("编码器: 已请求强制关键帧");
    }
}

// ============================================================
// 颜色空间转换：BGRA → YUV420P (I420)
// ============================================================

/// 将 BGRA 像素数据转换为 YUV420P (I420) 格式。
///
/// YUV420P 平面布局：
/// - Y 平面: width × height 字节
/// - U 平面: (width/2) × (height/2) 字节
/// - V 平面: (width/2) × (height/2) 字节
/// - 总大小: width * height * 3 / 2
///
/// 使用 BT.601 标准转换公式。
fn bgra_to_yuv420p(bgra: &[u8], width: usize, height: usize) -> YUVBuffer {
    let total_size = width * height * 3 / 2;
    let mut yuv = vec![0u8; total_size];

    let y_plane_size = width * height;
    let u_plane_size = (width / 2) * (height / 2);

    // 使用 split_at_mut 获取三个互不重叠的可变切片
    let (y_plane, rest) = yuv.split_at_mut(y_plane_size);
    let (u_plane, v_plane) = rest.split_at_mut(u_plane_size);

    // 填充 Y 平面并收集 UV 采样
    // BGRA 像素布局: bgra[offset+0]=B, bgra[offset+1]=G, bgra[offset+2]=R, bgra[offset+3]=A
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let b = bgra[offset] as f32;
            let g = bgra[offset + 1] as f32;
            let r = bgra[offset + 2] as f32;

            // Y 分量 (全分辨率)
            let y_val = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
            y_plane[y * width + x] = y_val;

            // U/V 分量 (2x2 下采样)
            if x % 2 == 0 && y % 2 == 0 {
                let u_idx = (y / 2) * (width / 2) + (x / 2);
                if u_idx < u_plane_size {
                    let u_val = (-0.169 * r - 0.331 * g + 0.500 * b + 128.0).round() as u8;
                    let v_val = (0.500 * r - 0.419 * g - 0.081 * b + 128.0).round() as u8;
                    u_plane[u_idx] = u_val;
                    v_plane[u_idx] = v_val;
                }
            }
        }
    }

    YUVBuffer::from_vec(yuv, width, height)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建一个最小测试帧
    fn make_test_frame(width: u32, height: u32) -> CapturedFrame {
        let size = (width * height * 4) as usize;
        let mut cpu_buffer = vec![128u8; size];
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                cpu_buffer[idx] = (x % 256) as u8;     // B
                cpu_buffer[idx + 1] = (y % 256) as u8; // G
                cpu_buffer[idx + 2] = 128;              // R
                cpu_buffer[idx + 3] = 255;              // A
            }
        }

        CapturedFrame {
            texture: None, // 测试不需要 D3D11 纹理
            cpu_buffer,
            display_index: 0,
            timestamp: std::time::Instant::now(),
            width,
            height,
        }
    }

    #[test]
    fn test_encoder_create() {
        let encoder = OpenH264Encoder::new(1920, 1080, 60);
        assert!(encoder.is_ok(), "编码器初始化应成功");
    }

    #[test]
    fn test_encode_keyframe() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        let result = encoder.encode(&frame).unwrap();
        assert!(!result.is_empty(), "编码应产生输出");
        assert_eq!(result[0].frame_type, FrameType::Keyframe, "第一帧应为关键帧");
    }

    #[test]
    fn test_encode_delta() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        let result1 = encoder.encode(&frame).unwrap();
        assert_eq!(result1[0].frame_type, FrameType::Keyframe);

        let result2 = encoder.encode(&frame).unwrap();
        assert!(!result2.is_empty());
        assert_eq!(result2[0].frame_type, FrameType::Delta);
    }

    #[test]
    fn test_force_keyframe() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        encoder.encode(&frame).unwrap();
        encoder.encode(&frame).unwrap();
        encoder.encode(&frame).unwrap();

        encoder.request_keyframe();
        let result = encoder.encode(&frame).unwrap();
        assert_eq!(result[0].frame_type, FrameType::Keyframe,
            "force_keyframe 后应输出关键帧");
    }

    #[test]
    fn test_frame_metadata() {
        let mut encoder = OpenH264Encoder::new(128, 64, 30).unwrap();
        let frame = make_test_frame(128, 64);

        let results = encoder.encode(&frame).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].width, 128);
        assert_eq!(results[0].height, 64);
        assert_eq!(results[0].display_index, 0);
    }

    #[test]
    fn test_encoded_data_valid() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        let results = encoder.encode(&frame).unwrap();
        assert!(!results.is_empty());
        assert!(!results[0].nal_units.is_empty(), "编码数据不应为空");
        assert!(results[0].nal_units.len() >= 4, "NAL 单元至少应有 4 字节");
        // H.264 起始码: 0x00 0x00 0x00 0x01
        assert_eq!(&results[0].nal_units[0..4], &[0x00, 0x00, 0x00, 0x01],
            "编码数据应以 H.264 起始码开头");
    }

    #[test]
    fn test_bgra_to_yuv420p_encodes_ok() {
        // 验证 BGRA→YUV 转换后的数据可以被编码器正常消费
        let width = 64u32;
        let height = 64u32;
        let mut encoder = OpenH264Encoder::new(width, height, 30).unwrap();
        let frame = make_test_frame(width, height);
        let result = encoder.encode(&frame).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_create_best_encoder() {
        let encoder = create_best_encoder(1920, 1080, 60);
        assert!(encoder.is_ok());
    }
}
