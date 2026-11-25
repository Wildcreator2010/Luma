use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use luma_model::{DeviceInfo, PrinterModel};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::BambuCloudConfig;
use crate::error::CloudError;
use crate::token::{now_ms, AuthToken};

#[derive(Clone)]
pub enum LoginOutcome {
    Success(AuthToken),
    VerifyCode { account: String },
    Tfa { tfa_key: String },
}

/// 登录请求的上下文。
///
/// 服务端的 `code` 语义只在验证码流程里有定义（`1` 过期、`2` 错误，
/// 见内部协议调研）。密码流程不能套用同一套映射：
/// 例如 `{"code":1,"error":"Enter a valid phone number."}` 是参数错误。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoginContext {
    Password,
    Code,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    login_type: Option<String>,
    tfa_key: Option<String>,
}

#[derive(Deserialize)]
struct BindResponse {
    devices: Option<Vec<BindDevice>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountProfile {
    pub uid: Option<String>,
    pub name: Option<String>,
    pub handle: Option<String>,
}

impl AccountProfile {
    pub fn display_name(&self) -> String {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                self.handle
                    .as_deref()
                    .map(str::trim)
                    .filter(|handle| !handle.is_empty())
            })
            .unwrap_or("Bambu Cloud")
            .to_string()
    }

    pub fn profile_url(&self) -> String {
        self.handle
            .as_deref()
            .map(str::trim)
            .filter(|handle| !handle.is_empty())
            .map(|handle| format!("https://makerworld.com/zh-cn/@{handle}"))
            .unwrap_or_else(|| "https://bambulab.cn/zh-cn/my".to_string())
    }
}

#[derive(Deserialize)]
struct BindDevice {
    dev_id: Option<String>,
    name: Option<String>,
    dev_model_name: Option<String>,
    dev_product_name: Option<String>,
    online: Option<bool>,
    dev_access_code: Option<String>,
}

pub struct BambuAuth {
    client: Client,
    config: BambuCloudConfig,
}

impl BambuAuth {
    pub fn new(config: BambuCloudConfig) -> Result<Self, CloudError> {
        crate::ensure_crypto_provider();
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("bambu_network_agent/02.09.01.54"),
        );
        headers.insert(
            "x-bbl-agent-version",
            HeaderValue::from_static("02.09.01.54"),
        );
        headers.insert("x-bbl-client-name", HeaderValue::from_static("BambuStudio"));
        headers.insert(
            "x-bbl-client-version",
            HeaderValue::from_static("02.09.01.70"),
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(25))
            .build()?;
        Ok(Self { client, config })
    }

    pub fn config(&self) -> &BambuCloudConfig {
        &self.config
    }

    pub async fn login_password(
        &self,
        account: &str,
        password: &str,
    ) -> Result<LoginOutcome, CloudError> {
        let url = format!("{}/v1/user-service/user/login", self.config.http_base);
        let body = self
            .send_login(
                &url,
                json!({ "account": account, "password": password, "apiError": "" }),
                LoginContext::Password,
            )
            .await?;
        self.login_outcome(body, account)
    }

    pub async fn login_code(&self, account: &str, code: &str) -> Result<AuthToken, CloudError> {
        let url = format!("{}/v1/user-service/user/login", self.config.http_base);
        let body = self
            .send_login(
                &url,
                json!({ "account": account, "code": code }),
                LoginContext::Code,
            )
            .await?;
        match self.login_outcome(body, account)? {
            LoginOutcome::Success(token) => Ok(token),
            LoginOutcome::VerifyCode { .. } => Err(CloudError::VerifyCodeRequired {
                account: account.to_string(),
            }),
            LoginOutcome::Tfa { tfa_key } => Err(CloudError::TfaRequired { tfa_key }),
        }
    }

    pub async fn send_email_code(&self, email: &str) -> Result<(), CloudError> {
        let url = format!(
            "{}/v1/user-service/user/sendemail/code",
            self.config.http_base
        );
        self.post_json(&url, json!({ "email": email, "type": "codeLogin" }), None)
            .await?;
        Ok(())
    }

    pub async fn send_sms_code(&self, phone: &str) -> Result<(), CloudError> {
        // 端点存在（假号码返回 400 参数错误），但真实号码返回 418 + captchaId：
        // CN 的短信发送受 Geetest 保护，端侧无法完成。桌面端应引导用户走
        // 浏览器登录。网页域名 bambulab.cn/api/... 另受 CSRF 保护，不能兜底。
        let url = format!("{}/v1/user-service/user/sendsmscode", self.config.http_base);
        self.post_json(&url, json!({ "phone": phone, "type": "codeLogin" }), None)
            .await?;
        Ok(())
    }

    pub async fn fetch_devices(&self, token: &AuthToken) -> Result<Vec<DeviceInfo>, CloudError> {
        let url = format!("{}/v1/iot-service/api/user/bind", self.config.http_base);
        let body = self.get_json(&url, Some(&token.access_token)).await?;
        let response: BindResponse = serde_json::from_value(body)?;
        let devices = response
            .devices
            .ok_or_else(|| CloudError::Protocol("bind response missing devices".to_string()))?;

        let mut result = Vec::with_capacity(devices.len());
        for device in devices {
            let Some(serial) = device
                .dev_id
                .clone()
                .filter(|serial| !serial.trim().is_empty())
            else {
                tracing::warn!("bind response contains a device without dev_id");
                continue;
            };
            let model = model_from_device(&device);
            result.push(DeviceInfo {
                serial,
                name: device.name.filter(|name| !name.trim().is_empty()),
                model,
                region: self.config.region.clone(),
                online: device.online.unwrap_or(false),
            });
        }

        if result.is_empty() {
            return Err(CloudError::NoDevices);
        }
        Ok(result)
    }

    /// 读取每台设备的局域网访问码，供本机摄像头取流使用。
    ///
    /// 返回值只包含非空访问码；调用方负责把它留在内存里，不能落盘或写日志。
    pub async fn fetch_device_access_codes(
        &self,
        token: &AuthToken,
    ) -> Result<HashMap<String, String>, CloudError> {
        let url = format!("{}/v1/iot-service/api/user/bind", self.config.http_base);
        let body = self.get_json(&url, Some(&token.access_token)).await?;
        let response: BindResponse = serde_json::from_value(body)?;
        let devices = response
            .devices
            .ok_or_else(|| CloudError::Protocol("bind response missing devices".to_string()))?;

        let mut result = HashMap::new();
        for device in devices {
            let (Some(serial), Some(code)) = (device.dev_id, device.dev_access_code) else {
                continue;
            };
            let serial = serial.trim().to_string();
            let code = code.trim().to_string();
            if serial.is_empty() || code.is_empty() {
                continue;
            }
            result.insert(serial, code);
        }
        Ok(result)
    }

    pub async fn resolve_uid(&self, token: &AuthToken) -> Result<String, CloudError> {
        if let Some(uid) = &token.uid {
            return Ok(uid.clone());
        }
        if let Some(uid) = uid_from_jwt(&token.access_token) {
            return Ok(uid);
        }

        let url = format!(
            "{}/v1/design-user-service/my/preference",
            self.config.http_base
        );
        let body = self.get_json(&url, Some(&token.access_token)).await?;
        find_uid(&body).ok_or_else(|| CloudError::Protocol("uid not found".to_string()))
    }

    /// 读取网页账号的显示名称，用于 UI。失败时只是回退到通用名称。
    pub async fn fetch_account_name(&self, token: &AuthToken) -> Option<String> {
        let url = format!(
            "{}/v1/design-user-service/my/preference",
            self.config.http_base
        );
        let body = self.get_json(&url, Some(&token.access_token)).await.ok()?;
        account_name_from_preference(&body)
    }

    pub async fn fetch_account_profile(
        &self,
        token: &AuthToken,
    ) -> Result<AccountProfile, CloudError> {
        let url = format!(
            "{}/v1/design-user-service/my/preference",
            self.config.http_base
        );
        let body = self.get_json(&url, Some(&token.access_token)).await?;
        Ok(AccountProfile {
            uid: find_uid(&body),
            name: profile_text(&body, "name"),
            handle: profile_text(&body, "handle"),
        })
    }

    /// 用已有 token 直接获取设备列表并解析 uid。
    pub async fn prepare_session(
        &self,
        mut token: AuthToken,
    ) -> Result<(AuthToken, Vec<DeviceInfo>, String), CloudError> {
        if token.is_expired(now_ms()) {
            return Err(CloudError::TokenExpired);
        }
        let devices = self.fetch_devices(&token).await?;
        let uid = self.resolve_uid(&token).await?;
        token.uid = Some(uid.clone());
        Ok((token, devices, uid))
    }

    fn login_outcome(&self, body: Value, account: &str) -> Result<LoginOutcome, CloudError> {
        let response: LoginResponse = serde_json::from_value(body)?;
        match response.login_type.as_deref() {
            Some("verifyCode") => Ok(LoginOutcome::VerifyCode {
                account: account.to_string(),
            }),
            Some("tfa") => Ok(LoginOutcome::Tfa {
                tfa_key: response.tfa_key.unwrap_or_default(),
            }),
            _ => match response.access_token {
                Some(access_token) => {
                    let mut token =
                        AuthToken::new(access_token, response.refresh_token, response.expires_in);
                    token.uid = uid_from_jwt(&token.access_token);
                    Ok(LoginOutcome::Success(token))
                }
                None => Err(CloudError::Protocol(
                    "login response missing accessToken".to_string(),
                )),
            },
        }
    }

    async fn get_json(&self, url: &str, token: Option<&str>) -> Result<Value, CloudError> {
        let mut request = self.client.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        self.send_json(request).await
    }

    async fn post_json(
        &self,
        url: &str,
        body: Value,
        token: Option<&str>,
    ) -> Result<Value, CloudError> {
        let mut request = self.client.post(url).json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        self.send_json(request).await
    }

    /// 登录专用请求：无论状态码，都解析响应体里的结构化字段。
    ///
    /// 服务端可能用 `400` 表达“需要验证码/需要二次验证”，也可能用
    /// `418`（或带 `captchaId` 的任意状态）表达 Geetest 风控。只有
    /// 统一解析才能把这些状态区分开，而不是一律显示“密码错误”。
    async fn send_login(
        &self,
        url: &str,
        body: Value,
        context: LoginContext,
    ) -> Result<Value, CloudError> {
        let response = self.client.post(url).json(&body).send().await?;
        let status = response.status();
        let value = parse_body(&response.text().await?);

        if let Some(captcha_id) = find_captcha_id(&value) {
            return Err(CloudError::CaptchaRequired {
                captcha_id: Some(captcha_id),
            });
        }
        if status.as_u16() == 418 || message_mentions_captcha(&value) {
            return Err(CloudError::CaptchaRequired { captcha_id: None });
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(CloudError::NotAuthenticated);
        }

        // 400 也可能带着 loginType/accessToken：这类响应要继续走状态机。
        let has_login_state = value
            .get("loginType")
            .and_then(Value::as_str)
            .map(|kind| !kind.is_empty())
            .unwrap_or(false)
            || value.get("accessToken").and_then(Value::as_str).is_some()
            || value.get("access_token").and_then(Value::as_str).is_some();

        if !status.is_success() && !has_login_state {
            return Err(classify_login_failure(status, &value, context));
        }

        if let Some((code, message)) = api_error(&value) {
            if context == LoginContext::Code {
                match code {
                    Some(1) => return Err(CloudError::VerifyCodeExpired),
                    Some(2) => return Err(CloudError::VerifyCodeInvalid),
                    _ => {}
                }
            }
            return Err(CloudError::Api { code, message });
        }
        Ok(value)
    }

    async fn send_json(&self, request: reqwest::RequestBuilder) -> Result<Value, CloudError> {
        let response = request.send().await?;
        let status = response.status();
        let value = parse_body(&response.text().await?);

        if status == StatusCode::UNAUTHORIZED {
            return Err(CloudError::NotAuthenticated);
        }

        // `captchaId` 可能出现在任意状态码、任意层级；先于其他错误判断。
        if let Some(captcha_id) = find_captcha_id(&value) {
            return Err(CloudError::CaptchaRequired {
                captcha_id: Some(captcha_id),
            });
        }
        if status.as_u16() == 418 || message_mentions_captcha(&value) {
            return Err(CloudError::CaptchaRequired { captcha_id: None });
        }

        if !status.is_success() {
            // 非 JSON（通常是 WAF/Cloudflare 挑战页）单独归类，避免把
            // HTML 当成空响应继续解析。
            if value.is_null() {
                return Err(CloudError::WafBlocked {
                    status: status.as_u16(),
                });
            }
            if let Some((code, message)) = api_error(&value) {
                return Err(CloudError::Api { code, message });
            }
            let message = api_message(&value).unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("HTTP error")
                    .to_string()
            });
            return Err(CloudError::HttpStatus {
                status: status.as_u16(),
                body: sanitize_message(&message),
            });
        }

        if let Some((code, message)) = api_error(&value) {
            return Err(CloudError::Api { code, message });
        }
        Ok(value)
    }
}

fn parse_body(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// 在顶层或 `data` 下查找 `captchaId`。
fn find_captcha_id(value: &Value) -> Option<String> {
    value
        .get("captchaId")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| data.get("captchaId"))
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
}

/// 部分 WAF 会返回 403/400 + 纯文本提示，而不是 418。
fn message_mentions_captcha(value: &Value) -> bool {
    api_message(value)
        .map(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("not a robot")
                || message.contains("captcha")
                || message.contains("human verification")
                || message.contains("人机验证")
        })
        .unwrap_or(false)
}

/// 把“非 2xx 且没有登录状态”的响应映射成可诊断的错误。
fn classify_login_failure(status: StatusCode, value: &Value, context: LoginContext) -> CloudError {
    if value.is_null() {
        return CloudError::WafBlocked {
            status: status.as_u16(),
        };
    }

    let code = value.get("code").and_then(|code| {
        code.as_i64()
            .or_else(|| code.as_str().and_then(|text| text.parse::<i64>().ok()))
    });

    // 验证码流程里 `code` 有明确语义（见内部协议调研）。
    if context == LoginContext::Code {
        match code {
            Some(1) => return CloudError::VerifyCodeExpired,
            Some(2) => return CloudError::VerifyCodeInvalid,
            _ => {}
        }
    }

    let message = api_message(value)
        .map(|message| sanitize_message(&message))
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("HTTP error")
                .to_string()
        });
    CloudError::LoginRejected {
        status: status.as_u16(),
        code,
        message,
    }
}

fn model_from_device(device: &BindDevice) -> PrinterModel {
    let raw = device
        .dev_product_name
        .as_deref()
        .or(device.dev_model_name.as_deref())
        .unwrap_or("");
    let cleaned = raw.strip_prefix("Bambu Lab ").unwrap_or(raw);
    PrinterModel::parse(cleaned)
}

pub fn uid_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    normalize_uid(value.get("username")?.as_str()?)
}

fn find_uid(value: &Value) -> Option<String> {
    let known_paths = [
        value
            .get("data")
            .and_then(|data| data.get("user"))
            .and_then(|user| user.get("uid")),
        value.get("data").and_then(|data| data.get("uid")),
        value.get("user").and_then(|user| user.get("uid")),
        value.get("uid"),
    ];
    for candidate in known_paths.into_iter().flatten() {
        if let Some(uid) = uid_from_value(candidate) {
            return Some(uid);
        }
    }
    find_uid_recursive(value)
}

fn profile_text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn find_uid_recursive(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in ["uid", "username"] {
                if let Some(uid) = map.get(key).and_then(uid_from_value) {
                    return Some(uid);
                }
            }
            map.values().find_map(find_uid_recursive)
        }
        Value::Array(items) => items.iter().find_map(find_uid_recursive),
        _ => None,
    }
}

fn uid_from_value(value: &Value) -> Option<String> {
    if let Some(raw) = value.as_str() {
        normalize_uid(raw)
    } else if let Some(raw) = value.as_i64() {
        (raw >= 0).then(|| raw.to_string())
    } else {
        value.as_u64().map(|raw| raw.to_string())
    }
}

fn account_name_from_preference(value: &Value) -> Option<String> {
    let candidates = [
        value.get("name"),
        value.get("handle"),
        value.get("data").and_then(|data| data.get("name")),
        value
            .get("data")
            .and_then(|data| data.get("user"))
            .and_then(|user| user.get("name")),
        value.get("user").and_then(|user| user.get("name")),
    ];
    for candidate in candidates.into_iter().flatten() {
        let Some(raw) = candidate.as_str() else {
            continue;
        };
        let name = raw.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

fn normalize_uid(raw: &str) -> Option<String> {
    let uid = raw.strip_prefix("u_").unwrap_or(raw);
    if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
        Some(uid.to_string())
    } else {
        None
    }
}

fn api_message(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    for key in ["message", "error", "msg"] {
        if let Some(message) = object.get(key).and_then(Value::as_str) {
            if !message.is_empty() && message != "success" {
                return Some(message.to_string());
            }
        }
    }
    None
}

fn api_error(value: &Value) -> Option<(Option<i64>, String)> {
    let object = value.as_object()?;

    if let Some(code) = object.get("code") {
        let code_value = code
            .as_i64()
            .or_else(|| code.as_str().and_then(|value| value.parse::<i64>().ok()));
        let is_error = matches!(code_value, Some(code) if code != 0);
        if is_error {
            let message = api_message(value)
                .unwrap_or_else(|| format!("API code {}", code_value.unwrap_or_default()));
            return Some((code_value, sanitize_message(&message)));
        }
    }

    if let Some(error) = object.get("error") {
        let is_error = !error.is_null()
            && error.as_bool() != Some(false)
            && !error
                .as_str()
                .map(|value| value.is_empty())
                .unwrap_or(false);
        if is_error {
            let message = api_message(value).unwrap_or_else(|| "API error".to_string());
            return Some((None, sanitize_message(&message)));
        }
    }

    None
}

fn sanitize_message(raw: &str) -> String {
    const MAX: usize = 200;
    let without_controls: String = raw.chars().filter(|c| !c.is_control()).collect();
    let compact = without_controls
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() <= MAX {
        compact
    } else {
        let mut truncated: String = compact.chars().take(MAX).collect();
        truncated.push('…');
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_uid_from_jwt_payload() {
        // {"username":"u_12345"}
        let payload = URL_SAFE_NO_PAD.encode(br#"{"username":"u_12345"}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(uid_from_jwt(&token).as_deref(), Some("12345"));
    }

    #[test]
    fn non_jwt_token_has_no_uid() {
        assert_eq!(uid_from_jwt("not-a-jwt"), None);
    }

    #[test]
    fn non_numeric_username_is_not_uid() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"username":"alice@example.com"}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(uid_from_jwt(&token), None);
    }

    #[test]
    fn finds_uid_in_nested_preference() {
        let value = json!({"data": {"user": {"uid": "u_999"}}});
        assert_eq!(find_uid(&value).as_deref(), Some("999"));
    }

    #[test]
    fn finds_numeric_uid_in_preference() {
        // 网页 token 对应的 /my/preference 返回顶层 uid 为 JSON 数字。
        let value = json!({"uid": 3547784302_i64, "name": "tester"});
        assert_eq!(find_uid(&value).as_deref(), Some("3547784302"));
    }

    #[test]
    fn finds_nested_numeric_uid() {
        let value = json!({"data": {"user": {"uid": 12345_i64}}});
        assert_eq!(find_uid(&value).as_deref(), Some("12345"));
    }

    #[test]
    fn account_profile_prefers_name_and_makerworld_url() {
        let profile = AccountProfile {
            uid: Some("123".into()),
            name: Some("Yolin".into()),
            handle: Some("yolin".into()),
        };
        assert_eq!(profile.display_name(), "Yolin");
        assert_eq!(profile.profile_url(), "https://makerworld.com/zh-cn/@yolin");
    }

    #[test]
    fn account_profile_falls_back_to_account_center() {
        let profile = AccountProfile::default();
        assert_eq!(profile.display_name(), "Bambu Cloud");
        assert_eq!(profile.profile_url(), "https://bambulab.cn/zh-cn/my");
    }

    #[test]
    fn detects_business_error_in_http_200() {
        let value = json!({"code": 1, "message": "invalid password"});
        assert_eq!(
            api_error(&value),
            Some((Some(1), "invalid password".to_string()))
        );
        assert_eq!(api_error(&json!({"code": 0, "message": "success"})), None);
    }

    #[test]
    fn code_semantics_only_apply_to_code_login() {
        let body = json!({"code": 1, "error": "verification code expired"});
        assert!(matches!(
            classify_login_failure(StatusCode::BAD_REQUEST, &body, LoginContext::Code),
            CloudError::VerifyCodeExpired
        ));
        // 同一份 body 在密码流程里是普通拒绝，不能谎称“验证码过期”。
        assert!(matches!(
            classify_login_failure(StatusCode::BAD_REQUEST, &body, LoginContext::Password),
            CloudError::LoginRejected {
                status: 400,
                code: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn invalid_code_is_reported_separately() {
        let body = json!({"code": 2, "error": "incorrect code"});
        assert!(matches!(
            classify_login_failure(StatusCode::BAD_REQUEST, &body, LoginContext::Code),
            CloudError::VerifyCodeInvalid
        ));
    }

    #[test]
    fn non_json_failure_is_treated_as_waf_block() {
        let error =
            classify_login_failure(StatusCode::FORBIDDEN, &Value::Null, LoginContext::Password);
        assert!(matches!(error, CloudError::WafBlocked { status: 403 }));
    }

    #[test]
    fn captcha_id_is_found_in_nested_data() {
        let value = json!({"data": {"captchaId": "geetest-1"}});
        assert_eq!(find_captcha_id(&value).as_deref(), Some("geetest-1"));
        assert!(message_mentions_captcha(
            &json!({"error": "We need to confirm that you are not a robot."})
        ));
    }

    #[tokio::test]
    #[ignore = "requires network; deliberately uses a fake token"]
    async fn bind_with_fake_token_is_unauthorized() {
        let auth = BambuAuth::new(BambuCloudConfig::china_mainland()).unwrap();
        let token = AuthToken::new("fake-token".into(), None, Some(3_600));
        let result = auth.fetch_devices(&token).await;
        assert!(
            matches!(result, Err(CloudError::NotAuthenticated)),
            "unexpected result: {result:?}"
        );
    }
}
