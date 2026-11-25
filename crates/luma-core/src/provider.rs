//! 数据提供者抽象。
//!
//! 第一实现为 `BambuCloudProvider`（中国大陆云端）。
//! 未来 LAN 模式实现同一 trait，上层 `PrinterManager` 不需要改动。

use async_trait::async_trait;
use luma_model::DeviceInfo;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

/// 提供者向上层发出的事件。
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    DevicesDiscovered(Vec<DeviceInfo>),
    Report {
        serial: String,
        payload: Value,
        received_at_ms: i64,
    },
    ConnectionChanged {
        online: bool,
        detail: Option<String>,
    },
    Error {
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("not implemented yet")]
    NotImplemented,
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("transport error: {0}")]
    Transport(String),
}

#[async_trait]
pub trait PrinterProvider: Send + Sync {
    /// 获取账号下的设备列表。
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>, ProviderError>;

    /// 启动长连接并持续发送事件，直到出错或被取消。
    async fn run(&self, events: mpsc::Sender<ProviderEvent>) -> Result<(), ProviderError>;
}
