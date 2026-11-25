# Luma Data Model

版本：0.1.0（Spark）  
状态：领域模型已实现；协议字段映射待验证

## 1. 原则

1. 未知就是 `None`，禁止用 `0`、`0%`、`0°C` 代替。
2. 离线（Offline）、未知（Unknown）、空闲（Idle）、打印中（Printing）、
   烘干（Drying）、暂停（Paused）、异常（Failed）语义独立。
3. 时间统一为 Unix 毫秒（UTC）。
4. 设备上报时间与本地接收时间分开保存。
5. 协议字段只出现在映射层；UI 与固件只消费 `PrinterState`。
6. 未验证的协议字段不得写入映射代码。

## 2. 类型

```rust
pub enum PrinterModel {
    X2D, X1C, X1E, P1S, P1P, A1, A1Mini, H2D,
    Other(String),
}

pub enum PrinterStatus {
    Unknown, Offline, Idle, Preparing, Printing, Drying,
    Paused, Finished, Failed, Maintenance,
}

pub struct DeviceInfo {
    pub serial: String,
    pub name: Option<String>,
    pub model: PrinterModel,
    pub region: Region,
    pub online: bool,       // bind 快照，仅作初值
}

pub struct PrinterState {
    pub serial: String,
    pub name: Option<String>,
    pub model: PrinterModel,
    pub online: bool,
    pub status: PrinterStatus,
    pub progress: Option<f32>,          // 0.0..=100.0
    pub remaining_seconds: Option<u32>,
    pub nozzle_temperature: Option<f32>,
    pub nozzle_target: Option<f32>,
    pub bed_temperature: Option<f32>,
    pub bed_target: Option<f32>,
    pub current_layer: Option<u32>,
    pub total_layers: Option<u32>,
    pub task_name: Option<String>,
    pub last_updated: Option<i64>,      // 设备时间
    pub received_at: Option<i64>,       // 本地时间
}
```

## 3. 字段语义

| 字段 | 语义 | 未知时 |
|---|---|---|
| `online` | 连接是否可用 | `false` |
| `status` | 运行状态 | `Unknown` |
| `progress` | 当前任务完成百分比 | `None` |
| `remaining_seconds` | 预计剩余秒数 | `None` |
| `nozzle_temperature` | 当前喷嘴温度 | `None` |
| `nozzle_target` | 目标喷嘴温度 | `None` |
| `bed_temperature` | 当前热床温度 | `None` |
| `bed_target` | 目标热床温度 | `None` |
| `current_layer` | 当前层 | `None` |
| `total_layers` | 总层数 | `None` |
| `task_name` | 当前任务名 | `None` |
| `last_updated` | 设备上报时间 | `None` |
| `received_at` | 本地接收时间 | `None` |

### 状态转换

- 连接断开：`online = false`，`status = Offline`。
- 恢复连接：`online = true`，`status = Unknown`，直到收到有效报告。
- 收到报告：由映射层决定具体状态。
- 数据陈旧：由 `received_at` 判断，不改变状态本身。

陈旧阈值建议默认 30 秒，可配置。`PrinterState::is_stale(now_ms, threshold_ms)` 已实现。

## 4. 增量合并

MQTT 报告可能是增量 JSON。当前实现：

- object + object：递归合并，未出现的键保留。
- 数组：若两侧都是对象数组，且每个元素都有唯一的 `id`（或 `node`），
  则按该键合并；新 id 追加，未提及的 id 保留。
- 其他数组（原始值、`hms` 等无稳定键的数组）：整体替换。
- 标量/`null`：整体替换。

**待验证**：`null` 究竟表示删除字段还是值置空。当前按"替换为 null"实现，
相关测试已明确标注 `pending_verification`。真实报文确认后必须修改规则与测试。

**待验证**：增量数组中"某个 id 不再出现"是删除还是未更新。当前按未更新处理，
保留旧元素。需要删除时依赖完整 `pushall` 覆盖，或用存在性位图校正。

合并结果保存在 `ReportAggregator`，并维护：

- `revision`：内容变化次数。
- `last_received_at`：最近一次收到报文的时间（即使内容未变）。

## 5. 协议映射（候选，尚未进入代码）

以下路径来自社区逆向与 X2D 公开抓包（详细调研属于内部资料，不随公开仓库发布），
**尚未用真实账号 + 真实 X2D 验证**。
Phase 1 只能作为候选实现，并必须在真实报文到达后修正。

| PrinterState | 候选路径 | 转换 | 可信度 |
|---|---|---|---|
| progress | `print.mc_percent` | int → f32，限定 0..=100 | B |
| remaining_seconds | `print.mc_remaining_time` | 分钟 × 60 → u32 | B |
| nozzle_temperature | `print.device.extruder.info[active].temp` 低 16 位 | 否则回退 `print.nozzle_temper` | B |
| nozzle_target | 同上高 16 位 | 否则回退 `print.nozzle_target_temper` | B |
| bed_temperature | `print.bed_temper` | f32 | B |
| bed_target | `print.bed_target_temper` | f32 | B |
| current_layer | `print.layer_num` | u32 | B |
| total_layers | `print.total_layer_num` | u32 | B |
| task_name | `print.subtask_name`，回退 `gcode_file` | string，空串视为 None | B |
| status | `print.gcode_state` | 见下表 | B |
| last_updated | 未找到可靠字段 | 保持 None | D |

补充：

- `mc_remaining_time` 的单位是**分钟**，不是秒。
- 顶层 `chamber_temper` 在 X2D 样例中缺失；腔体温度可读
  `print.device.ctc.info.temp`（打包值）。
- `hms` 可能是数组也可能是对象，不能假定单一类型。
- `failure reason` / `print_error` 的完整枚举未找到，状态先归入 `Failed`，保留原始码到日志。

### 状态映射（候选）

| `gcode_state` | `PrinterStatus` |
|---|---|
| `IDLE` | `Idle` |
| `PREPARE` | `Preparing` |
| `RUNNING` | `Printing` |
| `PAUSE` | `Paused` |
| `FINISH` | `Finished` |
| `FAILED` | `Failed` |
| `INIT` | `Preparing` |
| `SLICING` | `Preparing` |
| `OFFLINE` | `Offline` |
| 其他 / 缺失 | `Unknown` |

连接断开时由连接事件强制 `Offline`，不能只依赖 `gcode_state`。

`ams.ams[].dry_time > 0` 时，若 `gcode_state` 为 `IDLE` / `FINISH` / 未知，
状态投影为 `Drying`；打印、暂停、准备中仍保留打印任务状态。

### X2D 双喷嘴

- `print.device.extruder.state`：低 4 位喷嘴数，接下来 4 位活动喷嘴索引。
- `print.device.extruder.info[]`：每个元素 `id`、`snow`、`temp`、`stat`。
  `temp` 低 16 位当前温度，高 16 位目标温度。
- `print.device.nozzle.info[]`：喷嘴直径/类型/序列号等。
- 顶层 `nozzle_temper` 只代表活动喷嘴，双喷嘴展示必须优先使用数组。
- 数组按 `id` 合并；缺失元素保留旧值；`null` 表示未知，不表示 0。

X2D 与旧型号的字段差异必须单独确认，不能默认沿用 X1/P1 的映射。

## 6. 版本兼容

为固件预留：

- 快照可增加 `schema_version`。
- 字段只增不删；删除字段需升级主版本。
- 固件解析未知字段时忽略，不得报错。
- 可选字段用 `Option`，不用哨兵值。

## 7. 固件侧约束

- 共享 crate 只依赖 serde。
- 避免大字符串与深层嵌套；屏幕只需要展示字段。
- 时间戳用 `i64`，屏幕上按本地时区格式化。
- 传输格式优先 JSON（协议一致）；后续可评估更紧凑的二进制编码，但不得破坏桌面端一致性。

