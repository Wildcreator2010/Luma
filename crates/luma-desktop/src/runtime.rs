use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use luma_cloud::{
    delete_token, load_token, store_token, AuthToken, BambuAuth, BambuCloudConfig,
    BambuCloudProvider, CloudError,
};
use luma_core::{PrinterManager, PrinterProvider, ProviderEvent};
use luma_model::{PrinterState, PrinterStatus};
use luma_protocol::project_payload;
use slint::{invoke_from_event_loop, Model, VecModel};
use tokio::sync::mpsc;

use crate::{AppWindow, BarPrinter, PrinterItem, StatusBarWindow};

/// 运行时代际号。每次 `start` 递增，旧代际必须在下一轮循环自行退出，
/// 避免重复登录后出现两个 MQTT 会话互相覆盖 UI。
static GENERATION: AtomicU64 = AtomicU64::new(0);
static CURRENT_STOP: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();
static BAR_SMART_SORT: AtomicBool = AtomicBool::new(true);

fn current_stop() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    CURRENT_STOP.get_or_init(|| Mutex::new(None))
}

fn is_current(generation: u64) -> bool {
    GENERATION.load(Ordering::SeqCst) == generation
}

/// 状态条排序策略。设置变化后在下一次快照推送时生效。
pub fn set_bar_smart_sort(smart: bool) {
    BAR_SMART_SORT.store(smart, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    Demo,
    Cloud,
    Error,
}

/// 启动后台运行时。无 token 时返回 Demo，调用方继续使用 mock 数据。
pub fn start(ui: slint::Weak<AppWindow>, bar: Option<slint::Weak<StatusBarWindow>>) -> RuntimeMode {
    let token = match load_token() {
        Ok(Some(token)) => token,
        Ok(None) => return RuntimeMode::Demo,
        Err(error) => {
            tracing::warn!(%error, "failed to read stored token");
            set_logged_out(&ui);
            set_status(&ui, "无法读取登录凭据，请重新登录".to_string(), 3);
            set_placeholder(&ui, "无法读取登录凭据", "无法读取登录凭据");
            return RuntimeMode::Error;
        }
    };

    set_status(&ui, "正在连接…".to_string(), 1);

    // 先让上一次运行时退出：登录窗口二次登录时，旧连接会继续写 UI。
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = current_stop().lock() {
        if let Some(previous) = slot.take() {
            previous.store(true, Ordering::SeqCst);
        }
        *slot = Some(stop.clone());
    }

    let ui_for_thread = ui.clone();
    let bar_for_thread = bar.clone();
    let thread = std::thread::Builder::new()
        .name("luma-runtime".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "failed to start tokio runtime");
                    return;
                }
            };

            if let Err(error) = runtime.block_on(run_cloud(
                ui_for_thread.clone(),
                bar_for_thread.clone(),
                token,
                stop,
                generation,
            )) {
                tracing::warn!(error = %describe_error(&error), "cloud runtime stopped");
                if !is_current(generation) {
                    return;
                }
                let message = match error.downcast_ref::<CloudError>() {
                    Some(CloudError::NotAuthenticated) | Some(CloudError::TokenExpired) => {
                        // 失效的凭据留在凭据管理器里只会让下次启动继续失败。
                        let _ = delete_token();
                        set_logged_out(&ui_for_thread);
                        "登录已失效，请重新登录"
                    }
                    Some(CloudError::NoDevices) => {
                        set_placeholder(&ui_for_thread, "账号下没有绑定设备", "账号下没有绑定设备");
                        "账号下没有绑定设备"
                    }
                    Some(CloudError::HttpStatus { status: 403, .. }) => {
                        "云端拒绝了请求（403），请重新登录或查看日志"
                    }
                    _ => "云端连接失败，请查看日志",
                };
                set_status(&ui_for_thread, message.to_string(), 3);
            }
        })
        .context("spawn luma runtime");

    if let Err(error) = thread {
        tracing::error!(%error, "failed to spawn runtime thread");
        set_status(&ui, "后台运行时启动失败，请查看日志".to_string(), 3);
        return RuntimeMode::Error;
    }

    RuntimeMode::Cloud
}

async fn run_cloud(
    ui: slint::Weak<AppWindow>,
    bar: Option<slint::Weak<StatusBarWindow>>,
    token: AuthToken,
    stop: Arc<AtomicBool>,
    generation: u64,
) -> Result<()> {
    let config = BambuCloudConfig::china_mainland();
    let auth = BambuAuth::new(config.clone())?;
    let (token, devices, uid) = auth.prepare_session(token).await?;
    // 回写凭据（补 uid/过期时间）失败不应中断监控。
    if let Err(error) = store_token(&token) {
        tracing::warn!(%error, "failed to refresh stored token");
    }

    tracing::info!(devices = devices.len(), "cloud session prepared");
    // 摄像头取流需要设备的局域网访问码；失败只影响画面，不影响监控。
    match auth.fetch_device_access_codes(&token).await {
        Ok(codes) => {
            let count = codes.len();
            for (serial, code) in codes {
                crate::camera::register_access_code(&serial, &code);
            }
            tracing::info!(count, "camera access codes cached");
        }
        Err(error) => tracing::warn!(%error, "failed to fetch camera access codes"),
    }
    let profile = auth.fetch_account_profile(&token).await.ok();
    let account_name = profile
        .as_ref()
        .map(|profile| profile.display_name())
        .unwrap_or_else(|| "Bambu Cloud".to_string());
    let account_url = profile
        .as_ref()
        .map(|profile| profile.profile_url())
        .unwrap_or_else(|| "https://bambulab.cn/zh-cn/my".to_string());
    tracing::info!(
        name = %account_name,
        handle = profile.as_ref().and_then(|profile| profile.handle.as_deref()),
        "account profile resolved"
    );
    {
        let ui = ui.clone();
        let name = account_name.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_logged_in(true);
                ui.set_account_summary(name.into());
                ui.set_account_url(account_url.into());
            }
        });
    }

    let mut manager = PrinterManager::new();
    for device in &devices {
        manager.upsert_device(device.clone());
    }
    push_snapshot(
        &ui,
        bar.as_ref(),
        &manager,
        Some("已获取设备，等待 MQTT".to_string()),
        1,
    );

    let provider = BambuCloudProvider::connect(config, token, devices, uid).await?;
    let (tx, mut rx) = mpsc::channel::<ProviderEvent>(256);
    let provider_task = tokio::spawn(async move {
        if let Err(error) = provider.run(tx).await {
            tracing::warn!(%error, "provider stopped");
        }
    });

    let mut fatal = false;
    let mut last_report: HashMap<String, Instant> = HashMap::new();
    let mut pending_push = false;
    let mut last_push = Instant::now() - Duration::from_secs(1);

    let mut ui_ticker = tokio::time::interval(Duration::from_millis(200));
    let mut watchdog = tokio::time::interval(Duration::from_secs(15));
    ui_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if stop.load(Ordering::SeqCst) || !is_current(generation) {
            tracing::info!(generation, "cloud runtime superseded");
            break;
        }
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    ProviderEvent::Report {
                        serial,
                        payload,
                        received_at_ms,
                    } => {
                        last_report.insert(serial.clone(), Instant::now());
                        // 摄像头要用局域网地址直连，随 MQTT 上报顺带更新。
                        if let Some(ip) = extract_printer_ip(&payload) {
                            crate::camera::set_ip(&serial, ip);
                        }
                        let changed = match manager.apply_report(&serial, &payload, received_at_ms) {
                            Ok(changed) => changed,
                            Err(error) => {
                                tracing::warn!(%serial, %error, "report for unknown printer");
                                continue;
                            }
                        };
                        if let Some(merged) = manager.merged_report(&serial) {
                            crate::camera::update_from_report(&serial, merged);
                        }

                        if changed {
                            if let Some(info) = manager.device_info(&serial).cloned() {
                                if let Some(merged) = manager.merged_report(&serial) {
                                    match project_payload(&info, merged, received_at_ms) {
                                        Some(state) => {
                                            if let Err(error) = manager.set_state(state) {
                                                tracing::warn!(
                                                    %serial,
                                                    %error,
                                                    "failed to store projection"
                                                );
                                            }
                                        }
                                        None => {
                                            tracing::warn!(
                                                %serial,
                                                "payload has no print block"
                                            );
                                        }
                                    }
                                }
                            }

                            pending_push = true;
                            if last_push.elapsed() >= Duration::from_millis(200) {
                                push_snapshot(
                                    &ui,
                                    bar.as_ref(),
                                    &manager,
                                    Some("云端已连接".to_string()),
                                    2,
                                );
                                pending_push = false;
                                last_push = Instant::now();
                            }
                        }
                    }
                    ProviderEvent::ConnectionChanged { online, detail } => {
                        if let Some(detail) = &detail {
                            tracing::info!(online, %detail, "cloud connection changed");
                        }
                        if online {
                            let now = Instant::now();
                            for device in manager.devices() {
                                last_report.insert(device.serial.clone(), now);
                                let _ = manager.set_connection_pending(&device.serial);
                            }
                            push_snapshot(
                                &ui,
                                bar.as_ref(),
                                &manager,
                                Some("已连接 · 等待设备状态".to_string()),
                                2,
                            );
                        } else {
                            for device in manager.devices() {
                                let _ = manager.set_online(&device.serial, false);
                            }
                            push_snapshot(
                                &ui,
                                bar.as_ref(),
                                &manager,
                                Some("连接中断，重连中…".to_string()),
                                1,
                            );
                        }
                        pending_push = false;
                        last_push = Instant::now();
                    }
                    ProviderEvent::Error { message, retryable } => {
                        tracing::warn!(%message, retryable, "provider error");
                        if !retryable {
                            fatal = true;
                        }
                        let text = if retryable {
                            format!("连接异常：{message}")
                        } else {
                            format!("需要重新登录：{message}")
                        };
                        set_status(&ui, text, 3);
                    }
                    ProviderEvent::DevicesDiscovered(_) => {}
                }
            }
            _ = ui_ticker.tick() => {
                if pending_push {
                    push_snapshot(&ui, bar.as_ref(), &manager, None, 2);
                    pending_push = false;
                    last_push = Instant::now();
                }
            }
            _ = watchdog.tick() => {
                let now = Instant::now();
                let mut changed = false;
                for device in manager.devices() {
                    let stale = last_report
                        .get(&device.serial)
                        .map(|at| now.duration_since(*at) > Duration::from_secs(60))
                        .unwrap_or(false);
                    if stale
                        && manager
                            .state(&device.serial)
                            .map(|state| state.online)
                            .unwrap_or(false)
                    {
                        let _ = manager.set_online(&device.serial, false);
                        changed = true;
                    }
                }
                if changed {
                    push_snapshot(&ui, bar.as_ref(), &manager, None, 2);
                    pending_push = false;
                    last_push = Instant::now();
                }
            }
        }
    }

    tracing::warn!("provider event channel closed");
    provider_task.abort();
    if !fatal && is_current(generation) && !stop.load(Ordering::SeqCst) {
        set_status(&ui, "连接已停止".to_string(), 3);
    }
    Ok(())
}

fn push_snapshot(
    ui: &slint::Weak<AppWindow>,
    bar: Option<&slint::Weak<StatusBarWindow>>,
    manager: &PrinterManager,
    status: Option<String>,
    cloud_state: i32,
) {
    let mut states = manager.snapshot();
    if states.is_empty() {
        return;
    }
    if BAR_SMART_SORT.load(Ordering::SeqCst) {
        states.sort_by_key(|state| bar_rank(state.status));
    }
    let items: Vec<PrinterItem> = states.iter().map(to_item).collect();
    let bar_items: Vec<BarPrinter> = states.iter().map(to_bar_item).collect();
    let ui = ui.clone();
    let bar = bar.cloned();

    let _ = invoke_from_event_loop(move || {
        if let Some(bar) = bar.as_ref().and_then(|bar| bar.upgrade()) {
            crate::status_bar::set_printers(&bar, bar_items);
        }
        if let Some(ui) = ui.upgrade() {
            let previous_serial = ui
                .get_printers()
                .row_data(ui.get_selected().max(0) as usize)
                .map(|item| item.serial.clone());
            let selected = previous_serial
                .and_then(|serial| items.iter().position(|item| item.serial == serial))
                .unwrap_or(0) as i32;
            ui.set_selected(0);
            ui.set_printers(Rc::new(VecModel::from(items)).into());
            ui.set_selected(selected);
            ui.set_demo_mode(false);
            ui.set_cloud_state(cloud_state);
            if let Some(status) = status {
                ui.set_cloud_status(status.into());
            }
        }
    });
}

fn set_placeholder(ui: &slint::Weak<AppWindow>, message: &str, cloud_status: &str) {
    let cloud_status = cloud_status.to_string();
    let item = PrinterItem {
        name: "Luma".into(),
        model: "".into(),
        serial: "".into(),
        status: "无设备".into(),
        status_kind: 3,
        progress: 0.0,
        progress_text: "—".into(),
        progress_known: false,
        task: message.to_string().into(),
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
    };
    let ui = ui.clone();
    let _ = invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_printers(Rc::new(VecModel::from(vec![item])).into());
            ui.set_selected(0);
            ui.set_demo_mode(false);
            ui.set_cloud_state(3);
            ui.set_cloud_status(cloud_status.into());
        }
    });
}

fn set_status(ui: &slint::Weak<AppWindow>, text: String, cloud_state: i32) {
    let ui = ui.clone();
    let _ = invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_demo_mode(false);
            ui.set_cloud_status(text.into());
            ui.set_cloud_state(cloud_state);
        }
    });
}

fn set_logged_out(ui: &slint::Weak<AppWindow>) {
    // 登录失效后访问码也不再可信，避免继续用旧凭据尝试取流。
    crate::camera::clear_credentials();
    let ui = ui.clone();
    let _ = invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_logged_in(false);
            ui.set_account_summary("未登录".into());
            ui.set_account_url("".into());
        }
    });
}

fn describe_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<CloudError>() {
        Some(CloudError::HttpStatus { status, .. }) => format!("HTTP {status}"),
        Some(CloudError::Api { code, .. }) => format!("API error {code:?}"),
        Some(CloudError::CaptchaRequired { .. }) => "captcha required".to_string(),
        Some(CloudError::Http(error)) => error
            .status()
            .map(|status| format!("HTTP {}", status.as_u16()))
            .unwrap_or_else(|| "HTTP transport error".to_string()),
        Some(CloudError::NotAuthenticated) => "not authenticated".to_string(),
        Some(CloudError::TokenExpired) => "token expired".to_string(),
        Some(CloudError::NoDevices) => "no devices".to_string(),
        Some(CloudError::Keyring(_)) => "keyring error".to_string(),
        Some(CloudError::Json(_)) => "json error".to_string(),
        Some(CloudError::Mqtt(message)) => format!("mqtt error: {message}"),
        Some(CloudError::Protocol(message)) => format!("protocol error: {message}"),
        Some(CloudError::VerifyCodeRequired { .. }) => "verification code required".to_string(),
        Some(CloudError::TfaRequired { .. }) => "2FA required".to_string(),
        Some(CloudError::LoginRejected { status, code, .. }) => {
            format!("login rejected HTTP {status} code {code:?}")
        }
        Some(CloudError::VerifyCodeExpired) => "verification code expired".to_string(),
        Some(CloudError::VerifyCodeInvalid) => "verification code invalid".to_string(),
        Some(CloudError::WafBlocked { status }) => format!("WAF blocked HTTP {status}"),
        None => "internal error".to_string(),
    }
}

fn to_bar_item(state: &PrinterState) -> BarPrinter {
    let status = status_label(state.status).to_string();
    let detail = match state.status {
        PrinterStatus::Printing | PrinterStatus::Paused => {
            match (state.progress, state.remaining_seconds) {
                (Some(progress), Some(seconds)) => {
                    format!("{progress:.0}% · {}", compact_duration(seconds))
                }
                (Some(progress), None) => format!("{progress:.0}%"),
                (None, Some(seconds)) => compact_duration(seconds),
                (None, None) => status_label(state.status).to_string(),
            }
        }
        PrinterStatus::Finished => "已完成".to_string(),
        PrinterStatus::Failed => "异常".to_string(),
        PrinterStatus::Offline => "离线".to_string(),
        PrinterStatus::Idle => "空闲".to_string(),
        PrinterStatus::Preparing => "准备中".to_string(),
        PrinterStatus::Drying => "烘干中".to_string(),
        PrinterStatus::Maintenance => "维护中".to_string(),
        PrinterStatus::Unknown => "未知".to_string(),
    };

    BarPrinter {
        name: state
            .name
            .clone()
            .unwrap_or_else(|| state.serial.clone())
            .into(),
        detail: detail.into(),
        status: status.into(),
        progress_text: state
            .progress
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_default()
            .into(),
        remaining: match state.status {
            PrinterStatus::Printing | PrinterStatus::Paused => state
                .remaining_seconds
                .map(compact_duration)
                .unwrap_or_default()
                .into(),
            _ => "".into(),
        },
        progress: state.progress.unwrap_or(0.0),
        progress_known: state.progress.is_some(),
        tone: bar_tone(state.status),
    }
}

fn bar_rank(status: PrinterStatus) -> u8 {
    match status {
        PrinterStatus::Failed => 0,
        PrinterStatus::Paused => 1,
        PrinterStatus::Printing => 2,
        PrinterStatus::Preparing => 3,
        PrinterStatus::Drying => 4,
        PrinterStatus::Finished => 5,
        PrinterStatus::Idle => 6,
        PrinterStatus::Maintenance => 7,
        PrinterStatus::Offline => 8,
        PrinterStatus::Unknown => 9,
    }
}

fn bar_tone(status: PrinterStatus) -> i32 {
    match status {
        PrinterStatus::Printing
        | PrinterStatus::Preparing
        | PrinterStatus::Drying
        | PrinterStatus::Finished => 1,
        PrinterStatus::Paused => 2,
        PrinterStatus::Failed => 3,
        _ => 0,
    }
}

fn compact_duration(seconds: u32) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn to_item(state: &PrinterState) -> PrinterItem {
    let progress = state.progress.unwrap_or(0.0);
    let layer_progress = match (state.current_layer, state.total_layers) {
        (Some(current), Some(total)) if total > 0 => current as f32 / total as f32 * 100.0,
        _ => 0.0,
    };

    PrinterItem {
        name: state.display_name().to_string().into(),
        model: state.model.to_string().into(),
        serial: state.serial.clone().into(),
        status: status_label(state.status).into(),
        status_kind: status_kind(state.status),
        progress,
        progress_text: state
            .progress
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| "—".to_string())
            .into(),
        progress_known: state.progress.is_some(),
        task: state
            .task_name
            .clone()
            .unwrap_or_else(|| match state.status {
                PrinterStatus::Drying => "AMS 烘干".to_string(),
                _ => "无任务".to_string(),
            })
            .into(),
        remaining: remaining_text(state).into(),
        nozzle: temperature(state.nozzle_temperature).into(),
        nozzle_target: temperature(state.nozzle_target).into(),
        nozzle_value: state.nozzle_temperature.unwrap_or(0.0),
        bed: temperature(state.bed_temperature).into(),
        bed_target: temperature(state.bed_target).into(),
        bed_value: state.bed_temperature.unwrap_or(0.0),
        layers: match (state.current_layer, state.total_layers) {
            (Some(current), Some(total)) => format!("{current} / {total}"),
            _ => "—".to_string(),
        }
        .into(),
        layer_card: match (state.current_layer, state.total_layers) {
            (Some(current), Some(total)) => format!("{current} / {total}"),
            _ if matches!(state.status, PrinterStatus::Idle) => "0 / 0".to_string(),
            _ => "N/A".to_string(),
        }
        .into(),
        layer_current: state.current_layer.unwrap_or(0) as i32,
        layer_total: state.total_layers.unwrap_or(0) as i32,
        layer_progress,
    }
}

fn status_label(status: PrinterStatus) -> &'static str {
    match status {
        PrinterStatus::Idle => "空闲",
        PrinterStatus::Preparing => "准备中",
        PrinterStatus::Printing => "打印中",
        PrinterStatus::Drying => "烘干",
        PrinterStatus::Paused => "已暂停",
        PrinterStatus::Finished => "已完成",
        PrinterStatus::Failed => "异常",
        PrinterStatus::Offline => "离线",
        PrinterStatus::Maintenance => "维护中",
        PrinterStatus::Unknown => "未知",
    }
}

fn status_kind(status: PrinterStatus) -> i32 {
    match status {
        PrinterStatus::Printing => 1,
        PrinterStatus::Paused | PrinterStatus::Failed => 2,
        PrinterStatus::Offline | PrinterStatus::Maintenance | PrinterStatus::Unknown => 3,
        PrinterStatus::Finished => 4,
        PrinterStatus::Drying => 5,
        PrinterStatus::Idle | PrinterStatus::Preparing => 0,
    }
}

fn remaining_text(state: &PrinterState) -> String {
    match state.status {
        PrinterStatus::Printing | PrinterStatus::Paused => state
            .remaining_seconds
            .map(format_duration)
            .unwrap_or_else(|| "剩余时间未知".to_string()),
        PrinterStatus::Finished => "打印完成".to_string(),
        PrinterStatus::Failed => "打印异常".to_string(),
        PrinterStatus::Offline => "设备离线".to_string(),
        PrinterStatus::Idle | PrinterStatus::Preparing => format!(
            "{} · 喷嘴 {}",
            status_label(state.status),
            temperature(state.nozzle_temperature)
        ),
        PrinterStatus::Drying => "正在烘干耗材".to_string(),
        PrinterStatus::Maintenance => "维护中".to_string(),
        PrinterStatus::Unknown => "状态未知".to_string(),
    }
}

fn format_duration(seconds: u32) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("预计剩余 {hours} 小时 {minutes} 分")
    } else {
        format!("预计剩余 {minutes} 分钟")
    }
}

fn temperature(value: Option<f32>) -> String {
    value
        .map(|value| format!("{value:.0} °C"))
        .unwrap_or_else(|| "—".to_string())
}

/// 从 `pushall` / 增量报文里取第一个非零的局域网 IPv4。
fn extract_printer_ip(payload: &serde_json::Value) -> Option<std::net::Ipv4Addr> {
    let print = payload
        .get("print")
        .or_else(|| payload.get("pushall")?.get("print"))
        .unwrap_or(payload);
    let infos = print.get("net")?.get("info")?.as_array()?;
    for info in infos {
        let raw = info
            .get("ip")
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()));
        let Some(raw) = raw else {
            continue;
        };
        if raw == 0 || raw > u32::MAX as u64 {
            continue;
        }
        // Bambu 的 `net.info[].ip` 是 32 位整数的小端内存表示，例如
        // 192.168.31.254 会以 0xFE1FA8C0 下发。直接 `Ipv4Addr::from(u32)`
        // 会得到 254.31.168.192，导致摄像头连到错误地址。
        let address = std::net::Ipv4Addr::from((raw as u32).to_le_bytes());
        if !address.is_unspecified() && !address.is_loopback() {
            return Some(address);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(0), "预计剩余 0 分钟");
        assert_eq!(format_duration(59), "预计剩余 0 分钟");
        assert_eq!(format_duration(3600), "预计剩余 1 小时 0 分");
        assert_eq!(format_duration(7380), "预计剩余 2 小时 3 分");
    }

    #[test]
    fn maps_status_kinds() {
        assert_eq!(status_kind(PrinterStatus::Printing), 1);
        assert_eq!(status_kind(PrinterStatus::Paused), 2);
        assert_eq!(status_kind(PrinterStatus::Finished), 4);
        assert_eq!(status_kind(PrinterStatus::Drying), 5);
        assert_eq!(status_kind(PrinterStatus::Idle), 0);
        assert_eq!(status_kind(PrinterStatus::Offline), 3);
    }

    #[test]
    fn formats_compact_duration_for_status_bar() {
        assert_eq!(compact_duration(0), "0m");
        assert_eq!(compact_duration(59), "0m");
        assert_eq!(compact_duration(600), "10m");
        assert_eq!(compact_duration(3600), "1h00m");
        assert_eq!(compact_duration(7380), "2h03m");
    }

    #[test]
    fn bar_tone_matches_printer_status() {
        assert_eq!(bar_tone(PrinterStatus::Printing), 1);
        assert_eq!(bar_tone(PrinterStatus::Drying), 1);
        assert_eq!(bar_tone(PrinterStatus::Finished), 1);
        assert_eq!(bar_tone(PrinterStatus::Paused), 2);
        assert_eq!(bar_tone(PrinterStatus::Failed), 3);
        assert_eq!(bar_tone(PrinterStatus::Idle), 0);
        assert_eq!(bar_tone(PrinterStatus::Offline), 0);
    }

    #[test]
    fn extracts_lan_ip_from_pushall() {
        // 192.168.1.9 的小端整数表示。
        let raw = u32::from_le_bytes([192, 168, 1, 9]);
        let payload = serde_json::json!({
            "pushall": { "print": { "net": { "info": [
                { "ip": 0, "mask": 0 },
                { "ip": raw, "mask": 4294967040u32 }
            ] } } }
        });
        assert_eq!(
            extract_printer_ip(&payload),
            Some(std::net::Ipv4Addr::new(192, 168, 1, 9))
        );
    }

    #[test]
    fn decodes_little_endian_lan_ip() {
        let payload = serde_json::json!({
            "print": { "net": { "info": [
                { "ip": u32::from_le_bytes([192, 168, 31, 254]) }
            ] } }
        });
        assert_eq!(
            extract_printer_ip(&payload),
            Some(std::net::Ipv4Addr::new(192, 168, 31, 254))
        );
    }

    #[test]
    fn ignores_missing_or_unset_ip() {
        let payload = serde_json::json!({ "print": { "net": { "info": [{ "ip": 0 }] } } });
        assert_eq!(extract_printer_ip(&payload), None);
        assert_eq!(extract_printer_ip(&serde_json::json!({})), None);
    }
}
