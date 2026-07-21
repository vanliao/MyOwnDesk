# Ticket-01: 项目骨架 + 协议定义

## Context

MyOwnDesk 项目目前只有设计文档，无任何代码。Ticket-01 是整个项目的第一步，目标：搭建三个 crate 的工程骨架，定义所有 Protobuf 消息并编译生成 Rust 代码，`cargo build` 全部通过。这是后续所有 Tickets（02-11）的基础。

## 已确认决策（Grilling 结论）

| # | 决策 | 结论 |
|---|------|------|
| 1 | 断开消息 | `Disconnect`（主动）和 `PeerDisconnected`（被动通知，被控端也锁屏）两个独立消息 |
| 2 | 视频帧分片 | 方案 B — 编码器 slice 模式（单 NAL unit per datagram），分包逻辑封装为 trait，方便替换 |
| 3 | Client crate | lib + bin 结构（lib 放逻辑，bin 放入口），集成测试可直接 import 内部模块 |
| 4 | protoc 依赖 | `protoc-bin-vendored` crate，开箱即用 |
| 5 | 辅助字段 | 加 `ErrorCode` 枚举 + `protocol_version` 字段 |

## Deliverables

### 1. 根目录 Cargo Workspace

**文件：`Cargo.toml`**

```toml
[workspace]
members = [
    "myowndesk-protocol",
    "myowndesk-client",
    "myowndesk-relay",
]
resolver = "2"
```

### 2. `myowndesk-protocol` crate

核心目标：定义所有 Protobuf 消息 + `FrameCipher` trait + `NoOpCipher` 空实现 + 视频帧分包 trait。

#### 2a. `myowndesk-protocol/Cargo.toml`

```toml
[package]
name = "myowndesk-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
prost = "0.13"
bytes = "1"

[build-dependencies]
prost-build = "0.13"
protoc-bin-vendored = "3"
```

#### 2b. `myowndesk-protocol/build.rs`

```rust
fn main() {
    prost_build::compile_protos(
        &["src/proto/messages.proto"],
        &["src/proto/"],
    ).unwrap();
}
```

#### 2c. `myowndesk-protocol/src/proto/messages.proto`

包名 `myowndesk`，消息信封 `Message { oneof type }`。

**消息清单（14 个消息 + 4 个枚举）：**

| 消息 | 关键字段 | 说明 |
|------|---------|------|
| `Message` | `oneof type` | 消息信封 |
| `Register` | `device_id`, `auth_token`, `protocol_version` | 设备上线注册，protocol_version 当前为 1 |
| `RegisterResponse` | `error_code`, `error_message`, `online_devices` | 注册结果 + 在线设备列表 |
| `Pair` | `target_device_id` | 发起配对 |
| `PairResponse` | `error_code`, `error_message` | 配对结果 |
| `Disconnect` | `reason` | 控制端主动断开 |
| `PeerDisconnected` | `reason` | 中继通知：对端已离线（被控端收到后也锁屏） |
| `DataPacket` | `frame_type`, `display_index`, `payload` | 视频帧（单个 NAL unit） |
| `DataPacket` | `encrypted_payload`, `nonce`, `key_version` | **预留** E2E 加密字段 |
| `DataPacket` | `fragment_index`, `fragment_count` | **预留** 分包字段（NAL unit > MTU 时兜底） |
| `KeyEvent` | `key_code`, `pressed` | Windows VK_* 虚拟键码 |
| `MouseEvent` | `event_type`, `x`, `y`, `button`, `wheel_delta` | 绝对坐标鼠标事件 |
| `Ping` / `Pong` | `timestamp_ms` | 心跳保活 |
| `SwitchDisplay` | `display_index` | 切屏请求 |
| `KeyFrameRequest` | `display_index` | 丢包后请求 I 帧 |
| `DeviceList` | `device_ids` | 设备上下线增量推送 |

**枚举：**

- `ErrorCode`: `OK = 0`, `AUTH_FAILED = 1`, `DEVICE_NOT_FOUND = 2`, `ALREADY_PAIRED = 3`, `INTERNAL = 4`
- `FrameType`: `KEYFRAME = 0`, `DELTA = 1`
- `MouseEventType`: `MOVE = 0`, `BUTTON_DOWN = 1`, `BUTTON_UP = 2`, `WHEEL = 3`
- `MouseButton`: `LEFT = 0`, `RIGHT = 1`, `MIDDLE = 2`

#### 2d. `myowndesk-protocol/src/crypto.rs`

```rust
use std::error::Error;

pub trait FrameCipher: Send + Sync {
    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
}

pub struct NoOpCipher;

impl FrameCipher for NoOpCipher {
    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(data.to_vec())
    }
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(data.to_vec())
    }
}
```

#### 2e. `myowndesk-protocol/src/fragment.rs`

视频帧分包 trait（Ticket-01 定义接口，后续 Ticket 实现具体逻辑）：

```rust
/// 将大于 MTU 的编码数据拆分为可独立传输的片段
pub trait FrameFragmenter: Send + Sync {
    /// 拆分编码数据为 MTU 以内的小片段
    fn fragment(&self, data: &[u8], mtu: usize) -> Vec<Vec<u8>>;
    /// 尝试将收集的片段重组为完整帧，返回 None 表示尚未收集齐
    fn reassemble(&self, frame_seq: u32, fragment_index: u32, fragment_count: u32, data: &[u8]) -> Option<Vec<u8>>;
}

/// 无分片实现：假设 NAL unit 均 ≤ MTU
pub struct NoOpFragmenter;

impl FrameFragmenter for NoOpFragmenter {
    fn fragment(&self, data: &[u8], _mtu: usize) -> Vec<Vec<u8>> {
        vec![data.to_vec()]
    }
    fn reassemble(&self, _frame_seq: u32, _fragment_index: u32, _fragment_count: u32, data: &[u8]) -> Option<Vec<u8>> {
        Some(data.to_vec())
    }
}
```

#### 2f. `myowndesk-protocol/src/lib.rs`

```rust
pub mod crypto;
pub mod fragment;

// Prost 生成的代码
include!(concat!(env!("OUT_DIR"), "/myowndesk.rs"));

pub use crypto::{FrameCipher, NoOpCipher};
pub use fragment::{FrameFragmenter, NoOpFragmenter};
```

### 3. `myowndesk-client` crate（lib + bin）

**文件：`myowndesk-client/Cargo.toml`**
```toml
[package]
name = "myowndesk-client"
version = "0.1.0"
edition = "2021"

[dependencies]
myowndesk-protocol = { path = "../myowndesk-protocol" }
```

**文件：`myowndesk-client/src/lib.rs`**
```rust
// 后续 Tickets 在此添加模块：service, gui, capture, encoder, decoder, input, net, render
```

**文件：`myowndesk-client/src/main.rs`**
```rust
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            println!("[service] Starting Windows service mode...");
            // TODO: Ticket-03
        }
        _ => {
            println!("[gui] Starting GUI mode...");
            // TODO: Ticket-06
        }
    }
}
```

### 4. `myowndesk-relay` crate

**文件：`myowndesk-relay/Cargo.toml`**
```toml
[package]
name = "myowndesk-relay"
version = "0.1.0"
edition = "2021"

[dependencies]
myowndesk-protocol = { path = "../myowndesk-protocol" }
```

**文件：`myowndesk-relay/src/main.rs`**
```rust
fn main() {
    println!("[relay] MyOwnDesk Relay Server starting...");
    // TODO: Ticket-02
}
```

### 5. 更新 `.gitignore`

添加：`target/`

`Cargo.lock` 对 workspace 含 binary crate 的项目应提交，不忽略。

## Verification

1. `cargo build` 在项目根目录执行，三个 crate 均编译通过
2. 确认 prost 生成的代码：`cargo doc -p myowndesk-protocol` 可看到生成的消息类型
3. `cargo check -p myowndesk-client` 和 `cargo check -p myowndesk-relay` 通过
4. `cargo test -p myowndesk-protocol` 通过（暂时无测试，确保配置正确）

## Files to Create/Modify

| 操作 | 文件 | 说明 |
|------|------|------|
| 新建 | `Cargo.toml` (root) | Workspace 定义 |
| 新建 | `myowndesk-protocol/Cargo.toml` | Protocol crate + protoc-bin-vendored |
| 新建 | `myowndesk-protocol/build.rs` | Prost 编译脚本 |
| 新建 | `myowndesk-protocol/src/proto/messages.proto` | 14 消息 + 4 枚举 |
| 新建 | `myowndesk-protocol/src/lib.rs` | Crate 入口 + prost include |
| 新建 | `myowndesk-protocol/src/crypto.rs` | FrameCipher trait + NoOpCipher |
| 新建 | `myowndesk-protocol/src/fragment.rs` | FrameFragmenter trait + NoOpFragmenter |
| 新建 | `myowndesk-client/Cargo.toml` | lib + bin crate |
| 新建 | `myowndesk-client/src/lib.rs` | 模块占位 |
| 新建 | `myowndesk-client/src/main.rs` | --service / gui 路由骨架 |
| 新建 | `myowndesk-relay/Cargo.toml` | 中继 crate |
| 新建 | `myowndesk-relay/src/main.rs` | Relay 入口骨架 |
| 修改 | `.gitignore` | 添加 `target/` |

## Dependencies

| Crate                   | 用途                        |
| ----------------------- | ------------------------- |
| `prost` 0.13            | Protobuf 运行时编解码           |
| `prost-build` 0.13      | build.rs 编译 .proto → Rust |
| `protoc-bin-vendored` 3 | 自动提供 protoc 二进制（免手动安装）    |
| `bytes` 1               | 零拷贝字节缓冲（prost 所需）         |
