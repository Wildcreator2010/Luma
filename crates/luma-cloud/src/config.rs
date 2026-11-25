use luma_model::Region;

/// 中国大陆区 HTTP 入口。
///
/// 来源：项目需求文档（用户提供）。
/// 状态：**未验证**。Phase 0 必须用真实请求确认域名、路径与认证方式。
pub const CN_HTTP_BASE: &str = "https://api.bambulab.cn";

/// 中国大陆区 MQTT Broker。
///
/// 来源：项目需求文档（用户提供）。
/// 状态：**未验证**。
pub const CN_MQTT_HOST: &str = "cn.mqtt.bambulab.com";

/// MQTT over TLS 端口。
///
/// 来源：项目需求文档（用户提供）。
/// 状态：**未验证**。
pub const CN_MQTT_TLS_PORT: u16 = 8883;

#[derive(Debug, Clone)]
pub struct BambuCloudConfig {
    pub region: Region,
    pub http_base: String,
    pub mqtt_host: String,
    pub mqtt_tls_port: u16,
}

impl BambuCloudConfig {
    pub fn china_mainland() -> Self {
        Self {
            region: Region::ChinaMainland,
            http_base: CN_HTTP_BASE.to_string(),
            mqtt_host: CN_MQTT_HOST.to_string(),
            mqtt_tls_port: CN_MQTT_TLS_PORT,
        }
    }
}

impl Default for BambuCloudConfig {
    fn default() -> Self {
        Self::china_mainland()
    }
}
