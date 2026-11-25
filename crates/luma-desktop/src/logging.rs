//! 安装版没有控制台，日志必须落到文件，否则出问题无从排查。
//!
//! 默认路径：`%LOCALAPPDATA%\Luma\logs\luma.log`。
//! 可用 `LUMA_LOG_FILE` 覆盖；`RUST_LOG` 控制级别（默认 `info`）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;

const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

pub fn log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Luma")
        .join("logs")
}

fn log_file() -> PathBuf {
    std::env::var_os("LUMA_LOG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| log_dir().join("luma.log"))
}

/// 初始化全局日志。失败时退回 stderr（GUI 下通常看不到，但不会影响启动）。
pub fn init() {
    let path = log_file();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    match open_log(&path) {
        Ok(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .init(),
        Err(_) => builder.with_ansi(false).init(),
    }
}

fn open_log(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 简单滚动：只在启动时检查，超过 4 MiB 留一份上一轮日志。
    if std::fs::metadata(path)
        .map(|meta| meta.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let old = path.with_file_name("luma.old.log");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, &old);
    }

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(file)
}

/// 写崩溃日志。GUI 子系统没有控制台，文件是唯一可靠的现场。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let dir = log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let backtrace = std::backtrace::Backtrace::force_capture();
        let message = format!("[{}] {info}\n\n{backtrace}\n", timestamp());
        let _ = std::fs::write(dir.join("panic.log"), message);
    }));
}

pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    // 崩溃日志用 Unix 时间即可，避免为一行时间引入日期库。
    format!("unix:{secs}")
}

pub fn append_panic_note(note: &str) {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("panic.log"))
    {
        let _ = writeln!(file, "{note}");
    }
}
