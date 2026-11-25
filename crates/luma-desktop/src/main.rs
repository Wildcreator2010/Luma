#![cfg_attr(windows, windows_subsystem = "windows")]

//! Luma Desktop 入口。
//!
//! 有已保存的云端 Token 时进入真实模式；否则进入演示模式。
//! `--demo` / `--complete` 强制演示模式，用于截图与 UI 回归。

slint::include_modules!();

mod browser_login;
mod camera;
mod instance;
mod logging;
mod runtime;
mod status_bar;
mod window_icon;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slint::{Model, VecModel};
fn mock_printers() -> Vec<PrinterItem> {
    vec![
        PrinterItem {
            name: "车间一号".into(),
            model: "Bambu Lab X2D".into(),
            serial: "01S00C2D000001".into(),
            status: "打印中".into(),
            status_kind: 1,
            progress: 64.0,
            progress_text: "64%".into(),
            progress_known: true,
            task: "支架_v3.3mf".into(),
            remaining: "预计剩余 2 小时 16 分".into(),
            nozzle: "214 °C".into(),
            nozzle_target: "215 °C".into(),
            nozzle_value: 214.0,
            bed: "60 °C".into(),
            bed_target: "60 °C".into(),
            bed_value: 60.0,
            layers: "128 / 240".into(),
            layer_card: "128 / 240".into(),
            layer_current: 128,
            layer_total: 240,
            layer_progress: 53.33,
        },
        PrinterItem {
            name: "原型机".into(),
            model: "Bambu Lab P1S".into(),
            serial: "01P00A4B000002".into(),
            status: "打印中".into(),
            status_kind: 1,
            progress: 23.0,
            progress_text: "23%".into(),
            progress_known: true,
            task: "齿轮箱外壳.3mf".into(),
            remaining: "预计剩余 4 小时 02 分".into(),
            nozzle: "221 °C".into(),
            nozzle_target: "215 °C".into(),
            nozzle_value: 221.0,
            bed: "55 °C".into(),
            bed_target: "60 °C".into(),
            bed_value: 55.0,
            layers: "62 / 310".into(),
            layer_card: "62 / 310".into(),
            layer_current: 62,
            layer_total: 310,
            layer_progress: 20.0,
        },
        PrinterItem {
            name: "书房".into(),
            model: "Bambu Lab A1 mini".into(),
            serial: "03000A1M000003".into(),
            status: "空闲".into(),
            status_kind: 0,
            progress: 0.0,
            progress_text: "0%".into(),
            progress_known: true,
            task: "无任务".into(),
            remaining: "空闲 · 喷嘴 28 °C".into(),
            nozzle: "28 °C".into(),
            nozzle_target: "0 °C".into(),
            nozzle_value: 28.0,
            bed: "26 °C".into(),
            bed_target: "0 °C".into(),
            bed_value: 26.0,
            layers: "0 / 0".into(),
            layer_card: "0 / 0".into(),
            layer_current: 0,
            layer_total: 0,
            layer_progress: 0.0,
        },
    ]
}

/// 进度环的静态采样点。480 个点、每 0.75°，与轨道同宽的圆点重叠成连续弧。
/// 使用点阵而不是 Path ArcTo，规避 Slint/lyon 在小角度下弧线几何跑偏的问题。
fn arc_dots() -> Vec<ArcDot> {
    (0..480)
        .map(|i| ArcDot {
            angle: i as f32 * 0.75,
        })
        .collect()
}

fn connecting_printer() -> Vec<PrinterItem> {
    vec![PrinterItem {
        name: "Luma".into(),
        model: "".into(),
        serial: "".into(),
        status: "正在连接".into(),
        status_kind: 3,
        progress: 0.0,
        progress_text: "—".into(),
        progress_known: false,
        task: "正在获取设备…".into(),
        remaining: "".into(),
        nozzle: "—".into(),
        nozzle_target: "—".into(),
        nozzle_value: 0.0,
        bed: "—".into(),
        bed_target: "—".into(),
        bed_value: 0.0,
        layers: "—".into(),
        layer_card: "—".into(),
        layer_current: 0,
        layer_total: 0,
        layer_progress: 0.0,
    }]
}

/// 模拟实时进度，仅演示模式使用。
fn start_mock_ticker(
    printers: Rc<VecModel<PrinterItem>>,
    ticker: Rc<slint::Timer>,
    stop: Arc<AtomicBool>,
    bar: Option<slint::Weak<StatusBarWindow>>,
) {
    ticker.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1400),
        move || {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            for index in 0..printers.row_count() {
                if let Some(mut item) = printers.row_data(index) {
                    if item.status_kind == 1 {
                        let next = item.progress + 0.8;
                        if next > 99.0 {
                            item.progress = 12.0;
                            item.layer_current = 0;
                        } else {
                            item.progress = next;
                        }
                        item.progress_text = format!("{}%", item.progress.round() as i32).into();
                        if item.layer_total > 0 {
                            if item.layer_current < item.layer_total {
                                item.layer_current += 1;
                            }
                            item.layer_progress =
                                item.layer_current as f32 / item.layer_total as f32 * 100.0;
                            item.layers =
                                format!("{} / {}", item.layer_current, item.layer_total).into();
                            item.layer_card = item.layers.clone();
                        }
                        printers.set_row_data(index, item);
                    }
                }
            }

            // 演示模式也驱动顶部状态条，否则登录前只能看到“未连接”。
            if let Some(bar) = bar.as_ref().and_then(|bar| bar.upgrade()) {
                let items: Vec<BarPrinter> = (0..printers.row_count())
                    .filter_map(|index| printers.row_data(index))
                    .take(3)
                    .map(|item| BarPrinter {
                        name: item.name.clone(),
                        detail: if item.status_kind == 1 {
                            format!("{} · {}", item.progress_text, item.status).into()
                        } else if !item.status.is_empty() {
                            item.status.clone()
                        } else if !item.progress_text.is_empty() {
                            item.progress_text.clone()
                        } else {
                            "—".into()
                        },
                        status: item.status.clone(),
                        progress_text: item.progress_text.clone(),
                        remaining: if item.status_kind == 1 {
                            compact_remaining(&item.remaining).into()
                        } else {
                            "".into()
                        },
                        progress: item.progress,
                        progress_known: item.progress_known,
                        tone: match item.status_kind {
                            1 | 4 => 1,
                            2 => 2,
                            _ => 0,
                        },
                    })
                    .collect();
                status_bar::set_printers(&bar, items);
            }
        },
    );
}

fn compact_remaining(text: &str) -> String {
    let text = text.strip_prefix("预计剩余 ").unwrap_or(text);
    if text.is_empty() || text == "剩余时间未知" {
        return String::new();
    }
    text.replace(" 小时 ", "h")
        .replace(" 分", "m")
        .replace(" 分钟", "m")
        .trim()
        .to_string()
}

fn run_browser_login() -> Result<(usize, String), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("启动登录运行时失败：{error}"))?;

    runtime.block_on(async {
        let mut session = browser_login::BrowserLogin::launch()
            .map_err(|error| format!("无法打开浏览器登录：{error:#}"))?;
        let success = session
            .wait_for_token(Duration::from_secs(600))
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok((success.devices, success.uid))
    })
}

/// 登录成功后的统一切换：停演示、显示主窗口、启动云端运行时。
fn enter_cloud_mode(
    ui: &AppWindow,
    login: &LoginWindow,
    devices: usize,
    uid: &str,
    stop_demo: &Arc<AtomicBool>,
    bar: Option<&slint::Weak<StatusBarWindow>>,
) {
    stop_demo.store(true, Ordering::SeqCst);
    login.set_message(format!("登录成功，发现 {devices} 台设备").into());
    login.set_message_error(false);
    let _ = login.hide();

    ui.set_printers(Rc::new(VecModel::from(connecting_printer())).into());
    ui.set_selected(0);
    ui.set_demo_mode(false);
    ui.set_logged_in(true);
    ui.set_account_summary("正在同步账号…".into());
    ui.set_account_url("https://bambulab.cn/zh-cn/my".into());
    ui.set_cloud_state(1);
    ui.set_cloud_status("正在连接…".into());
    let _ = ui.show();
    let _ = runtime::start(ui.as_weak(), bar.cloned());
    tracing::info!(devices, uid, "GUI login succeeded");
}

/// GUI 子系统程序从终端启动时不会自动接上父控制台；
/// `--browser-login` 需要输出诊断信息，这里显式附加。
#[cfg(windows)]
fn attach_parent_console() {
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // ATTACH_PARENT_PROCESS
    unsafe {
        AttachConsole(u32::MAX);
    }
}

/// 状态条配置的唯一入口：窗口、设置页、托盘三处保持同步。
fn refresh_bar(
    config: &Rc<std::cell::RefCell<status_bar::BarConfig>>,
    bar: Option<&StatusBarWindow>,
    ui: Option<&AppWindow>,
    tray: Option<&LumaTray>,
) {
    let snapshot = config.borrow().clone();
    runtime::set_bar_smart_sort(snapshot.smart_sort);
    if let Some(bar) = bar {
        status_bar::apply(bar, &snapshot);
    }
    if let Some(ui) = ui {
        ui.set_bar_visible(snapshot.visible);
    }
    if let Some(tray) = tray {
        tray.set_bar_visible(snapshot.visible);
    }
    status_bar::save_config(&snapshot);
}

fn show_main_page(ui: &AppWindow, page: i32) {
    ui.set_page(page);
    let _ = ui.show();
    status_bar::activate(&ui.window());
}

fn open_external_url(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

const BAR_POLL_MS: u64 = 33;
const BAR_FADE_STEP_MS: u64 = 16;
const BAR_FADE_MS: f32 = 120.0;

struct BarFade {
    base: f32,
    alpha: f32,
    target: f32,
    hidden: bool,
    fading: bool,
}

const LIVE_HISTORY_LIMIT: usize = 90;

#[derive(Default)]
struct LiveHistory {
    serial: String,
    nozzle: Vec<f32>,
    bed: Vec<f32>,
    progress: Vec<f32>,
}

fn record_live_history(ui: &AppWindow, state: &std::cell::RefCell<LiveHistory>) {
    let selected = ui.get_selected().max(0) as usize;
    let printers = ui.get_printers();
    let Some(item) = printers.row_data(selected) else {
        return;
    };
    let mut history = state.borrow_mut();
    if history.serial.as_str() != item.serial.as_str() {
        history.serial = item.serial.to_string();
        history.nozzle.clear();
        history.bed.clear();
        history.progress.clear();
    }
    if let Some(value) = parse_celsius(&item.nozzle) {
        push_sample(&mut history.nozzle, value);
    }
    if let Some(value) = parse_celsius(&item.bed) {
        push_sample(&mut history.bed, value);
    }
    if item.progress_known {
        push_sample(&mut history.progress, item.progress);
    }
    ui.set_nozzle_history(Rc::new(VecModel::from(history.nozzle.clone())).into());
    ui.set_bed_history(Rc::new(VecModel::from(history.bed.clone())).into());
    ui.set_progress_history(Rc::new(VecModel::from(history.progress.clone())).into());
}

fn push_sample(values: &mut Vec<f32>, value: f32) {
    values.push(value);
    if values.len() > LIVE_HISTORY_LIMIT {
        values.remove(0);
    }
}

fn parse_celsius(text: &str) -> Option<f32> {
    text.split_whitespace().next()?.parse().ok()
}

/// 状态条始终鼠标穿透，因此收不到鼠标事件。悬停检测靠定时轮询光标位置；
/// 淡入淡出只改分层窗口 alpha，幅度完成后用 SW_HIDE / SW_SHOWNOACTIVATE。
fn bar_fade_tick(
    bar: slint::Weak<StatusBarWindow>,
    state: Rc<std::cell::RefCell<BarFade>>,
) -> bool {
    let Some(bar) = bar.upgrade() else {
        state.borrow_mut().fading = false;
        return true;
    };
    let mut fade = state.borrow_mut();
    let step = (fade.base * (BAR_FADE_STEP_MS as f32 / BAR_FADE_MS)).max(0.02);
    if (fade.alpha - fade.target).abs() <= step {
        fade.alpha = fade.target;
        status_bar::set_alpha(&bar, fade.alpha);
        if fade.target <= 0.001 && !fade.hidden {
            status_bar::set_native_visible(&bar, false);
            fade.hidden = true;
        }
        fade.fading = false;
        true
    } else {
        fade.alpha += if fade.alpha < fade.target {
            step
        } else {
            -step
        };
        status_bar::set_alpha(&bar, fade.alpha);
        false
    }
}

fn schedule_bar_fade(bar: slint::Weak<StatusBarWindow>, state: Rc<std::cell::RefCell<BarFade>>) {
    slint::Timer::single_shot(Duration::from_millis(BAR_FADE_STEP_MS), move || {
        if bar_fade_tick(bar.clone(), state.clone()) {
            return;
        }
        schedule_bar_fade(bar, state);
    });
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    let wants_console = args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--browser-login" | "--camera-check" | "--import-camera-runtime"
        )
    });
    #[cfg(windows)]
    if wants_console {
        attach_parent_console();
    }

    if args.iter().any(|arg| arg == "--version") {
        print_cli(&format!("Luma Desktop {}\n", env!("CARGO_PKG_VERSION")));
        return;
    }
    if args.iter().any(|arg| arg == "--help") {
        print_help();
        return;
    }
    if args.iter().any(|arg| arg == "--quit") {
        instance::request_quit();
        return;
    }
    if args.iter().any(|arg| arg == "--camera-check") {
        match camera::self_check() {
            Ok(detail) => print_cli(&format!("摄像头依赖检查通过\n{detail}\n")),
            Err(error) => {
                print_cli(&format!("摄像头依赖检查失败：{error}\n"));
                std::process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|arg| arg == "--import-camera-runtime") {
        match camera::import_runtime() {
            Ok(detail) => print_cli(&format!("{detail}\n")),
            Err(error) => {
                print_cli(&format!("导入摄像头运行库失败：{error}\n"));
                std::process::exit(1);
            }
        }
        return;
    }

    logging::install_panic_hook();
    logging::init();

    if let Err(error) = run() {
        let message = format!(
            "Luma 启动失败：{error}\n\n日志目录：{}",
            logging::log_dir().display()
        );
        logging::append_panic_note(&message);
        tracing::error!(%error, "Luma Desktop failed to start");
        #[cfg(windows)]
        fatal_message(&message);
        #[cfg(not(windows))]
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn print_help() {
    print_cli(&format!(
        "Luma Desktop {version}\n\
         \n\
         用法：Luma.exe [选项]\n\
         \n\
           --login          强制显示登录窗口\n\
           --demo           演示模式\n\
           --complete       演示模式 100% 完成态\n\
           --light          浅色主题\n\
           --bar            启动时显示状态条\n\
           --no-bar         启动时隐藏状态条\n\
           --taskbar        以任务栏模式启动状态条\n\
           --settings       打开设置页\n\
           --monitor        打开实时监控页\n\
           --browser-login  仅执行浏览器登录并导入 Token\n\
           --camera-check   检查摄像头运行库（不连接打印机）\n\
           --import-camera-runtime  从本机 Bambu Studio 导入摄像头依赖\n\
           --quit           请求已运行的 Luma 退出\n\
           --version        显示版本\n\
           --help           显示本帮助",
        version = env!("CARGO_PKG_VERSION")
    ));
}

/// 先把结果写到调用方给的标准输出；没有继承控制台时再附加父控制台重试。
fn print_cli(text: &str) {
    use std::io::Write;

    let mut stdout = std::io::stdout();
    if stdout
        .write_all(text.as_bytes())
        .and_then(|_| stdout.flush())
        .is_ok()
    {
        return;
    }

    #[cfg(windows)]
    {
        attach_parent_console();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }
}

/// GUI 子系统没有控制台；致命错误用系统对话框兜底，不能静默退出。
#[cfg(windows)]
fn fatal_message(text: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("Luma"),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::w;
    // 让任务栏把窗口、快捷方式和托盘归到同一个应用，而不是按 exe 路径分组。
    if let Err(error) = unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(w!("Luma.Desktop"))
    } {
        tracing::warn!(%error, "failed to set AppUserModelID");
    }
}

fn run() -> Result<(), slint::PlatformError> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        exe = %std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        "Luma Desktop starting"
    );
    #[cfg(windows)]
    set_app_user_model_id();

    // `--browser-login`：不打开主界面，直接走浏览器登录并导入 token。
    // 用于无 GUI 验证抓取链路；成功只输出设备数量。
    if std::env::args().any(|arg| arg == "--browser-login") {
        return match run_browser_login() {
            Ok((devices, uid)) => {
                tracing::info!(devices, uid, "browser login finished");
                println!("登录成功：{devices} 台设备（uid {uid}）");
                Ok(())
            }
            Err(message) => {
                eprintln!("浏览器登录失败：{message}");
                std::process::exit(1);
            }
        };
    }

    // 浏览器登录是一个一次性辅助进程，不能和主实例互斥。
    let single_instance = match instance::acquire() {
        instance::Acquire::Primary(single) => Some(single),
        instance::Acquire::AlreadyRunning => {
            tracing::info!("another Luma instance is running; activation requested");
            return Ok(());
        }
        instance::Acquire::Unavailable => None,
    };

    let ui = AppWindow::new()?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());

    let args: Vec<String> = std::env::args().collect();
    let light = args.iter().any(|arg| arg == "--light");
    let complete = args.iter().any(|arg| arg == "--complete");
    let force_demo = complete || args.iter().any(|arg| arg == "--demo");
    // `--login`：即使已有凭据也强制显示登录窗口，用于截图与切换账号。
    let force_login = args.iter().any(|arg| arg == "--login");
    // 顶部状态条：沿用上次配置；`--bar` / `--no-bar` 可临时强制开关。
    let mut bar_config = status_bar::load_config(false);
    if args.iter().any(|arg| arg == "--bar") {
        bar_config.visible = true;
    } else if args.iter().any(|arg| arg == "--no-bar") {
        bar_config.visible = false;
    }
    if args.iter().any(|arg| arg == "--taskbar") {
        bar_config.visible = true;
        bar_config.position = 2;
    }

    let mut items = mock_printers();
    if complete {
        let printer = &mut items[0];
        printer.status = "已完成".into();
        printer.status_kind = 4;
        printer.progress = 100.0;
        printer.progress_text = "100%".into();
        printer.remaining = "打印完成 · 用时 5 小时 12 分".into();
        printer.layers = "240 / 240".into();
        printer.layer_card = "240 / 240".into();
        printer.layer_current = 240;
        printer.layer_progress = 100.0;
    }

    // 先放占位数据，避免真实模式启动瞬间显示 mock 设备。
    ui.set_printers(Rc::new(VecModel::from(connecting_printer())).into());
    ui.set_arc_dots(Rc::new(VecModel::from(arc_dots())).into());
    ui.set_selected(0);
    // `--light` 仅用于 Phase 0 截图与主题验证。
    ui.set_dark_mode(!light);

    let login = LoginWindow::new()?;
    login.hide().ok();
    login.set_dark_mode(!light);
    login.set_app_version(env!("CARGO_PKG_VERSION").into());

    // 用户再次双击快捷方式时，把已有窗口拉到前台；没有 Token 时优先唤起登录窗。
    if let Some(single) = &single_instance {
        let ui_weak = ui.as_weak();
        let login_weak = login.as_weak();
        single.on_activate(move || {
            let ui = ui_weak.clone();
            let login = login_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(login) = login.upgrade() {
                    if login.window().is_visible() {
                        let _ = login.show();
                        status_bar::activate(&login.window());
                        return;
                    }
                }
                if let Some(ui) = ui.upgrade() {
                    show_main_page(&ui, ui.get_page());
                }
            });
        });
    }

    // 窗口创建后再补一次大图标：任务栏按钮取 ICON_BIG，winit 默认只设了小图标。
    {
        let ui_weak = ui.as_weak();
        let login_weak = login.as_weak();
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            if let Some(ui) = ui_weak.upgrade() {
                window_icon::apply(&ui);
            }
            if let Some(login) = login_weak.upgrade() {
                window_icon::apply_login(&login);
            }
        });
    }

    // 屏幕顶部常驻状态条：置顶、不进任务栏、不抢焦点。
    let bar_cfg = Rc::new(std::cell::RefCell::new(bar_config));
    let top_bar = StatusBarWindow::new()?;
    top_bar.set_dark_mode(!light);
    // 状态条不应被任务栏/Alt+F4 关闭；关闭请求一律忽略。
    top_bar
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::KeepWindowShown);
    status_bar::start_taskbar_watcher();
    status_bar::apply(&top_bar, &bar_cfg.borrow());
    {
        let weak = top_bar.as_weak();
        let cfg = bar_cfg.clone();
        // 窗口刚创建时 scale_factor 可能还是 1.0，事件循环起来后再套一次。
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            if let Some(bar) = weak.upgrade() {
                status_bar::apply(&bar, &cfg.borrow());
            }
        });
    }
    ui.set_bar_visible(bar_cfg.borrow().visible);

    let demo_printers = Rc::new(VecModel::from(items));
    let demo_ticker = Rc::new(slint::Timer::default());
    let stop_demo = Arc::new(AtomicBool::new(false));
    let stored_token = if force_demo || force_login {
        None
    } else {
        luma_cloud::load_token().ok().flatten()
    };
    let has_token = stored_token.is_some();

    // 内置摄像头：句柄只活 UI 线程；串流线程由 `camera` 模块管理。
    let camera_handle: Rc<RefCell<Option<camera::CameraHandle>>> = Rc::new(RefCell::new(None));
    // 真实模式默认在进入「实时监控」时自动开流；暂停后保持关闭。
    let camera_enabled = Arc::new(AtomicBool::new(has_token));
    let camera_failures = Rc::new(Cell::new(0u32));
    let camera_retry_ticks = Rc::new(Cell::new(0u32));

    if stored_token.is_some() {
        ui.set_logged_in(true);
        ui.set_account_summary("正在同步账号…".into());
        ui.set_account_url("https://bambulab.cn/zh-cn/my".into());
        tracing::debug!("stored account token loaded");
    }

    ui.on_toggle_theme({
        let ui = ui.as_weak();
        let login = login.as_weak();
        let bar = top_bar.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                let dark = !ui.get_dark_mode();
                ui.set_dark_mode(dark);
                if let Some(bar) = bar.upgrade() {
                    bar.set_dark_mode(dark);
                }
                if let Some(login) = login.upgrade() {
                    login.set_dark_mode(dark);
                }
            }
        }
    });

    ui.on_select_printer({
        let ui = ui.as_weak();
        move |index| {
            tracing::info!(index, "select-printer clicked");
            if let Some(ui) = ui.upgrade() {
                if index >= 0 && index < ui.get_printers().row_count() as i32 {
                    ui.set_selected(index);
                }
            }
        }
    });

    ui.on_add_printer({
        let login = login.as_weak();
        let ui = ui.as_weak();
        move || {
            if let (Some(login), Some(ui)) = (login.upgrade(), ui.upgrade()) {
                login.set_dark_mode(ui.get_dark_mode());
                login.set_busy(false);
                login.set_message("".into());
                login.set_message_error(false);
                let _ = login.show();
                window_icon::apply_login(&login);
            }
        }
    });

    ui.on_open_account({
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else {
                return;
            };
            if ui.get_logged_in() {
                let url = ui.get_account_url();
                let target = if url.is_empty() {
                    "https://bambulab.cn/zh-cn/my".to_string()
                } else {
                    url.to_string()
                };
                open_external_url(&target);
            } else {
                open_external_url("https://bambulab.cn/zh-cn/sign-in");
            }
        }
    });

    ui.on_toggle_camera({
        let ui = ui.as_weak();
        let handle = camera_handle.clone();
        let enabled = camera_enabled.clone();
        let failures = camera_failures.clone();
        let retry_ticks = camera_retry_ticks.clone();
        move || {
            let Some(ui) = ui.upgrade() else {
                return;
            };
            let next = !enabled.load(Ordering::SeqCst);
            enabled.store(next, Ordering::SeqCst);
            failures.set(0);
            retry_ticks.set(0);
            if next {
                if ui.get_demo_mode() {
                    ui.set_camera_live(false);
                    ui.set_camera_status("演示模式无摄像头".into());
                } else {
                    ui.set_camera_live(false);
                    ui.set_camera_status("正在连接摄像头…".into());
                }
            } else {
                if let Some(handle) = handle.borrow_mut().take() {
                    handle.stop();
                }
                ui.set_camera_live(false);
                ui.set_camera_status("摄像头已关闭".into());
            }
        }
    });

    let browser_login_active = Arc::new(AtomicBool::new(false));
    login.on_browser_login({
        let login = login.as_weak();
        let ui = ui.as_weak();
        let bar = top_bar.as_weak();
        let stop_demo = stop_demo.clone();
        let active = browser_login_active.clone();
        let camera_enabled = camera_enabled.clone();
        move || {
            let Some(login) = login.upgrade() else {
                return;
            };
            // 同一时刻只允许一个登录窗口，避免多开浏览器与调试端口。
            if active.swap(true, Ordering::SeqCst) {
                return;
            }
            login.set_busy(true);
            login.set_message_error(false);
            login.set_message(
                "已打开独立浏览器窗口。请在窗口中完成登录（手机号、密码、验证码或扫码），完成后会自动导入。"
                    .into(),
            );

            let login_for_thread = login.as_weak();
            let ui_for_thread = ui.clone();
            let bar_for_thread = bar.clone();
            let stop_demo = stop_demo.clone();
            let active = active.clone();
            let camera_enabled = camera_enabled.clone();
            std::thread::spawn(move || {
                let result = run_browser_login();
                let _ = slint::invoke_from_event_loop(move || {
                    active.store(false, Ordering::SeqCst);
                    let Some(login) = login_for_thread.upgrade() else {
                        return;
                    };
                    login.set_busy(false);
                    match result {
                        Ok((devices, uid)) => {
                            if let Some(ui) = ui_for_thread.upgrade() {
                                // 登录前 camera_enabled=false；登录成功后恢复为自动开流。
                                camera_enabled.store(true, Ordering::SeqCst);
                                enter_cloud_mode(
                                    &ui,
                                    &login,
                                    devices,
                                    &uid,
                                    &stop_demo,
                                    Some(&bar_for_thread),
                                );
                            }
                        }
                        Err(message) => {
                            login.set_message_error(true);
                            login.set_message(message.into());
                        }
                    }
                });
            });
        }
    });

    login.on_cancel({
        let login = login.as_weak();
        move || {
            if let Some(login) = login.upgrade() {
                let _ = login.hide();
            }
        }
    });

    // Slint 1.19 原生系统托盘。左键显示窗口，右键菜单可退出。
    let tray = LumaTray::new()?;
    tray.set_printer_summary("车间一号 · 打印中 66%".into());
    tray.set_bar_visible(bar_cfg.borrow().visible);

    tray.on_toggle_bar({
        let cfg = bar_cfg.clone();
        let bar = top_bar.as_weak();
        let ui_weak = ui.as_weak();
        let tray_weak = tray.as_weak();
        move || {
            let visible = !cfg.borrow().visible;
            cfg.borrow_mut().visible = visible;
            refresh_bar(
                &cfg,
                bar.upgrade().as_ref(),
                ui_weak.upgrade().as_ref(),
                tray_weak.upgrade().as_ref(),
            );
        }
    });

    // 「设置」页里的控件直接改这几项，改完立即应用到状态条。
    let initial = bar_cfg.borrow().clone();
    ui.set_bar_visible(initial.visible);
    ui.set_bar_monitor(initial.monitor);
    ui.set_bar_dock(initial.position);
    ui.set_bar_align(initial.align);
    ui.set_bar_auto_width(initial.auto_width);
    ui.set_bar_width(initial.width);
    ui.set_bar_height(initial.height);
    ui.set_bar_radius(initial.radius);
    ui.set_bar_opacity(initial.opacity);
    ui.set_bar_offset(initial.offset);
    ui.set_bar_scale(initial.scale);
    ui.set_bar_show_brand(initial.show_brand);
    ui.set_bar_show_name(initial.show_name);
    ui.set_bar_show_status(initial.show_status);
    ui.set_bar_show_progress(initial.show_progress);
    ui.set_bar_show_remaining(initial.show_remaining);
    ui.set_bar_show_progress_bar(initial.show_progress_bar);
    ui.set_bar_show_clock(initial.show_clock);
    ui.set_bar_clock_seconds(initial.clock_seconds);
    ui.set_bar_show_date(initial.show_date);
    ui.set_bar_show_battery(initial.show_battery);
    ui.set_bar_custom_text(initial.custom_text.clone().into());
    ui.set_bar_smart_sort(initial.smart_sort);
    ui.set_bar_max_printers(initial.max_printers);
    ui.set_bar_hide_hover(initial.hide_on_hover);
    ui.set_bar_hide_fullscreen(initial.hide_on_fullscreen);
    runtime::set_bar_smart_sort(initial.smart_sort);
    if args.iter().any(|arg| arg == "--settings") {
        ui.set_page(1);
    } else if args.iter().any(|arg| arg == "--monitor") {
        ui.set_page(2);
    }

    ui.on_apply_bar_settings({
        let cfg = bar_cfg.clone();
        let bar = top_bar.as_weak();
        let ui_weak = ui.as_weak();
        let ui_source = ui.as_weak();
        let tray_weak = tray.as_weak();
        move || {
            let Some(source) = ui_source.upgrade() else {
                return;
            };
            let next = status_bar::BarConfig {
                visible: source.get_bar_visible(),
                monitor: source.get_bar_monitor(),
                position: source.get_bar_dock(),
                align: source.get_bar_align(),
                auto_width: source.get_bar_auto_width(),
                width: source.get_bar_width(),
                height: source.get_bar_height(),
                radius: source.get_bar_radius(),
                opacity: source.get_bar_opacity(),
                offset: source.get_bar_offset(),
                scale: source.get_bar_scale(),
                show_brand: source.get_bar_show_brand(),
                show_name: source.get_bar_show_name(),
                show_status: source.get_bar_show_status(),
                show_progress: source.get_bar_show_progress(),
                show_remaining: source.get_bar_show_remaining(),
                show_progress_bar: source.get_bar_show_progress_bar(),
                show_clock: source.get_bar_show_clock(),
                clock_seconds: source.get_bar_clock_seconds(),
                show_date: source.get_bar_show_date(),
                show_battery: source.get_bar_show_battery(),
                custom_text: source.get_bar_custom_text().to_string(),
                smart_sort: source.get_bar_smart_sort(),
                max_printers: source.get_bar_max_printers(),
                hide_on_hover: source.get_bar_hide_hover(),
                hide_on_fullscreen: source.get_bar_hide_fullscreen(),
            }
            .sanitized();
            if (source.get_bar_height() - next.height).abs() > 0.5 {
                source.set_bar_height(next.height);
            }
            *cfg.borrow_mut() = next;
            refresh_bar(
                &cfg,
                bar.upgrade().as_ref(),
                ui_weak.upgrade().as_ref(),
                tray_weak.upgrade().as_ref(),
            );
        }
    });

    tray.on_show_window({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                show_main_page(&ui, 0);
            }
        }
    });
    tray.on_open_bar_settings({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                show_main_page(&ui, 1);
            }
        }
    });
    tray.on_quit_app({
        let handle = camera_handle.clone();
        move || {
            if let Some(handle) = handle.borrow_mut().take() {
                handle.stop();
            }
            let _ = slint::quit_event_loop();
        }
    });

    // 状态条始终点击穿透，鼠标事件不会到达窗口。这里轮询光标位置：
    // 悬停时 120ms 淡出后隐藏，离开后淡入；全屏/最大化时同样收起。
    let bar_fade = Rc::new(std::cell::RefCell::new(BarFade {
        base: initial.opacity,
        alpha: 0.0,
        target: 0.0,
        hidden: !initial.visible,
        fading: false,
    }));
    if initial.visible {
        status_bar::set_alpha(&top_bar, 0.0);
        {
            let mut fade = bar_fade.borrow_mut();
            fade.target = initial.opacity;
            fade.fading = true;
        }
        schedule_bar_fade(top_bar.as_weak(), bar_fade.clone());
    }
    let _bar_poll_timer = Rc::new(slint::Timer::default());
    let bar_raise_tick = Rc::new(std::cell::Cell::new(0u32));
    {
        let cfg = bar_cfg.clone();
        let bar = top_bar.as_weak();
        let state = bar_fade.clone();
        let raise_tick = bar_raise_tick.clone();
        _bar_poll_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(BAR_POLL_MS),
            move || {
                let Some(bar) = bar.upgrade() else {
                    return;
                };
                let config = cfg.borrow().clone();
                status_bar::sync_taskbar_items(&bar, &config);
                status_bar::sync_taskbar_position(&bar, &config);
                status_bar::sync_auto_width(&bar, &config);
                if config.position == 2 {
                    let next = raise_tick.get() + 1;
                    if next >= 20 {
                        status_bar::raise(&bar);
                        raise_tick.set(0);
                    } else {
                        raise_tick.set(next);
                    }
                } else {
                    raise_tick.set(0);
                }
                let native_visible = status_bar::is_native_visible(&bar);
                let mut start_fade = false;
                {
                    let mut fade = state.borrow_mut();
                    fade.base = config.opacity;

                    // 设置页/托盘会直接 apply 并显示或隐藏窗口，这里把状态拉回一致。
                    if native_visible && fade.hidden {
                        fade.hidden = false;
                        fade.alpha = config.opacity;
                        fade.target = config.opacity;
                        status_bar::set_alpha(&bar, config.opacity);
                    } else if !native_visible && !fade.hidden {
                        fade.hidden = true;
                        fade.alpha = 0.0;
                        fade.target = 0.0;
                    }

                    let hovered = config.hide_on_hover && status_bar::cursor_inside_bar(&bar);
                    let fullscreen = (config.hide_on_fullscreen
                        && status_bar::foreground_covers_bar(&bar))
                        || (config.position == 2 && status_bar::foreground_is_fullscreen(&bar));
                    let should_show = config.visible && !hovered && !fullscreen;
                    let target = if should_show { config.opacity } else { 0.0 };

                    if should_show && fade.hidden {
                        status_bar::set_native_visible(&bar, true);
                        status_bar::set_alpha(&bar, 0.0);
                        fade.hidden = false;
                        fade.alpha = 0.0;
                    }

                    if (fade.target - target).abs() > 0.001 {
                        fade.target = target;
                        if !fade.fading {
                            fade.fading = true;
                            start_fade = true;
                        }
                    } else if should_show
                        && !fade.fading
                        && !fade.hidden
                        && (fade.alpha - target).abs() > 0.001
                    {
                        fade.alpha = target;
                        status_bar::set_alpha(&bar, target);
                    }
                }

                if start_fade {
                    schedule_bar_fade(bar.as_weak(), state.clone());
                }
            },
        );
    }

    // 时钟/日期每秒刷新；电池每 30 秒查询一次。
    let _clock_timer = Rc::new(slint::Timer::default());
    let clock_tick = Rc::new(std::cell::Cell::new(0u32));
    {
        let cfg = bar_cfg.clone();
        let bar = top_bar.as_weak();
        let tick = clock_tick.clone();
        _clock_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                let Some(bar) = bar.upgrade() else {
                    return;
                };
                let config = cfg.borrow().clone();
                status_bar::update_clock(&bar, &config);
                let next = tick.get() + 1;
                if next >= 30 {
                    status_bar::update_battery(&bar, &config);
                    tick.set(0);
                } else {
                    tick.set(next);
                }
            },
        );
    }

    let mut login_shown = false;
    if force_demo {
        ui.set_printers(demo_printers.clone().into());
        ui.set_selected(0);
        ui.set_demo_mode(true);
        ui.set_logged_in(false);
        ui.set_account_summary("未登录".into());
        ui.set_account_url("".into());
        ui.set_cloud_state(0);
        ui.set_cloud_status("演示模式".into());
        tray.set_printer_summary("演示模式".into());
        start_mock_ticker(
            demo_printers.clone(),
            demo_ticker.clone(),
            stop_demo.clone(),
            Some(top_bar.as_weak()),
        );
    } else if has_token {
        ui.set_demo_mode(false);
        ui.set_cloud_state(1);
        ui.set_cloud_status("正在连接…".into());
        tray.set_printer_summary("云端监控中".into());
        let _ = runtime::start(ui.as_weak(), Some(top_bar.as_weak()));
    } else {
        ui.set_printers(demo_printers.clone().into());
        ui.set_selected(0);
        ui.set_demo_mode(true);
        ui.set_logged_in(false);
        ui.set_account_summary("未登录".into());
        ui.set_account_url("".into());
        ui.set_cloud_state(0);
        ui.set_cloud_status("未登录 · 演示模式".into());
        tray.set_printer_summary("未登录".into());
        start_mock_ticker(
            demo_printers.clone(),
            demo_ticker.clone(),
            stop_demo.clone(),
            Some(top_bar.as_weak()),
        );
        login.set_message("登录后切换到真实设备；也可以先看演示。".into());
        let _ = login.show();
        login_shown = true;
    }

    // 实时监控历史：每 2 秒采样一次当前选中设备，保留最近 3 分钟。
    let live_history = Rc::new(std::cell::RefCell::new(LiveHistory::default()));
    record_live_history(&ui, &live_history);
    let _live_history_timer = Rc::new(slint::Timer::default());
    {
        let ui = ui.as_weak();
        let state = live_history.clone();
        _live_history_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                if let Some(ui) = ui.upgrade() {
                    record_live_history(&ui, &state);
                }
            },
        );
    }

    // 摄像头只在「实时监控」页且勾选开启时运行；离开页面即停流，回到页面自动恢复。
    let _camera_timer = Rc::new(slint::Timer::default());
    {
        let ui = ui.as_weak();
        let handle = camera_handle.clone();
        let enabled = camera_enabled.clone();
        let failures = camera_failures.clone();
        let retry_ticks = camera_retry_ticks.clone();
        _camera_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let Some(ui) = ui.upgrade() else {
                    return;
                };
                let monitor_page = ui.get_page() == 2;
                let demo = ui.get_demo_mode();
                // `current` 是 Slint 私有属性，这里从模型里按选中项取序列号。
                let serial = {
                    let printers = ui.get_printers();
                    let index = ui.get_selected();
                    if index >= 0 && (index as usize) < printers.row_count() {
                        printers
                            .row_data(index as usize)
                            .map(|printer| printer.serial.to_string())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                };
                let want =
                    monitor_page && !demo && !serial.is_empty() && enabled.load(Ordering::SeqCst);

                if ui.get_camera_live() {
                    failures.set(0);
                    retry_ticks.set(0);
                }

                if !want {
                    if let Some(handle) = handle.borrow_mut().take() {
                        handle.stop();
                        ui.set_camera_live(false);
                    }
                    failures.set(0);
                    retry_ticks.set(0);
                    if monitor_page {
                        if demo {
                            ui.set_camera_status("演示模式无摄像头".into());
                        } else if serial.is_empty() {
                            ui.set_camera_status("未选择打印机".into());
                        }
                    }
                    return;
                }

                // 换设备或线程结束：先清理旧会话，再按冷却决定是否重建。
                let mut restart = false;
                {
                    let mut slot = handle.borrow_mut();
                    match slot.as_ref() {
                        Some(handle) if handle.serial() != serial => {
                            handle.stop();
                            *slot = None;
                            // 旧打印机的最后一帧可能还在事件队列里；先隐藏画面，
                            // 新会话收到首帧后才会重新置为 live。
                            ui.set_camera_live(false);
                            ui.set_camera_status("正在连接摄像头…".into());
                            restart = true;
                        }
                        Some(handle) if handle.is_finished() => {
                            handle.stop();
                            *slot = None;
                            let count = failures.get() + 1;
                            failures.set(count);
                            ui.set_camera_live(false);
                            if count > 3 {
                                enabled.store(false, Ordering::SeqCst);
                                retry_ticks.set(0);
                                ui.set_camera_status("摄像头暂时不可用，点击「开启」重试".into());
                                return;
                            }
                            // 5 秒后再试，避免失败时空转。
                            retry_ticks.set(10);
                        }
                        Some(_) => return,
                        None => restart = true,
                    }
                }

                let cooling = retry_ticks.get();
                if cooling > 0 {
                    retry_ticks.set(cooling - 1);
                    return;
                }
                if restart {
                    *handle.borrow_mut() = Some(camera::start(ui.as_weak(), serial));
                }
            },
        );
    }

    tracing::info!(
        scale_factor = ui.window().scale_factor(),
        selected = ui.get_selected(),
        dark = ui.get_dark_mode(),
        printers = ui.get_printers().row_count(),
        tray = true,
        login_shown,
        "Luma Desktop started"
    );

    ui.run()
}
