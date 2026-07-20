# MyOwnDesk 实现 Ticket 拆分

## 依赖关系图

```
01 ──┬── 02 ──┬── 05 ──┬── 07 ── 08 ──┬── 09 ── 10
     │        │        │              │
     ├── 03 ──┴── 04 ──┘              └── 11
     │
     └── 06 ──────────────────────────────┘
```

---

## 01 — 项目骨架 + 协议定义

**What to build:** 搭建三个 crate 工程结构（protocol / client / relay），定义所有 Protobuf 消息并编译生成 Rust 代码，`cargo build` 通过。

**Blocked by:** 无 — 可立即开始

**Status:** ready-for-agent

- [ ] 创建 `myowndesk-protocol` crate，含 `.proto` 文件（Register, Pair, Disconnect, DataPacket, KeyEvent, MouseEvent, Ping/Pong 等）
- [ ] 配置 `prost` 编译 `.proto` 生成 Rust 代码
- [ ] 创建 `myowndesk-client` crate，依赖 protocol
- [ ] 创建 `myowndesk-relay` crate，依赖 protocol
- [ ] 三个 crate 均 `cargo build` 通过
- [ ] `FrameCipher` trait 定义 + `NoOpCipher` 空实现

---

## 02 — 中继服务器

**What to build:** 中继服务器监听 QUIC 端口，客户端连接后可 Register（HMAC 认证）、Pair（配对）、中继双向转发数据、Disconnect（断开）。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ready-for-agent

- [ ] QUIC server 监听配置端口
- [ ] Register 消息处理：验证 HMAC-SHA256(预共享密钥, device_id)，注册到在线设备表
- [ ] 在线设备表：`HashMap<DeviceId, Connection>`，含超时清理
- [ ] Pair 消息处理：查找目标设备，配对双方连接
- [ ] 双向数据转发：A 收到 DataPacket → 发给 B，反之亦然
- [ ] Disconnect 消息处理：解绑配对，通知对端
- [ ] 心跳 Ping/Pong 保活
- [ ] 未知设备 Pair 时返回错误
- [ ] 错误 auth_token Register 时拒绝

---

## 03 — DXGI 屏幕捕获

**What to build:** Windows 服务骨架启动后，通过 DXGI Desktop Duplication 以 60fps 频率捕获主显示器画面，输出 D3D11 纹理。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ready-for-agent

- [ ] Windows 服务注册/启动/停止（`--service` 参数）
- [ ] DXGI 枚举显示器，选择主显示器
- [ ] `IDXGIOutputDuplication::AcquireNextFrame` 捕获 D3D11 纹理
- [ ] 60fps 捕获循环，纹理输出到 channel（`tokio::sync::mpsc`）
- [ ] 服务进程日志输出（`tracing`）

---

## 04 — H.264 视频编码

**What to build:** 从 D3D11 纹理管道中取出帧，通过 FFmpeg 硬件编码器编码为 H.264 NAL 单元，输出到编码帧 channel。

**Blocked by:** 03（DXGI 屏幕捕获）

**Status:** ready-for-agent

- [ ] `ffmpeg-next` 初始化 H.264 硬件编码器（自动发现 NVENC/QSV/AMF）
- [ ] 编码参数：CBR 15 Mbps、`zerolatency` tune、`ultrafast` preset、GOP 60 帧、`high` profile
- [ ] D3D11 纹理 → ffmpeg AVFrame → 编码 → NAL 单元
- [ ] 编码帧输出到 channel，标记帧类型（关键帧 / delta 帧）
- [ ] 无硬件编码器时回退到软件编码（openh264）

---

## 05 — 客户端网络层

**What to build:** 客户端通过 QUIC 连接中继服务器，Register 认证，建立 datagram 和 stream 通道，发送编码帧、接收对端帧。

**Blocked by:** 02（中继服务器）、04（H.264 视频编码）

**Status:** ready-for-agent

- [ ] QUIC 客户端连接中继（`quinn`）
- [ ] Register 消息发送（含 HMAC 认证令牌）
- [ ] 视频帧通过 datagram 发送（NAL 单元 + 帧元数据）
- [ ] 视频帧通过 datagram 接收（对端发来的帧）
- [ ] 控制消息通过 stream 发送/接收（Pair, Disconnect, 心跳）
- [ ] 断线检测，重连按钮触发重新 Register

---

## 06 — 视频解码与渲染

**What to build:** 接收到的 H.264 NAL 单元通过 FFmpeg 软解为 RGB 帧，上传到 D3D11 纹理后在 egui 窗口渲染。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ready-for-agent

- [ ] `ffmpeg-next` H.264 软件解码器初始化
- [ ] NAL 单元 → 解码 → RGB 帧
- [ ] RGB 帧 → D3D11 纹理（`UpdateSubresource`）
- [ ] egui 窗口 + 自定义 painter 贴 D3D11 纹理渲染
- [ ] 处理关键帧到 delta 帧的连续解码

---

## 07 — 端到端流式传输

**What to build:** 串起全链路：被控端捕获→编码→QUIC datagram→中继→QUIC datagram→控制端解码→egui 渲染，1080P 60fps 连续流畅。

**Blocked by:** 05（客户端网络层）、06（视频解码与渲染）

**Status:** ready-for-agent

- [ ] 被控端：03+04+05 串联，捕获→编码→发送循环
- [ ] 控制端：05+06 串联，接收→解码→渲染循环
- [ ] 1080P 60fps 连续传输，画面流畅无卡顿
- [ ] 丢帧时画面短暂闪烁但不阻塞后续帧（datagram 特性验证）
- [ ] 转圈/黑屏等加载状态处理

---

## 08 — 输入回传

**What to build:** 控制端捕获鼠标键盘事件，坐标映射到被控端桌面空间，通过 QUIC stream 发送，被控端 `DesktopInput` 实现调用 `SendInput` 注入。

**Blocked by:** 07（端到端流式传输）

**Status:** ready-for-agent

- [ ] 控制端 egui 捕获鼠标移动/点击/滚轮、键盘按下/释放
- [ ] 坐标映射：`(click_x / view_width) * host_width`，支持分辨率不一致
- [ ] 鼠标事件和键盘事件封装为 Protobuf 消息
- [ ] 通过 QUIC stream（可靠）发送到中继，中继转发
- [ ] 被控端 `DesktopInput` 实现：收到消息 → `SendInput` 注入
- [ ] 绝对坐标鼠标移动，不支持相对坐标

---

## 09 — 完整 GUI

**What to build:** 完整的 egui 用户界面：在线设备列表、点击连接/断开、连接状态提示、重连按钮、远程画面区域、TOML 配置文件。

**Blocked by:** 08（输入回传）

**Status:** ready-for-agent

- [ ] 在线设备列表（定时从中继获取或服务推送）
- [ ] 点击设备 → 发起 Pair 连接
- [ ] 连接中/已连接/已断开 状态显示
- [ ] 远程画面全屏渲染区域（egui 自定义 painter）
- [ ] 断开按钮 → 发 Disconnect
- [ ] 重连按钮 → 重新 Pair
- [ ] TOML 配置文件读取（server address, device id, pre-shared key）
- [ ] 本地 TCP 连接后台服务（127.0.0.1），获取状态和在线列表
- [ ] 错误提示（连接失败、认证失败、设备不在线）

---

## 10 — 双屏支持

**What to build:** 被控端多显示器枚举，每个显示器独立 DXGI 捕获和 H.264 编码，控制端可选择查看屏 1 或屏 2。

**Blocked by:** 09（完整 GUI）

**Status:** ready-for-agent

- [ ] 被控端枚举所有显示器（`EnumOutputs`）
- [ ] 每个显示器独立 `IDXGIOutputDuplication` 实例
- [ ] 每个显示器独立 H.264 编码器实例
- [ ] 控制端切屏按钮（屏 1 / 屏 2）
- [ ] 切屏请求通过 stream 发送 → 被控端切换编码源
- [ ] 双屏场景下的坐标映射（总桌面空间 3840×1080 → 当前选中屏的局部坐标）

---

## 11 — 锁屏功能

**What to build:** `ServiceInput` 实现锁屏安全桌面下的键盘输入（输入密码解锁），断开连接后被控端自动调用 `LockWorkStation` 锁屏。

**Blocked by:** 08（输入回传）

**Status:** ready-for-agent

- [ ] `ServiceInput` 实现 `InputBackend` trait
- [ ] Session 0 检测当前活动桌面（用户桌面 / Winlogon 安全桌面）
- [ ] `OpenInputDesktop` + `SetThreadDesktop` 临时 attach 到目标桌面
- [ ] 在目标桌面中执行 SendInput 注入
- [ ] 完成后 detach 回到 Session 0
- [ ] 收到 Disconnect 通知 → 找到用户会话 → 触发 `LockWorkStation`
- [ ] 失败回退：ServiceInput 不可用时降级到 DesktopInput（已登录桌面可用）
