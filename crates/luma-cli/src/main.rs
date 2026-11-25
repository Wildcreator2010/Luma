//! `luma` CLI：登录、查看设备、抓取 MQTT 原始上报。
//!
//! 安全约束：
//! - 不打印完整 token 或密码；
//! - 抓包文件可能包含设备与任务信息，不应上传或提交。

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use luma_cloud::{
    delete_token, load_token, store_token, AuthToken, BambuAuth, BambuCloudConfig,
    BambuCloudProvider, CloudError, LoginOutcome,
};
use luma_core::{PrinterProvider, ProviderEvent};
use luma_model::DeviceInfo;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "luma", version, about = "Luma 拓竹打印机命令行工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 登录 Bambu Cloud 并保存凭据
    Login(LoginArgs),
    /// 查看当前登录状态与设备
    Status,
    /// 列出账号下的设备
    Devices,
    /// 删除本地保存的登录凭据
    Logout,
    /// 导入已有的 accessToken（用于 2FA 账号）
    ImportToken,
    /// 抓取设备 MQTT 原始上报
    Capture(CaptureArgs),
}

#[derive(Args)]
struct LoginArgs {
    /// 登录账号（邮箱或手机号）
    #[arg(long)]
    account: String,

    /// 登录方式
    #[arg(long, value_enum, default_value = "password")]
    method: LoginMethod,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LoginMethod {
    Password,
    EmailCode,
    SmsCode,
}

#[derive(Args)]
struct CaptureArgs {
    /// 抓包时长（秒，5..=600）
    #[arg(long, default_value = "30", value_parser = parse_capture_seconds)]
    seconds: u64,

    /// 输出文件路径（默认 captures/x2d-<UTC时间戳>.jsonl）
    #[arg(long)]
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(error) = run().await {
        eprintln!("错误：{error:#}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(io::stderr)
        .with_target(false)
        .try_init();
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Login(args) => cmd_login(args).await,
        Command::Status => cmd_status().await,
        Command::Devices => cmd_devices().await,
        Command::Logout => cmd_logout(),
        Command::ImportToken => cmd_import_token().await,
        Command::Capture(args) => cmd_capture(args).await,
    }
}

fn build_auth() -> Result<(BambuCloudConfig, BambuAuth)> {
    let config = BambuCloudConfig::china_mainland();
    let auth = BambuAuth::new(config.clone()).context("初始化 Bambu Cloud 客户端失败")?;
    Ok((config, auth))
}

async fn cmd_login(args: LoginArgs) -> Result<()> {
    let (_, auth) = build_auth()?;

    let token = match args.method {
        LoginMethod::Password => {
            let password = prompt_secret("请输入密码（输入不回显）：")?;
            match auth.login_password(&args.account, &password).await {
                Ok(LoginOutcome::Success(token)) => token,
                Ok(LoginOutcome::VerifyCode { account }) => {
                    let sent = if account.contains('@') {
                        auth.send_email_code(&account).await
                    } else {
                        auth.send_sms_code(&account).await
                    };
                    match sent {
                        Ok(()) => println!("该账号需要验证码，已发送至：{account}"),
                        Err(error) => println!(
                            "发送验证码失败（{error}）。若已收到验证码可直接输入，否则请改用 --method email-code 或 sms-code。"
                        ),
                    }
                    let code = prompt_line("请输入验证码：")?;
                    login_with_code(&auth, &account, &code).await?
                }
                Ok(LoginOutcome::Tfa { .. }) | Err(CloudError::TfaRequired { .. }) => {
                    return Err(tfa_error());
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error).context("密码登录失败"));
                }
            }
        }
        LoginMethod::EmailCode => {
            auth.send_email_code(&args.account)
                .await
                .context("发送邮箱验证码失败")?;
            println!("验证码已发送至邮箱：{}", args.account);
            let code = prompt_line("请输入验证码：")?;
            login_with_code(&auth, &args.account, &code).await?
        }
        LoginMethod::SmsCode => {
            auth.send_sms_code(&args.account)
                .await
                .context("发送短信验证码失败")?;
            println!("验证码已发送至手机：{}", args.account);
            let code = prompt_line("请输入验证码：")?;
            login_with_code(&auth, &args.account, &code).await?
        }
    };

    store_token(&token).context("保存登录凭据失败")?;
    println!("登录成功，凭据已保存。");

    let fallback = token.clone();
    match auth.prepare_session(token).await {
        Ok((token, devices, uid)) => {
            println!("账号：{}", args.account);
            print_token_summary(&token);
            println!("UID: {uid}");
            print_devices(&devices);
        }
        Err(CloudError::NoDevices) => {
            println!("账号：{}", args.account);
            print_token_summary(&fallback);
            if let Some(uid) = &fallback.uid {
                println!("UID: {uid}");
            }
            println!("该账号下没有绑定设备。");
        }
        Err(error) if is_auth_error(&error) => {
            return Err(anyhow::anyhow!(
                "凭据已保存，但校验失败（{error}）。请重试 luma login。"
            ));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("凭据已保存，但获取设备列表失败"));
        }
    }

    Ok(())
}

async fn login_with_code(auth: &BambuAuth, account: &str, code: &str) -> Result<AuthToken> {
    match auth.login_code(account, code).await {
        Ok(token) => Ok(token),
        Err(CloudError::TfaRequired { .. }) => Err(tfa_error()),
        Err(CloudError::VerifyCodeRequired { .. }) => {
            Err(anyhow::anyhow!("验证码未通过，请确认后重试。"))
        }
        Err(error) => Err(anyhow::Error::new(error).context("验证码登录失败")),
    }
}

async fn cmd_status() -> Result<()> {
    let Some(token) = load_token().context("读取本地登录凭据失败")? else {
        println!("未登录。请运行：luma login --account <账号>");
        return Ok(());
    };

    let (_, auth) = build_auth()?;
    let fallback = token.clone();

    match auth.prepare_session(token).await {
        Ok((token, devices, uid)) => {
            println!("登录状态：有效");
            print_token_summary(&token);
            println!("UID: {uid}");
            print_devices(&devices);
        }
        Err(CloudError::NoDevices) => {
            println!("登录状态：有效（账号下没有绑定设备）");
            print_token_summary(&fallback);
            if let Some(uid) = &fallback.uid {
                println!("UID: {uid}");
            }
            println!("设备：0 台");
        }
        Err(error) if is_auth_error(&error) => {
            bail!("登录已失效（{error}），请重新运行：luma login --account <账号>");
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("读取账号状态失败"));
        }
    }

    Ok(())
}

async fn cmd_devices() -> Result<()> {
    let Some(token) = load_token().context("读取本地登录凭据失败")? else {
        println!("未登录。请运行：luma login --account <账号>");
        return Ok(());
    };

    let (_, auth) = build_auth()?;
    match auth.prepare_session(token).await {
        Ok((_, devices, _)) => print_devices(&devices),
        Err(CloudError::NoDevices) => println!("该账号下没有绑定设备。"),
        Err(error) if is_auth_error(&error) => {
            bail!("登录已失效（{error}），请重新运行：luma login --account <账号>");
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("获取设备列表失败"));
        }
    }

    Ok(())
}

fn cmd_logout() -> Result<()> {
    delete_token().context("删除本地登录凭据失败")?;
    println!("已退出登录，本地凭据已删除。");
    Ok(())
}

async fn cmd_import_token() -> Result<()> {
    let (_, auth) = build_auth()?;
    let access_token = prompt_secret("请粘贴 accessToken（输入不回显）：")?;
    let token = AuthToken::new(access_token, None, None);
    let (token, devices, uid) = auth
        .prepare_session(token)
        .await
        .context("Token 校验失败")?;
    store_token(&token).context("保存登录凭据失败")?;

    println!("Token 已校验并保存。");
    print_token_summary(&token);
    println!("UID: {uid}");
    print_devices(&devices);
    Ok(())
}

async fn cmd_capture(args: CaptureArgs) -> Result<()> {
    let Some(token) = load_token().context("读取本地登录凭据失败")? else {
        bail!("未登录。请先运行：luma login --account <账号>");
    };

    let (config, auth) = build_auth()?;
    let (token, devices, uid) = match auth.prepare_session(token).await {
        Ok(session) => session,
        Err(CloudError::NoDevices) => {
            bail!("该账号下没有绑定设备，无法抓包。");
        }
        Err(error) if is_auth_error(&error) => {
            bail!("登录已失效（{error}）。请重新运行：luma login --account <账号>");
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("准备云端会话失败"));
        }
    };

    println!("Token: {}", token.masked());
    println!("UID: {uid}");
    print_devices(&devices);

    let provider = BambuCloudProvider::connect(config, token, devices.clone(), uid)
        .await
        .context("连接 Bambu Cloud MQTT 失败")?;

    let path = args
        .out
        .unwrap_or_else(|| default_capture_path(unix_now_ms()));
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败：{}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| {
            format!(
                "创建抓包文件失败：{}（文件已存在时请换一个 --out）",
                path.display()
            )
        })?;
    let mut writer = BufWriter::new(file);

    println!("开始抓包 {} 秒，输出：{}", args.seconds, path.display());
    println!("注意：抓包文件可能包含设备/任务信息，默认位于本地应用数据目录，不要上传或提交。");

    let (tx, mut rx) = mpsc::channel::<ProviderEvent>(512);
    let mut provider_task = Some(tokio::spawn(async move { provider.run(tx).await }));

    let deadline = tokio::time::sleep(Duration::from_secs(args.seconds));
    tokio::pin!(deadline);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut total: u64 = 0;
    let stop_reason: &str;

    loop {
        tokio::select! {
            _ = &mut deadline => {
                stop_reason = "抓包时长结束";
                break;
            }
            _ = &mut ctrl_c => {
                stop_reason = "收到 Ctrl+C";
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(ProviderEvent::Report { serial, payload, received_at_ms }) => {
                        let serial_json = serde_json::to_string(&serial)
                            .unwrap_or_else(|_| "\"\"".to_string());
                        let payload_json = serde_json::to_string(&payload)
                            .unwrap_or_else(|_| "null".to_string());
                        if let Err(error) = writeln!(
                            writer,
                            "{{\"received_at_ms\":{received_at_ms},\"serial\":{serial_json},\"payload\":{payload_json}}}"
                        ) {
                            if let Some(task) = provider_task.take() {
                                task.abort();
                            }
                            return Err(error).context("写入抓包文件失败");
                        }
                        *counts.entry(serial).or_insert(0) += 1;
                        total += 1;
                    }
                    Some(ProviderEvent::ConnectionChanged { online, detail }) => {
                        if online {
                            println!("MQTT 已连接。");
                        } else {
                            match detail {
                                Some(detail) => println!("MQTT 连接断开：{detail}"),
                                None => println!("MQTT 连接断开。"),
                            }
                        }
                    }
                    Some(ProviderEvent::Error { message, retryable }) => {
                        println!(
                            "MQTT 错误：{message}（{}）",
                            if retryable { "可重试" } else { "不可重试" }
                        );
                    }
                    Some(ProviderEvent::DevicesDiscovered(discovered)) => {
                        println!("发现设备：{} 台。", discovered.len());
                    }
                    None => {
                        if let Some(task) = provider_task.take() {
                            match task.await {
                                Ok(Ok(())) => println!("MQTT 会话已正常结束。"),
                                Ok(Err(error)) => println!("MQTT 会话结束：{error}"),
                                Err(error) => println!("抓包任务异常：{error}"),
                            }
                        }
                        stop_reason = "MQTT 会话已结束";
                        break;
                    }
                }
            }
        }
    }

    if let Some(task) = provider_task.take() {
        task.abort();
    }
    writer.flush().context("刷新抓包文件失败")?;

    println!("抓包结束（{stop_reason}）：共收到 {total} 条上报。");
    for device in &devices {
        let name = device.name.as_deref().unwrap_or("（未命名）");
        let count = counts.get(&device.serial).copied().unwrap_or(0);
        println!("  {name} | SN {} | {count} 条", device.serial);
    }
    println!("文件：{}", path.display());
    println!("注意：抓包文件可能包含设备/任务信息，不要上传或提交。");

    Ok(())
}

fn prompt_secret(label: &str) -> Result<String> {
    let value = rpassword::prompt_password(label).context("读取密码失败")?;
    if value.is_empty() {
        bail!("输入不能为空。");
    }
    Ok(value)
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().context("刷新终端输出失败")?;
    let mut value = String::new();
    io::stdin().read_line(&mut value).context("读取输入失败")?;
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("输入不能为空。");
    }
    Ok(value)
}

fn tfa_error() -> anyhow::Error {
    anyhow::anyhow!(
        "该账号启用了两步验证（2FA），当前 CLI 暂不支持。可先用 BambuStudio 登录，再运行 `luma import-token` 导入 accessToken。"
    )
}

fn is_auth_error(error: &CloudError) -> bool {
    matches!(
        error,
        CloudError::TokenExpired | CloudError::NotAuthenticated
    )
}

fn print_token_summary(token: &AuthToken) {
    println!("Token: {}", token.masked());
    match token.expires_in {
        Some(seconds) if seconds > 0 => {
            let expires_at = token
                .obtained_at
                .saturating_add(seconds.saturating_mul(1_000));
            println!(
                "过期时间：{}（本地时间）",
                format_local_datetime(expires_at)
            );
        }
        _ => println!("过期时间：未知（服务端未提供有效期）"),
    }
}

fn print_devices(devices: &[DeviceInfo]) {
    println!("设备：{} 台", devices.len());
    for (index, device) in devices.iter().enumerate() {
        let name = device.name.as_deref().unwrap_or("（未命名）");
        println!(
            "  {}. {} | SN {} | 型号 {}",
            index + 1,
            name,
            device.serial,
            device.model
        );
    }
}

fn parse_capture_seconds(raw: &str) -> Result<u64, String> {
    let seconds: u64 = raw
        .parse()
        .map_err(|_| "请输入 5 到 600 之间的整数秒。".to_string())?;
    if (5..=600).contains(&seconds) {
        Ok(seconds)
    } else {
        Err("--seconds 必须在 5 到 600 之间。".to_string())
    }
}

fn default_capture_path(now_ms: i64) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Luma")
        .join("captures")
        .join(capture_file_name(now_ms))
}

fn capture_file_name(unix_ms: i64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_ms);
    format!("x2d-{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z.jsonl")
}

fn utc_parts(unix_ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = unix_ms.div_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
    )
}

/// Howard Hinnant 的 `civil_from_days` 算法（Unix 天数 -> 公历年月日）。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn format_utc_datetime(unix_ms: i64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_ms);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

#[cfg(windows)]
mod win_time {
    #[repr(C)]
    pub struct FileTime {
        pub low: u32,
        pub high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct SystemTime {
        pub year: u16,
        pub month: u16,
        pub day_of_week: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
        pub milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn FileTimeToSystemTime(
            file_time: *const FileTime,
            system_time: *mut SystemTime,
        ) -> i32;
        pub fn SystemTimeToTzSpecificLocalTime(
            time_zone: *const core::ffi::c_void,
            universal: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }
}

#[cfg(windows)]
fn format_local_datetime(unix_ms: i64) -> String {
    use win_time::{FileTime, SystemTime as WinSystemTime};

    let filetime_100ns = (unix_ms as i128) * 10_000 + 116_444_736_000_000_000i128;
    if !(0..=u64::MAX as i128).contains(&filetime_100ns) {
        return format_utc_datetime(unix_ms);
    }

    let value = filetime_100ns as u64;
    let file_time = FileTime {
        low: value as u32,
        high: (value >> 32) as u32,
    };
    let mut utc = WinSystemTime::default();
    let mut local = WinSystemTime::default();

    // SAFETY: 指针均指向本函数栈上的有效结构；时区参数为 NULL 表示使用系统当前时区。
    let ok = unsafe {
        win_time::FileTimeToSystemTime(&file_time, &mut utc) != 0
            && win_time::SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    if !ok {
        return format_utc_datetime(unix_ms);
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.year, local.month, local.day, local.hour, local.minute, local.second
    )
}

#[cfg(not(windows))]
fn format_local_datetime(unix_ms: i64) -> String {
    format_utc_datetime(unix_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_file_name_uses_utc_components() {
        assert_eq!(capture_file_name(0), "x2d-19700101T000000Z.jsonl");
        assert_eq!(
            capture_file_name(1_700_000_000_000),
            "x2d-20231114T221320Z.jsonl"
        );
    }

    #[test]
    fn capture_seconds_are_limited_to_5_600() {
        assert_eq!(parse_capture_seconds("5").unwrap(), 5);
        assert_eq!(parse_capture_seconds("600").unwrap(), 600);
        assert!(parse_capture_seconds("4").is_err());
        assert!(parse_capture_seconds("601").is_err());
        assert!(parse_capture_seconds("abc").is_err());
    }

    #[test]
    fn default_capture_path_is_under_captures() {
        let path = default_capture_path(0);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("x2d-19700101T000000Z.jsonl")
        );
    }
}
