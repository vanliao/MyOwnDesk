//! 客户端配置模块。
//!
//! 从 `client.toml` 加载服务器地址、设备 ID、预共享密钥等配置。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 客户端全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server: ServerConfig,
    pub device: DeviceConfig,
}

/// 中继服务器连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub address: String,
}

/// 本机设备配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// 设备标识（注册到中继时使用）
    pub id: String,
    /// 预共享密钥（hex 编码，从中继服务器的 relay.toml 复制）
    pub pre_shared_key: String,
}

impl ClientConfig {
    /// 加载 `client.toml`，不存在时创建默认配置并返回。
    ///
    /// 默认配置中：
    /// - `device.id` 留空（调用方需用 `resolve_device_id()` 填充）
    /// - `device.pre_shared_key` 留空（用户需手动填写）
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            let config = Self::default_for_path(path)?;
            println!(
                "[config] 已创建默认配置文件: {}",
                path.display()
            );
            println!("[config] 请编辑该文件，填写 pre_shared_key（从中继服务器获取）");
            return Ok(config);
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("无法读取 {}: {}", path.display(), e))?;

        let config: Self = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("配置文件 {} 格式错误: {}", path.display(), e))?;

        Ok(config)
    }

    /// 创建默认配置并写入文件
    fn default_for_path(path: &Path) -> anyhow::Result<Self> {
        let config = Self {
            server: ServerConfig {
                address: "127.0.0.1:21117".to_string(),
            },
            device: DeviceConfig {
                id: String::new(),
                pre_shared_key: String::new(),
            },
        };

        let content = toml::to_string_pretty(&config)
            .map_err(|e| anyhow::anyhow!("序列化默认配置失败: {}", e))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;

        Ok(config)
    }

    /// 解析设备 ID：如果配置中为空，则使用主机名。
    pub fn resolve_device_id(&self) -> String {
        let id = self.device.id.trim();
        if id.is_empty() {
            // 如果无法获取主机名，回退到 "unknown"
            std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "unknown".to_string())
        } else {
            id.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_config_load_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");

        let content = r#"
[server]
address = "192.168.1.100:21117"

[device]
id = "van-laptop"
pre_shared_key = "aabbccdd001122334455"
"#;
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let config = ClientConfig::load(&path).unwrap();
        assert_eq!(config.server.address, "192.168.1.100:21117");
        assert_eq!(config.device.id, "van-laptop");
        assert_eq!(config.device.pre_shared_key, "aabbccdd001122334455");
    }

    #[test]
    fn test_config_load_not_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");

        // 文件不存在时自动创建默认配置
        let config = ClientConfig::load(&path).unwrap();
        assert_eq!(config.server.address, "127.0.0.1:21117");
        assert!(config.device.id.is_empty());
        assert!(config.device.pre_shared_key.is_empty());
        // 验证文件已创建
        assert!(path.exists());
    }

    #[test]
    fn test_config_load_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");

        std::fs::write(&path, "this is not toml {{{").unwrap();

        let result = ClientConfig::load(&path);
        assert!(result.is_err(), "格式错误的 TOML 应返回错误");
    }

    #[test]
    fn test_resolve_device_id_uses_config() {
        let config = ClientConfig {
            server: ServerConfig {
                address: "127.0.0.1:21117".to_string(),
            },
            device: DeviceConfig {
                id: "my-pc".to_string(),
                pre_shared_key: "key".to_string(),
            },
        };
        assert_eq!(config.resolve_device_id(), "my-pc");
    }

    #[test]
    fn test_resolve_device_id_empty_fallback() {
        let config = ClientConfig {
            server: ServerConfig {
                address: "127.0.0.1:21117".to_string(),
            },
            device: DeviceConfig {
                id: "".to_string(),
                pre_shared_key: "key".to_string(),
            },
        };
        // 空 ID 时回退到环境变量，如果都没有则回退到 "unknown"
        let id = config.resolve_device_id();
        assert!(!id.is_empty(), "设备 ID 不应为空");
    }
}
