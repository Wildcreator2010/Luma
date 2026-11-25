Luma Desktop {{VERSION}}
=====================

启动
----
双击 Luma.exe。首次启动会打开登录窗口；也可以点“先看看演示”进入演示模式。

{{CAMERA_NOTE}}

退出
----
关闭主窗口只会隐藏到系统托盘。要彻底退出，请右键托盘图标 →“退出”。
升级或卸载前建议先这样退出；安装包也会尝试自动结束正在运行的实例。

登录凭据
--------
Access Token 存放在 Windows 凭据管理器（服务名 Luma，账户名以 bambu-cn 开头），
不写入安装目录或明文配置文件。

用户数据
--------
%APPDATA%\Luma\settings.json        状态条与界面设置
%LOCALAPPDATA%\Luma\logs\luma.log   运行日志（超过 4 MiB 时滚动为 luma.old.log）
%LOCALAPPDATA%\Luma\logs\panic.log  崩溃日志
%LOCALAPPDATA%\Luma\browser-login   浏览器登录使用的独立浏览器 profile

卸载不会删除上述用户数据。需要彻底清理时，退出 Luma 后手动删除这两个目录，
并在“凭据管理器 → Windows 凭据”中删除服务名 Luma、账户名以 bambu-cn 开头的
全部凭据（例如 bambu-cn-session 及其分片）。

许可证
------
GPL-3.0-only，完整文本见 LICENSE.txt。
