# 安装与发布

## 产物

```text
dist/
  Luma-<version>-win64.msi              WiX MSI 安装包
  Luma-<version>-win64-portable.zip     便携版
  Luma-<version>-win64-full.msi         全量版 MSI（内置摄像头依赖）
  Luma-<version>-win64-full-portable.zip
  payload/                              标准版内容
  payload-full/                         全量版内容（含 camera/）
  SHA256SUMS.txt                        产物校验值
```

构建命令：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-installer.ps1
```

常用选项：

```powershell
.\scripts\build-installer.ps1 -SkipBuild       # 复用 target\release\luma-desktop.exe
.\scripts\build-installer.ps1 -SkipPortable    # 只出 MSI
.\scripts\build-installer.ps1 -Version 0.2.0   # 须与 Cargo.toml workspace version 一致
.\scripts\build-installer.ps1 -Full            # 追加全量版（需要本机 Bambu Studio）
```

## 安装行为

- 范围：**每用户**（`Scope="perUserOrMachine"` + WiX 生成的
  `ALLUSERS=2` / `MSIINSTALLPERUSER=1`），标准用户无需 UAC。
- 目录：`%LOCALAPPDATA%\Programs\Luma`。
- 快捷方式：`开始菜单\Luma\Luma.lnk`。
- 卸载项：Windows“应用和功能”，卸载程序为 `MsiExec.exe /X{ProductCode}`。
- 注册：`HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\Luma.exe`，
  可在 Win+R 直接输入 `Luma`。
- 升级：固定 `UpgradeCode`，新版本安装包会自动移除旧版本；降级会给出提示。
- 卸载：删除程序、开始菜单快捷方式和 App Paths；**不删除用户数据**。
- 升级/卸载前 MSI 会执行 `Luma.exe --quit`，让正在运行的实例自行退出。
  若 Windows Installer 或安全软件仍在等待 exe 文件句柄，可能额外等待约一分钟；
  为获得最快卸载速度，建议先从托盘菜单“退出”再卸载。

卸载后彻底清理：

1. 退出 Luma（托盘 →“退出”）。
2. 删除 `%APPDATA%\Luma` 与 `%LOCALAPPDATA%\Luma`。
3. 打开“凭据管理器 → Windows 凭据”，删除服务名 `Luma`、账户名以 `bambu-cn`
   开头的全部凭据（主条目 `bambu-cn-session` 及分片）。

## 独立运行

安装版不依赖 Rust、.NET 或 VC++ 运行库：

- `.cargo/config.toml` 为 `x86_64-pc-windows-msvc` 开启 `+crt-static`；
- `scripts/build-installer.ps1` 会检查 exe 是否仍引用 `VCRUNTIME140.dll`，发现即失败；
- 只依赖 Windows 自带系统 DLL（`user32`、`dwmapi`、`opengl32` 等）。

便携版直接把 `payload` 目录复制到任意位置运行；用户数据仍在
`%APPDATA%` / `%LOCALAPPDATA%`，因此换目录不会丢设置。

标准版的打印机内置摄像头画面需要本机安装 Bambu Studio（复用其 `BambuSource` /
FFmpeg 组件）；未安装时应用照常启动，只禁用实时画面。

标准版 MSI 在 `InstallFinalize` 后会自动运行：

```powershell
Luma.exe --import-camera-runtime
```

如果本机存在 Bambu Studio，它会把 `BambuSource`、`live555`、`libaosl`、
`bambu_networking`、Agora、FFmpeg 与 VC++ Runtime 复制到：

```text
%LOCALAPPDATA%\Luma\camera
```

发布包本身不包含这些第三方二进制。便携版没有安装器，需要手动执行上面的命令。
导入后加载顺序为：exe 同级 `camera/` → `%LOCALAPPDATA%\Luma\camera` →
本机 Bambu Studio 目录。

```powershell
# 导入后验证实际加载路径
& "$env:LOCALAPPDATA\Programs\Luma\Luma.exe" --camera-check
```

卸载不会删除 `%LOCALAPPDATA%\Luma\camera`；需要彻底清理时一并删除该目录。

## 全量版（包含摄像头依赖）

全量版把摄像头运行库放进安装目录的 `camera/`，应用启动时优先从这里加载，
不再依赖 `%APPDATA%\BambuStudio\plugins` 或 `Program Files\Bambu Studio`。

```powershell
.\scripts\build-installer.ps1 -Full
```

如果自动探测失败，可指定目录：

```powershell
.\scripts\build-installer.ps1 -Full `
  -BambuStudioDir 'C:\Program Files\Bambu Studio' `
  -BambuPluginDir "$env:APPDATA\BambuStudio\plugins"
```

打包内容：

- BambuSource / live555 / libaosl / bambu_networking / agora 相关 DLL
- Bambu Studio 自带的 FFmpeg（avcodec / avutil / swscale / swresample）
- Microsoft VC++ Runtime（msvcp140 / vcruntime140 / concrt140 等）
- `camera/THIRD_PARTY_NOTICES.txt` 与 AGPL-3.0 文本

验证全量版依赖（不连接打印机）：

```powershell
& "$env:LOCALAPPDATA\Programs\Luma\Luma.exe" --camera-check
```

成功时输出 `BambuSource:` 和 `FFmpeg:` 的实际加载目录，应指向安装目录的
`camera/`。

> **许可证提示**：BambuSource / bambu_networking 等由 Bambu Lab 分发，
> Bambu Studio 为 AGPL-3.0，部分组件可能是非自由二进制；Microsoft VC++
> Runtime 也有单独的再分发条款。全量版只适合在你有权使用的机器上本地构建；
> 未经确认再分发权，不要把 `-full` 产物上传到公开 Releases。
>
> 已核查本机 Bambu Studio 的 `avutil-59.dll`，其中内置字符串明确为
> `libavutil license: nonfree and unredistributable`；Bambu Studio README
> 也说明 networking plugin 基于 non-free libraries。因此当前机器产出的
> full 包**不能**作为公开 GitHub Release 资产上传。

## 程序资源与清单

`crates/luma-desktop/build.rs` 在构建时生成 Windows 资源脚本：

- `assets/app.ico`：exe 图标、快捷方式图标、任务栏图标；
- `VERSIONINFO`：来自 Cargo package version，`FileVersion` / `ProductVersion` /
  `CompanyName` / `Copyright` 等；
- `app.manifest`：`asInvoker`、Per-Monitor V2 DPI、`longPathAware`、
  Windows 10/11 `supportedOS`。

版本号来自工作区 `Cargo.toml` 的 `workspace.package.version`；`VERSION` 供打包脚本
命名产物。两者必须一致，`scripts/build-installer.ps1` 会校验；UI 的“关于”页和
exe 属性由 Cargo 版本驱动。

## 单实例

`src/instance.rs` 使用当前登录会话的命名 mutex 判重，命名 event 做进程间唤醒。
第二次启动不会新建托盘、状态条或云端连接，只把已有窗口（或登录窗）拉到前台。
`--browser-login` 辅助模式不参与互斥，可与主程序并存。

## 日志

| 文件 | 内容 |
| --- | --- |
| `%LOCALAPPDATA%\Luma\logs\luma.log` | 运行日志，默认 `info`；超过 4 MiB 启动时滚动为 `luma.old.log` |
| `%LOCALAPPDATA%\Luma\logs\panic.log` | panic / 启动失败现场，含 backtrace |
| `%LOCALAPPDATA%\Luma\browser-login` | 浏览器登录独立 profile |

`LUMA_LOG_FILE` 可覆盖日志路径；`RUST_LOG` 控制级别（例如 `RUST_LOG=debug`）。
启动失败时 GUI 会弹出系统对话框并指向日志目录，不会静默退出。

## 代码签名

当前脚本不签名。未签名安装包在首次运行时可能触发 SmartScreen。正式分发时建议：

```powershell
# 先签名 exe，再打包；或打包后签名 MSI
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
  /sha1 <证书指纹> .\target\release\luma-desktop.exe
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
  /sha1 <证书指纹> .\dist\Luma-0.1.1-win64.msi
```

签名后再重新生成 `SHA256SUMS.txt`，并随安装包一起发布。

## 发布检查清单

- [ ] `VERSION` 与 `Cargo.toml` 的 workspace version 一致。
- [ ] `cargo test --workspace` 通过。
- [ ] `scripts/build-installer.ps1` 成功，静态 CRT 检查通过。
- [ ] 在干净 Windows 10/11 x64 上完成安装、启动、升级、卸载。
- [ ] 确认开始菜单、App Paths、单实例唤起作用。
- [ ] （正式发布）签名 exe 与 MSI，更新 SHA-256。
