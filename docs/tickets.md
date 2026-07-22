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

**Status:** ✅ done

- [x] 创建 `myowndesk-protocol` crate，含 `.proto` 文件（15 个消息 + 4 个枚举，详见下方消息清单）
- [x] 配置 `prost` + `protoc-bin-vendored` 编译 `.proto` 生成 Rust 代码
- [x] 创建 `myowndesk-client` crate（lib + bin 结构），依赖 protocol
- [x] 创建 `myowndesk-relay` crate，依赖 protocol
- [x] 三个 crate 均 `cargo build` 通过
- [x] `FrameCipher` trait 定义 + `NoOpCipher` 空实现
- [x] `FrameFragmenter` trait 定义 + `NoOpFragmenter` 空实现（视频帧分包，预留）
- [x] ADR: 视频帧分片策略 (`docs/adr/0001-video-frame-fragmentation.md`)

**实现细节：**

Proto 消息清单（`myowndesk-protocol/src/proto/messages.proto`），`Message` 信封 + `oneof type`：

| 消息                 | 关键字段                                            | 说明                               |
| ------------------ | ----------------------------------------------- | -------------------------------- |
| `Register`         | `device_id`, `auth_token`, `protocol_version`   | 设备上线注册，protocol_version 当前为 1    |
| `RegisterResponse` | `error_code`, `error_message`, `online_devices` | 注册结果 + 在线设备列表                    |
| `Pair`             | `target_device_id`                              | 发起配对                             |
| `PairResponse`     | `error_code`, `error_message`                   | 配对结果                             |
| `Disconnect`       | `reason`                                        | 控制端主动断开                          |
| `PeerDisconnected` | `reason`                                        | 中继通知对端已离线（被控端收到后也锁屏）             |
| `DataPacket`       | `frame_type`, `display_index`, `payload`        | 视频帧（单个 NAL unit），预留分包 + E2E 加密字段 |
| `KeyEvent`         | `key_code`, `pressed`                           | Windows 虚拟键码                     |
| `MouseEvent`       | `event_type`, `x`, `y`, `button`, `wheel_delta` | 绝对坐标鼠标事件                         |
| `Ping` / `Pong`    | `timestamp_ms`                                  | 心跳保活                             |
| `SwitchDisplay`    | `display_index`                                 | 切屏请求                             |
| `KeyFrameRequest`  | `display_index`                                 | 丢包后请求 I 帧                        |
| `DeviceList`       | `device_ids`                                    | 设备上下线增量推送                        |

枚举：`ErrorCode`（OK / AUTH_FAILED / DEVICE_NOT_FOUND / ALREADY_PAIRED / INTERNAL）、`FrameType`（KEYFRAME / DELTA）、`MouseEventType`（MOVE / BUTTON_DOWN / BUTTON_UP / WHEEL）、`MouseButton`（LEFT / RIGHT / MIDDLE）

预留字段：`DataPacket` 含 `frame_seq` / `fragment_index` / `fragment_count`（分包）、`encrypted_payload` / `nonce` / `key_version`（E2E 加密）

---

## 02 — 中继服务器

**What to build:** 中继服务器监听 QUIC 端口，客户端连接后可 Register（HMAC 认证）、Pair（配对）、中继双向转发数据、Disconnect（断开）。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ✅ done

- [x] QUIC server 监听配置端口（`0.0.0.0:21117`，可配置）
- [x] Register 消息处理：验证 HMAC-SHA256(预共享密钥, device_id)，注册到在线设备表
- [x] 在线设备表：`HashMap<DeviceId, DeviceEntry>`，含超时清理
- [x] Pair 消息处理：查找目标设备，配对双方连接
- [x] 双向数据转发：A 收到 DataPacket(datagram) → 发给 B；KeyEvent/MouseEvent(stream) → 转发给对端
- [x] Disconnect 消息处理：解绑配对，通知对端
- [x] 心跳 Ping/Pong 保活（10s 间隔，30s 超时）
- [x] 未知设备 Pair 时返回 `PairResponse(error_code=DEVICE_NOT_FOUND)`
- [x] 错误 auth_token Register 时返回 `RegisterResponse(error_code=AUTH_FAILED)`
- [x] 集成测试覆盖（9 个测试用例，真实 QUIC 连接）

---

## 03 — DXGI 屏幕捕获

**What to build:** Windows 服务骨架启动后，通过 DXGI Desktop Duplication 以 60fps 频率捕获主显示器画面，输出 D3D11 纹理。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ✅ done

- [x] Windows 服务注册/启动/停止（`--service` 参数，Ctrl+C 退出；SCM install/uninstall 预留）
- [x] DXGI 枚举显示器，选择主显示器（`IDXGIOutputDuplication`）
- [x] `IDXGIOutputDuplication::AcquireNextFrame` 捕获 D3D11 纹理（BGRA 格式）
- [x] 60fps 捕获循环，纹理输出到 channel（`tokio::sync::mpsc::UnboundedSender<CapturedFrame>`）
- [x] 服务进程日志输出（`tracing`）

---

## 04 — H.264 视频编码

**What to build:** 从 D3D11 纹理管道中取出帧，编码为 H.264 NAL 单元（软编 openh264 / 未来硬编 NVENC/QSV/AMF），输出到编码帧 channel。

**Blocked by:** 03（DXGI 屏幕捕获）

**Status:** ✅ done

- [x] `openh264` 初始化 H.264 软件编码器（因 ffmpeg-next-sys 无法从 Rust 镜像获取，改用 openh264）
- [x] `VideoEncoder` trait 定义 + `OpenH264Encoder` 实现（可替换为未来硬件编码器）
- [x] 编码参数：CBR 15 Mbps、`ScreenContentRealTime` usage、GOP 60 帧、`max_slice_len=1200`、4 线程
- [x] BGRA 像素 → BGRA→YUV420P 转换（BT.601）→ openh264 编码 → NAL 单元
- [x] 编码帧输出到 `EncodeSender/EncodeReceiver` channel，标记帧类型（关键帧 / delta 帧）
- [x] CPU 回读：capture 线程中 add staging 纹理 → CopyResource → Map → 读 BGRA 像素到 `Vec<u8>`
- [x] `CapturedFrame` 增加 `cpu_buffer` 字段，`texture` 改为 `Option`（回读失败时降级）
- [x] `service.rs` 集成：consumer task 使用 `encoder::create_best_encoder()` 编码捕获帧
- [x] 8 个单元测试全部通过（编码器创建、关键帧、delta 帧、强制关键帧、元数据、数据格式、颜色转换）

---

## 05 — 客户端网络层

**What to build:** 客户端通过 QUIC 连接中继服务器，Register 认证，建立 datagram 和 stream 通道，发送编码帧、接收对端帧。

**Blocked by:** 02（中继服务器）、04（H.264 视频编码）

**Status:** ✅ done

- [x] `QuicClient` 模块：connect + register + send_datagram + recv_datagram + send_message + recv_message
- [x] QUIC 客户端连接中继（`quinn` 0.11 + `rustls` 0.23），跳过证书验证
- [x] Register 消息发送（HMAC-SHA256 认证令牌）
- [x] 视频帧通过 datagram 发送（EncodedFrame → DataPacket protobuf → datagram）
- [x] 控制消息通过 stream 收发（4 字节 LE 长度前缀 + protobuf）
- [x] `client.toml` 配置文件（server 地址、设备 ID、预共享密钥）
- [x] `KeyFrameRequest` → 信号 channel → 编码器强制关键帧
- [x] 服务模式集成：捕获 → 编码 → 网络发送全链路串通
- [x] 13 个单元测试通过（编码器 8 + 配置 5），完整工作空间编译通过

---

## 06 — 视频解码与渲染

**What to build:** 接收到的 H.264 NAL 单元通过 openh264 软解为 RGB 帧，minifb 窗口渲染。

**Blocked by:** 01（项目骨架 + 协议定义）

**Status:** ✅ done

- [x] `openh264` H.264 软件解码器初始化（与编码器使用同一库）
- [x] NAL 单元 → 解码 → YUV → RGB 帧
- [x] `VideoDecoder` trait + `OpenH264Decoder` + `create_best_decoder()` 工厂
- [x] minifb 窗口渲染（RGB24 → ARGB buffer）
- [x] 帧重组：按 `frame_seq` 缓存 NAL 单元，完整帧一起送解码器
- [x] 解码错误恢复：失败后跳过 delta 帧，等下一个 IDR 重建解码器
- [x] bounded capture channel（容量 2, try_send 丢帧）控制延迟
- [x] datagram 发送 pacing（每 20 个暂停 1ms）防 quinn 内部丢包
- [x] 6 个单元测试（解码器创建、IDR 解码、delta 解码、初始化前丢弃、空数据、IDR 检测）

---

## 07 — 端到端流式传输

**What to build:** 串起全链路：被控端捕获→编码→QUIC datagram→中继→QUIC datagram→控制端解码→minifb 渲染，1080P 60fps 连续流畅。

**Blocked by:** 05（客户端网络层）、06（视频解码与渲染）

**Status:** ⏳ in-progress — 全链路可跑通、能显示画面，但帧率和延迟未达标

- [x] 被控端：03+04+05 串联，捕获→编码→发送循环
- [x] 控制端：05+06 串联，接收→解码→渲染循环
- [ ] 1080P 60fps 连续传输，画面流畅无卡顿
- [ ] 丢帧时画面短暂闪烁但不阻塞后续帧（datagram 特性验证）
- [ ] 转圈/黑屏等加载状态处理

**当前性能基线（2026-07-22）：**

| 项目 | 值 | 说明 |
|------|-----|------|
| 编码帧率 | ~18 fps（静态桌面） | 屏幕静止时，编码器 ~55ms/帧 |
| 编码帧率 | ~3-8 fps（复杂内容） | 屏幕剧烈变化时，编码器飙到 300-460ms/帧 |
| 端到端延迟 | ~1-2s（静态）~5s+（复杂） | 编码器最慢时延迟积累 |
| BGRA→YUV | ~8ms ✅ | 已整数优化（原 ~52ms，6.5x 提升） |
| openh264 编码 | ~45ms（Delta）/ ~120ms（Keyframe） | 主瓶颈，软编 1080p 上限 |
| 控制端解码 | ~15ms | 正常，不是瓶颈 |

**已知问题：**

1. **编码器峰值 300-460ms** — `ScreenContentRealTime` + `max_slice_len=1200` 导致屏幕内容复杂时 openh264 软编单帧耗时暴增
2. **无丢帧处理** — 当前帧重组直接用 `extend_from_slice` 追加 fragment，中间 datagram 丢失时残缺帧直送解码器，可能崩溃
3. **加载状态仅标题文字** — 窗口标题显示状态文本，无真正的 loading 动画/黑屏指示
4. **配对时序延迟（继承 T06）** — GUI 配对时 service 已编码多帧，配对前的 datagram 被 relay 丢弃，需 KeyFrameRequest 往返

**已知优化方向（待实施）：**

| 方向 | 预期效果 | 代价 |
|------|---------|------|
| 降分辨率到 720p | 编码像素减半 → ~35fps，延迟 ~500ms | 画面清晰度下降 |
| WMF 硬件编码 | 编码 <1ms → 60fps，延迟 <100ms | 需要新 encoder 实现，对集显/NVIDIA 均适用 |
| NVENC 专用硬编 | 编码 <1ms → 60fps | 仅 NVIDIA GPU |

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
- [ ] **T06 遗留**：配对时序导致首帧延迟 ~3-5s。GUI 配对时 service 已编码多帧，配对前的 datagram 被 relay 丢弃，需等 KeyFrameRequest 往返 + 编码器重建。修复方向：① GUI 配对后 service 侧清空编码器再重建（减少等待）② 或者 relay 缓存配对前 N 帧（内存换延迟）③ 或者 GUI 连接时预先指定目标设备一起注册，注册成功即配对，省一次 round-trip

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
