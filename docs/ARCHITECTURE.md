# Luma Architecture

版本：0.1.0（Spark）  
状态：Phase 0，架构已定，云端实现待协议验证

## 1. 目标与非目标

### 目标

- Windows 原生桌面应用，实时展示多台 Bambu Lab 打印机状态。
- 「实时监控」页可显示打印机内置摄像头画面（BambuSource + FFmpeg，按需开流）。
- 统一数据模型，桌面端与 ESP32-S3 固件共用。
- 单设备故障不影响其他设备。
- 长期可维护：分层清晰、协议可替换、测试可运行。

### 非目标（第一阶段）

- 不做打印控制（暂停/继续/停止）。
- 不依赖 WebView，不使用 Electron / Tauri + React。
- 不把业务逻辑放到前端脚本。

## 2. 技术栈

| 层 | 选型 | 状态 |
|---|---|---|
| 语言 | Rust | 已确认 |
| 桌面 UI | Slint 1.19 | 已确认并验证可编译运行 |
| 系统托盘 | Slint `SystemTrayIcon` | 已验证注册成功 |
| 异步 | Tokio | 已引入 |
| MQTT | rumqttc（计划） | 未引入，等协议验证 |
| HTTP | reqwest + rustls（计划） | 未引入 |
| 序列化 | serde / serde_json | 已使用 |
| 数据库 | SQLite（rusqlite, bundled） | 已使用 |
| 凭据 | keyring / Windows Credential Manager（计划） | Phase 2 |
| 日志 | tracing / tracing-subscriber | 已使用 |

Rust 下限：1.95（Slint 1.19 要求）。

## 3. 分层

```text
┌──────────────────────────────────────────────┐
│ UI Layer        Slint .slint + 视图模型       │
├──────────────────────────────────────────────┤
│ Application    用例编排、事件分发、节流        │
├──────────────────────────────────────────────┤
│ Domain         PrinterManager / 聚合器 / 状态  │
├──────────────────────────────────────────────┤
│ Infrastructure Cloud / MQTT / HTTP / SQLite   │
└──────────────────────────────────────────────┘
```

约束：

- Slint 页面不直接做网络、数据库、复杂业务。
- UI 只消费不可变状态快照。
- Domain 不依赖 UI、网络库或数据库。
- Infrastructure 通过 `PrinterProvider` 接口向上提供事件。

## 4. Crate 依赖

```text
luma-model  ← 所有人
luma-protocol → luma-model
luma-core   → luma-model
luma-cloud  → luma-core, luma-model
luma-storage→ luma-model
luma-desktop→ luma-model, luma-core, luma-cloud, luma-protocol
luma-cli    → luma-model, luma-core, luma-cloud
```

`luma-model` 只依赖 serde，保证可被 ESP32 固件复用。禁止在此 crate 引入
tokio、slint、rusqlite。

## 5. 线程模型

```text
Slint UI 线程
   │  命令（选择设备、主题）
   ▼
后台运行时线程（Tokio，多线程）
   │  BambuAuth → prepare_session
   │  BambuCloudProvider → ProviderEvent
   ▼
PrinterManager（单写者）+ luma-protocol 投影
   │  状态快照
   ▼
Slint 事件循环（invoke_from_event_loop）
```

规则：

- UI 线程不执行阻塞 I/O。
- SQLite 操作放到 `spawn_blocking` 或专用线程。
- 每台打印机独立任务；单台重连不影响其他设备。
- 状态更新合并节流（目标 ≤ 4 Hz），避免 MQTT 高频增量导致重绘风暴。
- 跨线程只传所有权数据，不共享可变引用。

## 6. 状态流

1. Provider 收到原始 JSON 增量。
2. `ReportAggregator` 深合并为完整原始状态（对象递归合并；带唯一 `id`/`node`
   的对象数组按键合并；其他数组整体替换）。
3. 映射层把原始状态投影为 `PrinterState`（字段映射在协议验证后实现）。
4. `PrinterManager` 更新单设备状态。
5. 应用层生成快照，节流后投递 UI。

第 1、2、4、5 步已实现；第 3 步由 `luma-protocol` 完成。
真实账号/设备验证仍是发布前必须步骤。

## 7. 多设备模型

- 以序列号作为唯一键。
- 设备列表来自云端，用户名称存本地 SQLite。
- 每台设备独立维护：连接状态、聚合器、最后接收时间。
- 未知序列号的报告被拒绝，防止状态串台。
- Provider 抽象允许未来接入 LAN 模式（`BambuLanProvider`）。

## 8. 错误处理

- 库层使用 `thiserror`，错误类型明确。
- 应用边界可使用 `anyhow`（尚未引入）。
- 网络错误必须区分可重试与不可重试。
- 认证失败、Token 过期、协议解析失败必须分别上报，不吞错。
- UI 展示用户可理解的错误，日志保留技术细节（脱敏）。

## 9. 凭据与日志

- 账号密码不明文落盘。
- Token 存 Windows Credential Manager。
- 日志字段白名单；Token、Cookie、密码永不输出。
- 调试抓包文件不提交仓库。

## 10. 持久化

已建表：

- `printers`：序列号、名称、型号、区域、时间戳。
- `print_history`：任务名、开始/结束时间、结果、耗时。

后续：配置表、通知规则、应用设置。数据库迁移采用递增版本号（待实现）。

## 11. 测试策略

- 纯领域逻辑单元测试（已覆盖聚合器、管理器、模型、存储）。
- 协议解析使用脱敏真实报文做 fixture（已接入公开 X2D 夹具）。
- Provider 使用 trait 替身测试状态流。
- UI 以手工截图 + 后续 Slint testing backend 验证。

## 12. 架构决策记录（简）

| 决策 | 选择 | 理由 |
|---|---|---|
| UI 框架 | Slint | 项目硬约束；原生渲染、无 WebView |
| SQLite 驱动 | rusqlite bundled | 单 exe 无外部 DLL；阻塞操作可隔离 |
| 协议投影 | 独立 `luma-protocol` | 纯函数、可夹具回归、桌面/固件可复用 |
| 凭据 | keyring 4 | Windows Credential Manager |
| 序列号做主键 | 是 | 云端唯一标识，名称可改 |
| null 合并语义 | 暂定替换 | 待真实报文验证，测试已标注 |
| 固件构建 | 独立 workspace | ESP 工具链与桌面端隔离 |

## 13. 待验证

- 无边框/自定义标题栏与系统主题联动。
- 运行时按显示器工作区计算窗口尺寸（125% DPI 下固定尺寸会溢出）。
- ESP32-S3 上 Slint 不可用时的墨水屏 UI 方案（LVGL 或自绘）。
- 安装包、开机启动、单实例、Toast 通知。

