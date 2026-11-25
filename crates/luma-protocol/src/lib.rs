//! Bambu Lab 协议投影层。
//!
//! 输入：已经完成深合并的原始 `print` JSON 树。
//! 输出：桌面端/固件共用的 `PrinterState`。
//!
//! 这里只做纯函数，不做 I/O。字段来源见内部协议调研（不随公开仓库发布），
//! 全部标记为社区逆向，真实账号 + 真实 X2D 联调前需保留 Unknown 兜底。

use luma_model::{DeviceInfo, PrinterState, PrinterStatus};
use serde_json::Value;

/// 从 MQTT payload 中取状态块。
///
/// 支持：
/// - `{"print": {...}}`：普通 `push_status`
/// - `{"pushall": {"print": {...}}}`：`pushall` 响应
pub fn extract_print(payload: &Value) -> Option<&Value> {
    payload.get("print").or_else(|| {
        payload
            .get("pushall")
            .and_then(|pushall| pushall.get("print"))
    })
}

/// 投影完整状态。
pub fn project_x2d(info: &DeviceInfo, print: &Value, received_at_ms: i64) -> PrinterState {
    let mut state = PrinterState::new(&info.serial, info.model.clone());
    state.name = info.name.clone();
    state.online = true;
    state.received_at = Some(received_at_ms);

    let gcode_status = status_from_gcode_state(string_at(print, &["gcode_state"]));
    state.status = gcode_status;
    if is_failed(print) {
        state.status = PrinterStatus::Failed;
    } else if is_drying(print)
        && matches!(
            gcode_status,
            PrinterStatus::Idle | PrinterStatus::Finished | PrinterStatus::Unknown
        )
    {
        // AMS 烘干不改变 gcode_state；空闲/完成/未知时用烘干覆盖展示状态。
        // 打印/暂停/准备中仍以打印任务为主，避免隐藏进度。
        state.status = PrinterStatus::Drying;
    }

    state.progress =
        number_at(print, &["mc_percent"]).filter(|value| (0.0..=100.0).contains(value));
    state.remaining_seconds = number_at(print, &["mc_remaining_time"])
        .filter(|value| *value >= 0.0)
        .and_then(|minutes| {
            let seconds = (minutes * 60.0).round();
            (seconds.is_finite() && seconds <= u32::MAX as f32).then_some(seconds as u32)
        });

    let (nozzle, nozzle_target) = nozzle_temperatures(print);
    state.nozzle_temperature = nozzle;
    state.nozzle_target = nozzle_target;
    let (bed, bed_target) = bed_temperatures(print);
    state.bed_temperature = bed;
    state.bed_target = bed_target;

    state.current_layer = integer_at(print, &["layer_num"]).and_then(|v| u32::try_from(v).ok());
    state.total_layers = integer_at(print, &["total_layer_num"])
        .filter(|value| *value > 0)
        .and_then(|v| u32::try_from(v).ok());
    state.task_name = string_at(print, &["subtask_name"])
        .filter(|s| !s.is_empty())
        .or_else(|| string_at(print, &["gcode_file"]).filter(|s| !s.is_empty()))
        .map(ToOwned::to_owned);

    state
}

pub fn project_payload(
    info: &DeviceInfo,
    payload: &Value,
    received_at_ms: i64,
) -> Option<PrinterState> {
    extract_print(payload).map(|print| project_x2d(info, print, received_at_ms))
}

pub fn status_from_gcode_state(raw: Option<&str>) -> PrinterStatus {
    let Some(raw) = raw else {
        return PrinterStatus::Unknown;
    };
    match raw.trim().to_ascii_uppercase().as_str() {
        "IDLE" => PrinterStatus::Idle,
        "PREPARE" => PrinterStatus::Preparing,
        "RUNNING" => PrinterStatus::Printing,
        "PAUSE" => PrinterStatus::Paused,
        "FINISH" => PrinterStatus::Finished,
        "FAILED" => PrinterStatus::Failed,
        "SLICING" => PrinterStatus::Preparing,
        "OFFLINE" => PrinterStatus::Offline,
        _ => PrinterStatus::Unknown,
    }
}

fn is_failed(print: &Value) -> bool {
    let error_code = integer_at(print, &["print_error"]).unwrap_or(0);
    let fail_reason = string_at(print, &["fail_reason"]).unwrap_or("");
    error_code != 0 || (!fail_reason.is_empty() && fail_reason != "0")
}

/// AMS 是否正在烘干。
///
/// X2D 的 AMS 2 Pro 在 `print.ams.ams[]` 下上报 `dry_time`（剩余分钟）。
/// 空闲夹具中为 `0`，ha-bambulab 也以 `dry_time > 0` 判定 `is_drying`。
fn is_drying(print: &Value) -> bool {
    print
        .get("ams")
        .and_then(|ams| ams.get("ams"))
        .and_then(Value::as_array)
        .map(|units| {
            units
                .iter()
                .any(|unit| number_at(unit, &["dry_time"]).is_some_and(|minutes| minutes > 0.0))
        })
        .unwrap_or(false)
}

/// 双喷嘴：优先读取 `device.extruder.info[active]`。
///
/// `temp` 是打包值：低 16 位当前温度，高 16 位目标温度。
/// 顶层 `nozzle_temper` 只代表活动喷嘴，仅在缺少 `device.extruder` 时回退。
fn nozzle_temperatures(print: &Value) -> (Option<f32>, Option<f32>) {
    let extruder = print
        .get("device")
        .and_then(|device| device.get("extruder"));

    if let Some(extruder) = extruder {
        if let Some(info) = extruder.get("info").and_then(Value::as_array) {
            let active = extruder
                .get("state")
                .and_then(Value::as_u64)
                .map(|state| (state >> 4) & 0xF)
                .unwrap_or(0);

            let selected = info
                .iter()
                .find(|item| item.get("id").and_then(Value::as_u64) == Some(active))
                .or_else(|| info.first());

            if let Some(raw) = selected
                .and_then(|item| item.get("temp"))
                .and_then(Value::as_u64)
            {
                let current = (raw & 0xFFFF) as f32;
                let target = ((raw >> 16) & 0xFFFF) as f32;
                return (Some(current), Some(target));
            }
        }
    }

    (
        number_at(print, &["nozzle_temper"]),
        number_at(print, &["nozzle_target_temper"]),
    )
}

fn bed_temperatures(print: &Value) -> (Option<f32>, Option<f32>) {
    if let Some(raw) = print
        .get("device")
        .and_then(|device| device.get("bed"))
        .and_then(|bed| bed.get("info"))
        .and_then(|info| info.get("temp"))
        .and_then(Value::as_u64)
    {
        let current = (raw & 0xFFFF) as f32;
        let target = ((raw >> 16) & 0xFFFF) as f32;
        return (Some(current), Some(target));
    }

    (
        number_at(print, &["bed_temper"]),
        number_at(print, &["bed_target_temper"]),
    )
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn number_at(value: &Value, path: &[&str]) -> Option<f32> {
    value_at(value, path).and_then(|v| {
        v.as_f64()
            .map(|n| n as f32)
            .or_else(|| v.as_str().and_then(|s| s.parse::<f32>().ok()))
    })
}

fn integer_at(value: &Value, path: &[&str]) -> Option<i64> {
    value_at(value, path).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    value_at(value, path).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use luma_model::{PrinterModel, Region};
    use serde_json::json;

    fn info() -> DeviceInfo {
        DeviceInfo {
            serial: "TEST-X2D".into(),
            name: Some("测试机".into()),
            model: PrinterModel::X2D,
            region: Region::ChinaMainland,
            online: true,
        }
    }

    #[test]
    fn status_mapping_has_unknown_fallback() {
        assert_eq!(
            status_from_gcode_state(Some("RUNNING")),
            PrinterStatus::Printing
        );
        assert_eq!(
            status_from_gcode_state(Some("FINISH")),
            PrinterStatus::Finished
        );
        assert_eq!(
            status_from_gcode_state(Some("NEW_STATE")),
            PrinterStatus::Unknown
        );
        assert_eq!(
            status_from_gcode_state(Some("running")),
            PrinterStatus::Printing
        );
        assert_eq!(
            status_from_gcode_state(Some("INIT")),
            PrinterStatus::Unknown
        );
        assert_eq!(status_from_gcode_state(None), PrinterStatus::Unknown);
    }

    #[test]
    fn packed_temperatures_and_active_nozzle() {
        let print = json!({
            "gcode_state": "RUNNING",
            "nozzle_temper": 10,
            "nozzle_target_temper": 0,
            "device": {
                "extruder": {
                    "state": 33042,
                    "info": [
                        { "id": 0, "temp": 50 },
                        { "id": 1, "temp": 16384250 }
                    ]
                }
            }
        });
        let state = project_x2d(&info(), &print, 1);
        assert_eq!(state.nozzle_temperature, Some(250.0));
        assert_eq!(state.nozzle_target, Some(250.0));
    }

    #[test]
    fn missing_extruder_falls_back_to_top_level() {
        let print = json!({
            "gcode_state": "RUNNING",
            "nozzle_temper": 214.5,
            "nozzle_target_temper": 215
        });
        let state = project_x2d(&info(), &print, 1);
        assert_eq!(state.nozzle_temperature, Some(214.5));
        assert_eq!(state.nozzle_target, Some(215.0));
    }

    #[test]
    fn remaining_minutes_convert_to_seconds() {
        let print = json!({"mc_remaining_time": 120, "mc_percent": 25});
        let state = project_x2d(&info(), &print, 1);
        assert_eq!(state.remaining_seconds, Some(7200));
        assert_eq!(state.progress, Some(25.0));
    }

    #[test]
    fn invalid_progress_and_zero_total_layers_are_none() {
        let print = json!({"mc_percent": 101, "total_layer_num": 0});
        let state = project_x2d(&info(), &print, 1);
        assert_eq!(state.progress, None);
        assert_eq!(state.total_layers, None);
    }

    #[test]
    fn packed_bed_temperature_is_preferred() {
        let print = json!({
            "bed_temper": 1,
            "bed_target_temper": 1,
            "device": { "bed": { "info": { "temp": 4587590 } } }
        });
        let state = project_x2d(&info(), &print, 1);
        assert_eq!(state.bed_temperature, Some(70.0));
        assert_eq!(state.bed_target, Some(70.0));
    }

    #[test]
    fn empty_task_name_is_none() {
        let print = json!({"subtask_name": "", "gcode_file": ""});
        assert_eq!(project_x2d(&info(), &print, 1).task_name, None);
    }

    #[test]
    fn failure_overrides_status() {
        let print = json!({"gcode_state": "RUNNING", "print_error": 83935248});
        assert_eq!(
            project_x2d(&info(), &print, 1).status,
            PrinterStatus::Failed
        );
    }

    #[test]
    fn ams_drying_replaces_idle_status() {
        let print = json!({
            "gcode_state": "IDLE",
            "ams": { "ams": [{ "id": "0", "dry_time": 87 }] }
        });
        assert_eq!(
            project_x2d(&info(), &print, 1).status,
            PrinterStatus::Drying
        );
    }

    #[test]
    fn ams_drying_does_not_hide_printing() {
        let print = json!({
            "gcode_state": "RUNNING",
            "ams": { "ams": [{ "id": "0", "dry_time": 87 }] }
        });
        assert_eq!(
            project_x2d(&info(), &print, 1).status,
            PrinterStatus::Printing
        );
    }

    #[test]
    fn ams_dry_setting_alone_is_not_drying() {
        let print = json!({
            "gcode_state": "IDLE",
            "ams": {
                "ams": [{
                    "id": "0",
                    "dry_time": 0,
                    "dry_setting": {
                        "dry_duration": 60,
                        "dry_temperature": 65,
                        "dry_filament": "PLA"
                    }
                }]
            }
        });
        assert_eq!(project_x2d(&info(), &print, 1).status, PrinterStatus::Idle);
    }

    #[test]
    fn pushes_and_pushall_envelopes_are_supported() {
        let print = json!({"gcode_state": "IDLE"});
        assert_eq!(
            extract_print(&json!({"print": print.clone()})),
            Some(&print)
        );
        assert_eq!(
            extract_print(&json!({"pushall": {"print": print.clone()}})),
            Some(&print)
        );
    }

    #[test]
    fn projects_public_pushall_fixture() {
        let raw = include_str!("../../../tests/fixtures/x2d_pushall.json");
        let payload: Value = serde_json::from_str(raw).unwrap();
        let state = project_payload(&info(), &payload, 123).unwrap();

        assert_eq!(state.status, PrinterStatus::Printing);
        assert_eq!(state.progress, Some(98.0));
        assert_eq!(state.remaining_seconds, Some(120));
        assert_eq!(state.current_layer, Some(129));
        assert_eq!(state.total_layers, Some(140));
        assert_eq!(state.nozzle_temperature, Some(250.0));
        assert_eq!(state.nozzle_target, Some(250.0));
        assert_eq!(state.bed_temperature, Some(70.0));
        assert_eq!(state.received_at, Some(123));
        assert!(state.task_name.is_some());
    }

    #[test]
    fn projects_synthetic_expected_merge() {
        let raw = include_str!("../../../tests/fixtures/x2d_delta_synthetic.json");
        let fixture: Value = serde_json::from_str(raw).unwrap();
        let print = &fixture["expected_merged"]["print"];
        let state = project_x2d(&info(), print, 456);

        assert_eq!(state.status, PrinterStatus::Printing);
        assert_eq!(state.progress, Some(25.0));
        assert_eq!(state.remaining_seconds, Some(7200));
        assert_eq!(state.nozzle_temperature, Some(255.0));
        assert_eq!(state.nozzle_target, Some(255.0));
        assert_eq!(state.current_layer, Some(6));
        assert_eq!(state.total_layers, Some(200));
        assert_eq!(state.task_name, None);
    }
}
