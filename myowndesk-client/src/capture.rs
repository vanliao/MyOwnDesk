//! DXGI Desktop Duplication 屏幕捕获模块。
//!
//! 通过 `IDXGIOutputDuplication` 以 60fps 捕获主显示器桌面画面，
//! 输出 BGRA 格式的 D3D11 纹理。

use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;

// ============================================================
// 数据类型
// ============================================================

/// 捕获帧——通过 channel 传递给编码器（Ticket-04）
///
/// 纹理格式: `DXGI_FORMAT_B8G8R8A8_UNORM`
///
/// 包含两路数据：
/// - `texture`: GPU 纹理（供未来硬件编码使用，None 时表示不可用）
/// - `cpu_buffer`: BGRA 像素数据（供当前软编使用，由 capture 线程回读）
pub struct CapturedFrame {
    /// D3D11 纹理（自有纹理，ID3D11Device::CreateTexture2D 创建）
    pub texture: Option<ID3D11Texture2D>,
    /// BGRA 像素数据（CPU 回读，供软编使用）
    pub cpu_buffer: Vec<u8>,
    /// 显示器索引（0 = 主屏）
    pub display_index: u32,
    /// 捕获时间戳
    pub timestamp: Instant,
    /// 纹理宽度（像素）
    pub width: u32,
    /// 纹理高度（像素）
    pub height: u32,
}

/// DXGI Desktop Duplication 屏幕捕获器
///
/// **线程安全**：不是 `Send`——绑定到创建时的 D3D11 线程。
pub struct ScreenDuplicator {
    /// DXGI Output Duplication 接口
    duplication: IDXGIOutputDuplication,
    /// 显示器名称（调试用）
    #[allow(dead_code)]
    device_name: String,
    /// 共享 D3D11 设备
    device: ID3D11Device,
    /// D3D11 即时上下文（与 device 绑定到同一线程）
    context: ID3D11DeviceContext,
    /// 自有纹理（用于拷贝桌面表面，每帧复用）
    owned_texture: Option<ID3D11Texture2D>,
    /// Staging 纹理（用于 CPU 回读，每帧复用）
    staging_texture: Option<ID3D11Texture2D>,
    /// 纹理宽度
    width: u32,
    /// 纹理高度
    height: u32,
}

// ============================================================
// impl ScreenDuplicator
// ============================================================

impl ScreenDuplicator {
    /// 枚举显示器，选择第一个 AttachedToDesktop 的显示器，
    /// 创建 Duplication 实例。
    ///
    /// `device` / `context`: 由上层 service 创建并注入
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> anyhow::Result<Self> {
        // 1. 创建 DXGI Factory
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
                .map_err(|e| anyhow::anyhow!("CreateDXGIFactory1 失败: {}", e))?;

        // 2. 枚举 Adapter
        let adapter: IDXGIAdapter1 =
            unsafe { factory.EnumAdapters1(0) }
                .map_err(|e| anyhow::anyhow!("EnumAdapters1 失败: {}", e))?;

        // 3. 枚举 Output
        let output: IDXGIOutput =
            unsafe { adapter.EnumOutputs(0) }
                .map_err(|e| anyhow::anyhow!("EnumOutputs 失败: {}", e))?;

        // 4. 获取输出描述
        let desc: DXGI_OUTPUT_DESC =
            unsafe { output.GetDesc() }
                .map_err(|e| anyhow::anyhow!("GetDesc 失败: {}", e))?;

        if !desc.AttachedToDesktop.as_bool() {
            anyhow::bail!("显示器未连接到桌面");
        }

        let device_name = format!(
            "{}",
            String::from_utf16_lossy(&desc.DeviceName)
                .trim_end_matches('\0')
        );

        // 5. 升级到 IDXGIOutput1（DuplicateOutput 在 IDXGIOutput1 上）
        let output1: IDXGIOutput1 = output.cast()
            .map_err(|e| anyhow::anyhow!("Cast 到 IDXGIOutput1 失败: {}", e))?;

        // 6. 创建 Duplication
        let duplication: IDXGIOutputDuplication =
            unsafe { output1.DuplicateOutput(device) }
                .map_err(|e| anyhow::anyhow!("DuplicateOutput 失败: {}", e))?;

        let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
        let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

        tracing::info!(
            "屏幕捕获器已初始化: {}x{}, 设备: {}",
            width,
            height,
            device_name
        );

        // 6. 预创建自有纹理（GPU 端，用于复制桌面表面）
        let owned_texture = create_default_texture(device, width, height)?;
        // 预创建 staging 纹理（CPU 端，用于回读像素数据）
        let staging_texture = create_staging_readback_texture(device, width, height)?;

        Ok(Self {
            duplication,
            device_name,
            device: device.clone(),
            context: context.clone(),
            owned_texture: Some(owned_texture),
            staging_texture: Some(staging_texture),
            width,
            height,
        })
    }

    /// 获取下一帧
    ///
    /// - `timeout_ms`: 等待超时（毫秒），推荐 50ms
    /// - `Ok(Some(frame))` — 新帧
    /// - `Ok(None)` — 超时无新帧
    /// - `Err` — DXGI 错误
    pub fn acquire_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut desktop_resource: Option<IDXGIResource> = None;

        // AcquireNextFrame
        let result = unsafe {
            self.duplication.AcquireNextFrame(
                timeout_ms,
                &mut frame_info,
                &mut desktop_resource,
            )
        };

        match result {
            Ok(()) => {}
            Err(e) => {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                return Err(anyhow::anyhow!("AcquireNextFrame 失败: {}", e));
            }
        }

        // 获取桌面纹理
        let desktop_texture: ID3D11Texture2D =
            desktop_resource
                .ok_or_else(|| anyhow::anyhow!("AcquireNextFrame 返回空资源"))?
                .cast()
                .map_err(|e| anyhow::anyhow!("桌面资源 cast 到纹理失败: {}", e))?;

        // 复制到自有纹理（必须在 ReleaseFrame 前完成）
        if let Some(ref owned) = self.owned_texture {
            let dst: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                owned.clone().cast()?;
            let src: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                desktop_texture.cast()?;
            unsafe {
                self.context.CopyResource(&dst, &src);
            }

            // CPU 回读：CopyResource(owned → staging) → Map → 读像素
            if let Some(ref staging) = self.staging_texture {
                let staging_res: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                    staging.clone().cast()?;
                unsafe {
                    self.context.CopyResource(&staging_res, &dst);
                }

            use windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE;

                // Map staging texture 读像素
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                let map_result = unsafe {
                    self.context.Map(
                        &staging_res,
                        0,
                        windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ,
                        0,
                        Some(&mut mapped),
                    )
                };

                match map_result {
                    Ok(()) => {
                        let src_ptr = mapped.pData as *const u8;
                        let row_pitch = mapped.RowPitch as usize;
                        let buf_size = (self.height as usize) * row_pitch;
                        let mut cpu_buffer = vec![0u8; buf_size];
                        unsafe {
                            std::ptr::copy_nonoverlapping(src_ptr, cpu_buffer.as_mut_ptr(), buf_size);
                        }
                        unsafe {
                            self.context.Unmap(&staging_res, 0);
                        }

                        // 构造 CapturedFrame，包含 cpu_buffer
                        let frame = CapturedFrame {
                            texture: self.owned_texture.as_ref().map(|t| t.clone()),
                            cpu_buffer,
                            display_index: 0,
                            timestamp: Instant::now(),
                            width: self.width,
                            height: self.height,
                        };
                        // 释放帧
                        unsafe { self.duplication.ReleaseFrame() }
                            .map_err(|e| anyhow::anyhow!("ReleaseFrame 失败: {}", e))?;
                        return Ok(Some(frame));
                    }
                    Err(e) => {
                        tracing::warn!("Map staging 纹理失败: {}, 跳过 CPU 回读", e);
                    }
                }
            }
        }

        // 释放帧
        unsafe { self.duplication.ReleaseFrame() }
            .map_err(|e| anyhow::anyhow!("ReleaseFrame 失败: {}", e))?;

        let frame = CapturedFrame {
            texture: self.owned_texture.as_ref().map(|t| t.clone()),
            cpu_buffer: Vec::new(), // 回读失败时为空
            display_index: 0,
            timestamp: Instant::now(),
            width: self.width,
            height: self.height,
        };

        Ok(Some(frame))
    }

    /// 重建 duplicator（`DXGI_ERROR_ACCESS_LOST` 后调用）
    pub fn recreate(&mut self) -> anyhow::Result<()> {
        tracing::warn!("重建 IDXGIOutputDuplication...");

        // 先创建新的 duplicator，再替换旧的（替换时旧值自动 drop）
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
                .map_err(|e| anyhow::anyhow!("CreateDXGIFactory1(重建) 失败: {}", e))?;

        let adapter: IDXGIAdapter1 =
            unsafe { factory.EnumAdapters1(0) }
                .map_err(|e| anyhow::anyhow!("EnumAdapters1(重建) 失败: {}", e))?;

        let output: IDXGIOutput =
            unsafe { adapter.EnumOutputs(0) }
                .map_err(|e| anyhow::anyhow!("EnumOutputs(重建) 失败: {}", e))?;

        let output1: IDXGIOutput1 = output.cast()
            .map_err(|e| anyhow::anyhow!("Cast 到 IDXGIOutput1(重建) 失败: {}", e))?;

        let new_dup =
            unsafe { output1.DuplicateOutput(&self.device) }
                .map_err(|e| anyhow::anyhow!("DuplicateOutput(重建) 失败: {}", e))?;

        // 替换 —— 旧 duplicator 在此处自动 Drop（COM Release）
        self.duplication = new_dup;

        tracing::info!("IDXGIOutputDuplication 重建成功");
        Ok(())
    }
}

// ============================================================
// 辅助函数
// ============================================================

/// 创建 GPU 默认纹理（用于 CopyResource 目的，GPU 端储存）
fn create_default_texture(
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
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    unsafe {
        let mut texture: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| anyhow::anyhow!("CreateTexture2D(默认) 失败: {}", e))?;
        texture.ok_or_else(|| anyhow::anyhow!("CreateTexture2D(默认) 返回空纹理"))
    }
}

/// 创建 CPU 可读的 staging 纹理（用于 D3D11 纹理 → CPU 回读）
fn create_staging_readback_texture(
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
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    unsafe {
        let mut texture: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| anyhow::anyhow!("CreateTexture2D(staging) 失败: {}", e))?;
        texture.ok_or_else(|| anyhow::anyhow!("CreateTexture2D(staging) 返回空纹理"))
    }
}
