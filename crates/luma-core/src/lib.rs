//! Luma 核心业务层。
//!
//! 约束：
//! - 不访问网络、不访问数据库、不依赖 UI 框架。
//! - 状态只由明确的事件（报告、连接变化）驱动。
//! - 单台设备失败不影响其他设备。

pub mod aggregator;
pub mod error;
pub mod manager;
pub mod provider;

pub use aggregator::ReportAggregator;
pub use error::CoreError;
pub use manager::PrinterManager;
pub use provider::{PrinterProvider, ProviderError, ProviderEvent};
