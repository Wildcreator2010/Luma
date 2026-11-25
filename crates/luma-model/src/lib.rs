//! Luma 共享领域模型。
//!
//! 本 crate 只包含数据类型与纯函数，不做任何 I/O，不依赖 tokio / sqlx / slint。
//! 桌面端与 ESP32-S3 固件共用这一层，固件侧可关闭 `std` 依赖（后续按需拆分 feature）。
//!
//! 设计规则：
//! - 未知数据一律使用 `Option::None`，禁止用 0 代替未知。
//! - 离线、未知、空闲、打印中、暂停、异常是不同状态，不可合并。
//! - 时间统一使用 Unix 毫秒（UTC），不存本地时区字符串。

use serde::{Deserialize, Serialize};

/// 打印机型号。
///
/// 未知型号保留原始字符串，避免协议升级后丢失信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrinterModel {
    X2D,
    X1C,
    X1E,
    P1S,
    P1P,
    P2S,
    A1,
    A1Mini,
    H2D,
    Other(String),
}

impl PrinterModel {
    /// 从云端返回的型号字符串构造。大小写与分隔符差异由映射层处理后传入。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "X2D" | "N6" => Self::X2D,
            "X1C" | "X1-CARBON" | "X1 CARBON" | "BL-P001" => Self::X1C,
            "X1E" => Self::X1E,
            "P1S" | "C12" => Self::P1S,
            "P1P" | "C11" => Self::P1P,
            "P2S" | "N7" => Self::P2S,
            "A1" => Self::A1,
            "A1MINI" | "A1-MINI" | "A1 MINI" => Self::A1Mini,
            "H2D" | "O1D" => Self::H2D,
            _ => Self::Other(raw.to_string()),
        }
    }

    pub fn as_canonical_str(&self) -> &str {
        match self {
            Self::X2D => "X2D",
            Self::X1C => "X1C",
            Self::X1E => "X1E",
            Self::P1S => "P1S",
            Self::P1P => "P1P",
            Self::P2S => "P2S",
            Self::A1 => "A1",
            Self::A1Mini => "A1 mini",
            Self::H2D => "H2D",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for PrinterModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_canonical_str())
    }
}

impl Default for PrinterModel {
    fn default() -> Self {
        Self::Other("unknown".to_string())
    }
}

/// 打印机运行状态。
///
/// `Unknown` 表示"从未收到有效状态"或"无法判定"，
/// `Offline` 表示"已知设备不可达"，两者语义不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrinterStatus {
    #[default]
    Unknown,
    Offline,
    Idle,
    Preparing,
    Printing,
    /// AMS 正在烘干耗材（`ams[].dry_time > 0`）。
    Drying,
    Paused,
    Finished,
    Failed,
    Maintenance,
}

impl PrinterStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Printing | Self::Drying | Self::Paused
        )
    }
}

/// 云端区域。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    #[default]
    ChinaMainland,
    Other(String),
}

impl Region {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ChinaMainland => "china_mainland",
            Self::Other(s) => s.as_str(),
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "china_mainland" | "cn" => Self::ChinaMainland,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 设备基础信息（来自账号设备列表）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub serial: String,
    pub name: Option<String>,
    pub model: PrinterModel,
    pub region: Region,
    /// 云端设备列表中的在线快照；仅作初值，最终以 MQTT 报告为准。
    pub online: bool,
}

/// 单台打印机的完整状态快照。
///
/// 该结构是 UI 与固件的唯一展示输入。协议增量先合并为原始 JSON，
/// 再由映射层投影为这个结构；禁止把 MQTT 增量直接反序列化覆盖进来。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrinterState {
    pub serial: String,
    pub name: Option<String>,
    pub model: PrinterModel,

    pub online: bool,
    pub status: PrinterStatus,

    /// 0.0 ..= 100.0，未知为 None。
    pub progress: Option<f32>,
    pub remaining_seconds: Option<u32>,

    pub nozzle_temperature: Option<f32>,
    pub nozzle_target: Option<f32>,
    pub bed_temperature: Option<f32>,
    pub bed_target: Option<f32>,

    pub current_layer: Option<u32>,
    pub total_layers: Option<u32>,
    pub task_name: Option<String>,

    /// 设备上报时间（Unix 毫秒）。来源未验证前保持 None。
    pub last_updated: Option<i64>,
    /// 客户端接收时间（Unix 毫秒），用于判断数据陈旧。
    pub received_at: Option<i64>,
}

impl PrinterState {
    pub fn new(serial: impl Into<String>, model: PrinterModel) -> Self {
        Self {
            serial: serial.into(),
            name: None,
            model,
            online: false,
            status: PrinterStatus::Unknown,
            progress: None,
            remaining_seconds: None,
            nozzle_temperature: None,
            nozzle_target: None,
            bed_temperature: None,
            bed_target: None,
            current_layer: None,
            total_layers: None,
            task_name: None,
            last_updated: None,
            received_at: None,
        }
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.serial)
    }

    /// 数据是否超过阈值未更新。
    pub fn is_stale(&self, now_ms: i64, threshold_ms: i64) -> bool {
        match self.received_at {
            Some(at) => now_ms.saturating_sub(at) > threshold_ms,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_are_none_not_zero() {
        let state = PrinterState::new("SERIAL", PrinterModel::X2D);
        assert_eq!(state.progress, None);
        assert_eq!(state.nozzle_temperature, None);
        assert_eq!(state.remaining_seconds, None);
        assert_eq!(state.status, PrinterStatus::Unknown);
    }

    #[test]
    fn display_name_falls_back_to_serial() {
        let mut state = PrinterState::new("01S00C123", PrinterModel::X2D);
        assert_eq!(state.display_name(), "01S00C123");
        state.name = Some("车间一号".into());
        assert_eq!(state.display_name(), "车间一号");
    }

    #[test]
    fn staleness_requires_received_at() {
        let mut state = PrinterState::new("SERIAL", PrinterModel::X2D);
        assert!(state.is_stale(1_000, 500));
        state.received_at = Some(800);
        assert!(!state.is_stale(1_000, 500));
        assert!(state.is_stale(1_600, 500));
    }

    #[test]
    fn model_parse_keeps_unknown_string() {
        assert_eq!(PrinterModel::parse("X2D"), PrinterModel::X2D);
        assert_eq!(PrinterModel::parse("N6"), PrinterModel::X2D);
        assert_eq!(PrinterModel::parse("O1D"), PrinterModel::H2D);
        assert_eq!(
            PrinterModel::parse("SomeNewModel"),
            PrinterModel::Other("SomeNewModel".into())
        );
    }
}
