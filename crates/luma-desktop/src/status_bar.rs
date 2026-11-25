//! 顶部/底部状态条：吸边、置顶、不进任务栏、不抢焦点。
//!
//! 交互与外观全部由「设置 → 顶部状态条」控制：
//! - 停靠：屏幕顶部 / 底部，距工作区边缘可调
//! - 对齐：靠左 / 居中 / 靠右
//! - 尺寸：宽度、高度、圆角、缩放
//! - 外观：不透明度
//! - 内容：品牌、设备名、状态、进度、剩余时间、进度线、排序、数量
//! - 交互：始终点击穿透；鼠标悬停时淡出隐藏，离开后淡入恢复
//! - 全屏或最大化前台窗口时自动隐藏（ClassIsland 的 `HideOnFullscreen` 思路）
//!
//! 状态条固定贴边，不支持拖拽；位置由配置和显示器工作区计算。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use slint::{ComponentHandle, VecModel};

use crate::StatusBarWindow;

const SIDE_MARGIN: f32 = 16.0;
const MIN_OPACITY: f32 = 0.2;

/// 每次 apply 递增，延迟校正发现代际变化就放弃，避免跨屏/跨配置抖动。
static APPLY_GENERATION: AtomicU64 = AtomicU64::new(0);
static TASKBAR_SLOT: OnceLock<Mutex<Option<TaskbarSlot>>> = OnceLock::new();
static TASKBAR_WATCHER_STARTED: OnceLock<()> = OnceLock::new();
static TASKBAR_GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_TASKBAR_GENERATION: AtomicU64 = AtomicU64::new(0);
static FULL_PRINTERS: OnceLock<Mutex<Vec<crate::BarPrinter>>> = OnceLock::new();
static DISPLAY_MAX: AtomicU64 = AtomicU64::new(3);

#[derive(Clone, Debug)]
pub struct BarConfig {
    pub visible: bool,
    /// 0 = 主屏，1 = 鼠标所在屏。只在应用配置时取一次，之后不跟随鼠标。
    pub monitor: i32,
    /// 0 = 顶部，1 = 底部。
    pub position: i32,
    /// 0 = 靠左，1 = 居中，2 = 靠右。
    pub align: i32,
    /// 按内容自动计算宽度；关闭后使用 `width`。
    pub auto_width: bool,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
    pub opacity: f32,
    /// 距工作区边缘的偏移，逻辑像素。
    pub offset: f32,
    /// 字号/图标/内边距缩放。
    pub scale: f32,
    pub show_brand: bool,
    pub show_name: bool,
    pub show_status: bool,
    pub show_progress: bool,
    pub show_remaining: bool,
    pub show_progress_bar: bool,
    pub show_clock: bool,
    pub clock_seconds: bool,
    pub show_date: bool,
    pub show_battery: bool,
    pub custom_text: String,
    pub smart_sort: bool,
    pub max_printers: i32,
    /// 鼠标进入状态条矩形时淡出隐藏，离开后恢复。
    pub hide_on_hover: bool,
    pub hide_on_fullscreen: bool,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            visible: true,
            monitor: 0,
            position: 0,
            align: 1,
            auto_width: true,
            width: 760.0,
            height: 40.0,
            radius: 10.0,
            opacity: 0.85,
            offset: 0.0,
            scale: 1.0,
            show_brand: true,
            show_name: true,
            show_status: true,
            show_progress: true,
            show_remaining: true,
            show_progress_bar: true,
            show_clock: true,
            clock_seconds: false,
            show_date: false,
            show_battery: false,
            custom_text: String::new(),
            smart_sort: true,
            max_printers: 3,
            hide_on_hover: true,
            hide_on_fullscreen: false,
        }
    }
}

impl BarConfig {
    /// 把外部输入（设置页、旧配置文件）收敛到可用范围。
    pub fn sanitized(mut self) -> Self {
        self.monitor = self.monitor.clamp(0, 1);
        self.position = self.position.clamp(0, 2);
        self.align = self.align.clamp(0, 2);
        self.width = self.width.clamp(280.0, 1400.0);
        self.radius = self.radius.clamp(0.0, 32.0);
        self.opacity = self.opacity.clamp(MIN_OPACITY, 1.0);
        self.offset = self.offset.clamp(0.0, 64.0);
        self.scale = self.scale.clamp(0.8, 1.6);
        // 缩放会同时放大文字、图标和进度线；高度不足时内容会被上下裁切。
        let line_height = if self.show_progress_bar { 17.6 } else { 13.6 };
        let min_height = (line_height * self.scale + 4.0).ceil().max(24.0);
        self.height = self.height.clamp(min_height.min(64.0), 64.0);
        self.max_printers = self.max_printers.clamp(1, 5);
        self.custom_text = self.custom_text.trim().chars().take(48).collect();
        self
    }
}

#[cfg(windows)]
use win32::WinRect;

#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Clone, Copy, Debug)]
struct MonitorTarget {
    work: WinRect,
    handle: isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TaskbarSlot {
    taskbar: WinRect,
    slot: WinRect,
}

/// 保存完整打印机快照，按当前显示上限裁剪后推给 Slint。
pub fn set_printers(bar: &StatusBarWindow, printers: Vec<crate::BarPrinter>) {
    if let Ok(mut current) = FULL_PRINTERS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        *current = printers;
    }
    update_display_printers(bar);
}

pub fn set_display_max(bar: &StatusBarWindow, max_items: i32) {
    let max_items = max_items.max(1) as u64;
    if DISPLAY_MAX.swap(max_items, Ordering::SeqCst) != max_items {
        update_display_printers(bar);
    }
}

fn update_display_printers(bar: &StatusBarWindow) {
    let max_items = DISPLAY_MAX.load(Ordering::SeqCst) as usize;
    let (displayed, overflow) = FULL_PRINTERS
        .get()
        .and_then(|printers| printers.lock().ok())
        .map(|printers| {
            let displayed = printers.iter().take(max_items).cloned().collect::<Vec<_>>();
            let overflow = printers.len().saturating_sub(displayed.len());
            (displayed, overflow)
        })
        .unwrap_or_default();
    bar.set_printers(std::rc::Rc::new(VecModel::from(displayed)).into());
    bar.set_overflow_count(overflow as i32);
}

fn full_printers() -> Vec<crate::BarPrinter> {
    FULL_PRINTERS
        .get()
        .and_then(|printers| printers.lock().ok().map(|printers| printers.clone()))
        .unwrap_or_default()
}

pub fn update_clock(bar: &StatusBarWindow, config: &BarConfig) {
    #[cfg(windows)]
    unsafe {
        let mut time = win32::SystemTime::default();
        win32::GetLocalTime(&mut time);
        let clock = if config.show_clock {
            if config.clock_seconds {
                format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second)
            } else {
                format!("{:02}:{:02}", time.hour, time.minute)
            }
        } else {
            String::new()
        };
        let date = if config.show_date {
            let weekday = ["日", "一", "二", "三", "四", "五", "六"]
                .get(time.day_of_week as usize)
                .copied()
                .unwrap_or("?");
            format!("{:02}-{:02} 周{weekday}", time.month, time.day)
        } else {
            String::new()
        };
        bar.set_clock_text(clock.into());
        bar.set_date_text(date.into());
    }
    #[cfg(not(windows))]
    {
        let _ = (bar, config);
    }
}

pub fn update_battery(bar: &StatusBarWindow, config: &BarConfig) {
    #[cfg(windows)]
    unsafe {
        let mut status = win32::SystemPowerStatus::default();
        let ok = win32::GetSystemPowerStatus(&mut status) != 0;
        let has_battery = ok
            && status.battery_flag & win32::BATTERY_FLAG_NO_BATTERY == 0
            && status.battery_life_percent != 255;
        if config.show_battery && has_battery {
            let percent = status.battery_life_percent as i32;
            let charging = status.battery_flag & win32::BATTERY_FLAG_CHARGING != 0
                || status.ac_line_status == 1;
            bar.set_battery_text(format!("{percent}%").into());
            bar.set_battery_percent(percent);
            bar.set_battery_charging(charging);
        } else {
            bar.set_battery_text("".into());
            bar.set_battery_percent(-1);
            bar.set_battery_charging(false);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (bar, config);
    }
}

/// 后台探测任务栏按钮与空白区域。只有任务栏是水平方向时才启用任务栏内停靠。
pub fn start_taskbar_watcher() {
    #[cfg(windows)]
    {
        if TASKBAR_WATCHER_STARTED.set(()).is_err() {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("luma-taskbar".to_string())
            .spawn(|| {
                use windows::Win32::System::Com::{
                    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
                };
                use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let mut automation: Option<IUIAutomation> =
                    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() };

                loop {
                    if automation.is_none() {
                        automation = unsafe {
                            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
                        };
                    }
                    let next = automation.as_ref().and_then(detect_taskbar_slot);
                    let changed = TASKBAR_SLOT
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map(|mut slot| {
                            if *slot == next {
                                false
                            } else {
                                *slot = next;
                                true
                            }
                        })
                        .unwrap_or(false);
                    if changed {
                        TASKBAR_GENERATION.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                }
            });
    }
}

#[cfg(windows)]
fn detect_taskbar_slot(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
) -> Option<TaskbarSlot> {
    use windows::core::w;
    use windows::Win32::UI::Accessibility::{TreeScope_Descendants, UIA_ButtonControlTypeId};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW};

    unsafe {
        let hwnd = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if hwnd.0.is_null() {
            return None;
        }
        let hwnd_raw = hwnd.0 as isize;
        let mut taskbar = WinRect::default();
        if win32::GetWindowRect(hwnd_raw, &mut taskbar) == 0 {
            return None;
        }
        let taskbar_width = taskbar.right - taskbar.left;
        let taskbar_height = taskbar.bottom - taskbar.top;
        if taskbar_width <= taskbar_height || taskbar_height < 24 {
            return None;
        }
        let mut appbar = win32::AppBarData {
            cb_size: std::mem::size_of::<win32::AppBarData>() as u32,
            h_wnd: hwnd_raw,
            callback_message: 0,
            edge: 0,
            rect: WinRect::default(),
            l_param: 0,
        };
        if win32::SHAppBarMessage(win32::ABM_GETSTATE, &mut appbar) & win32::ABS_AUTOHIDE != 0 {
            return None;
        }

        let mut occupied: Vec<(i32, i32)> = Vec::new();
        if let Ok(root) = automation.ElementFromHandle(hwnd) {
            if let Ok(condition) = automation.CreateTrueCondition() {
                if let Ok(elements) = root.FindAll(TreeScope_Descendants, &condition) {
                    if let Ok(length) = elements.Length() {
                        for index in 0..length {
                            let Ok(element) = elements.GetElement(index) else {
                                continue;
                            };
                            if element.CurrentControlType().ok() != Some(UIA_ButtonControlTypeId) {
                                continue;
                            }
                            let Ok(rect) = element.CurrentBoundingRectangle() else {
                                continue;
                            };
                            if rect.right > rect.left && rect.bottom > rect.top {
                                occupied.push((rect.left, rect.right));
                            }
                        }
                    }
                }
            }
        }

        // UIA 失败或漏掉托盘时，用已知子窗口兜底。
        for class in [w!("TrayNotifyWnd"), w!("ReBarWindow32")] {
            if let Ok(child) = FindWindowExW(Some(hwnd), None, class, None) {
                let mut rect = WinRect::default();
                if !child.0.is_null() && win32::GetWindowRect(child.0 as isize, &mut rect) != 0 {
                    occupied.push((rect.left, rect.right));
                }
            }
        }

        occupied.retain(|(left, right)| right > left);
        if occupied.is_empty() {
            return None;
        }
        occupied.sort_by_key(|interval| interval.0);
        let mut merged: Vec<(i32, i32)> = Vec::new();
        for (left, right) in occupied {
            let left = left.max(taskbar.left);
            let right = right.min(taskbar.right);
            if right <= left {
                continue;
            }
            if let Some(last) = merged.last_mut() {
                if left <= last.1 {
                    last.1 = last.1.max(right);
                    continue;
                }
            }
            merged.push((left, right));
        }

        let mut gaps: Vec<(i32, i32)> = Vec::new();
        let mut cursor = taskbar.left;
        for (left, right) in merged {
            if left > cursor {
                gaps.push((cursor, left));
            }
            cursor = cursor.max(right);
        }
        if cursor < taskbar.right {
            gaps.push((cursor, taskbar.right));
        }

        let mut best: Option<(i32, i32)> = None;
        for (left, right) in gaps {
            let left = left + 4;
            let right = right - 4;
            let width = right - left;
            if width >= 240
                && best
                    .map(|current| width > current.1 - current.0)
                    .unwrap_or(true)
            {
                best = Some((left, right));
            }
        }

        let (left, right) = best?;
        let top = taskbar.top + 4;
        let bottom = taskbar.bottom - 4;
        if bottom - top < 24 {
            return None;
        }
        Some(TaskbarSlot {
            taskbar,
            slot: WinRect {
                left,
                top,
                right,
                bottom,
            },
        })
    }
}

#[cfg(not(windows))]
fn detect_taskbar_slot(_automation: &()) -> Option<TaskbarSlot> {
    None
}

fn taskbar_slot() -> Option<TaskbarSlot> {
    TASKBAR_SLOT
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|slot| *slot))
}

fn slot_matches_target(slot: TaskbarSlot, target: MonitorTarget) -> bool {
    let taskbar = slot.taskbar;
    let work = target.work;
    let horizontal_overlap = taskbar.left < work.right && taskbar.right > work.left;
    let edge_distance = (taskbar.top - work.bottom)
        .abs()
        .min((taskbar.bottom - work.top).abs());
    horizontal_overlap && edge_distance <= 8
}

/// 任务栏图标变化后重新贴合空白槽位。
pub fn sync_taskbar_position(bar: &StatusBarWindow, config: &BarConfig) {
    if config.position != 2 {
        return;
    }
    let generation = TASKBAR_GENERATION.load(Ordering::SeqCst);
    if generation == LAST_TASKBAR_GENERATION.load(Ordering::SeqCst) {
        return;
    }
    LAST_TASKBAR_GENERATION.store(generation, Ordering::SeqCst);
    if let Some(target) = monitor_target(config.monitor) {
        place(bar, config, target, false, config.auto_width);
    }
}

/// 任务栏模式会按空白槽位宽度自动减少同时显示的打印机数量。
pub fn sync_taskbar_items(bar: &StatusBarWindow, config: &BarConfig) {
    if config.position != 2 {
        set_display_max(bar, config.max_printers);
        return;
    }
    let Some(target) = monitor_target(config.monitor) else {
        set_display_max(bar, config.max_printers);
        return;
    };
    let Some(slot) = taskbar_slot().filter(|slot| slot_matches_target(*slot, target)) else {
        set_display_max(bar, config.max_printers);
        return;
    };
    let scale = scale_for_bar(bar, target);
    let available = (slot.slot.right - slot.slot.left).max(1) as f32 / scale - 8.0;
    let mut chosen = 1;
    for count in (1..=config.max_printers.max(1)).rev() {
        if measure_content_width_for(config, count as usize) <= available {
            chosen = count;
            break;
        }
    }
    set_display_max(bar, chosen);
}

/// 任务栏会周期性重绘并把自己抬到最上层，定时把状态条重新压回顶部。
pub fn raise(bar: &StatusBarWindow) {
    #[cfg(windows)]
    if let Some(hwnd) = hwnd_of(bar) {
        unsafe {
            win32::SetWindowPos(
                hwnd,
                win32::HWND_TOPMOST,
                0,
                0,
                0,
                0,
                win32::SWP_NOSIZE | win32::SWP_NOMOVE | win32::SWP_NOACTIVATE,
            );
        }
    }
}

/// 把配置同步到 Slint 侧（字号、内容开关、点击行为）。
pub fn apply_visual(bar: &StatusBarWindow, config: &BarConfig) {
    let config = config.clone().sanitized();
    bar.set_ui_scale(config.scale);
    set_display_max(bar, config.max_printers);
    bar.set_show_brand(config.show_brand);
    bar.set_show_name(config.show_name);
    bar.set_show_status(config.show_status);
    bar.set_show_progress(config.show_progress);
    bar.set_show_remaining(config.show_remaining);
    bar.set_show_progress_bar(config.show_progress_bar);
    bar.set_show_clock(config.show_clock);
    bar.set_show_date(config.show_date);
    bar.set_show_battery(config.show_battery);
    bar.set_custom_text(config.custom_text.clone().into());
    bar.set_corner_radius(config.radius);
    update_clock(bar, &config);
    update_battery(bar, &config);
    sync_taskbar_items(bar, &config);
}

/// 把配置应用到状态条窗口。
pub fn apply(bar: &StatusBarWindow, config: &BarConfig) {
    let config = config.clone().sanitized();
    apply_visual(bar, &config);

    if !config.visible {
        let _ = bar.hide();
        return;
    }

    let Some(target) = monitor_target(config.monitor) else {
        let _ = bar.show();
        return;
    };

    let keep_width = config.auto_width && is_native_visible(bar);
    place(bar, &config, target, true, keep_width);

    // resize 与样式提交都是异步的。多次延迟校正，但只接受最新一次 apply。
    let generation = APPLY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    for delay in [80u64, 300, 1000] {
        let config = config.clone();
        let weak = bar.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            if APPLY_GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Some(bar) = weak.upgrade() {
                place(&bar, &config, target, false, config.auto_width);
            }
        });
    }
}

fn place(
    bar: &StatusBarWindow,
    config: &BarConfig,
    target: MonitorTarget,
    first_show: bool,
    keep_width: bool,
) {
    let scale = scale_for_bar(bar, target);
    let width = if keep_width && config.auto_width && bar.window().size().width > 0 {
        bar.window().size().width as f32 / scale
    } else {
        desired_width(bar, config, target, scale)
    };
    place_sized(bar, config, target, width, first_show);
}

fn place_sized(
    bar: &StatusBarWindow,
    config: &BarConfig,
    target: MonitorTarget,
    width: f32,
    first_show: bool,
) {
    let scale = scale_for_bar(bar, target);
    let mut width_px = (width * scale).round().max(1.0) as i32;
    let mut height_px = (config.height * scale).round().max(1.0) as i32;
    let radius_px = (config.radius * scale).round().max(0.0) as i32;
    let offset_px = (config.offset * scale).round().max(0.0) as i32;
    let margin = (SIDE_MARGIN * scale).round() as i32;

    let taskbar = if config.position == 2 {
        taskbar_slot().filter(|slot| slot_matches_target(*slot, target))
    } else {
        None
    };
    let (x, y) = if let Some(taskbar) = taskbar {
        let slot_width = (taskbar.slot.right - taskbar.slot.left).max(1);
        let slot_height = (taskbar.slot.bottom - taskbar.slot.top).max(1);
        width_px = width_px.min(slot_width).max(1);
        height_px = height_px.min(slot_height).max(1);
        // 任务栏模式始终贴近图标区，避免状态条与图标之间出现一大块空白。
        let x = taskbar.slot.right - width_px;
        let y = taskbar.slot.top + ((slot_height - height_px) / 2).max(0);
        (x, y)
    } else {
        // 任务栏不可用（竖向任务栏、自动隐藏、副屏等）时回退到底部贴边。
        let dock = if config.position == 2 {
            1
        } else {
            config.position
        };
        let area_width = (target.work.right - target.work.left).max(1);
        let x = match config.align {
            0 => target.work.left + margin,
            2 => target.work.right - width_px - margin,
            _ => target.work.left + ((area_width - width_px) / 2).max(0),
        };
        let y = if dock == 1 {
            target.work.bottom - height_px - offset_px
        } else {
            target.work.top + offset_px
        };
        (x, y)
    };

    // 只调用 window.set_size() 不会更新 Slint 根元素的 width，
    // 内容仍会按初始 560px 布局。这里同时更新根元素逻辑尺寸。
    bar.set_window_width(width_px.max(1) as f32 / scale);
    bar.set_window_height(height_px.max(1) as f32 / scale);
    bar.window()
        .set_position(slint::PhysicalPosition::new(x, y));

    #[cfg(windows)]
    {
        if let Some(hwnd) = hwnd_of(bar) {
            make_overlay(hwnd, config.opacity);
            round_corners(hwnd, width_px, height_px, radius_px);
        }
    }

    if first_show {
        let _ = bar.show();
        #[cfg(windows)]
        if let Some(hwnd) = hwnd_of(bar) {
            // 显式用 SW_SHOWNOACTIVATE，避免 show() 后抢焦点。
            unsafe {
                win32::ShowWindow(hwnd, win32::SW_SHOWNOACTIVATE);
            }
        }
    }
}

fn desired_width(
    bar: &StatusBarWindow,
    config: &BarConfig,
    target: MonitorTarget,
    scale: f32,
) -> f32 {
    if !config.auto_width {
        return config.width.clamp(280.0, 1400.0);
    }
    let area_width = (target.work.right - target.work.left).max(1) as f32 / scale;
    let mut max_width = (area_width - SIDE_MARGIN * 2.0).clamp(280.0, 1400.0);
    let mut min_width = 280.0;
    if config.position == 2 {
        if let Some(slot) = taskbar_slot().filter(|slot| slot_matches_target(*slot, target)) {
            let slot_width = (slot.slot.right - slot.slot.left).max(1) as f32 / scale;
            max_width = slot_width.max(120.0);
            min_width = 140.0f32.min(max_width);
        }
    }
    // ceil 避免文字布局在半像素边界被裁掉。
    measure_content_width(bar, config)
        .clamp(min_width, max_width)
        .ceil()
}

fn measure_content_width(bar: &StatusBarWindow, config: &BarConfig) -> f32 {
    let _ = bar;
    measure_content_width_for(config, DISPLAY_MAX.load(Ordering::SeqCst).max(1) as usize)
}

fn measure_content_width_for(config: &BarConfig, max_items: usize) -> f32 {
    let ui = config.scale;
    let font = 11.0 * ui;
    let spacing = 8.0 * ui;
    let mut width = 0.0;
    let mut child_count = 0usize;

    if config.show_brand {
        width += 16.0 * ui;
        width += text_width("Luma", font, true);
        child_count += 2;
    }
    if config.show_clock {
        let clock = if config.clock_seconds {
            "00:00:00"
        } else {
            "00:00"
        };
        width += text_width(clock, font, false);
        child_count += 1;
    }
    if config.show_date {
        width += text_width("00-00 周一", font, false);
        child_count += 1;
    }
    if config.show_battery {
        width += 18.0 * ui + 4.0 * ui + text_width("100%", font, false);
        child_count += 1;
    }
    if !config.custom_text.is_empty() {
        width += text_width(&config.custom_text, font, false);
        child_count += 1;
    }

    let printers = full_printers();
    let visible = printers.len().min(max_items.max(1));
    if child_count > 0 && !printers.is_empty() {
        width += 1.0;
        child_count += 1;
    }
    if visible == 0 {
        width += text_width("未连接", font, false);
        child_count += 1;
    } else {
        // 打印区块等宽分配窗口，自动宽度必须保证最长的一台也放得下。
        let mut widest = 0.0f32;
        for printer in printers.iter().take(visible) {
            widest = widest.max(printer_block_width(&printer, config, font));
        }
        width += widest * visible as f32;
        child_count += visible;
        if printers.len() > visible {
            let overflow = format!("+{}", printers.len() - visible);
            width += text_width(&overflow, font, false);
            child_count += 1;
        }
    }

    width
        + spacing * child_count.saturating_sub(1) as f32
        + 24.0 * ui
        + 2.0
        // 字体度量与 Slint 渲染存在细微差异，留一点余量。
        + 12.0 * ui
}

fn printer_block_width(printer: &crate::BarPrinter, config: &BarConfig, font: f32) -> f32 {
    let ui = config.scale;
    let gap = 5.0 * ui;
    let mut parts = vec![6.0 * ui];
    if config.show_name && !printer.name.is_empty() {
        parts.push(text_width(printer.name.as_str(), font, false));
    }
    if config.show_status && !printer.status.is_empty() {
        parts.push(text_width(printer.status.as_str(), font, false));
    }
    if config.show_progress && printer.progress_known && !printer.progress_text.is_empty() {
        parts.push(text_width(printer.progress_text.as_str(), font, false));
    }
    if config.show_remaining && !printer.remaining.is_empty() {
        parts.push(text_width(printer.remaining.as_str(), font, false));
    }
    parts.iter().sum::<f32>() + gap * parts.len().saturating_sub(1) as f32
}

fn text_width(text: &str, font_px: f32, semibold: bool) -> f32 {
    let mut units = 0.0;
    for ch in text.chars() {
        units += if ch.is_ascii() {
            match ch {
                ' ' => 0.28,
                '.' | ',' | ':' | '%' | '|' | '·' | '-' | '/' => 0.32,
                '0'..='9' => 0.58,
                'a'..='z' => 0.55,
                'A'..='Z' => 0.62,
                _ => 0.45,
            }
        } else {
            // CJK、全角标点与 emoji 按一个 em 估算。
            1.0
        };
    }
    units * font_px * if semibold { 1.04 } else { 1.0 }
}

/// 内容变化后重新贴合宽度。由主线程的 60ms 状态条轮询调用。
pub fn sync_auto_width(bar: &StatusBarWindow, config: &BarConfig) {
    if !config.auto_width {
        return;
    }
    let Some(target) = monitor_target(config.monitor) else {
        return;
    };
    let scale = scale_for_bar(bar, target);
    let desired = desired_width(bar, config, target, scale);
    let desired_px = (desired * scale).round().max(1.0) as i32;
    let current_px = bar.window().size().width as i32;
    if (desired_px - current_px).abs() > 1 {
        let diff = desired_px - current_px;
        let step = (diff as f32 * 0.25).round() as i32;
        let next_px = if diff.abs() <= 4 || step == 0 {
            desired_px
        } else {
            current_px + step
        };
        place_sized(bar, config, target, next_px as f32 / scale, false);
    }
}

#[cfg(windows)]
fn scale_for_bar(bar: &StatusBarWindow, target: MonitorTarget) -> f32 {
    if let Some(hwnd) = hwnd_of(bar) {
        let dpi = unsafe { win32::GetDpiForWindow(hwnd) };
        if dpi > 0 {
            return (dpi as f32 / 96.0).clamp(0.5, 4.0);
        }
    }
    scale_for_monitor(target, bar.window().scale_factor())
}

#[cfg(not(windows))]
fn scale_for_bar(_bar: &StatusBarWindow, target: MonitorTarget) -> f32 {
    scale_for_monitor(target, 1.0)
}

/// 隐藏/恢复原生窗口，不改变配置。用于全屏自动隐藏。
#[cfg(windows)]
pub fn set_native_visible(bar: &StatusBarWindow, visible: bool) {
    if let Some(hwnd) = hwnd_of(bar) {
        unsafe {
            win32::ShowWindow(
                hwnd,
                if visible {
                    win32::SW_SHOWNOACTIVATE
                } else {
                    win32::SW_HIDE
                },
            );
        }
    }
}

#[cfg(not(windows))]
pub fn set_native_visible(bar: &StatusBarWindow, visible: bool) {
    if visible {
        let _ = bar.show();
    } else {
        let _ = bar.hide();
    }
}

/// 原生窗口当前是否可见。用于淡入淡出与外部 apply 的同步。
#[cfg(windows)]
pub fn is_native_visible(bar: &StatusBarWindow) -> bool {
    hwnd_of(bar)
        .map(|hwnd| unsafe { win32::IsWindowVisible(hwnd) != 0 })
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn is_native_visible(bar: &StatusBarWindow) -> bool {
    let _ = bar;
    true
}

/// 直接设置分层窗口 alpha；淡入淡出用，范围 0..=1。
pub fn set_alpha(bar: &StatusBarWindow, alpha: f32) {
    #[cfg(windows)]
    if let Some(hwnd) = hwnd_of(bar) {
        let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        unsafe {
            win32::SetLayeredWindowAttributes(hwnd, 0, alpha, win32::LWA_ALPHA);
        }
    }
    #[cfg(not(windows))]
    let _ = (bar, alpha);
}

/// 鼠标是否位于状态条窗口矩形内。窗口已隐藏时仍使用最后的矩形，
/// 这样悬停隐藏后不会因为“窗口不可见”而立刻恢复、造成闪烁。
#[cfg(windows)]
pub fn cursor_inside_bar(bar: &StatusBarWindow) -> bool {
    let Some(hwnd) = hwnd_of(bar) else {
        return false;
    };
    unsafe {
        let mut point = win32::Point { x: 0, y: 0 };
        if win32::GetCursorPos(&mut point) == 0 {
            return false;
        }
        let mut rect = win32::WinRect::default();
        if win32::GetWindowRect(hwnd, &mut rect) == 0 {
            return false;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 1 || height <= 1 {
            return false;
        }
        point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
    }
}

#[cfg(not(windows))]
pub fn cursor_inside_bar(_bar: &StatusBarWindow) -> bool {
    false
}

/// 前台窗口是否全屏或最大化到状态条所在显示器。
#[cfg(windows)]
pub fn foreground_covers_bar(bar: &StatusBarWindow) -> bool {
    foreground_fills_monitor(bar, true)
}

/// 仅判断真正的全屏（覆盖整个显示器），不把普通最大化算进去。
#[cfg(windows)]
pub fn foreground_is_fullscreen(bar: &StatusBarWindow) -> bool {
    foreground_fills_monitor(bar, false)
}

#[cfg(windows)]
fn foreground_fills_monitor(bar: &StatusBarWindow, include_maximized: bool) -> bool {
    let Some(bar_hwnd) = hwnd_of(bar) else {
        return false;
    };
    unsafe {
        let foreground = win32::GetForegroundWindow();
        if foreground == 0 || foreground == bar_hwnd {
            return false;
        }
        let mut pid = 0u32;
        win32::GetWindowThreadProcessId(foreground, &mut pid);
        if pid == win32::GetCurrentProcessId() {
            return false;
        }
        if include_maximized && win32::IsZoomed(foreground) != 0 {
            return true;
        }

        let monitor = win32::MonitorFromWindow(foreground, win32::MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return false;
        }
        let mut info = win32::MonitorInfo {
            cb_size: std::mem::size_of::<win32::MonitorInfo>() as u32,
            rc_monitor: win32::WinRect::default(),
            rc_work: win32::WinRect::default(),
            flags: 0,
        };
        if win32::GetMonitorInfoW(monitor, &mut info) == 0 {
            return false;
        }
        let mut rect = win32::WinRect::default();
        if win32::GetWindowRect(foreground, &mut rect) == 0 {
            return false;
        }
        rect.left <= info.rc_monitor.left
            && rect.top <= info.rc_monitor.top
            && rect.right >= info.rc_monitor.right
            && rect.bottom >= info.rc_monitor.bottom
    }
}

#[cfg(not(windows))]
pub fn foreground_covers_bar(_bar: &StatusBarWindow) -> bool {
    false
}

#[cfg(not(windows))]
pub fn foreground_is_fullscreen(_bar: &StatusBarWindow) -> bool {
    false
}

/// 把主窗口带到前台。状态条本身是 NOACTIVATE，点击后需要显式激活主窗口。
pub fn activate(window: &slint::Window) {
    #[cfg(windows)]
    {
        if let Some(hwnd) = hwnd_of_window(window) {
            unsafe {
                if win32::IsIconic(hwnd) != 0 {
                    win32::ShowWindow(hwnd, win32::SW_RESTORE);
                } else {
                    win32::ShowWindow(hwnd, win32::SW_SHOW);
                }
                win32::SetForegroundWindow(hwnd);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
}

fn settings_file() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("Luma").join("settings.json"))
}

pub fn load_config(default_visible: bool) -> BarConfig {
    let config = BarConfig {
        visible: default_visible,
        ..BarConfig::default()
    };

    let Some(path) = settings_file() else {
        return config;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return config;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return config;
    };
    parse_config(&value, config).sanitized()
}

fn parse_config(value: &serde_json::Value, mut config: BarConfig) -> BarConfig {
    if let Some(visible) = value.get("status_bar").and_then(serde_json::Value::as_bool) {
        config.visible = visible;
    }
    let Some(bar) = value.get("bar") else {
        return config;
    };
    if let Some(raw) = bar.get("w").and_then(serde_json::Value::as_f64) {
        config.width = raw as f32;
    }
    if let Some(raw) = bar.get("auto_width").and_then(serde_json::Value::as_bool) {
        config.auto_width = raw;
    }
    if let Some(raw) = bar.get("h").and_then(serde_json::Value::as_f64) {
        config.height = raw as f32;
    }
    if let Some(raw) = bar.get("radius").and_then(serde_json::Value::as_f64) {
        config.radius = raw as f32;
    }
    if let Some(raw) = bar.get("opacity").and_then(serde_json::Value::as_f64) {
        config.opacity = raw as f32;
    }
    if let Some(raw) = bar.get("align").and_then(serde_json::Value::as_i64) {
        config.align = raw as i32;
    }
    if let Some(raw) = bar.get("monitor").and_then(serde_json::Value::as_i64) {
        config.monitor = raw as i32;
    }
    if let Some(raw) = bar.get("dock").and_then(serde_json::Value::as_i64) {
        config.position = raw as i32;
    }
    if let Some(raw) = bar.get("offset").and_then(serde_json::Value::as_f64) {
        config.offset = raw as f32;
    }
    if let Some(raw) = bar.get("scale").and_then(serde_json::Value::as_f64) {
        config.scale = raw as f32;
    }
    if let Some(raw) = bar.get("brand").and_then(serde_json::Value::as_bool) {
        config.show_brand = raw;
    }
    if let Some(raw) = bar.get("name").and_then(serde_json::Value::as_bool) {
        config.show_name = raw;
    }
    if let Some(raw) = bar.get("status").and_then(serde_json::Value::as_bool) {
        config.show_status = raw;
    }
    if let Some(raw) = bar.get("progress").and_then(serde_json::Value::as_bool) {
        config.show_progress = raw;
    }
    if let Some(raw) = bar.get("remaining").and_then(serde_json::Value::as_bool) {
        config.show_remaining = raw;
    }
    if let Some(raw) = bar.get("progress_bar").and_then(serde_json::Value::as_bool) {
        config.show_progress_bar = raw;
    }
    if let Some(raw) = bar.get("clock").and_then(serde_json::Value::as_bool) {
        config.show_clock = raw;
    }
    if let Some(raw) = bar
        .get("clock_seconds")
        .and_then(serde_json::Value::as_bool)
    {
        config.clock_seconds = raw;
    }
    if let Some(raw) = bar.get("date").and_then(serde_json::Value::as_bool) {
        config.show_date = raw;
    }
    if let Some(raw) = bar.get("battery").and_then(serde_json::Value::as_bool) {
        config.show_battery = raw;
    }
    if let Some(raw) = bar.get("custom_text").and_then(serde_json::Value::as_str) {
        config.custom_text = raw.to_string();
    }
    if let Some(raw) = bar.get("smart_sort").and_then(serde_json::Value::as_bool) {
        config.smart_sort = raw;
    }
    if let Some(raw) = bar.get("max").and_then(serde_json::Value::as_i64) {
        config.max_printers = raw as i32;
    }
    if let Some(raw) = bar.get("hide_hover").and_then(serde_json::Value::as_bool) {
        config.hide_on_hover = raw;
    }
    if let Some(raw) = bar
        .get("hide_fullscreen")
        .and_then(serde_json::Value::as_bool)
    {
        config.hide_on_fullscreen = raw;
    }
    config
}

pub fn save_config(config: &BarConfig) {
    let config = config.clone().sanitized();
    merge_settings(serde_json::json!({
        "status_bar": config.visible,
        "bar": {
            "auto_width": config.auto_width,
            "w": config.width.round(),
            "h": config.height.round(),
            "radius": config.radius.round(),
            "opacity": config.opacity,
            "align": config.align,
            "monitor": config.monitor,
            "dock": config.position,
            "offset": config.offset.round(),
            "scale": config.scale,
            "brand": config.show_brand,
            "name": config.show_name,
            "status": config.show_status,
            "progress": config.show_progress,
            "remaining": config.show_remaining,
            "progress_bar": config.show_progress_bar,
            "clock": config.show_clock,
            "clock_seconds": config.clock_seconds,
            "date": config.show_date,
            "battery": config.show_battery,
            "custom_text": config.custom_text,
            "smart_sort": config.smart_sort,
            "max": config.max_printers,
            "hide_hover": config.hide_on_hover,
            "hide_fullscreen": config.hide_on_fullscreen,
        }
    }));
}

/// 合并写入 settings.json，避免状态条开关和几何信息互相覆盖。
pub fn merge_settings(patch: serde_json::Value) {
    let Some(path) = settings_file() else {
        return;
    };
    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    if let (Some(target), Some(extra)) = (root.as_object_mut(), patch.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, root.to_string());
}

#[cfg(windows)]
fn hwnd_of(bar: &StatusBarWindow) -> Option<isize> {
    hwnd_of_window(&bar.window())
}

#[cfg(windows)]
fn hwnd_of_window(window: &slint::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
fn monitor_target(prefer_cursor: i32) -> Option<MonitorTarget> {
    unsafe {
        let mut point = win32::Point { x: 0, y: 0 };
        let monitor = if prefer_cursor == 1 && win32::GetCursorPos(&mut point) != 0 {
            win32::MonitorFromPoint(point, win32::MONITOR_DEFAULTTONEAREST)
        } else {
            win32::MonitorFromPoint(win32::Point { x: 0, y: 0 }, win32::MONITOR_DEFAULTTOPRIMARY)
        };
        if monitor.is_null() {
            return None;
        }
        let mut info = win32::MonitorInfo {
            cb_size: std::mem::size_of::<win32::MonitorInfo>() as u32,
            rc_monitor: win32::WinRect::default(),
            rc_work: win32::WinRect::default(),
            flags: 0,
        };
        if win32::GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        Some(MonitorTarget {
            work: info.rc_work,
            handle: monitor as isize,
        })
    }
}

#[cfg(windows)]
fn scale_for_monitor(target: MonitorTarget, fallback: f32) -> f32 {
    unsafe {
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let ok = win32::GetDpiForMonitor(
            target.handle as *mut std::ffi::c_void,
            win32::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        );
        if ok == 0 && dpi_x > 0 {
            (dpi_x as f32 / 96.0).clamp(0.5, 4.0)
        } else {
            fallback
        }
    }
}

#[cfg(not(windows))]
fn monitor_target(_prefer_cursor: i32) -> Option<MonitorTarget> {
    None
}

#[cfg(not(windows))]
fn scale_for_monitor(_target: MonitorTarget, fallback: f32) -> f32 {
    fallback
}

/// 无边框 + 工具窗口 + 不激活 + 置顶 + 点击穿透。
#[cfg(windows)]
fn make_overlay(hwnd: isize, opacity: f32) {
    unsafe {
        // Slint 的 no-frame 在这个窗口上没有稳定生效，直接按 Win32 去掉标题栏。
        let style = win32::GetWindowLongPtrW(hwnd, win32::GWL_STYLE);
        let frameless = (style & !(win32::WS_CAPTION | win32::WS_THICKFRAME)) | win32::WS_POPUP;
        if frameless != style {
            win32::SetWindowLongPtrW(hwnd, win32::GWL_STYLE, frameless);
        }

        let current = win32::GetWindowLongPtrW(hwnd, win32::GWL_EXSTYLE);
        let wanted = (current
            | win32::WS_EX_TOOLWINDOW
            | win32::WS_EX_NOACTIVATE
            | win32::WS_EX_LAYERED
            | win32::WS_EX_TRANSPARENT)
            & !win32::WS_EX_APPWINDOW;
        if wanted != current {
            win32::SetWindowLongPtrW(hwnd, win32::GWL_EXSTYLE, wanted);
        }

        let alpha = (opacity.clamp(MIN_OPACITY, 1.0) * 255.0).round() as u8;
        win32::SetLayeredWindowAttributes(hwnd, 0, alpha, win32::LWA_ALPHA);

        win32::SetWindowPos(
            hwnd,
            win32::HWND_TOPMOST,
            0,
            0,
            0,
            0,
            win32::SWP_NOSIZE | win32::SWP_NOMOVE | win32::SWP_NOACTIVATE | win32::SWP_FRAMECHANGED,
        );
    }
}

/// 用窗口 Region 把矩形窗口裁成圆角，避免依赖透明渲染。
#[cfg(windows)]
fn round_corners(hwnd: isize, width: i32, height: i32, radius: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    unsafe {
        if radius <= 0 {
            win32::SetWindowRgn(hwnd, std::ptr::null_mut(), 1);
            return;
        }
        let region = win32::CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2);
        if region.is_null() {
            return;
        }
        if win32::SetWindowRgn(hwnd, region, 1) == 0 {
            win32::DeleteObject(region);
        }
    }
}

#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct WinRect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct MonitorInfo {
        pub cb_size: u32,
        pub rc_monitor: WinRect,
        pub rc_work: WinRect,
        pub flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct AppBarData {
        pub cb_size: u32,
        pub h_wnd: isize,
        pub callback_message: u32,
        pub edge: u32,
        pub rect: WinRect,
        pub l_param: isize,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SystemTime {
        pub year: u16,
        pub month: u16,
        pub day_of_week: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
        pub milliseconds: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SystemPowerStatus {
        pub ac_line_status: u8,
        pub battery_flag: u8,
        pub battery_life_percent: u8,
        pub system_status_flag: u8,
        pub battery_life_time: u32,
        pub battery_full_life_time: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        pub fn GetCursorPos(point: *mut Point) -> i32;
        pub fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
        pub fn MonitorFromWindow(hwnd: isize, flags: u32) -> *mut c_void;
        pub fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
        pub fn SetWindowRgn(hwnd: isize, region: *mut c_void, redraw: i32) -> i32;
        pub fn SetLayeredWindowAttributes(
            hwnd: isize,
            color_key: u32,
            alpha: u8,
            flags: u32,
        ) -> i32;
        pub fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        pub fn ShowWindow(hwnd: isize, command: i32) -> i32;
        pub fn SetForegroundWindow(hwnd: isize) -> i32;
        pub fn GetForegroundWindow() -> isize;
        pub fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
        pub fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        pub fn IsZoomed(hwnd: isize) -> i32;
        pub fn IsIconic(hwnd: isize) -> i32;
        pub fn IsWindowVisible(hwnd: isize) -> i32;
        pub fn GetDpiForWindow(hwnd: isize) -> u32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        pub fn CreateRoundRectRgn(
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
            width: i32,
            height: i32,
        ) -> *mut c_void;
        pub fn DeleteObject(object: *mut c_void) -> i32;
    }

    #[link(name = "Shcore")]
    extern "system" {
        pub fn GetDpiForMonitor(
            monitor: *mut c_void,
            dpi_type: u32,
            dpi_x: *mut u32,
            dpi_y: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcessId() -> u32;
        pub fn GetLocalTime(time: *mut SystemTime);
        pub fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        pub fn SHAppBarMessage(message: u32, data: *mut AppBarData) -> usize;
    }

    pub const GWL_EXSTYLE: i32 = -20;
    pub const GWL_STYLE: i32 = -16;
    pub const WS_CAPTION: isize = 0x00C0_0000;
    pub const WS_THICKFRAME: isize = 0x0004_0000;
    pub const WS_POPUP: isize = 0x8000_0000u32 as isize;
    pub const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    pub const WS_EX_TRANSPARENT: isize = 0x0000_0020;
    pub const WS_EX_APPWINDOW: isize = 0x0004_0000;
    pub const WS_EX_NOACTIVATE: isize = 0x0800_0000;
    pub const WS_EX_LAYERED: isize = 0x0008_0000;
    pub const LWA_ALPHA: u32 = 0x0000_0002;
    pub const HWND_TOPMOST: isize = -1;
    pub const SWP_NOSIZE: u32 = 0x0001;
    pub const SWP_NOMOVE: u32 = 0x0002;
    pub const SWP_NOACTIVATE: u32 = 0x0010;
    pub const SWP_FRAMECHANGED: u32 = 0x0020;
    pub const MONITOR_DEFAULTTOPRIMARY: u32 = 0x0000_0001;
    pub const MONITOR_DEFAULTTONEAREST: u32 = 0x0000_0002;
    pub const MDT_EFFECTIVE_DPI: u32 = 0;
    pub const ABM_GETSTATE: u32 = 4;
    pub const ABS_AUTOHIDE: usize = 0x1;
    pub const BATTERY_FLAG_CHARGING: u8 = 0x08;
    pub const BATTERY_FLAG_NO_BATTERY: u8 = 0x80;
    pub const SW_HIDE: i32 = 0;
    pub const SW_SHOWNOACTIVATE: i32 = 4;
    pub const SW_SHOW: i32 = 5;
    pub const SW_RESTORE: i32 = 9;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_customization_ranges() {
        let config = BarConfig {
            width: 10.0,
            height: 200.0,
            radius: 99.0,
            opacity: 0.0,
            offset: 999.0,
            scale: 9.0,
            max_printers: 42,
            hide_on_hover: false,
            monitor: 5,
            position: 8,
            align: 8,
            ..BarConfig::default()
        }
        .sanitized();
        assert_eq!(config.width, 280.0);
        assert_eq!(config.height, 64.0);
        assert_eq!(config.radius, 32.0);
        assert_eq!(config.opacity, MIN_OPACITY);
        assert_eq!(config.offset, 64.0);
        assert_eq!(config.scale, 1.6);
        assert_eq!(config.max_printers, 5);
        assert!(!config.hide_on_hover);
        assert_eq!(config.monitor, 1);
        assert_eq!(config.position, 2);
        assert_eq!(config.align, 2);
    }

    #[test]
    fn parses_legacy_and_new_keys() {
        let value = serde_json::json!({
            "status_bar": false,
            "bar": {
                "auto_width": false,
                "w": 900,
                "h": 48,
                "radius": 18,
                "opacity": 0.7,
                "align": 2,
                "dock": 1,
                "scale": 1.25,
                "brand": false,
                "progress_bar": false,
                "max": 4,
                "clock": false,
                "clock_seconds": true,
                "date": true,
                "battery": true,
                "custom_text": "  书房 · 23°C  ",
                "hide_hover": false,
                "hide_fullscreen": true
            }
        });
        let config = parse_config(&value, BarConfig::default()).sanitized();
        assert!(!config.visible);
        assert!(!config.auto_width);
        assert_eq!(config.width, 900.0);
        assert_eq!(config.height, 48.0);
        assert_eq!(config.radius, 18.0);
        assert_eq!(config.opacity, 0.7);
        assert_eq!(config.align, 2);
        assert_eq!(config.position, 1);
        assert_eq!(config.scale, 1.25);
        assert!(!config.show_brand);
        assert!(!config.show_progress_bar);
        assert!(!config.show_clock);
        assert!(config.clock_seconds);
        assert!(config.show_date);
        assert!(config.show_battery);
        assert_eq!(config.custom_text, "书房 · 23°C");
        assert_eq!(config.max_printers, 4);
        assert!(!config.hide_on_hover);
        assert!(config.hide_on_fullscreen);
    }

    #[test]
    fn hover_hide_defaults_on() {
        let config = parse_config(&serde_json::json!({}), BarConfig::default()).sanitized();
        assert!(config.auto_width);
        assert!(config.hide_on_hover);
        assert!(config.show_clock);
        assert!(!config.show_date);
        assert!(!config.show_battery);
        assert!(config.custom_text.is_empty());
    }
}
