use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 解析命令行参数：第一个非 flag 参数作为配置文件路径
    let config_path = std::env::args()
        .nth(1)
        .filter(|arg| !arg.starts_with('-'))
        .unwrap_or_else(|| "relay.toml".to_string());

    info!("加载配置文件: {}", config_path);

    // 加载 / 创建配置
    let config = myowndesk_relay::config::RelayConfig::load_or_create(&config_path)?;

    let key_hex = config.pre_shared_key.as_deref().unwrap_or("(未设置)");
    info!("预共享密钥: {}", key_hex);
    info!("监听地址: {}", config.listen_address);
    info!(
        "心跳: 间隔 {}s, 超时 {}s",
        config.heartbeat_interval_secs, config.heartbeat_timeout_secs
    );

    // 启动服务器
    myowndesk_relay::server::run(config).await
}
