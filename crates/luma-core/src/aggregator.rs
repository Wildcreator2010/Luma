//! 增量报告聚合器。
//!
//! 拓竹 MQTT 的状态报告可能是增量 JSON：每次只包含发生变化的字段。
//! 因此必须先把增量深合并成"当前完整原始状态"，再投影为 `PrinterState`。
//!
//! 当前合并语义（**待真实报文验证**）：
//! - object + object：递归合并，未出现的键保留。
//! - 数组：元素是带唯一 `id` 或 `node` 的对象时按该键合并；
//!   其他数组（原始值、`hms` 这类无稳定键的数组）整体替换。
//! - 标量 / `null`：整体替换。
//!
//! 注意：null 究竟表示"删除该字段"还是"值置空"，必须在 Phase 0
//! 用真实报文验证后才能定论，不能凭猜测实现。

use serde_json::Value;

use crate::error::CoreError;

#[derive(Debug, Clone)]
pub struct ReportAggregator {
    merged: Value,
    revision: u64,
    last_received_at: Option<i64>,
}

impl ReportAggregator {
    pub fn new() -> Self {
        Self {
            merged: Value::Object(serde_json::Map::new()),
            revision: 0,
            last_received_at: None,
        }
    }

    /// 合并一条增量报告。
    ///
    /// 返回 `true` 表示合并后的数据发生了变化。
    /// 无论是否变化，`last_received_at` 都会更新，因为报文确实到达了。
    pub fn apply(&mut self, patch: &Value, received_at_ms: i64) -> Result<bool, CoreError> {
        if !patch.is_object() {
            return Err(CoreError::InvalidReport(
                "report must be a JSON object".to_string(),
            ));
        }

        let changed = merge_value(&mut self.merged, patch);
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        self.last_received_at = Some(received_at_ms);
        Ok(changed)
    }

    pub fn merged(&self) -> &Value {
        &self.merged
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn last_received_at(&self) -> Option<i64> {
        self.last_received_at
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for ReportAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// 将 `patch` 深合并进 `target`，返回是否产生变化。
pub fn merge_value(target: &mut Value, patch: &Value) -> bool {
    merge_value_at(target, patch, None)
}

fn merge_value_at(target: &mut Value, patch: &Value, key: Option<&str>) -> bool {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            let mut changed = false;
            for (child_key, patch_value) in patch_map {
                match target_map.get_mut(child_key) {
                    Some(target_value) => {
                        changed |= merge_value_at(target_value, patch_value, Some(child_key));
                    }
                    None => {
                        target_map.insert(child_key.clone(), patch_value.clone());
                        changed = true;
                    }
                }
            }
            changed
        }
        (Value::Array(target_items), Value::Array(patch_items)) => {
            let merge_key = key
                .filter(|key| is_id_merge_array(key))
                .and_then(|_| array_merge_key(target_items, patch_items));
            match merge_key {
                Some(merge_key) => merge_arrays_by_key(target_items, patch_items, merge_key),
                None => replace_value(target_items, patch_items),
            }
        }
        (target_value, patch_value) => replace_value(target_value, patch_value),
    }
}

/// 只允许已知对象数组按 id/node 合并；其他数组一律整体替换。
fn is_id_merge_array(key: &str) -> bool {
    matches!(key, "ams" | "tray" | "info" | "vir_slot" | "lights_report")
}

fn replace_value<T: PartialEq + Clone>(target: &mut T, patch: &T) -> bool {
    if target == patch {
        false
    } else {
        *target = patch.clone();
        true
    }
}

/// 找到可用于合并数组的稳定键。
///
/// 仅当两侧数组都非空、每个元素都是对象、键值唯一，且值是字符串或数字时才返回。
/// 这样 `hms`（无 id）与 `mapping`（原始值数组）仍然整体替换。
fn array_merge_key(target: &[Value], patch: &[Value]) -> Option<&'static str> {
    if target.is_empty() || patch.is_empty() {
        return None;
    }

    ["id", "node"].into_iter().find(|key| {
        array_has_unique_object_key(target, key) && array_has_unique_object_key(patch, key)
    })
}

fn array_has_unique_object_key(items: &[Value], key: &str) -> bool {
    let mut seen: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Some(value) = item.get(key).and_then(normalized_key) else {
            return false;
        };
        if seen.contains(&value) {
            return false;
        }
        seen.push(value);
    }
    true
}

fn normalized_key(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn merge_arrays_by_key(target: &mut Vec<Value>, patch: &[Value], key: &str) -> bool {
    let mut changed = false;
    for patch_item in patch {
        let Some(patch_key) = patch_item.get(key).and_then(normalized_key) else {
            continue;
        };
        let existing = target.iter_mut().find(|target_item| {
            target_item.get(key).and_then(normalized_key).as_deref() == Some(patch_key.as_str())
        });
        match existing {
            Some(target_item) => {
                changed |= merge_value_at(target_item, patch_item, None);
            }
            None => {
                target.push(patch_item.clone());
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_merge_preserves_siblings() {
        let mut agg = ReportAggregator::new();
        agg.apply(&json!({"print": {"a": 1, "b": 2}}), 100).unwrap();
        let changed = agg.apply(&json!({"print": {"b": 30}}), 200).unwrap();

        assert!(changed);
        assert_eq!(agg.merged(), &json!({"print": {"a": 1, "b": 30}}));
        assert_eq!(agg.revision(), 2);
        assert_eq!(agg.last_received_at(), Some(200));
    }

    #[test]
    fn arrays_are_replaced_whole() {
        let mut agg = ReportAggregator::new();
        agg.apply(&json!({"ams": {"tray": [1, 2, 3]}}), 1).unwrap();
        agg.apply(&json!({"ams": {"tray": [9]}}), 2).unwrap();
        assert_eq!(agg.merged(), &json!({"ams": {"tray": [9]}}));
    }

    #[test]
    fn arrays_with_id_merge_by_id() {
        let mut agg = ReportAggregator::new();
        agg.apply(
            &json!({
                "print": {
                    "device": {
                        "extruder": {
                            "info": [
                                {"id": 0, "temp": 50},
                                {"id": 1, "temp": 16384250}
                            ]
                        }
                    }
                }
            }),
            1,
        )
        .unwrap();

        agg.apply(
            &json!({
                "print": {
                    "device": {
                        "extruder": {
                            "info": [
                                {"id": 1, "temp": 16000000}
                            ]
                        }
                    }
                }
            }),
            2,
        )
        .unwrap();

        let info = agg.merged()["print"]["device"]["extruder"]["info"]
            .as_array()
            .unwrap();
        assert_eq!(info.len(), 2);
        assert_eq!(info[0]["temp"], json!(50));
        assert_eq!(info[1]["temp"], json!(16000000));
    }

    #[test]
    fn tray_array_merges_by_id_and_appends_new() {
        let mut agg = ReportAggregator::new();
        agg.apply(
            &json!({"print": {"ams": {"ams": [{"id": "0", "tray": [
                {"id": "0", "remain": 90},
                {"id": "1", "remain": 80}
            ]}]}}}),
            1,
        )
        .unwrap();

        agg.apply(
            &json!({"print": {"ams": {"ams": [{"id": "0", "tray": [
                {"id": "1", "remain": 70},
                {"id": "2", "remain": 60}
            ]}]}}}),
            2,
        )
        .unwrap();

        let trays = agg.merged()["print"]["ams"]["ams"][0]["tray"]
            .as_array()
            .unwrap();
        assert_eq!(trays.len(), 3);
        assert_eq!(trays[0]["remain"], json!(90));
        assert_eq!(trays[1]["remain"], json!(70));
        assert_eq!(trays[2]["remain"], json!(60));
    }

    #[test]
    fn arrays_without_stable_key_are_replaced() {
        let mut agg = ReportAggregator::new();
        agg.apply(&json!({"print": {"hms": [{"code": 1}, {"code": 2}]}}), 1)
            .unwrap();
        agg.apply(&json!({"print": {"hms": [{"code": 3}]}}), 2)
            .unwrap();
        assert_eq!(agg.merged(), &json!({"print": {"hms": [{"code": 3}]}}));
    }

    #[test]
    fn arrays_outside_whitelist_are_replaced_even_with_id() {
        let mut agg = ReportAggregator::new();
        agg.apply(
            &json!({"print": {"settings": [{"id": 1, "v": 1}, {"id": 2, "v": 2}]}}),
            1,
        )
        .unwrap();
        agg.apply(&json!({"print": {"settings": [{"id": 1, "v": 9}]}}), 2)
            .unwrap();
        assert_eq!(
            agg.merged(),
            &json!({"print": {"settings": [{"id": 1, "v": 9}]}})
        );
    }

    #[test]
    fn numeric_and_string_ids_are_normalized() {
        let mut agg = ReportAggregator::new();
        agg.apply(
            &json!({"print": {"device": {"extruder": {"info": [
                {"id": 1, "temp": 100}
            ]}}}}),
            1,
        )
        .unwrap();
        agg.apply(
            &json!({"print": {"device": {"extruder": {"info": [
                {"id": "1", "temp": 200}
            ]}}}}),
            2,
        )
        .unwrap();

        let info = agg.merged()["print"]["device"]["extruder"]["info"]
            .as_array()
            .unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0]["temp"], json!(200));
    }

    #[test]
    fn null_replaces_value_pending_verification() {
        let mut agg = ReportAggregator::new();
        agg.apply(&json!({"print": {"progress": 42}}), 1).unwrap();
        agg.apply(&json!({"print": {"progress": null}}), 2).unwrap();
        assert_eq!(agg.merged(), &json!({"print": {"progress": null}}));
    }

    #[test]
    fn identical_patch_does_not_bump_revision_but_updates_time() {
        let mut agg = ReportAggregator::new();
        agg.apply(&json!({"a": 1}), 100).unwrap();
        let changed = agg.apply(&json!({"a": 1}), 300).unwrap();
        assert!(!changed);
        assert_eq!(agg.revision(), 1);
        assert_eq!(agg.last_received_at(), Some(300));
    }

    #[test]
    fn non_object_report_is_rejected() {
        let mut agg = ReportAggregator::new();
        assert!(agg.apply(&json!([1, 2]), 1).is_err());
        assert!(agg.apply(&json!("text"), 1).is_err());
        assert_eq!(agg.revision(), 0);
    }
}
