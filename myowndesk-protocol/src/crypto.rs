use std::error::Error;

/// 视频帧数据加密/解密 trait。
///
/// 当前实现为 [`NoOpCipher`]（透传）。
/// 预留给未来的 ChaCha20-Poly1305 端到端加密。
pub trait FrameCipher: Send + Sync {
    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
}

/// 透传加密器：原样返回数据。
///
/// 在端到端加密实现之前使用。
pub struct NoOpCipher;

impl FrameCipher for NoOpCipher {
    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(data.to_vec())
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(data.to_vec())
    }
}
