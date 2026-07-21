
# MyOwnDesk Spec

## Problem Statement

我有 3-5 台运行 Windows 11 的个人设备（台式机、笔记本、小型服务器），分散在不同地点。我需要从其中任意一台设备远程控制另一台设备的桌面——包括在锁屏状态下输入密码解锁、操作完成后自动锁屏。市面上的方案（TeamViewer、AnyDesk）要么收费、要么依赖第三方服务器。我需要一个自建的、流畅的远程桌面工具。

## Solution

一个两组件系统：

1. **中继服务器**：部署在 Ubuntu 云服务器上，一个进程负责设备上线注册、会话配对、加密数据转发。客户端通过预共享密钥认证。

2. **桌面客户端**：一个 Windows exe，以两种模式运行——后台服务模式（开机自启，SYSTEM 权限，负责屏幕捕获/编码/输入注入）和前台 GUI 模式（用户双击打开，查看在线设备列表、发起/断开连接）。客户端间通过中继转发，视频帧走 QUIC datagram（低延迟、不重传），控制消息和输入事件走 QUIC stream（可靠、有序）。

被控端屏幕以 1080P 60fps 实时传输，H.264 硬件编码，画面流畅。双屏被控端时控制端可选择查看任意一个屏幕。连接断开后被控端自动锁屏。

## User Stories

1. 作为一个远程办公者，我希望在 van-laptop 上打开客户端就能看到 van-pc 在线，点击后直接看到 van-pc 的桌面画面，不用经过任何确认步骤，因为两台都是我的设备。

2. 作为一个远程办公者，我希望 van-pc 锁屏后我也能远程连进去，看到锁屏画面后直接输入密码解锁进入桌面，因为下班后公司电脑是锁屏状态。

3. 作为一个远程办公者，我希望远程操作结束后断开连接时，van-pc 自动进入锁屏状态，因为我不希望断开后桌面敞开着。

4. 作为一个多设备用户，我希望从任意一台设备都能连到任意另一台在线设备，不受方向限制。

5. 作为一个需要流畅体验的用户，我希望操作远程桌面时的鼠标移动和点击感觉跟本地一样即时，画面不掉帧、不撕裂。

6. 作为一个弱网环境下的用户，我希望网络偶尔丢包时画面可能短暂花一下，但鼠标键盘操作不受影响、不会卡住。

7. 作为一个双屏用户，当被控端有两台显示器时，我希望能选择查看屏 1 或屏 2，不用同时传两路视频浪费带宽。

8. 作为一个笔记本用户，当控制端屏幕分辨率（例如 2560×1440）和被控端（1920×1080）不一致时，我希望鼠标点击能映射到正确的位置。

9. 作为一个安装者，我希望安装客户端只需要复制一个 exe 和一个配置文件，注册为 Windows 服务后重启，所有机器共用同一把预共享密钥。

10. 作为一个运维者，我希望中继服务器在 Ubuntu 上一键启动，自动生成密钥，并打印出来方便我复制到各客户端配置文件。

11. 作为一个遇到网络中断的用户，我希望连接断开后客户端明确告诉我已断开，并提供一个重连按钮。

12. 作为一个希望保护隐私的用户，我希望未来能开启端到端加密，让中继服务器无法看到桌面画面内容——即使服务器被入侵。

## Implementation Decisions

### 系统拓扑

被控端（Windows 服务）通过 QUIC 长连接注册到中继服务器（Ubuntu），维持在线状态。控制端（Windows GUI）连接中继后获取在线设备列表，选择目标发起配对。中继查到双方连接后将它们绑定，此后所有数据在中继侧原样转发，不解密、不解析。

### 连接流程

1. 客户端启动时读取 TOML 配置（服务器地址、设备 ID、预共享密钥）。
2. 服务模式向中继发送 `Register` 消息（含 `HMAC(key, device_id)` 认证令牌）。
3. GUI 模式通过本地 TCP（127.0.0.1）连接同机的后台服务，获取连接状态和在线设备列表。
4. 用户点击目标设备 → GUI 通过后台服务向中继发 `Pair` 消息。
5. 中继用 `target_device_id` 查找在线设备表，配对成功后进入转发模式。
6. 控制端发 `Disconnect` → 中继通知被控端 → 被控端触发锁屏。

### 视频管道

被控端每个显示器独立 DXGI Desktop Duplication 实例，控制端选择屏 1 或屏 2。捕获的 D3D11 纹理直接交给 FFmpeg 硬件编码器（NVENC/QSV/AMF），H.264 CBR 编码，`zerolatency` tune，GOP 1 秒。编码出的 NAL 单元封装为 QUIC datagram 发送，不重传。

控制端收到 datagram 后用 FFmpeg 软解 H.264，RBG 帧上传到 D3D11 纹理后在 egui 窗口的自定义 painter 中贴图渲染。

### 输入管道

控制端 egui 捕获鼠标键盘事件 → 坐标乘以 scale_x/scale_y 映射到被控端桌面空间 → 封装为 protobuf 消息 → QUIC stream 发送 → 中继转发 → 被控端收到后调用 `SendInput` 注入。坐标映射支持控制端/被控端分辨率不一致和双屏场景。

### QUIC 通道分工

一个 QUIC 连接内两条通道：
- **Datagram（不可靠）**：视频帧，丢帧直接跳过
- **Stream（可靠）**：Register、Pair、Disconnect、切屏请求、键鼠事件、心跳 Ping/Pong、关键帧请求

### 进程架构

单 exe，`main()` 按命令行参数路由：
- `--service` → Windows 服务模式（Session 0，LOCAL SYSTEM），负责捕获/编码/输入/网络
- 无参数 → GUI 模式（Session 1，普通权限），通过 127.0.0.1 TCP 连接服务进程

### 共享密钥认证

中继服务器首次启动时用 `openssl rand -hex 32` 等价逻辑生成 256 位随机密钥，打印到控制台并写入配置文件。所有客户端配置文件中填写同一密钥。客户端 `Register` 时发送 `HMAC-SHA256(key, device_id)`，中继验证后注册设备。

### 模块与 trait

- `myowndesk-protocol`：Protobuf 消息定义的 `.proto` 文件（`Register`, `Pair`, `Disconnect`, `DataPacket`, `KeyEvent`, `MouseEvent`, 等），由 `prost` 编译为 Rust 代码。`FrameCipher` trait 预留加密接口，当前为 `NoOpCipher` 透传。
- 屏幕捕获 (`capture`)：`ScreenDuplicator` 封装 DXGI Desktop Duplication，输出 D3D11 纹理。
- 视频编码 (`encoder`)：`VideoEncoder` 封装 `ffmpeg-next`，配置 H.264 硬件编码，输入纹理输出 NAL 单元。
- 视频解码 (`decoder`)：`VideoDecoder` 封装 `ffmpeg-next` 软件解码，输入 NAL 单元输出 RGB 帧。
- 输入注入 (`input`)：`InputBackend` trait，`DesktopInput` 和 `ServiceInput` 两个实现。
- 网络 (`net`)：`QuicClient` 管理到中继的 QUIC 连接、datagram 发送、stream 读写。
- GUI (`gui`)：egui 窗口，在线设备列表、远程画面区域、连接/断开/切屏按钮。
- 中继核心 (`relay`)：连接池 `HashMap<DeviceId, Connection>`，配对逻辑 `make_pair(uuid)`，双向转发 `relay(a, b)`，带宽控制。

### Proto 消息（关键字段）

Register 含 device_id、auth_token。Pair 含 target_device_id。DataPacket 含 frame_type (keyframe/delta)、display_index (0/1)、payload (NAL 单元)。预留 encrypted_payload、nonce、key_version 字段。

### 配置文件

客户端 `client.toml`：server.address、device.id、device.pre_shared_key、display.target_fps、display.quality。中继 `relay.toml`：listen_address、pre_shared_key、bandwidth.single_mbps、bandwidth.total_mbps。

## Testing Decisions

### 测试哲学

测试验证外部可观测行为，不验证内部实现。对一个远程桌面系统，外部行为是：发了什么消息、收到了什么消息、消息内容是否正确。

### 唯一集成测试接缝：协议层

测试中继服务器的 QUIC 端口：

- 模拟客户端 A 连接 → 发送 Register → 验证中继已注册该设备
- 模拟客户端 B 连接 → 发送 Register → 发送 Pair(target=A) → 验证中继返回配对成功
- 配对后 A 发送 DataPacket → 验证 B 收到相同的 payload
- A 发送 Disconnect → 验证 B 收到断开通知
- 验证未注册的设备发起 Pair 时收到错误响应
- 验证错误的 auth_token 在 Register 时被拒绝

用真实 QUIC 连接（`quinn`），在测试中启动本地中继实例（随机端口），客户端连接后发送 protobuf 消息并验证响应。

### 单元测试边界

- `InputBackend::DesktopInput`：Mock 验证坐标映射计算、锁屏调用
- 坐标映射函数：纯数学，输入分辨率参数验证输出坐标

### 不做的事

- 不 Mock DXGI/FFmpeg/egui——它们的行为由操作系统和 GPU 驱动保证
- 不做像素级画面验证——这是视频编码器的正确性，不是我们代码的正确性
- 不做 e2e UI 自动化测试

## Out of Scope

- 文件传输
- 声音传输
- 端到端加密（预留接口）
- NAT 打洞 / P2P 直连（预留扩展点）
- 浏览器客户端 / 移动端客户端
- 自动重连
- 多用户 / 权限管理
- 被控端连接确认弹窗

## Further Notes

- 锁屏下的输入注入（`ServiceInput`）是整个项目技术难度最高的模块，排在最后实现。如果最终做不出来，已登录桌面的远程不受影响——仅锁屏场景不可用。
- 中继不解密数据的原则从第一版开始严格遵守，确保后续加 E2E 加密时服务端零改动。
- 双屏扩展点是架构级的：`ScreenDuplicator` 是 per-display 的，编码器也是 per-display 的，加第二路只需多开一个实例。
