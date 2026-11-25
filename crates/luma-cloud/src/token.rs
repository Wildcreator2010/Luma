//! Token 模型与 Windows Credential Manager 存储。
//!
//! # 存储格式（v1）
//!
//! 新格式把整个会话序列化为一个带版本号的 JSON 对象：
//!
//! ```json
//! {"v":1,"access_token":"...","refresh_token":null,"expires_in":86400,"obtained_at":1700000000000,"uid":null}
//! ```
//!
//! 默认写入单个 keyring entry（服务 `Luma`，账号 `bambu-cn-session`）。
//! 已查证 Windows Credential Manager 的凭据 blob 上限为
//! `CRED_MAX_CREDENTIAL_BLOB_SIZE = 2560` 字节，而 keyring v4 的 Windows
//! 后端会把密码按 UTF-16 编码写入，因此单条实际只能容纳约 1280 个
//! UTF-16 码元。真实 JWT 加上 refresh token 和 JSON 字段很容易超出，
//! 所以当 JSON 超过 `MAX_SINGLE_ENTRY_UNITS`（1200）时退化为分片：
//!
//! - `bambu-cn-session` 保存清单：
//!   `{"v":1,"chunked":true,"total":N,"length":L,"checksum":H}`；
//! - `bambu-cn-session-0` … `bambu-cn-session-{N-1}` 保存按字符切分的 JSON 片段。
//!
//! 读取分片时要求 `total` 落在 `1..=MAX_CHUNKS`、所有片段都存在、拼接后的
//! 字符数与 `length` 一致、FNV-1a 64 校验和与 `checksum` 一致。任一条件不
//! 满足都返回明确的损坏错误，不会静默拼出半截 token。
//!
//! # 兼容策略
//!
//! 读取顺序：先读 `bambu-cn-session`；只有该 entry 不存在时才回退到旧格式
//! （`bambu-cn-access-*` / `bambu-cn-refresh-*` / `bambu-cn-meta`）。旧格式
//! 读取成功后会立即以新格式写回并删除旧分片（自动迁移）。旧格式没有总片数
//! 元数据，因此读取时扫描完整 `0..MAX_CHUNKS`：一旦在缺口之后仍发现分片，
//! 视为损坏并报错。
//!
//! # 失败语义
//!
//! `store_token` 先在内存中构造并校验完整载荷，再开始写 keyring；写入前会
//! 快照将被覆盖的 entry，任一写入失败都会尽力恢复旧值后返回错误。读取遇到
//! 缺失分片、校验失败、未知版本或无法解析的元数据时一律返回明确的损坏错误。
//! `expires_in: None` 表示有效期未知、本地不判过期，由服务端 401 兜底清理。
//!
//! `CloudError` 不在本模块维护范围内，无法新增 `TokenCorrupted` 变体，因此
//! 损坏统一用 `CloudError::Protocol("token corrupted: ...")` 表达。

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::CloudError;

const KEYRING_SERVICE: &str = "Luma";
/// 新格式的单一 entry：完整 token JSON 或分片清单。
const SESSION_KEY: &str = "bambu-cn-session";
const LEGACY_ACCESS_KEY: &str = "bambu-cn-access";
const LEGACY_REFRESH_KEY: &str = "bambu-cn-refresh";
const LEGACY_META_KEY: &str = "bambu-cn-meta";

const FORMAT_VERSION: u32 = 1;
const CHUNK_CHARS: usize = 1_000;
const MAX_CHUNKS: usize = 32;
/// Windows 凭据 blob 上限 2560 字节，UTF-16 编码下约 1280 码元；留出余量。
const MAX_SINGLE_ENTRY_UNITS: usize = 1_200;
const EXPIRY_SKEW_MS: i64 = 60_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub obtained_at: i64,
    pub uid: Option<String>,
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthToken")
            .field("access_token", &self.masked())
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("expires_in", &self.expires_in)
            .field("obtained_at", &self.obtained_at)
            .field("uid", &self.uid)
            .finish()
    }
}

impl AuthToken {
    pub fn new(
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<i64>,
    ) -> Self {
        Self {
            access_token,
            refresh_token,
            expires_in,
            obtained_at: now_ms(),
            uid: None,
        }
    }

    /// 提前 60 秒判定过期，避免临界点请求失败。
    ///
    /// `expires_in: None` 表示服务端没有给出有效期，本地无法判断，视为不过期；
    /// 这类 token 由后续请求的 401 兜底（调用方清理并提示重新登录）。
    /// 极端大的 `expires_in` 使用饱和运算，不会溢出。
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self.expires_in {
            Some(seconds) if seconds <= 0 => true,
            Some(seconds) => {
                let lifetime_ms = seconds.saturating_mul(1_000);
                let expires_at = self
                    .obtained_at
                    .saturating_add(lifetime_ms)
                    .saturating_sub(EXPIRY_SKEW_MS);
                now_ms >= expires_at
            }
            None => false,
        }
    }

    /// 用于日志/CLI 的安全展示。
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.access_token.chars().collect();
        if chars.len() <= 12 {
            "***".to_string()
        } else {
            let prefix: String = chars[..6].iter().collect();
            let suffix: String = chars[chars.len() - 4..].iter().collect();
            format!("{prefix}...{suffix}")
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 旧格式的 meta entry。保留用于向后兼容读取。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct TokenMeta {
    expires_in: Option<i64>,
    obtained_at: i64,
    uid: Option<String>,
}

/// 新格式的 token JSON。
#[derive(Serialize, Deserialize)]
struct StoredToken {
    v: u32,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_in: Option<i64>,
    #[serde(default)]
    obtained_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
}

impl StoredToken {
    fn from_auth(token: &AuthToken) -> Self {
        Self {
            v: FORMAT_VERSION,
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_in: token.expires_in,
            obtained_at: token.obtained_at,
            uid: token.uid.clone(),
        }
    }

    fn into_auth_checked(self) -> Result<AuthToken, CloudError> {
        if self.v != FORMAT_VERSION {
            return Err(corrupted(format!(
                "unsupported token format version {}",
                self.v
            )));
        }
        Ok(AuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_in: self.expires_in,
            obtained_at: self.obtained_at,
            uid: self.uid,
        })
    }
}

/// 分片清单，写入 `SESSION_KEY`，片段写入 `bambu-cn-session-{i}`。
#[derive(Serialize, Deserialize)]
struct ChunkManifest {
    v: u32,
    chunked: bool,
    total: usize,
    length: usize,
    checksum: u64,
}

/// 可注入的存储后端抽象；生产实现是 Windows Credential Manager，
/// 测试使用内存实现，避免依赖真实凭据管理器。
trait TokenStore {
    fn get(&self, account: &str) -> Result<Option<String>, CloudError>;
    fn set(&self, account: &str, value: &str) -> Result<(), CloudError>;
    fn delete(&self, account: &str) -> Result<(), CloudError>;
}

struct KeyringStore;

impl TokenStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, CloudError> {
        match entry(account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(CloudError::Keyring(error)),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), CloudError> {
        entry(account)?.set_password(value)?;
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), CloudError> {
        match entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(CloudError::Keyring(error)),
        }
    }
}

enum WritePlan {
    Single {
        payload: String,
    },
    Chunked {
        chunks: Vec<String>,
        manifest: String,
    },
}

fn entry(account: &str) -> Result<keyring::Entry, CloudError> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, account)?)
}

fn session_chunk_key(index: usize) -> String {
    format!("{SESSION_KEY}-{index}")
}

fn legacy_chunk_key(prefix: &str, index: usize) -> String {
    format!("{prefix}-{index}")
}

fn corrupted(message: impl Into<String>) -> CloudError {
    CloudError::Protocol(format!("token corrupted: {}", message.into()))
}

/// FNV-1a 64 位校验和，用于分片完整性校验。
fn checksum(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn split_chunks(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(CHUNK_CHARS)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn build_plan(token: &AuthToken) -> Result<WritePlan, CloudError> {
    let payload = serde_json::to_string(&StoredToken::from_auth(token))?;
    if payload.encode_utf16().count() <= MAX_SINGLE_ENTRY_UNITS {
        return Ok(WritePlan::Single { payload });
    }

    let chunks = split_chunks(&payload);
    if chunks.len() > MAX_CHUNKS {
        return Err(CloudError::Protocol(format!(
            "token too large to store: {} chunks exceeds limit {MAX_CHUNKS}",
            chunks.len()
        )));
    }
    let manifest = ChunkManifest {
        v: FORMAT_VERSION,
        chunked: true,
        total: chunks.len(),
        length: payload.chars().count(),
        checksum: checksum(payload.as_bytes()),
    };
    Ok(WritePlan::Chunked {
        chunks,
        manifest: serde_json::to_string(&manifest)?,
    })
}

fn write_plan<S: TokenStore>(store: &S, plan: &WritePlan) -> Result<(), CloudError> {
    match plan {
        WritePlan::Single { payload } => store.set(SESSION_KEY, payload),
        WritePlan::Chunked { chunks, manifest } => {
            // 先写数据分片，最后写清单作为提交点。
            for (index, chunk) in chunks.iter().enumerate() {
                store.set(&session_chunk_key(index), chunk)?;
            }
            store.set(SESSION_KEY, manifest)
        }
    }
}

fn snapshot<S: TokenStore>(
    store: &S,
    keys: &[String],
) -> Result<Vec<(String, Option<String>)>, CloudError> {
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        values.push((key.clone(), store.get(key)?));
    }
    Ok(values)
}

fn restore<S: TokenStore>(store: &S, snapshot: Vec<(String, Option<String>)>) {
    for (key, value) in snapshot {
        // 尽力恢复；恢复失败也不掩盖原始写入错误。
        let _ = match value {
            Some(value) => store.set(&key, &value),
            None => store.delete(&key),
        };
    }
}

/// 删除新格式的多余分片和全部旧格式 entry；失败不影响已成功写入的新值。
fn cleanup<S: TokenStore>(store: &S, plan: &WritePlan) {
    let keep_from = match plan {
        WritePlan::Single { .. } => 0,
        WritePlan::Chunked { chunks, .. } => chunks.len(),
    };
    for index in keep_from..MAX_CHUNKS {
        let _ = store.delete(&session_chunk_key(index));
    }
    for index in 0..MAX_CHUNKS {
        let _ = store.delete(&legacy_chunk_key(LEGACY_ACCESS_KEY, index));
        let _ = store.delete(&legacy_chunk_key(LEGACY_REFRESH_KEY, index));
    }
    let _ = store.delete(LEGACY_META_KEY);
}

fn store_token_with<S: TokenStore>(store: &S, token: &AuthToken) -> Result<(), CloudError> {
    // 先在内存中完成序列化和长度/片数校验，再触碰存储。
    let plan = build_plan(token)?;

    let mut snapshot_keys = match &plan {
        WritePlan::Single { .. } => vec![SESSION_KEY.to_string()],
        WritePlan::Chunked { chunks, .. } => {
            let mut keys: Vec<String> = (0..chunks.len()).map(session_chunk_key).collect();
            keys.push(SESSION_KEY.to_string());
            keys
        }
    };
    snapshot_keys.dedup();

    let previous = snapshot(store, &snapshot_keys)?;
    if let Err(error) = write_plan(store, &plan) {
        restore(store, previous);
        return Err(error);
    }

    cleanup(store, &plan);
    Ok(())
}

fn load_chunked<S: TokenStore>(store: &S, manifest: &ChunkManifest) -> Result<String, CloudError> {
    if manifest.v != FORMAT_VERSION {
        return Err(corrupted(format!(
            "unsupported chunk manifest version {}",
            manifest.v
        )));
    }
    if manifest.total == 0 || manifest.total > MAX_CHUNKS {
        return Err(corrupted(format!(
            "invalid chunk total {} (limit {MAX_CHUNKS})",
            manifest.total
        )));
    }

    let mut payload = String::new();
    for index in 0..manifest.total {
        match store.get(&session_chunk_key(index))? {
            Some(chunk) => payload.push_str(&chunk),
            None => {
                return Err(corrupted(format!(
                    "missing chunk {index} of {}",
                    manifest.total
                )));
            }
        }
    }

    let actual_len = payload.chars().count();
    if actual_len != manifest.length {
        return Err(corrupted(format!(
            "length mismatch: expected {}, got {actual_len}",
            manifest.length
        )));
    }
    if checksum(payload.as_bytes()) != manifest.checksum {
        return Err(corrupted("checksum mismatch"));
    }
    Ok(payload)
}

fn decode_token(payload: &str) -> Result<AuthToken, CloudError> {
    let stored: StoredToken = serde_json::from_str(payload)
        .map_err(|error| corrupted(format!("invalid token record: {error}")))?;
    stored.into_auth_checked()
}

fn decode_session<S: TokenStore>(store: &S, payload: &str) -> Result<AuthToken, CloudError> {
    let value: serde_json::Value = serde_json::from_str(payload)
        .map_err(|error| corrupted(format!("invalid JSON: {error}")))?;
    if value.get("chunked").and_then(serde_json::Value::as_bool) == Some(true) {
        let manifest: ChunkManifest = serde_json::from_value(value)
            .map_err(|error| corrupted(format!("invalid chunk manifest: {error}")))?;
        return decode_token(&load_chunked(store, &manifest)?);
    }
    let stored: StoredToken = serde_json::from_value(value)
        .map_err(|error| corrupted(format!("invalid token record: {error}")))?;
    stored.into_auth_checked()
}

/// 旧格式没有总片数元数据，扫描完整范围；缺口之后若仍有分片则视为损坏。
fn load_legacy_chunks<S: TokenStore>(
    store: &S,
    prefix: &str,
) -> Result<Option<String>, CloudError> {
    let mut parts: Vec<Option<String>> = Vec::with_capacity(MAX_CHUNKS);
    for index in 0..MAX_CHUNKS {
        parts.push(store.get(&legacy_chunk_key(prefix, index))?);
    }

    let Some(gap) = parts.iter().position(Option::is_none) else {
        // 旧版写入不限制片数；若第 33 片存在，说明旧值超出可读范围，
        // 直接读 0..MAX_CHUNKS 会静默截断，因此按损坏处理。
        if store.get(&legacy_chunk_key(prefix, MAX_CHUNKS))?.is_some() {
            return Err(corrupted(format!(
                "legacy chunks '{prefix}' exceed {MAX_CHUNKS} chunks"
            )));
        }
        return Ok(Some(parts.into_iter().flatten().collect()));
    };

    if parts[gap + 1..].iter().any(Option::is_some) {
        let detail = if gap == 0 {
            format!("legacy chunks '{prefix}' start at a nonzero index")
        } else {
            format!("legacy chunks '{prefix}' are missing chunk {gap} but later chunks exist")
        };
        return Err(corrupted(detail));
    }

    if gap == 0 {
        Ok(None)
    } else {
        Ok(Some(parts[..gap].iter().flatten().cloned().collect()))
    }
}

fn load_legacy<S: TokenStore>(store: &S) -> Result<Option<AuthToken>, CloudError> {
    let Some(access_token) = load_legacy_chunks(store, LEGACY_ACCESS_KEY)? else {
        return Ok(None);
    };
    let refresh_token = load_legacy_chunks(store, LEGACY_REFRESH_KEY)?;
    let meta = match store.get(LEGACY_META_KEY)? {
        Some(payload) => serde_json::from_str::<TokenMeta>(&payload)
            .map_err(|error| corrupted(format!("invalid legacy meta: {error}")))?,
        None => TokenMeta::default(),
    };

    Ok(Some(AuthToken {
        access_token,
        refresh_token,
        expires_in: meta.expires_in,
        obtained_at: meta.obtained_at,
        uid: meta.uid,
    }))
}

fn load_token_with<S: TokenStore>(store: &S) -> Result<Option<AuthToken>, CloudError> {
    if let Some(payload) = store.get(SESSION_KEY)? {
        return decode_session(store, &payload).map(Some);
    }

    let Some(token) = load_legacy(store)? else {
        return Ok(None);
    };

    // 自动迁移；迁移失败不应让本次已成功读取的登录失效。
    if let Err(error) = store_token_with(store, &token) {
        tracing::warn!(%error, "failed to migrate legacy token to v1 format");
    }
    Ok(Some(token))
}

fn delete_token_with<S: TokenStore>(store: &S) -> Result<(), CloudError> {
    let mut keys = Vec::with_capacity(1 + MAX_CHUNKS * 3 + 1);
    keys.push(SESSION_KEY.to_string());
    for index in 0..MAX_CHUNKS {
        keys.push(session_chunk_key(index));
        keys.push(legacy_chunk_key(LEGACY_ACCESS_KEY, index));
        keys.push(legacy_chunk_key(LEGACY_REFRESH_KEY, index));
    }
    keys.push(LEGACY_META_KEY.to_string());

    let mut first_error = None;
    for key in keys {
        if let Err(error) = store.delete(&key) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub fn store_token(token: &AuthToken) -> Result<(), CloudError> {
    store_token_with(&KeyringStore, token)
}

pub fn load_token() -> Result<Option<AuthToken>, CloudError> {
    load_token_with(&KeyringStore)
}

pub fn delete_token() -> Result<(), CloudError> {
    delete_token_with(&KeyringStore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryStore {
        data: RefCell<BTreeMap<String, String>>,
        fail_set: RefCell<Option<String>>,
    }

    impl MemoryStore {
        fn get_raw(&self, key: &str) -> Option<String> {
            self.data.borrow().get(key).cloned()
        }

        fn insert_raw(&self, key: &str, value: &str) {
            self.data
                .borrow_mut()
                .insert(key.to_string(), value.to_string());
        }

        fn remove_raw(&self, key: &str) {
            self.data.borrow_mut().remove(key);
        }

        fn keys(&self) -> Vec<String> {
            self.data.borrow().keys().cloned().collect()
        }
    }

    impl TokenStore for MemoryStore {
        fn get(&self, account: &str) -> Result<Option<String>, CloudError> {
            Ok(self.data.borrow().get(account).cloned())
        }

        fn set(&self, account: &str, value: &str) -> Result<(), CloudError> {
            if self.fail_set.borrow().as_deref() == Some(account) {
                return Err(CloudError::Protocol("injected write failure".into()));
            }
            self.data
                .borrow_mut()
                .insert(account.to_string(), value.to_string());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), CloudError> {
            self.data.borrow_mut().remove(account);
            Ok(())
        }
    }

    fn sample_token(access_len: usize) -> AuthToken {
        let mut token = AuthToken::new(
            "a".repeat(access_len),
            Some("refresh-value".into()),
            Some(3_600),
        );
        token.obtained_at = 1_700_000_000_000;
        token.uid = Some("uid-123".into());
        token
    }

    #[test]
    fn expiry_uses_obtained_at_and_skew() {
        let mut token = AuthToken::new("token".into(), None, Some(90));
        token.obtained_at = 1_000;
        assert!(!token.is_expired(1_000));
        assert!(!token.is_expired(30_000));
        assert!(token.is_expired(91_000));
        token.expires_in = Some(0);
        assert!(token.is_expired(1_000));
    }

    #[test]
    fn expiry_saturates_on_extreme_values() {
        let mut token = AuthToken::new("token".into(), None, Some(i64::MAX));
        token.obtained_at = i64::MAX - 10;
        assert!(!token.is_expired(0));
        assert!(token.is_expired(i64::MAX));
        token.expires_in = None;
        assert!(!token.is_expired(i64::MAX));
    }

    #[test]
    fn splits_long_values_without_breaking_chars() {
        let value = "啊".repeat(2_500);
        let chunks = split_chunks(&value);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), value);
    }

    #[test]
    fn token_is_masked() {
        let token = AuthToken::new("abcdefghijklmnop".into(), None, None);
        assert_eq!(token.masked(), "abcdef...mnop");
        let debug = format!("{token:?}");
        assert!(!debug.contains("abcdefghijklmnop"));
        assert!(debug.contains("abcdef...mnop"));
    }

    #[test]
    fn json_roundtrip_uses_single_entry() {
        let store = MemoryStore::default();
        let token = sample_token(64);

        store_token_with(&store, &token).unwrap();
        assert_eq!(store.keys(), vec![SESSION_KEY.to_string()]);
        let payload = store.get_raw(SESSION_KEY).unwrap();
        assert!(payload.contains("\"v\":1"));

        let loaded = load_token_with(&store).unwrap().unwrap();
        assert!(loaded.access_token == token.access_token);
        assert!(loaded.refresh_token == token.refresh_token);
        assert_eq!(loaded.expires_in, token.expires_in);
        assert_eq!(loaded.obtained_at, token.obtained_at);
        assert_eq!(loaded.uid, token.uid);
    }

    #[test]
    fn chunked_roundtrip_detects_missing_and_corrupted_chunks() {
        let store = MemoryStore::default();
        let token = sample_token(3_000);

        store_token_with(&store, &token).unwrap();
        let manifest: ChunkManifest =
            serde_json::from_str(&store.get_raw(SESSION_KEY).unwrap()).unwrap();
        assert!(manifest.chunked);
        assert!(manifest.total > 1);
        assert!(store.get_raw(&session_chunk_key(0)).is_some());

        let loaded = load_token_with(&store).unwrap().unwrap();
        assert!(loaded.access_token == token.access_token);

        let middle = manifest.total / 2;
        store.remove_raw(&session_chunk_key(middle));
        let error = load_token_with(&store).unwrap_err().to_string();
        assert!(error.contains("token corrupted"));
        assert!(error.contains("missing chunk"));

        store.insert_raw(&session_chunk_key(middle), "corrupted");
        let error = load_token_with(&store).unwrap_err().to_string();
        assert!(error.contains("token corrupted"));
    }

    #[test]
    fn migrates_legacy_chunks_to_single_entry() {
        let store = MemoryStore::default();
        store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, 0), "legacy-access");
        store.insert_raw(&legacy_chunk_key(LEGACY_REFRESH_KEY, 0), "legacy-refresh");
        store.insert_raw(
            LEGACY_META_KEY,
            r#"{"expires_in":7200,"obtained_at":1700000000000,"uid":"legacy-uid"}"#,
        );

        let loaded = load_token_with(&store).unwrap().unwrap();
        assert!(loaded.access_token == "legacy-access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("legacy-refresh"));
        assert_eq!(loaded.expires_in, Some(7_200));
        assert_eq!(loaded.obtained_at, 1_700_000_000_000);
        assert_eq!(loaded.uid.as_deref(), Some("legacy-uid"));

        // 自动迁移为新格式并清理旧分片。
        assert!(store.get_raw(SESSION_KEY).is_some());
        assert!(store
            .get_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, 0))
            .is_none());
        assert!(store
            .get_raw(&legacy_chunk_key(LEGACY_REFRESH_KEY, 0))
            .is_none());
        assert!(store.get_raw(LEGACY_META_KEY).is_none());
    }

    #[test]
    fn legacy_gap_is_reported_as_corruption() {
        let store = MemoryStore::default();
        store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, 0), "aaa");
        store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, 2), "ccc");

        let error = load_token_with(&store).unwrap_err().to_string();
        assert!(error.contains("token corrupted"));
        assert!(error.contains("missing chunk 1"));
    }

    #[test]
    fn legacy_overflow_is_reported_as_corruption() {
        let store = MemoryStore::default();
        for index in 0..=MAX_CHUNKS {
            store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, index), "a");
        }

        let error = load_token_with(&store).unwrap_err().to_string();
        assert!(error.contains("token corrupted"));
        assert!(error.contains("exceed"));
    }

    #[test]
    fn failed_chunked_write_restores_previous_token() {
        let store = MemoryStore::default();
        let original = sample_token(64);
        store_token_with(&store, &original).unwrap();

        let replacement = sample_token(3_000);
        *store.fail_set.borrow_mut() = Some(session_chunk_key(1));
        let error = store_token_with(&store, &replacement)
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected write failure"));
        *store.fail_set.borrow_mut() = None;

        let loaded = load_token_with(&store).unwrap().unwrap();
        assert!(loaded.access_token == original.access_token);
        assert!(store.get_raw(&session_chunk_key(0)).is_none());
    }

    #[test]
    fn oversized_token_is_rejected_before_writing() {
        let store = MemoryStore::default();
        let token = sample_token(MAX_CHUNKS * CHUNK_CHARS + 1);

        let error = store_token_with(&store, &token).unwrap_err().to_string();
        assert!(error.contains("too large"));
        assert!(store.keys().is_empty());
    }

    #[test]
    fn delete_removes_new_and_legacy_orphans() {
        let store = MemoryStore::default();
        store.insert_raw(SESSION_KEY, "{}");
        store.insert_raw(&session_chunk_key(0), "a");
        store.insert_raw(&session_chunk_key(MAX_CHUNKS - 1), "z");
        store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, 0), "a");
        store.insert_raw(&legacy_chunk_key(LEGACY_ACCESS_KEY, MAX_CHUNKS - 1), "z");
        store.insert_raw(&legacy_chunk_key(LEGACY_REFRESH_KEY, 5), "r");
        store.insert_raw(LEGACY_META_KEY, "{}");

        delete_token_with(&store).unwrap();
        assert!(store.keys().is_empty());
    }
}
