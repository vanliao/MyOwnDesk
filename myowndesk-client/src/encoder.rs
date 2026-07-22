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
use myowndesk_protocol::FrameType;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod,
                       RateControlMode, UsageType};
use openh264::formats::YUVBuffer;

// ============================================================
// 数据类型
// ============================================================

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
    reset_requested: bool,
    frame_count: u64,
}

impl OpenH264Encoder {
    /// 创建 OpenH264 软编。
    pub fn new(width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        let encoder = Self::create_encoder(fps)?;
        tracing::info!(
            "OpenH264 编码器已初始化: {}x{} @ {}fps, CBR 8Mbps, 屏幕实时模式",
            width, height, fps,
        );
        Ok(Self {
            encoder,
            width,
            height,
            fps,
            force_keyframe: false,
            reset_requested: false,
            frame_count: 0,
        })
    }

    /// 创建编码器实例（公开用于重建）。
    fn create_encoder(fps: u32) -> anyhow::Result<Encoder> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(8_000_000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 1))
            .max_slice_len(1200)
            .num_threads(4);

        Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| anyhow::anyhow!("openh264 编码器初始化失败: {}", e))
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(&mut self, frame: &CapturedFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        self.frame_count += 1;
        let t_total = std::time::Instant::now();

        // 0. 如果请求了重建编码器，直接创建新实例（第一帧必为 IDR）
        if self.reset_requested {
            self.reset_requested = false;
            self.force_keyframe = false;
            self.encoder = Self::create_encoder(self.fps)?;
            tracing::info!("编码器已重建，帧 #{}", self.frame_count);
        }

        // 1. 转换 BGRA → YUV420P
        let t0 = std::time::Instant::now();
        let yuv = bgra_to_yuv420p(&frame.cpu_buffer, frame.width as usize, frame.height as usize);
        let convert_us = t0.elapsed().as_micros();

        // 2. 如果需要关键帧，在编码前设置标志（不提前消费——Skip 帧不消耗）
        if self.force_keyframe {
            self.encoder.force_intra_frame();
        }

        // 3. 编码
        let t1 = std::time::Instant::now();
        let bitstream = self.encoder
            .encode(&yuv)
            .map_err(|e| anyhow::anyhow!("openh264 编码帧 #{} 失败: {}", self.frame_count, e))?;
        let encode_us = t1.elapsed().as_micros();

        let raw_frame_type = bitstream.frame_type();

        // 4. 检查是否被跳过（Skip 帧不消耗 force_keyframe）
        if raw_frame_type == openh264::encoder::FrameType::Skip
            || raw_frame_type == openh264::encoder::FrameType::Invalid
        {
            return Ok(Vec::new());
        }

        let was_forced = self.force_keyframe;
        self.force_keyframe = false;
        if was_forced {
            tracing::info!("编码帧 #{}: 强制关键帧已产出", self.frame_count);
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

        let total_us = t_total.elapsed().as_micros();
        let total_ms = total_us as f64 / 1000.0;
        let convert_ms = convert_us as f64 / 1000.0;
        let encode_ms = encode_us as f64 / 1000.0;
        if self.frame_count % 60 == 1 {
            tracing::info!(
                "编码帧 #{}: {} ({} bytes, conv={:.1}ms encode={:.1}ms total={:.1}ms)",
                self.frame_count,
                if is_keyframe { "KEYFRAME" } else { "DELTA" },
                encoded_frame.nal_units.len(),
                convert_ms,
                encode_ms,
                total_ms,
            );
        } else if self.frame_count % 10 == 1 {
            tracing::info!(
                "编码帧 #{}: {} ({} bytes, conv={:.1}ms encode={:.1}ms total={:.1}ms)",
                self.frame_count,
                if is_keyframe { "KEYFRAME" } else { "DELTA" },
                encoded_frame.nal_units.len(),
                convert_ms,
                encode_ms,
                total_ms,
            );
        }

        Ok(vec![encoded_frame])
    }

    fn request_keyframe(&mut self) {
        // 重建编码器比 force_intra_frame 更可靠（ScreenContentRealTime 下可能忽略）
        self.reset_requested = true;
        tracing::info!("编码器: 已请求重建（下一帧产出 IDR）");
    }
}

// ============================================================
// 颜色空间转换：BGRA → YUV420P (I420)
// ============================================================

/// BT.601 整数系数（取整到 256 倍，用右移 8 位代替浮点）
const Y_R: i32 = 77;   // 0.299 * 256
const Y_G: i32 = 150;  // 0.587 * 256
const Y_B: i32 = 29;   // 0.114 * 256
const U_R: i32 = -43;  // -0.169 * 256
const U_G: i32 = -85;  // -0.331 * 256
const U_B: i32 = 128;  // 0.500 * 256
const V_R: i32 = 128;  // 0.500 * 256
const V_G: i32 = -107; // -0.419 * 256
const V_B: i32 = -21;  // -0.081 * 256

/// 将 BGRA 像素数据转换为 YUV420P (I420) 格式。
///
/// YUV420P 平面布局：
/// - Y 平面: width × height 字节
/// - U 平面: (width/2) × (height/2) 字节
/// - V 平面: (width/2) × (height/2) 字节
/// - 总大小: width * height * 3 / 2
///
/// 使用整数 BT.601 近似（零浮点、零分支关键路径）。
fn bgra_to_yuv420p(bgra: &[u8], width: usize, height: usize) -> YUVBuffer {
    let total_size = width * height * 3 / 2;
    let mut yuv = vec![0u8; total_size];

    let (y_plane, rest) = yuv.split_at_mut(width * height);
    let uv_w = width / 2;
    let (u_plane, v_plane) = rest.split_at_mut(uv_w * (height / 2));

    // 按 2x2 块处理：4 个像素共享一对 UV
    for y in (0..height).step_by(2) {
        let y_next = (y + 1).min(height - 1);
        let row0 = y * width;
        let row1 = y_next * width;

        for x in (0..width).step_by(2) {
            let x_next = (x + 1).min(width - 1);

            // 直接索引 4 个 BGRA 像素（无循环、无分支）
            let off_tl = (row0 + x) * 4;
            let off_tr = (row0 + x_next) * 4;
            let off_bl = (row1 + x) * 4;
            let off_br = (row1 + x_next) * 4;

            let b0 = bgra[off_tl] as i32; let g0 = bgra[off_tl + 1] as i32; let r0 = bgra[off_tl + 2] as i32;
            let b1 = bgra[off_tr] as i32; let g1 = bgra[off_tr + 1] as i32; let r1 = bgra[off_tr + 2] as i32;
            let b2 = bgra[off_bl] as i32; let g2 = bgra[off_bl + 1] as i32; let r2 = bgra[off_bl + 2] as i32;
            let b3 = bgra[off_br] as i32; let g3 = bgra[off_br + 1] as i32; let r3 = bgra[off_br + 2] as i32;

            // Y = (77*R + 150*G + 29*B + 128) >> 8
            y_plane[row0 + x] = ((Y_R * r0 + Y_G * g0 + Y_B * b0 + 128) >> 8) as u8;
            y_plane[row0 + x_next] = ((Y_R * r1 + Y_G * g1 + Y_B * b1 + 128) >> 8) as u8;
            y_plane[row1 + x] = ((Y_R * r2 + Y_G * g2 + Y_B * b2 + 128) >> 8) as u8;
            y_plane[row1 + x_next] = ((Y_R * r3 + Y_G * g3 + Y_B * b3 + 128) >> 8) as u8;

            // UV 用 2x2 块平均色
            let avg_r = (r0 + r1 + r2 + r3) >> 2;
            let avg_g = (g0 + g1 + g2 + g3) >> 2;
            let avg_b = (b0 + b1 + b2 + b3) >> 2;

            let u_idx = (y >> 1) * uv_w + (x >> 1);
            // U = ((-43*R - 85*G + 128*B + 128) >> 8) + 128
            u_plane[u_idx] = (((U_R * avg_r + U_G * avg_g + U_B * avg_b + 128) >> 8) + 128) as u8;
            // V = ((128*R - 107*G - 21*B + 128) >> 8) + 128
            v_plane[u_idx] = (((V_R * avg_r + V_G * avg_g + V_B * avg_b + 128) >> 8) + 128) as u8;
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
