//! H.264 视频解码器模块。
//!
//! 当前实现：基于 `openh264` 的 CPU 软件解码（与编码器使用同一库）。
//! 未来扩展：通过 `VideoDecoder` trait 添加 DXVA2 硬件解码器。
//!
//! # 架构
//!
//! ```text
//! NAL units (annex B, Vec<u8>)
//!     │
//!     ▼
//! VideoDecoder::decode()
//!     ├── 首帧非 IDR → 丢弃（等待 KeyFrameRequest 响应）
//!     ├── openh264 decoder.decode(&nal_units) → DecodedYUV
//!     ├── DecodedYUV::write_rgb8() → RGB24
//!     └── Vec<DecodedFrame>
//!           │
//!           ▼
//!     rgb_tx ──► [gui.rs 渲染]
//! ```

use crate::encoder::FrameType;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;

// ============================================================
// 数据类型
// ============================================================

/// 解码后的帧——解码器输出，供 GUI 渲染消费。
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

// ============================================================
// VideoDecoder trait
// ============================================================

/// 解码器 trait——支持软解和未来硬解的统一接口。
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

// ============================================================
// 解码器工厂
// ============================================================

/// 创建最佳可用解码器。
pub fn create_best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    let decoder = OpenH264Decoder::new()?;
    tracing::info!("openh264 H.264 软解解码器已就绪");
    Ok(Box::new(decoder))
}

// ============================================================
// OpenH264Decoder — openh264 软解实现
// ============================================================

pub struct OpenH264Decoder {
    decoder: Decoder,
    initialized: bool,
}

impl OpenH264Decoder {
    pub fn new() -> anyhow::Result<Self> {
        let config = DecoderConfig::default();
        let decoder = Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| anyhow::anyhow!("openh264 解码器初始化失败: {}", e))?;

        Ok(Self {
            decoder,
            initialized: false,
        })
    }

    /// 检查数据中是否包含 IDR NAL 单元（可独立解码的关键帧）。
    ///
    /// 扫描 Annex B 格式数据中的所有 NAL 单元起始码（0x00 0x00 0x00 0x01），
    /// 检查是否存在 nal_unit_type == 5 的 IDR 帧。
    pub fn contains_idr(data: &[u8]) -> bool {
        let mut pos = 0;
        while pos + 4 < data.len() {
            // 查找 4 字节起始码 0x00 0x00 0x00 0x01
            if data[pos] == 0x00
                && data[pos + 1] == 0x00
                && data[pos + 2] == 0x00
                && data[pos + 3] == 0x01
            {
                if pos + 5 <= data.len() {
                    let nal_header = data[pos + 4];
                    let nal_unit_type = nal_header & 0x1F;
                    if nal_unit_type == 5 {
                        return true;
                    }
                }
                pos += 5; // 跳过起始码 + NAL header
            } else if data[pos] == 0x00
                && data[pos + 1] == 0x00
                && data[pos + 2] == 0x01
            {
                // 3 字节起始码变体
                if pos + 4 <= data.len() {
                    let nal_header = data[pos + 3];
                    let nal_unit_type = nal_header & 0x1F;
                    if nal_unit_type == 5 {
                        return true;
                    }
                }
                pos += 4;
            } else {
                pos += 1;
            }
        }
        false
    }

    /// 将 DecodedYUV 转换为 DecodedFrame（使用自己的 YUV→RGB 替代慢速 write_rgb8）。
    fn convert_yuv(yuv: &openh264::decoder::DecodedYUV, is_keyframe: bool) -> DecodedFrame {
        let (width, height) = yuv.dimensions();
        let slices = yuv.split::<1>();
        let (y_stride, u_stride, _v_stride) = slices[0].strides();
        let y_plane = slices[0].y();
        let u_plane = slices[0].u();
        let v_plane = slices[0].v();
        let rgb_data = yuv_to_rgb_int(
            y_plane, u_plane, v_plane,
            width, height,
            y_stride as usize, u_stride as usize,
        );

        DecodedFrame {
            rgb_data,
            width: width as u32,
            height: height as u32,
            display_index: 0,
            frame_type: if is_keyframe {
                FrameType::Keyframe
            } else {
                FrameType::Delta
            },
        }
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, nal_units: &[u8]) -> anyhow::Result<Vec<DecodedFrame>> {
        if nal_units.is_empty() {
            return Ok(Vec::new());
        }

        // 初始化前只允许含 IDR 的完整帧通过
        if !self.initialized && !Self::contains_idr(nal_units) {
            tracing::debug!("初始化前丢弃帧（无 IDR）");
            return Ok(Vec::new());
        }

        let is_keyframe = Self::contains_idr(nal_units);

        // 喂给解码器
        match self
            .decoder
            .decode(nal_units)
            .map_err(|e| anyhow::anyhow!("openh264 解码失败: {}", e))?
        {
            Some(yuv) => {
                if !self.initialized {
                    self.initialized = true;
                    let (w, h) = yuv.dimensions();
                    tracing::info!(
                        "解码器已初始化: {}x{} (首帧含IDR={})",
                        w, h, is_keyframe
                    );
                }

                Ok(vec![Self::convert_yuv(&yuv, is_keyframe)])
            }
            None => {
                // 解码器需要更多数据
                Ok(Vec::new())
            }
        }
    }

    fn flush(&mut self) -> anyhow::Result<Vec<DecodedFrame>> {
        let remaining = self
            .decoder
            .flush_remaining()
            .map_err(|e| anyhow::anyhow!("flush_remaining 失败: {}", e))?;

        Ok(remaining
            .iter()
            .map(|yuv| Self::convert_yuv(yuv, false))
            .collect())
    }
}

// ============================================================
// 测试
// ============================================================

// ============================================================
// YUV420P → RGB24 整数转换（优化版，比 openh264 write_rgb8 快 100x）
// ============================================================

/// BT.601 整数近似，考虑 stride（行可能有 padding）
fn yuv_to_rgb_int(
    y: &[u8], u: &[u8], v: &[u8],
    width: usize, height: usize,
    y_stride: usize, uv_stride: usize,
) -> Vec<u8> {
    let size = width * height;
    let mut rgb = vec![0u8; size * 3];
    let w = width;
    for row in 0..height {
        let y_row_off = row * y_stride;
        let uv_row_off = (row / 2) * uv_stride;
        for col in 0..w {
            let y_val = y[y_row_off + col] as i32;
            let uv_idx = uv_row_off + (col / 2);
            let u_val = u[uv_idx] as i32 - 128;
            let v_val = v[uv_idx] as i32 - 128;
            let r = (y_val + ((359 * v_val) >> 8)).clamp(0, 255);
            let g = (y_val - ((88 * u_val + 183 * v_val) >> 8)).clamp(0, 255);
            let b = (y_val + ((454 * u_val) >> 8)).clamp(0, 255);
            let out = (row * w + col) * 3;
            rgb[out] = r as u8;
            rgb[out + 1] = g as u8;
            rgb[out + 2] = b as u8;
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CapturedFrame;
    use crate::encoder::{OpenH264Encoder, VideoEncoder as _};

    fn make_test_frame(width: u32, height: u32) -> CapturedFrame {
        let size = (width * height * 4) as usize;
        let mut cpu_buffer = vec![128u8; size];
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                cpu_buffer[idx] = (x % 256) as u8;
                cpu_buffer[idx + 1] = (y % 256) as u8;
                cpu_buffer[idx + 2] = 128;
                cpu_buffer[idx + 3] = 255;
            }
        }
        CapturedFrame {
            texture: None,
            cpu_buffer,
            display_index: 0,
            timestamp: std::time::Instant::now(),
            width,
            height,
        }
    }

    fn encode_one_frame(encoder: &mut OpenH264Encoder, frame: &CapturedFrame) -> Vec<u8> {
        let encoded = encoder.encode(frame).unwrap();
        assert!(!encoded.is_empty(), "编码应产生输出");
        encoded[0].nal_units.clone()
    }

    #[test]
    fn test_create_decoder() {
        let decoder = create_best_decoder();
        assert!(decoder.is_ok(), "解码器初始化应成功: {:?}", decoder.err());
    }

    #[test]
    fn test_decode_keyframe() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);
        let nal_data = encode_one_frame(&mut encoder, &frame);

        assert!(OpenH264Decoder::contains_idr(&nal_data), "第一帧应为 IDR");

        let mut decoder = OpenH264Decoder::new().unwrap();
        let result = decoder.decode(&nal_data).unwrap();
        assert!(!result.is_empty(), "IDR 解码应产生输出");
        assert_eq!(result[0].width, 64);
        assert_eq!(result[0].height, 64);
        assert_eq!(result[0].rgb_data.len(), 64 * 64 * 3);
    }

    #[test]
    fn test_decode_delta_after_keyframe() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        let idr_nal = encode_one_frame(&mut encoder, &frame);
        let delta_nal = encode_one_frame(&mut encoder, &frame);

        let mut decoder = OpenH264Decoder::new().unwrap();
        assert!(!decoder.decode(&idr_nal).unwrap().is_empty());
        let r2 = decoder.decode(&delta_nal).unwrap();
        assert!(!r2.is_empty(), "delta in IDR 后应产出帧");
        assert_eq!(r2[0].frame_type, FrameType::Delta);
    }

    #[test]
    fn test_decode_delta_skipped_before_init() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30).unwrap();
        let frame = make_test_frame(64, 64);

        let _idr_nal = encode_one_frame(&mut encoder, &frame);
        let delta_nal = encode_one_frame(&mut encoder, &frame);

        let mut decoder = OpenH264Decoder::new().unwrap();
        let result = decoder.decode(&delta_nal).unwrap();
        assert!(result.is_empty(), "首帧 delta 应被丢弃");
    }

    #[test]
    fn test_decode_empty() {
        let mut decoder = OpenH264Decoder::new().unwrap();
        let result = decoder.decode(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_contains_idr_detection() {
        // Single IDR NAL unit
        let idr = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x00];
        assert!(OpenH264Decoder::contains_idr(&idr));

        // Single P-slice
        let p = vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x00];
        assert!(!OpenH264Decoder::contains_idr(&p));

        // Single SPS
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x00];
        assert!(!OpenH264Decoder::contains_idr(&sps));

        // SPS + PPS + IDR (typical first encoded frame)
        let multi = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, // IDR
        ];
        assert!(OpenH264Decoder::contains_idr(&multi),
            "应检测到多 NAL 数据中的 IDR");

        // Empty / short
        assert!(!OpenH264Decoder::contains_idr(&[0x00]));
        assert!(!OpenH264Decoder::contains_idr(&[]));
    }
}
