# Ticket 08 — 输入回传 技术分析

> 日期: 2026-07-23
> 状态: 分析完成，待实现

## 概述

控制端捕获鼠标键盘事件，坐标映射到被控端桌面空间，通过 QUIC stream 发送，被控端 `DesktopInput` 调用 `SendInput` 注入。

**依赖:** Ticket 07（端到端流式传输）
**被依赖:** Ticket 09（完整 GUI）、Ticket 11（锁屏功能）

---

## 关键设计决策

| 决策 | 结论 | 理由 |
|------|------|------|
| 输入捕获框架 | minifb poll 模式 | egui 无法安装 |
| InputBackend trait | 新建 `input.rs` | 为 Ticket 11 预留 |
| 键码映射方向 | 控制端映射为 VK 再发送 | 协议 key_code 即 VK 码 |
| 被控端分辨率来源 | 解码帧 `DecodedFrame.width/height` | 零额外开销 |
| 鼠标发送策略 | 位置变化 ≥ 2px 才发送 | 平衡精度和消息量 |
| 键盘覆盖范围 | minifb Key 全部 → VK 全映射 | 完整远程操作 |
| 通信通道 | QUIC stream（可靠） | 输入事件不能丢 |
| 坐标映射公式 | `host_x = mouse_x * frame_w / win_w` | 支持分辨率不一致 |

---

## 架构数据流

```
控制端 (minifb 窗口)          ──QUIC stream──▶     中继          ──QUIC stream──▶     被控端 (Service)
┌─────────────────────┐                        ┌──────────┐                        ┌──────────────────┐
│ 窗口循环 (主线程)     │                        │ 已支持：  │                        │ handle_control   │
│  ├ mouse pos/threshold│   MouseEvent          │ KeyEvent │   forward_encoded_msg  │  ├ KeyEvent       │
│  ├ mouse button      │  ──────────────────▶  │ MouseEvent│ ────────────────────▶ │  ├ MouseEvent     │
│  ├ scroll wheel      │                        │ SwitchDisp│                        │  └ DesktopInput  │
│  └ keyboard press/rel│                        └──────────┘                        │    └ SendInput    │
│       │                                                                          └──────────────────┘
│       ▼ persistent stream
│  input_sender [tokio task]
│    └ 1 条 bi-stream，不发 finish()
│       复用发送所有输入事件
└─────────────────────┘
```

**关键改进：** 输入事件使用**一条持久化 QUIC stream**（不调用 `finish()`），避免每事件开闭流带来的性能开销。

---

## 改动清单

| 文件 | 操作 | 内容 |
|------|------|------|
| `myowndesk-client/Cargo.toml` | 修改 | +2 Windows features |
| `myowndesk-client/src/input.rs` | **新建** | `InputBackend` trait + `DesktopInput` |
| `myowndesk-client/src/keymap.rs` | **新建** | minifb Key → VK 映射表 |
| `myowndesk-client/src/lib.rs` | 修改 | +`pub mod input;` + `pub mod keymap;` |
| `myowndesk-client/src/gui.rs` | 修改 | 输入捕获 + 输入 channel + 持久流发送 |
| `myowndesk-client/src/service.rs` | 修改 | `handle_control_message` 接入 `DesktopInput` |

**不改的文件：** 中继层（已支持 KeyEvent/MouseEvent 转发）、协议层（消息已定义）

---

## 模块设计

### 1. `keymap.rs` — minifb Key → VK 映射

```rust
pub fn minifb_key_to_vk(key: minifb::Key) -> Option<i32>;
pub fn is_extended_key(key: minifb::Key) -> bool;
```

覆盖：A-Z、0-9、F1-F15、方向键、编辑键、修饰键、小键盘、符号键、锁定键、媒体键。约 60 项。

### 2. `input.rs` — InputBackend trait + DesktopInput

```rust
pub trait InputBackend: Send {
    fn send_key(&mut self, key_code: i32, pressed: bool, extended: bool) -> Result<()>;
    fn send_mouse_move(&mut self, x: i32, y: i32) -> Result<()>;
    fn send_mouse_button(&mut self, button: MouseButton, pressed: bool) -> Result<()>;
    fn send_mouse_wheel(&mut self, delta: i32) -> Result<()>;
}

pub struct DesktopInput { screen_width: u32, screen_height: u32 }
```

SendInput 要点：
- 鼠标绝对定位：`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE`，坐标 0-65535
- 鼠标按键：`MOUSEEVENTF_LEFTDOWN/UP` 等
- 滚轮：`MOUSEEVENTF_WHEEL`，delta × WHEEL_DELTA
- 键盘：`INPUT_KEYBOARD` + `KEYEVENTF_SCANCODE`，按下/释放用 `KEYEVENTF_KEYUP`

### 3. `gui.rs` — 控制端输入捕获

**窗口循环新增逻辑（轻量级）：**

```rust
// 新增 channel
let (input_tx, input_rx) = mpsc::unbounded_channel::<proto::Message>();

// 窗口循环内（每帧）
// 鼠标位置：get_mouse_pos() → 与 prev 比较 ≥ 2px → 构造 MouseEvent → input_tx.send()
// 鼠标按键：get_mouse_down() × 3 → 比较变化 → send
// 滚轮：get_scroll_wheel() → 非零 → send
// 键盘：get_keys_pressed() + get_keys_released() → 逐个映射 VK → send
```

**性能优化（吸取上次教训）：**
- 键盘用 `get_keys_pressed()` / `get_keys_released()`，跳过全量 `get_keys()` + HashSet diff
- 鼠标移动加入可选的 30Hz 限速防止过度发送

**网络 task 新增：持久流发送器：**

```rust
let input_sender = tokio::spawn(async move {
    let (mut send, _recv) = input_conn.open_bi().await?;
    while let Some(msg) = input_rx.recv().await {
        let payload = msg.encode_to_vec();
        let len = (payload.len() as u32).to_le_bytes();
        if send.write_all(&len).await.is_err() { break; }
        if send.write_all(&payload).await.is_err() { break; }
        // 不 finish()，保持流打开复用
    }
});
```

### 4. `service.rs` — 被控端输入注入

`handle_control_message` 中 KeyEvent/MouseEvent 分支改为实际调用 `DesktopInput`：

```rust
Some(Type::KeyEvent(ke)) => {
    input.send_key(ke.key_code as i32, ke.pressed, false)?;
}
Some(Type::MouseEvent(me)) => {
    match me.event_type() {
        Move => input.send_mouse_move(me.x, me.y)?;
        ButtonDown | ButtonUp => { ... }
        Wheel => input.send_mouse_wheel(me.wheel_delta)?;
    }
}
```

---

## 已知问题（上次版本的经验）

### 1. ⚠️ 同机测试反馈循环

上次实现发现：GUI 和 Service 在同一台机器上时，`SendInput` 移动光标后，下一帧 `get_mouse_pos()` 读到注入后的新位置，触发重复发送，导致光标漂移。

**修复方式：** 发送 MouseEvent::Move 后，用 `window.get_position()` 计算注入后光标预期位置，写入 `prev_mouse_pos` 而非当前读取位置。

### 2. ⚠️ 流创建开销

上次实现每事件开闭一条 QUIC stream，鼠标高频移动时产生大量流创建，与视频 datagram 竞争连接资源，导致画面延迟。

**修复方式：** 使用持久化 stream（不开 `finish()`），一条流复用所有输入事件。

---

## 实现顺序

1. `Cargo.toml` — 加 Windows features
2. `keymap.rs` — 映射表（纯数据）
3. `input.rs` — trait + DesktopInput
4. `lib.rs` — 注册模块
5. `gui.rs` — 输入捕获 + 持久流发送
6. `service.rs` — 接入 DesktopInput

---

## 测试要点

| 场景 | 预期 |
|------|------|
| 鼠标移动 | 被控端鼠标跟随，≥ 2px 才发送 |
| 鼠标左/右/中键 | 被控端正确执行点击 |
| 滚轮 | 被控端页面滚动 |
| 键盘输入 | 被控端文本输入 |
| Ctrl+C 等组合键 | 被控端执行复制 |
| 分辨率不一致 | 坐标正确映射 |
| 同机测试 | 光标不漂移，无反馈循环 |
| 断开连接 | 输入事件不再发送 |
