//! 打印机内置摄像头。
//!
//! 取流链路：Bambu 私有 RTSPS（`bambu:///rtsps___bblp:<code>@<ip>/…`）→
//! 通过 Bambu Studio 随附的 `BambuSource.dll` 拿到 H.264 Annex B 样本 →
//! 用同一安装目录下的 FFmpeg 解码 → RGBA 推给 Slint。
//!
//! 约束：
//! - 访问码只存在于本模块的内存注册表里，绝不进入 Slint 属性、日志或错误文案。
//! - 所有阻塞调用（建流、读取、关闭）都在后台线程，UI 线程只接收 `Image`。
//! - 直播地址不走本地 6000 端口，也不使用 `rtsp__` 前缀（实测均失败）。

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use libloading::Library;
use serde_json::Value;
use slint::{Image, Model, Rgba8Pixel, SharedPixelBuffer};
use windows::core::PCWSTR;
use windows::Win32::Networking::WinSock::{WSAStartup, WSADATA};
use windows::Win32::System::LibraryLoader::SetDllDirectoryW;

use crate::AppWindow;

/// 输出宽度上限。1080p 源缩到 960 宽，省一半拷贝又不影响观感。
const TARGET_WIDTH: i32 = 960;
/// UI 推送节流（约 15 fps）。解码仍然逐帧进行，否则 P 帧链会断。
const FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// StartStream 的 would_block 轮询上限，超过后认为摄像头无响应。
const START_WAIT: Duration = Duration::from_secs(12);
/// StartStream 的轮询间隔。Bambu Studio 自身用 100ms；但 RTSPS 握手在
/// 打印机负载较高时会超过 150ms，此时过密轮询会让 live555 读到一个尚未
/// 连上的 socket（WSAENOTCONN，-10057），所以这里留足余量。
const START_POLL_INTERVAL: Duration = Duration::from_millis(300);
/// 等待 MQTT 上报局域网 IP 的上限。
const IP_WAIT: Duration = Duration::from_secs(20);
/// Bambu_ReadSample 返回 3 表示内部缓冲已满，短暂让出再继续，不能当断流。
const BUFFER_LIMIT_PAUSE: Duration = Duration::from_millis(5);

const AV_CODEC_ID_H264: c_int = 27;
const AV_PIX_FMT_RGBA: c_int = 26;
const SWS_BILINEAR: c_int = 2;
const AVERROR_EAGAIN: c_int = -11;
const AVERROR_EOF: c_int = -541_478_725;

// ---------------------------------------------------------------------------
// 凭据 / 地址注册表
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    access: HashMap<String, String>,
    address: HashMap<String, Ipv4Addr>,
    version: HashMap<String, String>,
}

/// 摄像头会话代际。新会话会让旧会话仍在排队的 UI 回调失效，避免旧打印机的
/// 最后一帧在新打印机/停止后重新把画面点亮。
static SESSION: AtomicU64 = AtomicU64::new(0);

fn next_session() -> u64 {
    SESSION.fetch_add(1, Ordering::SeqCst) + 1
}

fn is_current_session(generation: u64) -> bool {
    SESSION.load(Ordering::SeqCst) == generation
}

fn invalidate_session(generation: u64) {
    let _ = SESSION.compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst);
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// 记录云端返回的局域网访问码。空值与空序列号一律忽略。
pub fn register_access_code(serial: &str, code: &str) {
    let serial = serial.trim();
    let code = code.trim();
    if serial.is_empty() || code.is_empty() {
        return;
    }
    if let Ok(mut registry) = registry().lock() {
        registry.access.insert(serial.to_string(), code.to_string());
    }
}

/// MQTT 上报的 `net.info` 里解析出的设备地址。
pub fn set_ip(serial: &str, ip: Ipv4Addr) {
    if serial.is_empty() || ip.is_unspecified() || ip.is_loopback() {
        return;
    }
    if let Ok(mut registry) = registry().lock() {
        registry.address.insert(serial.to_string(), ip);
    }
}

/// 从聚合后的完整报文里提取设备固件版本，供 BambuSource 的 `dev_ver` 参数使用。
pub fn update_from_report(serial: &str, report: &Value) {
    let Some(version) = device_version(report) else {
        return;
    };
    if serial.is_empty() || version.is_empty() {
        return;
    }
    if let Ok(mut registry) = registry().lock() {
        registry
            .version
            .insert(serial.to_string(), version.to_string());
    }
}

fn device_version(report: &Value) -> Option<&str> {
    report
        .get("get_version")
        .and_then(|value| value.get("info"))
        .and_then(|value| value.get("module"))
        .and_then(Value::as_array)?
        .iter()
        .find(|module| module.get("name").and_then(Value::as_str) == Some("ota"))
        .and_then(|module| module.get("sw_ver"))
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
}

/// 退出登录时清掉内存里的访问码。
pub fn clear_credentials() {
    if let Ok(mut registry) = registry().lock() {
        registry.access.clear();
        registry.address.clear();
        registry.version.clear();
    }
}

fn lookup(serial: &str) -> (Option<Ipv4Addr>, Option<String>, Option<String>) {
    let Ok(registry) = registry().lock() else {
        return (None, None, None);
    };
    (
        registry.address.get(serial).copied(),
        registry.access.get(serial).cloned(),
        registry.version.get(serial).cloned(),
    )
}

// ---------------------------------------------------------------------------
// 会话句柄
// ---------------------------------------------------------------------------

/// UI 侧持有的摄像头会话。丢弃即停止后台线程。
pub struct CameraHandle {
    serial: String,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    generation: u64,
}

impl CameraHandle {
    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn is_finished(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // 已自然结束的会话保留代际，让最后一条错误状态有机会显示；
        // 运行中的会话立即失效，阻止排队中的旧帧重新点亮画面。
        if !self.done.load(Ordering::SeqCst) {
            invalidate_session(self.generation);
        }
    }
}

impl Drop for CameraHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(ui: slint::Weak<AppWindow>, serial: String) -> CameraHandle {
    let generation = next_session();
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let handle = CameraHandle {
        serial: serial.clone(),
        stop: stop.clone(),
        done: done.clone(),
        generation,
    };
    let worker_stop = stop.clone();
    let worker_done = done.clone();
    let spawned = std::thread::Builder::new()
        .name("luma-camera".to_string())
        .spawn(move || {
            run(ui, serial, worker_stop, generation);
            worker_done.store(true, Ordering::SeqCst);
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "failed to spawn camera thread");
        invalidate_session(generation);
        done.store(true, Ordering::SeqCst);
    }
    handle
}

/// 只加载摄像头运行库，不建立打印机连接。用于验证全量版依赖是否完整。
pub fn self_check() -> Result<String, String> {
    let plugin = bambu_plugin_dir().ok_or_else(|| {
        "未找到 BambuSource.dll（标准版需安装 Bambu Studio，全量版需 camera 目录）".to_string()
    })?;
    let ffmpeg = bambu_studio_dir().ok_or_else(|| {
        "未找到 FFmpeg DLL（标准版需安装 Bambu Studio，全量版需 camera 目录）".to_string()
    })?;
    load_bambu().map_err(|error| error.message().to_string())?;
    load_ffmpeg().map_err(|error| error.message().to_string())?;
    Ok(format!(
        "BambuSource: {}\nFFmpeg: {}",
        plugin.display(),
        ffmpeg.display()
    ))
}

/// 当前摄像头运行库的加载来源；用于设置页和首次启动提示。
pub fn runtime_status() -> String {
    if let Some(dir) = bundled_camera_dir() {
        if dir.join("BambuSource.dll").is_file() && find_dll(&dir, "avcodec-").is_some() {
            return "已内置（安装目录 camera/）".to_string();
        }
    }
    if let Some(dir) = imported_camera_dir() {
        if dir.join("BambuSource.dll").is_file() && find_dll(&dir, "avcodec-").is_some() {
            return "已导入（用户数据 camera/）".to_string();
        }
    }
    if local_bambu_plugin_dir().is_some() && local_bambu_studio_dir().is_some() {
        return "使用本机 Bambu Studio".to_string();
    }
    "未检测到 Bambu Studio".to_string()
}

pub fn runtime_available() -> bool {
    runtime_status() != "未检测到 Bambu Studio"
}

pub fn should_prompt_setup() -> bool {
    !runtime_available()
        && setup_marker_path()
            .map(|path| !path.exists())
            .unwrap_or(true)
}

pub fn mark_setup_prompted() {
    let Some(path) = setup_marker_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"1");
}

fn setup_marker_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("Luma").join("camera-setup-prompted"))
}

/// 从用户本机已安装的 Bambu Studio 复制摄像头依赖到 Luma 用户数据目录。
///
/// 不随 Luma 分发第三方二进制；只在用户自己的机器上做本地复制。
pub fn import_runtime() -> Result<String, String> {
    let studio = local_bambu_studio_dir()
        .ok_or_else(|| "未找到 Bambu Studio 安装目录（需要 avcodec-*.dll）".to_string())?;
    let plugin = local_bambu_plugin_dir()
        .ok_or_else(|| "未找到 Bambu Studio 摄像头插件目录（需要 BambuSource.dll）".to_string())?;
    let target = imported_camera_target()?;
    std::fs::create_dir_all(&target)
        .map_err(|error| format!("创建 {} 失败：{error}", target.display()))?;

    let plugin_files = [
        "BambuSource.dll",
        "live555.dll",
        "libaosl.dll",
        "bambu_networking.dll",
        "agora_rtc_sdk.dll",
        "libagora-ffmpeg.dll",
        "libagora-soundtouch.dll",
    ];
    for required in ["BambuSource.dll", "live555.dll", "libaosl.dll"] {
        if !plugin.join(required).is_file() {
            return Err(format!("Bambu Studio 插件缺少 {required}"));
        }
    }

    let mut copied = 0usize;
    for name in plugin_files {
        let source = plugin.join(name);
        if source.is_file() {
            copy_file(&source, &target.join(name))?;
            copied += 1;
        }
    }

    for prefix in ["avcodec-", "avutil-", "swscale-", "swresample-"] {
        let files = files_with_prefix(&studio, prefix)?;
        if files.is_empty() {
            return Err(format!("Bambu Studio 目录缺少 {prefix}*.dll"));
        }
        for file in files {
            let name = file
                .file_name()
                .ok_or_else(|| "无效的 FFmpeg 文件名".to_string())?;
            copy_file(&file, &target.join(name))?;
            copied += 1;
        }
    }

    let system32 = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .map(|dir| dir.join("System32"))
        .ok_or_else(|| "无法定位 Windows System32".to_string())?;
    for name in [
        "msvcp140.dll",
        "msvcp140_1.dll",
        "msvcp140_2.dll",
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "concrt140.dll",
    ] {
        let source = system32.join(name);
        if source.is_file() {
            copy_file(&source, &target.join(name))?;
            copied += 1;
        }
    }
    for required in ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"] {
        if !target.join(required).is_file() {
            return Err(format!("系统缺少 VC++ 运行库：{required}"));
        }
    }

    std::fs::write(
        target.join("THIRD_PARTY_NOTICES.txt"),
        include_str!("../../../installer/THIRD_PARTY_NOTICES.txt"),
    )
    .map_err(|error| format!("写入第三方说明失败：{error}"))?;
    std::fs::write(
        target.join("AGPL-3.0.txt"),
        include_str!("../../../installer/AGPL-3.0.txt"),
    )
    .map_err(|error| format!("写入 AGPL 文本失败：{error}"))?;

    Ok(format!(
        "已从 {} 导入 {copied} 个文件到 {}",
        studio.display(),
        target.display()
    ))
}

fn copy_file(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::copy(source, target)
        .map(|_| ())
        .map_err(|error| format!("复制 {} 失败：{error}", source.display()))
}

fn files_with_prefix(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("读取 {} 失败：{error}", dir.display()))?;
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.to_ascii_lowercase()
                            .starts_with(&prefix.to_ascii_lowercase())
                    })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// UI 推送
// ---------------------------------------------------------------------------

fn push_status(ui: &slint::Weak<AppWindow>, generation: u64, text: &str, live: bool) {
    let weak = ui.clone();
    let text = text.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if !is_current_session(generation) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_camera_status(text.into());
            ui.set_camera_live(live);
        }
    });
}

fn push_frame(
    ui: &slint::Weak<AppWindow>,
    generation: u64,
    serial: &str,
    buffer: SharedPixelBuffer<Rgba8Pixel>,
) {
    let weak = ui.clone();
    let serial = serial.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if !is_current_session(generation) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            if ui.get_page() != 2 {
                return;
            }
            let printers = ui.get_printers();
            let selected = ui.get_selected();
            let selected_matches = selected >= 0
                && (selected as usize) < printers.row_count()
                && printers
                    .row_data(selected as usize)
                    .map(|printer| printer.serial.as_str() == serial)
                    .unwrap_or(false);
            if !selected_matches {
                return;
            }
            ui.set_camera_frame(Image::from_rgba8(buffer));
            if !ui.get_camera_live() {
                ui.set_camera_live(true);
                ui.set_camera_status("摄像头已连接".into());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 错误与重试
// ---------------------------------------------------------------------------

enum CameraError {
    /// 页面已关闭或用户主动停止。
    Cancelled,
    MissingIp,
    MissingAccessCode,
    PluginMissing,
    DecoderMissing,
    Create,
    Open,
    StartTimeout,
    UnsupportedCodec,
    StreamEnded,
    Decode,
}

impl CameraError {
    fn message(&self) -> &'static str {
        match self {
            CameraError::Cancelled => "摄像头已关闭",
            CameraError::MissingIp => "未获取到打印机 IP，请确认打印机已联网",
            CameraError::MissingAccessCode => "无法获取局域网访问码，请重新登录账号",
            CameraError::PluginMissing => "未找到 Bambu 摄像头组件（需安装 Bambu Studio）",
            CameraError::DecoderMissing => "未找到摄像头解码组件（需安装 Bambu Studio）",
            CameraError::Create | CameraError::Open => {
                "无法连接摄像头，请确认打印机已开启局域网实时预览"
            }
            CameraError::StartTimeout => {
                "摄像头无响应：请在打印机屏幕开启「局域网实时预览」，或关闭 Bambu Studio 的实时画面"
            }
            CameraError::UnsupportedCodec => "该机型的摄像头编码暂不支持",
            CameraError::StreamEnded | CameraError::Decode => "摄像头画面已中断",
        }
    }

    /// 可重试的错误：连接类问题值得再试一次。
    fn retryable(&self) -> bool {
        matches!(
            self,
            CameraError::Open | CameraError::StartTimeout | CameraError::StreamEnded
        )
    }
}

fn run(ui: slint::Weak<AppWindow>, serial: String, stop: Arc<AtomicBool>, generation: u64) {
    push_status(&ui, generation, "正在连接摄像头…", false);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match stream_once(&ui, &serial, &stop, generation) {
            Ok(()) => break,
            Err(CameraError::Cancelled) => break,
            Err(error) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                tracing::warn!(serial = %serial, reason = error.message(), "camera stream failed");
                if attempt >= 2 || !error.retryable() {
                    push_status(&ui, generation, error.message(), false);
                    break;
                }
                push_status(&ui, generation, "摄像头连接失败，正在重试…", false);
                if sleep_unless_stopped(&stop, Duration::from_millis(1500)) {
                    break;
                }
            }
        }
    }
}

/// 返回 true 表示等待期间收到了停止信号。
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.load(Ordering::SeqCst)
}

fn wait_for_target(
    ui: &slint::Weak<AppWindow>,
    serial: &str,
    stop: &AtomicBool,
    generation: u64,
) -> Result<(Ipv4Addr, String, Option<String>), CameraError> {
    let deadline = Instant::now() + IP_WAIT;
    let mut announced = false;
    loop {
        let (ip, code, version) = lookup(serial);
        if let Some(ip) = ip {
            return match code {
                Some(code) => Ok((ip, code, version)),
                None => Err(CameraError::MissingAccessCode),
            };
        }
        if !announced {
            push_status(ui, generation, "正在等待打印机的局域网地址…", false);
            announced = true;
        }
        if stop.load(Ordering::SeqCst) {
            return Err(CameraError::Cancelled);
        }
        if Instant::now() > deadline {
            return Err(CameraError::MissingIp);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// 单次建流
// ---------------------------------------------------------------------------

fn stream_once(
    ui: &slint::Weak<AppWindow>,
    serial: &str,
    stop: &AtomicBool,
    generation: u64,
) -> Result<(), CameraError> {
    ensure_winsock();
    let bambu = load_bambu()?;
    let (ip, code, dev_ver) = wait_for_target(ui, serial, stop, generation)?;
    if stop.load(Ordering::SeqCst) {
        return Err(CameraError::Cancelled);
    }

    // 访问码只进入这个 CString，用完即随函数栈释放。
    let url = build_stream_url(&ip, &code, serial, dev_ver.as_deref());
    tracing::debug!(
        ip = %ip,
        serial = %serial,
        code_len = code.len(),
        url_len = url.len(),
        "camera target resolved"
    );
    let url = CString::new(url).map_err(|_| CameraError::Create)?;

    let session = open_session(&bambu, &url, ui, stop)?;
    let info = read_stream_info(&bambu, session.tunnel);
    if let Some(info) = &info {
        if info.sub_type != 0 {
            return Err(CameraError::UnsupportedCodec);
        }
    }

    let ffmpeg = load_ffmpeg()?;
    let mut decoder = H264Decoder::new(ffmpeg)?;
    let mut last_push = Instant::now() - FRAME_INTERVAL;
    let mut announced = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut sample = BambuSample::default();
        let rc = unsafe { (bambu.read_sample)(session.tunnel, &mut sample) };
        match rc {
            0 => {
                if sample.size <= 0 || sample.buffer.is_null() {
                    continue;
                }
                let data =
                    unsafe { std::slice::from_raw_parts(sample.buffer, sample.size as usize) };
                let now = Instant::now();
                let display = now.duration_since(last_push) >= FRAME_INTERVAL;
                match decoder.push(data, display) {
                    Ok(Some(buffer)) => {
                        if !announced {
                            announced = true;
                            tracing::info!(
                                serial = %serial,
                                width = buffer.width(),
                                height = buffer.height(),
                                "camera stream is live"
                            );
                        }
                        push_frame(ui, generation, serial, buffer);
                        last_push = now;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(serial = %serial, code = error, "camera decode error");
                        return Err(CameraError::Decode);
                    }
                }
            }
            2 => std::thread::sleep(Duration::from_millis(4)),
            3 => std::thread::sleep(BUFFER_LIMIT_PAUSE),
            1 => return Err(CameraError::StreamEnded),
            other => {
                tracing::warn!(serial = %serial, code = other, "camera read failed");
                return Err(CameraError::StreamEnded);
            }
        }
    }
}

fn ensure_winsock() {
    static WSA: OnceLock<()> = OnceLock::new();
    WSA.get_or_init(|| unsafe {
        let mut data = WSADATA::default();
        let rc = WSAStartup(0x0202, &mut data);
        if rc != 0 {
            tracing::warn!(code = rc, "WSAStartup failed");
        }
    });
}

struct StreamSession {
    tunnel: *mut c_void,
    api: Arc<BambuApi>,
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        // Close/Destroy 可能耗时数秒，必须在后台线程执行；到这里本来就在 worker 里。
        unsafe {
            (self.api.close)(self.tunnel);
            (self.api.destroy)(self.tunnel);
        }
    }
}

fn client_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string()).as_str()
}

fn client_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let raw = env!("CARGO_PKG_VERSION");
            if raw.split('.').count() == 3 {
                format!("{raw}.0")
            } else {
                raw.to_string()
            }
        })
        .as_str()
}

/// Bambu Studio 会在本地 RTSPS URL 后附加客户端/设备版本信息；BambuSource
/// 会解析这些参数。此前 Luma 只带 `device=`，在部分固件上会卡在
/// `Bambu_StartStream` 的 `would_block` 循环里。
fn build_stream_url(ip: &Ipv4Addr, code: &str, serial: &str, dev_ver: Option<&str>) -> String {
    let mut url = format!(
        "bambu:///rtsps___bblp:{code}@{ip}/streaming/live/1?proto=rtsps&device={serial}\
         &net_ver={}&cli_id={}&cli_ver={}",
        client_version(),
        client_id(),
        client_version(),
    );
    if let Some(dev_ver) = dev_ver.filter(|version| !version.is_empty()) {
        url.push_str("&dev_ver=");
        url.push_str(dev_ver);
    }
    url
}

fn open_session(
    api: &Arc<BambuApi>,
    url: &CString,
    _ui: &slint::Weak<AppWindow>,
    stop: &AtomicBool,
) -> Result<StreamSession, CameraError> {
    // BambuSource 会在运行时按搜索路径动态加载 live555/libaosl，
    // 所以每次建流前都要把 DLL 目录切回插件目录。
    set_dll_directory(&api.plugin_dir);

    let mut tunnel: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { (api.create)(&mut tunnel, url.as_ptr()) };
    tracing::debug!(code = rc, "Bambu_Create returned");
    if rc != 0 || tunnel.is_null() {
        return Err(CameraError::Create);
    }

    if std::env::var_os("LUMA_CAMERA_DEBUG").is_some() {
        unsafe {
            (api.set_logger)(tunnel, bambu_log, std::ptr::null_mut());
        }
    }

    let rc = unsafe { (api.open)(tunnel) };
    tracing::debug!(code = rc, "Bambu_Open returned");
    if rc != 0 {
        unsafe {
            (api.close)(tunnel);
            (api.destroy)(tunnel);
        }
        return Err(CameraError::Open);
    }

    let session = StreamSession {
        tunnel,
        api: api.clone(),
    };

    let deadline = Instant::now() + START_WAIT;
    let mut announced = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(CameraError::Cancelled);
        }
        let rc = unsafe { (api.start_stream)(session.tunnel, 1) };
        if rc == 0 {
            tracing::debug!("Bambu_StartStream succeeded");
            return Ok(session);
        }
        // 2 = would_block：握手进行中。
        if rc != 2 || Instant::now() > deadline {
            tracing::debug!(
                code = rc,
                detail = %last_error_message(api),
                "Bambu_StartStream failed"
            );
            return Err(CameraError::StartTimeout);
        }
        if !announced {
            tracing::debug!("Bambu_StartStream is waiting for the printer");
            announced = true;
        }
        std::thread::sleep(START_POLL_INTERVAL);
    }
}

fn read_stream_info(api: &BambuApi, tunnel: *mut c_void) -> Option<BambuStreamInfo> {
    for _ in 0..30 {
        let mut info = BambuStreamInfo::default();
        let rc = unsafe { (api.get_stream_info)(tunnel, 0, &mut info) };
        if rc == 0 && info.width > 0 && info.height > 0 {
            return Some(info);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

// ---------------------------------------------------------------------------
// BambuSource.dll
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BambuSample {
    itrack: c_int,
    size: c_int,
    flags: c_int,
    buffer: *const u8,
    decode_time: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BambuStreamInfo {
    typ: c_int,
    sub_type: c_int,
    width: c_int,
    height: c_int,
    frame_rate: c_int,
    format_type: c_int,
    format_size: c_int,
    max_frame_size: c_int,
    format_buffer: *const u8,
}

struct BambuApi {
    _bambu: Library,
    _live555: Library,
    _aosl: Library,
    plugin_dir: PathBuf,
    create: unsafe extern "C" fn(*mut *mut c_void, *const c_char) -> c_int,
    set_logger: unsafe extern "C" fn(
        *mut c_void,
        extern "C" fn(*mut c_void, c_int, *const u16),
        *mut c_void,
    ),
    open: unsafe extern "C" fn(*mut c_void) -> c_int,
    start_stream: unsafe extern "C" fn(*mut c_void, u8) -> c_int,
    get_stream_info: unsafe extern "C" fn(*mut c_void, c_int, *mut BambuStreamInfo) -> c_int,
    read_sample: unsafe extern "C" fn(*mut c_void, *mut BambuSample) -> c_int,
    last_error: unsafe extern "C" fn() -> *const c_char,
    close: unsafe extern "C" fn(*mut c_void),
    destroy: unsafe extern "C" fn(*mut c_void),
}

// 句柄只在 worker 线程里使用；函数指针本身是线程安全的。
unsafe impl Send for BambuApi {}
unsafe impl Sync for BambuApi {}

fn load_bambu() -> Result<Arc<BambuApi>, CameraError> {
    static API: OnceLock<Arc<BambuApi>> = OnceLock::new();
    if let Some(api) = API.get() {
        return Ok(api.clone());
    }
    match load_bambu_inner() {
        Ok(api) => {
            let api = Arc::new(api);
            let _ = API.set(api.clone());
            Ok(api)
        }
        Err(error) => {
            tracing::warn!(%error, "BambuSource unavailable");
            Err(CameraError::PluginMissing)
        }
    }
}

fn load_bambu_inner() -> anyhow::Result<BambuApi> {
    let plugin_dir = bambu_plugin_dir().ok_or_else(|| anyhow::anyhow!("plugin dir not found"))?;
    set_dll_directory(&plugin_dir);
    let live555 = unsafe { Library::new(plugin_dir.join("live555.dll")) }?;
    let aosl = unsafe { Library::new(plugin_dir.join("libaosl.dll")) }?;
    let bambu = unsafe { Library::new(plugin_dir.join("BambuSource.dll")) }?;

    unsafe {
        let init: unsafe extern "C" fn() -> c_int = *bambu.get(b"Bambu_Init")?;
        let create: unsafe extern "C" fn(*mut *mut c_void, *const c_char) -> c_int =
            *bambu.get(b"Bambu_Create")?;
        let set_logger: unsafe extern "C" fn(
            *mut c_void,
            extern "C" fn(*mut c_void, c_int, *const u16),
            *mut c_void,
        ) = *bambu.get(b"Bambu_SetLogger")?;
        let open: unsafe extern "C" fn(*mut c_void) -> c_int = *bambu.get(b"Bambu_Open")?;
        let start_stream: unsafe extern "C" fn(*mut c_void, u8) -> c_int =
            *bambu.get(b"Bambu_StartStream")?;
        let get_stream_info: unsafe extern "C" fn(
            *mut c_void,
            c_int,
            *mut BambuStreamInfo,
        ) -> c_int = *bambu.get(b"Bambu_GetStreamInfo")?;
        let read_sample: unsafe extern "C" fn(*mut c_void, *mut BambuSample) -> c_int =
            *bambu.get(b"Bambu_ReadSample")?;
        let last_error: unsafe extern "C" fn() -> *const c_char =
            *bambu.get(b"Bambu_GetLastErrorMsg")?;
        let close: unsafe extern "C" fn(*mut c_void) = *bambu.get(b"Bambu_Close")?;
        let destroy: unsafe extern "C" fn(*mut c_void) = *bambu.get(b"Bambu_Destroy")?;

        let rc = init();
        if rc != 0 {
            tracing::warn!(code = rc, "Bambu_Init returned non-zero");
        }

        Ok(BambuApi {
            _bambu: bambu,
            _live555: live555,
            _aosl: aosl,
            plugin_dir,
            create,
            set_logger,
            open,
            start_stream,
            get_stream_info,
            read_sample,
            last_error,
            close,
            destroy,
        })
    }
}

/// 取 BambuSource 的最近一次错误文本。只用于调试日志，并做访问码脱敏。
fn last_error_message(api: &BambuApi) -> String {
    let raw = unsafe { (api.last_error)() };
    if raw.is_null() {
        return "unknown".to_string();
    }
    let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
    redact_bambu_log(text.trim())
}

fn bambu_plugin_dir() -> Option<PathBuf> {
    // 全量版把 BambuSource 等 DLL 放在 exe 同级的 camera/ 目录。
    if let Some(dir) = bundled_camera_dir() {
        if dir.join("BambuSource.dll").is_file() {
            return Some(dir);
        }
    }
    // 标准版安装时/手动导入的依赖放在用户数据目录。
    if let Some(dir) = imported_camera_dir() {
        if dir.join("BambuSource.dll").is_file() {
            return Some(dir);
        }
    }
    local_bambu_plugin_dir()
}

fn local_bambu_plugin_dir() -> Option<PathBuf> {
    let roaming = std::env::var_os("APPDATA")?;
    let dir = PathBuf::from(roaming).join("BambuStudio").join("plugins");
    dir.join("BambuSource.dll").is_file().then_some(dir)
}

fn bundled_camera_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("camera");
    dir.is_dir().then_some(dir)
}

fn imported_camera_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Luma")
        .join("camera");
    dir.is_dir().then_some(dir)
}

fn imported_camera_target() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "无法定位 LOCALAPPDATA".to_string())?;
    Ok(base.join("Luma").join("camera"))
}

fn set_dll_directory(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let _ = SetDllDirectoryW(PCWSTR(wide.as_ptr()));
    }
}

/// BambuSource 的日志回调（`Bambu_SetLogger`）。只在 `LUMA_CAMERA_DEBUG` 下注册。
extern "C" fn bambu_log(_ctx: *mut c_void, level: c_int, msg: *const u16) {
    if msg.is_null() {
        return;
    }
    let mut len = 0usize;
    while len < 2048 && unsafe { *msg.add(len) } != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(msg, len) });
    let lower = text.to_ascii_lowercase();
    let interesting = level <= 1
        || lower.contains("error")
        || lower.contains("fail")
        || lower.contains("stream")
        || lower.contains("open")
        || lower.contains("read");
    if interesting {
        tracing::debug!(level, "BambuSource: {}", redact_bambu_log(&text));
    }
}

/// 回调消息里可能带直播 URL，把访问码遮掉再写日志。
fn redact_bambu_log(text: &str) -> String {
    let mut out = text.to_string();
    if let Some(start) = out.find("bblp:") {
        let from = start + "bblp:".len();
        if let Some(offset) = out[from..].find('@') {
            out.replace_range(from..from + offset, "[redacted]");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// FFmpeg 解码（Bambu Studio 自带 libav*）
// ---------------------------------------------------------------------------

struct FfmpegApi {
    _avutil: Library,
    _swresample: Library,
    _avcodec: Library,
    _swscale: Library,
    av_frame_alloc: unsafe extern "C" fn() -> *mut c_void,
    av_frame_free: unsafe extern "C" fn(*mut *mut c_void),
    avcodec_find_decoder: unsafe extern "C" fn(c_int) -> *const c_void,
    avcodec_alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    avcodec_open2: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int,
    avcodec_send_packet: unsafe extern "C" fn(*mut c_void, *const AVPacket) -> c_int,
    avcodec_receive_frame: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    avcodec_free_context: unsafe extern "C" fn(*mut *mut c_void),
    sws_get_cached_context: unsafe extern "C" fn(
        *mut c_void,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        *mut c_void,
        *mut c_void,
        *const f64,
    ) -> *mut c_void,
    sws_scale: unsafe extern "C" fn(
        *mut c_void,
        *const *const u8,
        *const c_int,
        c_int,
        c_int,
        *const *mut u8,
        *const c_int,
    ) -> c_int,
    sws_free_context: unsafe extern "C" fn(*mut *mut c_void),
}

unsafe impl Send for FfmpegApi {}
unsafe impl Sync for FfmpegApi {}

fn load_ffmpeg() -> Result<Arc<FfmpegApi>, CameraError> {
    static API: OnceLock<Arc<FfmpegApi>> = OnceLock::new();
    if let Some(api) = API.get() {
        return Ok(api.clone());
    }
    match load_ffmpeg_inner() {
        Ok(api) => {
            let api = Arc::new(api);
            let _ = API.set(api.clone());
            Ok(api)
        }
        Err(error) => {
            tracing::warn!(%error, "FFmpeg unavailable");
            Err(CameraError::DecoderMissing)
        }
    }
}

fn load_ffmpeg_inner() -> anyhow::Result<FfmpegApi> {
    let dir = bambu_studio_dir().ok_or_else(|| anyhow::anyhow!("Bambu Studio dir not found"))?;
    set_dll_directory(&dir);
    let avutil = unsafe {
        Library::new(find_dll(&dir, "avutil-").ok_or_else(|| anyhow::anyhow!("avutil missing"))?)
    }?;
    let swresample = unsafe {
        Library::new(
            find_dll(&dir, "swresample-").ok_or_else(|| anyhow::anyhow!("swresample missing"))?,
        )
    }?;
    let avcodec = unsafe {
        Library::new(find_dll(&dir, "avcodec-").ok_or_else(|| anyhow::anyhow!("avcodec missing"))?)
    }?;
    let swscale = unsafe {
        Library::new(find_dll(&dir, "swscale-").ok_or_else(|| anyhow::anyhow!("swscale missing"))?)
    }?;

    unsafe {
        let api = FfmpegApi {
            av_frame_alloc: *avutil.get(b"av_frame_alloc")?,
            av_frame_free: *avutil.get(b"av_frame_free")?,
            avcodec_find_decoder: *avcodec.get(b"avcodec_find_decoder")?,
            avcodec_alloc_context3: *avcodec.get(b"avcodec_alloc_context3")?,
            avcodec_open2: *avcodec.get(b"avcodec_open2")?,
            avcodec_send_packet: *avcodec.get(b"avcodec_send_packet")?,
            avcodec_receive_frame: *avcodec.get(b"avcodec_receive_frame")?,
            avcodec_free_context: *avcodec.get(b"avcodec_free_context")?,
            sws_get_cached_context: *swscale.get(b"sws_getCachedContext")?,
            sws_scale: *swscale.get(b"sws_scale")?,
            sws_free_context: *swscale.get(b"sws_freeContext")?,
            _avutil: avutil,
            _swresample: swresample,
            _avcodec: avcodec,
            _swscale: swscale,
        };
        Ok(api)
    }
}

fn bambu_studio_dir() -> Option<PathBuf> {
    if let Some(dir) = bundled_camera_dir() {
        if find_dll(&dir, "avcodec-").is_some() {
            return Some(dir);
        }
    }
    if let Some(dir) = imported_camera_dir() {
        if find_dll(&dir, "avcodec-").is_some() {
            return Some(dir);
        }
    }
    local_bambu_studio_dir()
}

fn local_bambu_studio_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LUMA_BAMBU_STUDIO_DIR") {
        let dir = PathBuf::from(dir);
        if find_dll(&dir, "avcodec-").is_some() {
            return Some(dir);
        }
    }
    for root in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = std::env::var_os(root) else {
            continue;
        };
        let dir = PathBuf::from(base).join("Bambu Studio");
        if find_dll(&dir, "avcodec-").is_some() {
            return Some(dir);
        }
    }
    None
}

fn find_dll(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.starts_with(prefix) || !name.ends_with(".dll") {
            continue;
        }
        let version: u32 = name[prefix.len()..name.len() - 4].parse().unwrap_or(0);
        if best.as_ref().map(|(v, _)| version > *v).unwrap_or(true) {
            best = Some((version, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// `AVPacket` 的前缀布局（FFmpeg 7 x64）。只用于把外部样本喂给解码器，
/// 结构体本身留在栈上，解码器会自行复制引用。
#[repr(C)]
struct AVPacket {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
    stream_index: c_int,
    flags: c_int,
    side_data: *mut c_void,
    side_data_elems: c_int,
    duration: i64,
    pos: i64,
    opaque: *mut c_void,
    opaque_ref: *mut c_void,
    time_base: AVRational,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AVRational {
    num: c_int,
    den: c_int,
}

/// `AVFrame` 的前缀布局（FFmpeg 7 x64）。只读，内存由解码器分配。
#[repr(C)]
struct AVFrame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

struct H264Decoder {
    api: Arc<FfmpegApi>,
    ctx: *mut c_void,
    frame: *mut c_void,
    sws: *mut c_void,
}

impl H264Decoder {
    fn new(api: Arc<FfmpegApi>) -> Result<Self, CameraError> {
        unsafe {
            let codec = (api.avcodec_find_decoder)(AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err(CameraError::DecoderMissing);
            }
            let ctx = (api.avcodec_alloc_context3)(codec);
            if ctx.is_null() {
                return Err(CameraError::DecoderMissing);
            }
            let rc = (api.avcodec_open2)(ctx, codec, std::ptr::null_mut());
            if rc < 0 {
                let mut ctx = ctx;
                (api.avcodec_free_context)(&mut ctx);
                return Err(CameraError::DecoderMissing);
            }
            let frame = (api.av_frame_alloc)();
            if frame.is_null() {
                let mut ctx = ctx;
                (api.avcodec_free_context)(&mut ctx);
                return Err(CameraError::DecoderMissing);
            }
            Ok(Self {
                api,
                ctx,
                frame,
                sws: std::ptr::null_mut(),
            })
        }
    }

    /// 送入一个 Annex B 样本；`convert` 为真时返回最新一帧 RGBA。
    fn push(
        &mut self,
        data: &[u8],
        convert: bool,
    ) -> Result<Option<SharedPixelBuffer<Rgba8Pixel>>, c_int> {
        let packet = AVPacket {
            buf: std::ptr::null_mut(),
            pts: i64::MIN,
            dts: i64::MIN,
            data: data.as_ptr() as *mut u8,
            size: data.len() as c_int,
            stream_index: 0,
            flags: 0,
            side_data: std::ptr::null_mut(),
            side_data_elems: 0,
            duration: 0,
            pos: -1,
            opaque: std::ptr::null_mut(),
            opaque_ref: std::ptr::null_mut(),
            time_base: AVRational::default(),
        };

        let mut attempts = 0;
        let mut rc;
        loop {
            rc = unsafe { (self.api.avcodec_send_packet)(self.ctx, &packet) };
            if rc != AVERROR_EAGAIN || attempts >= 2 {
                break;
            }
            // 解码器输入还没消化完：先收一波帧再重送。
            let _ = self.receive(false)?;
            attempts += 1;
        }
        if rc < 0 && rc != AVERROR_EAGAIN {
            return Err(rc);
        }
        self.receive(convert)
    }

    fn receive(&mut self, convert: bool) -> Result<Option<SharedPixelBuffer<Rgba8Pixel>>, c_int> {
        let mut latest = None;
        loop {
            let rc = unsafe { (self.api.avcodec_receive_frame)(self.ctx, self.frame) };
            if rc == 0 {
                if convert {
                    latest = Some(self.convert_frame()?);
                }
            } else if rc == AVERROR_EAGAIN || rc == AVERROR_EOF {
                break;
            } else {
                return Err(rc);
            }
        }
        Ok(latest)
    }

    fn convert_frame(&mut self) -> Result<SharedPixelBuffer<Rgba8Pixel>, c_int> {
        let frame = self.frame as *mut AVFrame;
        let (width, height, format) = unsafe { ((*frame).width, (*frame).height, (*frame).format) };
        if width <= 0 || height <= 0 {
            return Err(-1);
        }

        let scaled_width = width.min(TARGET_WIDTH);
        let out_width = scaled_width.max(2) & !1;
        let out_height =
            ((scaled_width as i64 * height as i64 + width as i64 / 2) / width as i64) as i32;
        let out_height = out_height.max(2) & !1;

        let sws = unsafe {
            (self.api.sws_get_cached_context)(
                self.sws,
                width,
                height,
                format,
                out_width,
                out_height,
                AV_PIX_FMT_RGBA,
                SWS_BILINEAR,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if sws.is_null() {
            return Err(-1);
        }
        self.sws = sws;

        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(out_width as u32, out_height as u32);
        let stride = out_width * 4;
        let mut dst_data = [buffer.make_mut_bytes().as_mut_ptr()];
        let dst_stride = [stride];
        let src_data = unsafe { (*frame).data.as_ptr() as *const *const u8 };
        let src_stride = unsafe { (*frame).linesize.as_ptr() };
        let rows = unsafe {
            (self.api.sws_scale)(
                sws,
                src_data,
                src_stride,
                0,
                height,
                dst_data.as_mut_ptr(),
                dst_stride.as_ptr(),
            )
        };
        if rows <= 0 {
            return Err(-1);
        }
        Ok(buffer)
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.frame.is_null() {
                (self.api.av_frame_free)(&mut self.frame);
            }
            if !self.ctx.is_null() {
                (self.api.avcodec_free_context)(&mut self.ctx);
            }
            if !self.sws.is_null() {
                (self.api.sws_free_context)(&mut self.sws);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unspecified_addresses() {
        set_ip("SERIAL", Ipv4Addr::UNSPECIFIED);
        assert!(lookup("SERIAL").0.is_none());
    }

    #[test]
    fn registry_roundtrip() {
        set_ip("A1", Ipv4Addr::new(192, 168, 1, 9));
        register_access_code("A1", "12345678");
        let (ip, code, _) = lookup("A1");
        assert_eq!(ip, Some(Ipv4Addr::new(192, 168, 1, 9)));
        assert_eq!(code.as_deref(), Some("12345678"));
    }

    #[test]
    fn device_version_is_cached_from_pushall() {
        let report = serde_json::json!({
            "get_version": {
                "info": {
                    "module": [
                        { "name": "ota", "sw_ver": "01.01.00.00" },
                        { "name": "n3f/0", "sw_ver": "04.00.21.90" }
                    ]
                }
            }
        });
        update_from_report("VERSION-TEST", &report);
        let (_, _, version) = lookup("VERSION-TEST");
        assert_eq!(version.as_deref(), Some("01.01.00.00"));
    }

    #[test]
    fn stream_url_matches_studio_identity_fields() {
        let url = build_stream_url(
            &Ipv4Addr::new(192, 168, 1, 9),
            "12345678",
            "20P5BJ673000062",
            Some("01.01.00.00"),
        );
        assert!(url.starts_with("bambu:///rtsps___bblp:12345678@192.168.1.9/"));
        assert!(url.contains("device=20P5BJ673000062"));
        assert!(url.contains("dev_ver=01.01.00.00"));
        assert!(url.contains("cli_id="));
        assert!(url.contains("cli_ver="));
        assert!(url.contains("net_ver="));
        assert!(!url.contains(' '));
        assert_eq!(client_version().split('.').count(), 4);
    }

    #[test]
    fn new_session_invalidates_old_callbacks() {
        let old = next_session();
        assert!(is_current_session(old));
        let new = next_session();
        assert!(!is_current_session(old));
        assert!(is_current_session(new));
        invalidate_session(new);
        assert!(!is_current_session(new));
    }
}
