# Changelog

本项目遵循 [Semantic Versioning](https://semver.org/)。

## [0.1.1] - 2026-11-26

代号：**Spark 0.1.1（补丁）**

### Added

- AMS 烘干状态：`ams.ams[].dry_time > 0` 时，空闲/完成/未知状态显示为
  `PrinterStatus::Drying`（“烘干”），主界面、状态条与托盘同步。
- 摄像头按 Bambu Studio 的格式补齐 `net_ver` / `dev_ver` / `cli_id` /
  `cli_ver` 参数，并从 `get_version` 缓存设备固件版本。
- 新增 `-Full` 全量版构建：把 BambuSource、live555、agora、FFmpeg 与
  VC++ Runtime 放入安装目录 `camera/`，无需另装 Bambu Studio；新增
  `--camera-check` 验证依赖加载路径。
- 标准版安装包在安装结束时自动从本机 Bambu Studio 导入摄像头依赖到
  `%LOCALAPPDATA%\Luma\camera`（便携版可用 `--import-camera-runtime`），
  发布包本身不携带第三方二进制。
- 首次启动未检测到 Bambu Studio 时提示打开官方下载页；设置页新增
  “摄像头 → 运行库”，可一键检测并导入本机依赖。
- Windows 安装包与便携版：`scripts/build-installer.ps1` 生成每用户 MSI 与便携 ZIP；
  安装到 `%LOCALAPPDATA%\Programs\Luma`，包含开始菜单快捷方式、App Paths、
  Major Upgrade 升级与卸载项。
- exe 版本资源、应用清单（Per-Monitor V2 DPI、长路径、Windows 10/11）与静态 CRT；
  安装版不依赖 VC++ 运行库。
- 单实例：第二次启动通过命名 mutex/event 唤醒已有主窗口或登录窗。
- 新增 `--quit`：安装/升级/卸载前由 MSI 请求运行中的实例退出，避免文件占用失败。
- 文件日志：`%LOCALAPPDATA%\Luma\logs\luma.log`（4 MiB 滚动）与 `panic.log`；
  启动失败时显示系统错误对话框并指向日志目录。
- `--version` / `--help` 启动参数；关于页版本号由构建版本动态注入。
- 「实时监控」页新增打印机内置摄像头实时画面：通过 Bambu Studio 的
  BambuSource 建 RTSPS 流、FFmpeg 解码后推给 Slint；进入页面自动开流，
  可手动暂停，离开页面自动停流。修复 MQTT 局域网 IP 的小端解析
  （此前会连到反向地址并报 `-10057`），并对解码缓冲满（返回 3）做容错。
- 顶部状态条：屏幕顶部常驻 30px 状态条，显示最多 3 台打印机的进度/状态；
  `WS_EX_TOOLWINDOW` 不进任务栏、`WS_EX_NOACTIVATE` 不抢焦点，托盘菜单可开关。
- 状态条始终鼠标穿透（`WS_EX_TRANSPARENT`），点击落到下方窗口；
  鼠标悬停时约 120ms 淡出隐藏，离开后淡入恢复。
- 状态条支持自动宽度：按可见设备、显示内容和字号计算宽度，
  内容变化后自动贴合，关闭自动宽度后仍可手动设置 280–1400 px。
- 状态条高度会随缩放下限自动抬高，避免文字、图标和进度线被裁切；
  自动宽度改为平滑过渡，进度线、开关、分段控件和悬停状态增加过渡动画。
- 状态条彻底移出任务栏（清除 `WS_EX_APPWINDOW`），并忽略关闭请求，
  避免从任务栏误关后让屏幕上的状态条失效。
- 新增「任务栏」停靠：通过 UI Automation 读取任务栏按钮矩形，自动选择最大空白区，
  按槽位宽度减少可见打印机并显示 `+N`；图标变化后自动重新贴合。
- 状态条新增可配置工具栏：时钟（可选秒）、日期、电池百分比/充电状态；
  时钟每秒刷新、电池每 30 秒刷新，参与自动宽度与任务栏空间计算。
- 任务栏模式状态条自动贴近图标区，避免与图标之间留下大片空白；
  宽度过渡轮询提升到 33ms，主题切换与主要表面配色过渡更平滑。
- 状态条启动时淡入；电池图标按电量显示绿/橙/红，充电状态保持绿色。
- 工具栏新增自定义文本（最多 48 字符），可随状态条自动宽度和任务栏避让一起布局。
- 修复浏览器登录完成但不回调：网页 token 校验时接受数字型 `uid`，
  并只把 `token`/access Cookie 作为认证候选，忽略 CSRF 与 refreshToken 噪声。
- 修复已登录仍显示“登录/账户”和“等待设备状态”的 UI：登录后显示 Bambu 账号与 uid，
  收到设备状态后云端状态切换为“云端已连接”，真实模式空活动提示同步更新。
- 状态条支持屏幕/停靠/对齐/尺寸/缩放/透明度/内容自定义，配置写入
  `%APPDATA%\Luma\settings.json`；默认关闭，主界面设置页与托盘均可开关。
- 应用图标（`assets/app-icon.png`）与托盘图标重新绘制；窗口 `icon` 已绑定，
  品牌标记改用同一图标。
- Phase 1/2：`luma-protocol` X2D 状态投影。
- `luma-cloud` 登录、验证码、设备列表、uid、Token（Windows Credential Manager）。
- `luma-cloud` MQTT TLS、订阅、`pushall`、自动重连。
- 显式安装 rustls CryptoProvider；假 Token 网络测试验证 HTTP 401 与 MQTT 认证拒绝。
- `luma-desktop` 后台运行时：真实云端接入、演示模式回退、连接状态。
- `luma-desktop` GUI 登录窗口：密码、验证码、accessToken 导入。
- `luma-desktop` 浏览器登录：独立 Edge/Chrome 实例 + 本地 CDP，抓取
  Cookie / Web Storage / `Authorization` 头，校验后自动导入 token；
  也可用 `--browser-login` 无界面运行。
- `CloudError::CaptchaRequired`：短信接口返回 Geetest `418` 时给出明确提示，
  引导到浏览器登录。
- `luma-cli`：`login/status/devices/import-token/capture/logout`。
- GPL-3.0-only 许可证。
- Phase 0 技术验证工程骨架。

### Fixed

- 摄像头画面串台/停止后又亮起：每次建流分配会话代际，停止、切页或切换打印机后
  丢弃旧会话排队中的帧与状态；切换打印机时先隐藏旧画面。

### Changed

- 桌面端改为 Windows GUI 子系统：双击 exe 不再弹出控制台窗口。
- 数据模型增加 `P2S` 与 N6/O1D/N7/C11/C12/BL-P001 型号码映射。
- 聚合器按 `id`/`node` 合并对象数组。
- UI 增加“演示数据”标识；真实模式隐藏。
- 登录成功后的切换收敛到 `enter_cloud_mode`，密码/验证码/token/浏览器四条
  路径共用同一套状态更新。
- 状态条移除“点击打开主窗口/设置”行为，改为始终点击穿透；设置入口收敛到
  托盘「状态条设置…」与主界面设置页。

### Removed

- 桌面端移除了密码登录、短信验证码、手动粘贴 accessToken 三个入口，
  只保留浏览器登录。CLI 仍保留这些能力用于调试。

## [0.1.0] - 2026-11-23

代号：**Spark（火花）**

### Added

- Cargo workspace 与 5 个 crate：`luma-model`、`luma-core`、`luma-cloud`、`luma-storage`、`luma-desktop`。
- 共享数据模型：`PrinterState`、`PrinterStatus`、`PrinterModel`、`DeviceInfo`。
- 增量报告聚合器（深合并 + 单元测试）。
- 数组按 `id` / `node` 合并，保留未出现的元素；无稳定键数组整体替换。
- 多打印机管理器（独立状态、离线处理、重命名）。
- SQLite 持久化骨架（设备表、历史表、迁移）。
- Slint 1.19 桌面界面 Spike：深浅主题、中文、自定义进度环、动画、模型绑定。
- Slint 原生系统托盘：图标、tooltip、左键显示窗口、右键菜单（显示/退出）。
- Phase 0 文档：架构、协议、数据模型、路线图、风险、品牌。
- 调研文档：CN 云端协议、X2D 字段映射与夹具、Slint Windows 集成、ESP32-S3 可行性。
- UI 设计复审与 P0 修复：层进度同源、对比度、状态语义、完成态、排版。
- 测试：20 个（含 2 个 X2D 增量集成测试）。

### Known Limitations

- 云端登录、设备列表、MQTT 均未实现。
- 桌面端使用模拟数据，`--light` 仅用于截图验证。
- `--complete` 用于 100% 完成态截图验证。
- 开机启动、安装包、DWM 标题栏未验证。
- X2D 协议字段尚未经过真实设备确认。
- 窗口最小尺寸与响应式未完成。

