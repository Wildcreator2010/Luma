//! 拓竹中国大陆云端接入。
//!
//! 模块：
//! - `auth`：登录、设备列表、uid 解析
//! - `token`：Token 模型与 Windows Credential Manager 存储
//! - `mqtt`：TLS MQTT 订阅与重连
//!
//! 协议细节来自公开社区资料（详细调研未随公开仓库发布），真实账号与 X2D 联调
//! 仍是必须的验收步骤。日志中禁止输出 Token。

pub mod auth;
pub mod config;
pub mod error;
pub mod mqtt;
pub mod token;

pub use auth::{AccountProfile, BambuAuth, LoginOutcome};
pub use config::BambuCloudConfig;
pub use error::CloudError;
pub use mqtt::BambuCloudProvider;
pub use token::{delete_token, load_token, store_token, AuthToken};

/// rustls 同时被 reqwest/rumqttc 引入，特性可能包含多个 provider。
/// 首次 TLS 前显式安装，避免 “Could not automatically determine CryptoProvider” panic。
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
