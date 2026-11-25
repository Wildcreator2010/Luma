use thiserror::Error;

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("api error {code:?}: {message}")]
    Api { code: Option<i64>, message: String },
    #[error("captcha required")]
    CaptchaRequired {
        #[allow(dead_code)]
        captcha_id: Option<String>,
    },
    /// 账号/密码被拒，或未归类的登录失败。保留原始 `code` 与服务端文案，
    /// 用于区分“确实凭据错误”和“风控/参数错误”。
    #[error("login rejected (http {status}, code {code:?}): {message}")]
    LoginRejected {
        status: u16,
        code: Option<i64>,
        message: String,
    },
    /// 验证码登录：服务端 `code == 1`，验证码已过期/需重新获取。
    #[error("verification code expired")]
    VerifyCodeExpired,
    /// 验证码登录：服务端 `code == 2`，验证码错误。
    #[error("verification code invalid")]
    VerifyCodeInvalid,
    /// 非 2xx 且响应不是 JSON：通常是 Cloudflare/WAF 挑战页。
    #[error("blocked by WAF (http {status})")]
    WafBlocked { status: u16 },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("verification code required for {account}")]
    VerifyCodeRequired { account: String },
    #[error("two-factor authentication required")]
    TfaRequired {
        #[allow(dead_code)]
        tfa_key: String,
    },
    #[error("token expired")]
    TokenExpired,
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("mqtt error: {0}")]
    Mqtt(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("no devices on this account")]
    NoDevices,
}
