fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            println!("[service] Windows 服务模式启动中...");
            // TODO: Ticket-03（DXGI 捕获 + 编码 + QUIC 连接）
        }
        _ => {
            println!("[gui] GUI 模式启动中...");
            // TODO: Ticket-06（视频解码 + 渲染）/ Ticket-09（完整 GUI）
        }
    }
}
