use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_listen_address() -> String {
    "0.0.0.0:21117".to_string()
}

fn default_heartbeat_interval() -> u64 {
    10
}

fn default_heartbeat_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayConfig {
    #[serde(default = "default_listen_address")]
    pub listen_address: String,

    /// 预共享密钥（hex 编码的 32 字节），首次启动自动生成
    pub pre_shared_key: Option<String>,

    /// 心跳间隔（秒）
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// 心跳超时（秒），超过此时间未收到 Pong 则断开
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,
}

impl RelayConfig {
    /// 从 TOML 文件加载配置；文件不存在时创建默认配置并生成密钥
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("无法读取配置文件: {}", path.display()))?;
            let mut config: Self =
                toml::from_str(&content).with_context(|| "配置文件格式错误")?;

            // 密钥为空则自动生成并写回
            let need_save = config
                .pre_shared_key
                .as_deref()
                .map_or(true, |k| k.is_empty());
            if need_save {
                let key = Self::generate_key();
                println!("[relay] 已生成预共享密钥: {}", key);
                config.pre_shared_key = Some(key);
                config.save(path)?;
            }
            Ok(config)
        } else {
            let key = Self::generate_key();
            println!("[relay] 已生成预共享密钥: {}", key);
            let config = Self {
                listen_address: default_listen_address(),
                pre_shared_key: Some(key),
                heartbeat_interval_secs: default_heartbeat_interval(),
                heartbeat_timeout_secs: default_heartbeat_timeout(),
            };
            config.save(path)?;
            println!("[relay] 已创建配置文件: {}", path.display());
            Ok(config)
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("序列化配置失败")?;
        std::fs::write(path, content).context("写入配置文件失败")?;
        Ok(())
    }

    /// 生成 256-bit 随机密钥，hex 编码（64 个 hex 字符）
    pub fn generate_key() -> String {
        let mut key = [0u8; 32];
        rand::thread_rng().fill(&mut key);
        hex::encode(key)
    }

    /// 获取密钥的原始 32 字节
    pub fn key_bytes(&self) -> Result<Vec<u8>> {
        let key_str = self.pre_shared_key.as_deref().unwrap_or("");
        if key_str.is_empty() {
            anyhow::bail!("预共享密钥未设置，请在 relay.toml 中填写 pre_shared_key");
        }
        hex::decode(key_str).with_context(|| "预共享密钥格式错误，应为 hex 编码的 64 个字符")
    }
}
