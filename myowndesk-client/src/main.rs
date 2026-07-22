#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            myowndesk_client::service::run().await?;
        }
        Some("--install") => {
            println!("[install] 服务注册功能将在后续实现");
        }
        Some("--uninstall") => {
            println!("[uninstall] 服务卸载功能将在后续实现");
        }
        _ => {
            // Ticket-06: GUI 模式——硬编码直连版本
            myowndesk_client::gui::run().await?;
        }
    }
    Ok(())
}
