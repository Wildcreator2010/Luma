//! 多打印机管理器。
//!
//! 从第一天起按多设备设计：每台打印机独立保存聚合后的原始状态与投影状态。

use std::collections::BTreeMap;

use luma_model::{DeviceInfo, PrinterState, PrinterStatus};
use serde_json::Value;

use crate::aggregator::ReportAggregator;
use crate::error::CoreError;

#[derive(Debug)]
struct ManagedPrinter {
    info: DeviceInfo,
    state: PrinterState,
    aggregator: ReportAggregator,
}

#[derive(Debug, Default)]
pub struct PrinterManager {
    printers: BTreeMap<String, ManagedPrinter>,
}

impl PrinterManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 新增或更新设备基础信息，保留已有运行状态。
    pub fn upsert_device(&mut self, info: DeviceInfo) {
        match self.printers.get_mut(&info.serial) {
            Some(existing) => {
                if info.name.is_some() {
                    existing.info.name = info.name.clone();
                    existing.state.name = info.name;
                }
                existing.info.model = info.model.clone();
                existing.state.model = info.model;
                existing.info.region = info.region;
            }
            None => {
                let mut state = PrinterState::new(&info.serial, info.model.clone());
                state.name = info.name.clone();
                state.online = info.online;
                state.status = if info.online {
                    PrinterStatus::Unknown
                } else {
                    PrinterStatus::Offline
                };
                let serial = info.serial.clone();
                self.printers.insert(
                    serial,
                    ManagedPrinter {
                        info,
                        state,
                        aggregator: ReportAggregator::new(),
                    },
                );
            }
        }
    }

    pub fn remove_device(&mut self, serial: &str) -> bool {
        self.printers.remove(serial).is_some()
    }

    pub fn rename(&mut self, serial: &str, name: Option<String>) -> Result<(), CoreError> {
        let printer = self
            .printers
            .get_mut(serial)
            .ok_or_else(|| CoreError::UnknownPrinter(serial.to_string()))?;
        printer.info.name = name.clone();
        printer.state.name = name;
        Ok(())
    }

    /// 更新在线状态。离线时状态置为 `Offline`，恢复在线时回到 `Unknown`，
    /// 直到收到新的有效报告。
    pub fn set_online(&mut self, serial: &str, online: bool) -> Result<(), CoreError> {
        let printer = self
            .printers
            .get_mut(serial)
            .ok_or_else(|| CoreError::UnknownPrinter(serial.to_string()))?;
        printer.state.online = online;
        printer.state.status = if online {
            PrinterStatus::Unknown
        } else {
            PrinterStatus::Offline
        };
        Ok(())
    }

    /// 连接已建立但尚未收到该设备报告：在线状态未知，不做“在线”假设。
    pub fn set_connection_pending(&mut self, serial: &str) -> Result<(), CoreError> {
        let printer = self
            .printers
            .get_mut(serial)
            .ok_or_else(|| CoreError::UnknownPrinter(serial.to_string()))?;
        if printer.state.status != PrinterStatus::Offline {
            printer.state.online = false;
            printer.state.status = PrinterStatus::Unknown;
        }
        Ok(())
    }

    /// 应用一条增量报告。未知序列号会被拒绝，避免状态串台。
    pub fn apply_report(
        &mut self,
        serial: &str,
        patch: &Value,
        received_at_ms: i64,
    ) -> Result<bool, CoreError> {
        let printer = self
            .printers
            .get_mut(serial)
            .ok_or_else(|| CoreError::UnknownPrinter(serial.to_string()))?;
        let changed = printer.aggregator.apply(patch, received_at_ms)?;
        printer.state.received_at = Some(received_at_ms);
        Ok(changed)
    }

    pub fn state(&self, serial: &str) -> Option<&PrinterState> {
        self.printers.get(serial).map(|p| &p.state)
    }

    /// 聚合后的原始状态树，供协议投影层读取。
    pub fn merged_report(&self, serial: &str) -> Option<&serde_json::Value> {
        self.printers.get(serial).map(|p| p.aggregator.merged())
    }

    pub fn device_info(&self, serial: &str) -> Option<&DeviceInfo> {
        self.printers.get(serial).map(|p| &p.info)
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.printers.values().map(|p| p.info.clone()).collect()
    }

    /// 用投影后的完整状态替换单台设备状态。
    pub fn set_state(&mut self, state: PrinterState) -> Result<(), CoreError> {
        let printer = self
            .printers
            .get_mut(&state.serial)
            .ok_or_else(|| CoreError::UnknownPrinter(state.serial.clone()))?;
        printer.state = state;
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<PrinterState> {
        self.printers.values().map(|p| p.state.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.printers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.printers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luma_model::{PrinterModel, Region};
    use serde_json::json;

    fn device(serial: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            serial: serial.to_string(),
            name: Some(name.to_string()),
            model: PrinterModel::X2D,
            region: Region::ChinaMainland,
            online: true,
        }
    }

    #[test]
    fn devices_are_independent() {
        let mut mgr = PrinterManager::new();
        mgr.upsert_device(device("A", "一号"));
        mgr.upsert_device(device("B", "二号"));

        mgr.apply_report("A", &json!({"print": {"progress": 40}}), 100)
            .unwrap();
        mgr.set_online("B", true).unwrap();

        assert_eq!(mgr.len(), 2);
        assert_eq!(mgr.state("A").unwrap().received_at, Some(100));
        assert_eq!(mgr.state("A").unwrap().status, PrinterStatus::Unknown);
        assert!(mgr.state("B").unwrap().online);
    }

    #[test]
    fn unknown_serial_report_is_rejected() {
        let mut mgr = PrinterManager::new();
        let err = mgr
            .apply_report("missing", &json!({"a": 1}), 1)
            .unwrap_err();
        assert!(matches!(err, CoreError::UnknownPrinter(_)));
    }

    #[test]
    fn offline_overrides_status() {
        let mut mgr = PrinterManager::new();
        mgr.upsert_device(device("A", "一号"));
        mgr.set_online("A", true).unwrap();
        mgr.set_online("A", false).unwrap();
        assert_eq!(mgr.state("A").unwrap().status, PrinterStatus::Offline);
    }

    #[test]
    fn upsert_preserves_existing_state() {
        let mut mgr = PrinterManager::new();
        mgr.upsert_device(device("A", "一号"));
        mgr.apply_report("A", &json!({"x": 1}), 50).unwrap();
        mgr.upsert_device(device("A", "新名字"));

        let state = mgr.state("A").unwrap();
        assert_eq!(state.received_at, Some(50));
        assert_eq!(state.display_name(), "新名字");
    }

    #[test]
    fn set_state_replaces_projection() {
        let mut mgr = PrinterManager::new();
        mgr.upsert_device(device("A", "一号"));
        let mut state = mgr.state("A").unwrap().clone();
        state.status = PrinterStatus::Printing;
        state.progress = Some(42.0);
        mgr.set_state(state).unwrap();
        assert_eq!(mgr.state("A").unwrap().progress, Some(42.0));
    }

    #[test]
    fn connection_pending_is_unknown_not_online() {
        let mut mgr = PrinterManager::new();
        mgr.upsert_device(device("A", "一号"));
        mgr.set_connection_pending("A").unwrap();
        let state = mgr.state("A").unwrap();
        assert!(!state.online);
        assert_eq!(state.status, PrinterStatus::Unknown);
    }
}
