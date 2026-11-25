//! X2D 报文合并回归测试。
//!
//! `x2d_delta_synthetic.json` 是人工构造数据，只验证合并规则，不作为协议证据。
//! `x2d_pushall.json` 来自公开的 ha-bambulab MOCK-X2D.json（已脱敏）。

use luma_core::ReportAggregator;
use serde_json::Value;

#[test]
fn synthetic_x2d_deltas_match_expected_merge() {
    let raw = include_str!("../../../tests/fixtures/x2d_delta_synthetic.json");
    let fixture: Value = serde_json::from_str(raw).expect("fixture must be valid JSON");

    let mut agg = ReportAggregator::new();
    agg.apply(&fixture["base"], 1).expect("base report");

    for (index, delta) in fixture["deltas"]
        .as_array()
        .expect("deltas array")
        .iter()
        .enumerate()
    {
        agg.apply(delta, 100 + index as i64).expect("delta report");
    }

    assert_eq!(agg.merged(), &fixture["expected_merged"]);
}

#[test]
fn x2d_pushall_survives_partial_extruder_delta() {
    let raw = include_str!("../../../tests/fixtures/x2d_pushall.json");
    let payload: Value = serde_json::from_str(raw).expect("pushall fixture must be valid JSON");

    let mut agg = ReportAggregator::new();
    agg.apply(&payload, 1).expect("pushall report");

    let delta = serde_json::json!({
        "pushall": {
            "print": {
                "device": {
                    "extruder": {
                        "info": [
                            { "id": 1, "temp": 16000000 }
                        ]
                    }
                }
            }
        }
    });
    agg.apply(&delta, 2).expect("partial delta");

    let info = agg.merged()["pushall"]["print"]["device"]["extruder"]["info"]
        .as_array()
        .expect("extruder info array");
    assert_eq!(info.len(), 2, "id=0 must not be dropped");
    assert_eq!(info[0]["id"], 0);
    assert_eq!(info[1]["temp"], 16000000);
}
