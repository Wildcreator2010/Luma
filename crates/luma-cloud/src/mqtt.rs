use std::time::Duration;

use async_trait::async_trait;
use luma_core::{PrinterProvider, ProviderError, ProviderEvent};
use luma_model::DeviceInfo;
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, MqttOptions, Packet, QoS,
    SubscribeReasonCode, Transport,
};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::BambuCloudConfig;
use crate::error::CloudError;
use crate::token::{now_ms, AuthToken};

const PUSHALL_REQUEST: &str =
    r#"{"pushing":{"sequence_id":"0","command":"pushall","version":1,"push_target":1}}"#;

pub struct BambuCloudProvider {
    config: BambuCloudConfig,
    token: AuthToken,
    devices: Vec<DeviceInfo>,
    uid: String,
}

impl BambuCloudProvider {
    pub async fn connect(
        config: BambuCloudConfig,
        token: AuthToken,
        devices: Vec<DeviceInfo>,
        uid: String,
    ) -> Result<Self, CloudError> {
        crate::ensure_crypto_provider();
        if token.is_expired(now_ms()) {
            return Err(CloudError::TokenExpired);
        }
        if devices.is_empty() {
            return Err(CloudError::NoDevices);
        }
        Ok(Self {
            config,
            token,
            devices,
            uid,
        })
    }

    pub fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    fn mqtt_options(&self) -> MqttOptions {
        let mut options = MqttOptions::new(
            format!("luma-{}", Uuid::new_v4()),
            &self.config.mqtt_host,
            self.config.mqtt_tls_port,
        );
        options.set_credentials(format!("u_{}", self.uid), self.token.access_token.clone());
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);
        options.set_inflight(16);
        // rumqttc 默认只允许 10 KiB 入站包；X2D 的 pushall 夹具约 21 KiB。
        options.set_max_packet_size(4 * 1024 * 1024, 64 * 1024);
        options.set_transport(Transport::tls_with_default_config());
        options
    }
}

#[async_trait]
impl PrinterProvider for BambuCloudProvider {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>, ProviderError> {
        Ok(self.devices.clone())
    }

    async fn run(&self, events: mpsc::Sender<ProviderEvent>) -> Result<(), ProviderError> {
        let mut backoff_secs = 1u64;

        loop {
            let request_capacity = (self.devices.len() * 2 + 16).max(64);
            let (client, mut eventloop) = AsyncClient::new(self.mqtt_options(), request_capacity);
            let mut pending_subacks = 0usize;
            let mut successful_subacks = 0usize;

            loop {
                let poll_result = if pending_subacks > 0 {
                    match tokio::time::timeout(Duration::from_secs(10), eventloop.poll()).await {
                        Ok(result) => result,
                        Err(_) => {
                            let _ = events
                                .send(ProviderEvent::Error {
                                    message: "等待 MQTT SUBACK 超时".to_string(),
                                    retryable: true,
                                })
                                .await;
                            break;
                        }
                    }
                } else {
                    eventloop.poll().await
                };

                match poll_result {
                    Ok(Event::Incoming(Packet::ConnAck(connack))) => {
                        if connack.code != ConnectReturnCode::Success {
                            let message = format!("MQTT CONNACK {:?}", connack.code);
                            let _ = events
                                .send(ProviderEvent::Error {
                                    message: message.clone(),
                                    retryable: false,
                                })
                                .await;
                            return Err(ProviderError::Auth(message));
                        }

                        pending_subacks = self.devices.len();
                        successful_subacks = 0;

                        for device in &self.devices {
                            let report_topic = format!("device/{}/report", device.serial);
                            if let Err(error) =
                                client.subscribe(report_topic, QoS::AtMostOnce).await
                            {
                                tracing::warn!(
                                    serial = %device.serial,
                                    %error,
                                    "failed to subscribe"
                                );
                                pending_subacks = pending_subacks.saturating_sub(1);
                                continue;
                            }
                        }

                        if pending_subacks == 0 {
                            let _ = events
                                .send(ProviderEvent::Error {
                                    message: "没有成功订阅任何设备".to_string(),
                                    retryable: true,
                                })
                                .await;
                            break;
                        }
                    }
                    Ok(Event::Incoming(Packet::SubAck(suback))) => {
                        let all_ok = suback
                            .return_codes
                            .iter()
                            .all(|code| matches!(code, SubscribeReasonCode::Success(_)));
                        if !all_ok {
                            let _ = events
                                .send(ProviderEvent::Error {
                                    message: "MQTT 订阅被拒绝（SUBACK failure）".to_string(),
                                    retryable: false,
                                })
                                .await;
                        } else {
                            successful_subacks += 1;
                        }

                        pending_subacks = pending_subacks.saturating_sub(1);
                        if pending_subacks == 0 {
                            if successful_subacks == 0 {
                                let _ = events
                                    .send(ProviderEvent::Error {
                                        message: "没有成功订阅任何设备".to_string(),
                                        retryable: true,
                                    })
                                    .await;
                                break;
                            }

                            let _ = events
                                .send(ProviderEvent::ConnectionChanged {
                                    online: true,
                                    detail: Some("MQTT subscribed".to_string()),
                                })
                                .await;
                            tracing::info!(
                                devices = successful_subacks,
                                "Bambu Cloud MQTT subscribed"
                            );

                            for device in &self.devices {
                                let request_topic = format!("device/{}/request", device.serial);
                                if let Err(error) = client
                                    .publish(
                                        request_topic,
                                        QoS::AtMostOnce,
                                        false,
                                        PUSHALL_REQUEST.as_bytes(),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        serial = %device.serial,
                                        %error,
                                        "failed to request pushall"
                                    );
                                }
                            }
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        let Some(serial) = serial_from_topic(&publish.topic) else {
                            continue;
                        };
                        match serde_json::from_slice::<Value>(&publish.payload) {
                            Ok(payload) => {
                                // 只有真正收到有效上报才重置退避，避免短连接风暴。
                                backoff_secs = 1;
                                let _ = events
                                    .send(ProviderEvent::Report {
                                        serial: serial.to_string(),
                                        payload,
                                        received_at_ms: now_ms(),
                                    })
                                    .await;
                            }
                            Err(error) => {
                                tracing::warn!(%serial, %error, "invalid MQTT JSON");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if let ConnectionError::ConnectionRefused(code) = &error {
                            if matches!(
                                code,
                                ConnectReturnCode::BadUserNamePassword
                                    | ConnectReturnCode::NotAuthorized
                                    | ConnectReturnCode::BadClientId
                                    | ConnectReturnCode::RefusedProtocolVersion
                            ) {
                                let message = format!("MQTT CONNACK {code:?}");
                                let _ = events
                                    .send(ProviderEvent::Error {
                                        message: message.clone(),
                                        retryable: false,
                                    })
                                    .await;
                                return Err(ProviderError::Auth(message));
                            }
                        }
                        tracing::warn!(%error, "MQTT connection error; reconnecting");
                        let _ = events
                            .send(ProviderEvent::ConnectionChanged {
                                online: false,
                                detail: Some(error.to_string()),
                            })
                            .await;
                        break;
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    }
}

pub fn serial_from_topic(topic: &str) -> Option<&str> {
    let rest = topic.strip_prefix("device/")?;
    let (serial, suffix) = rest.split_once('/')?;
    (suffix == "report" && !serial.is_empty()).then_some(serial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use luma_model::{PrinterModel, Region};

    #[test]
    fn parses_device_report_topic() {
        assert_eq!(
            serial_from_topic("device/20P6AJ123456/report"),
            Some("20P6AJ123456")
        );
        assert_eq!(serial_from_topic("device/20P6AJ123456/request"), None);
        assert_eq!(serial_from_topic("device//report"), None);
        assert_eq!(serial_from_topic("other/topic"), None);
    }

    #[tokio::test]
    #[ignore = "requires network; deliberately uses fake credentials"]
    async fn mqtt_rejects_fake_credentials_without_retry_loop() {
        let config = BambuCloudConfig::china_mainland();
        let token = AuthToken::new("fake-token".into(), None, Some(3_600));
        let devices = vec![DeviceInfo {
            serial: "FAKE-SERIAL".into(),
            name: None,
            model: PrinterModel::X2D,
            region: Region::ChinaMainland,
            online: true,
        }];
        let provider = BambuCloudProvider::connect(config, token, devices, "123".into())
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let result = tokio::time::timeout(Duration::from_secs(20), provider.run(tx))
            .await
            .expect("provider should stop on auth failure");
        assert!(
            matches!(&result, Err(ProviderError::Auth(_))),
            "unexpected result: {result:?}"
        );
    }
}
