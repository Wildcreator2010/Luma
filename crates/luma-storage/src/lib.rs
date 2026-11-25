//! 本地持久化。
//!
//! Phase 0 只包含设备配置表。打印历史、配置与凭据在后续阶段逐步加入。
//! 所有数据库操作都是阻塞的，调用方必须放在 `spawn_blocking` 或专用线程执行。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use luma_model::{DeviceInfo, PrinterModel, Region};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid data: {0}")]
    InvalidData(String),
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.prepare()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.prepare()?;
        Ok(db)
    }

    fn prepare(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS printers (
                serial      TEXT PRIMARY KEY,
                name        TEXT,
                model       TEXT NOT NULL,
                region      TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS print_history (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                serial        TEXT NOT NULL,
                task_name     TEXT,
                started_at    INTEGER,
                finished_at   INTEGER,
                outcome       TEXT NOT NULL,
                duration_secs INTEGER,
                FOREIGN KEY (serial) REFERENCES printers(serial) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_history_serial_started
                ON print_history(serial, started_at DESC);
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_device(&self, info: &DeviceInfo) -> Result<(), StorageError> {
        let now = now_ms();
        self.conn.execute(
            r#"
            INSERT INTO printers (serial, name, model, region, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?5)
            ON CONFLICT(serial) DO UPDATE SET
                name       = excluded.name,
                model      = excluded.model,
                region     = excluded.region,
                updated_at = excluded.updated_at
            "#,
            params![
                info.serial,
                info.name,
                info.model.as_canonical_str(),
                info.region.as_str(),
                now,
            ],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceInfo>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT serial, name, model, region FROM printers ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut devices = Vec::new();
        for row in rows {
            let (serial, name, model, region) = row?;
            devices.push(DeviceInfo {
                serial,
                name,
                model: PrinterModel::parse(&model),
                region: Region::parse(&region),
                online: false,
            });
        }
        Ok(devices)
    }

    pub fn rename_device(&self, serial: &str, name: Option<&str>) -> Result<bool, StorageError> {
        let changed = self.conn.execute(
            "UPDATE printers SET name = ?2, updated_at = ?3 WHERE serial = ?1",
            params![serial, name, now_ms()],
        )?;
        Ok(changed > 0)
    }

    pub fn remove_device(&self, serial: &str) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute("DELETE FROM printers WHERE serial = ?1", params![serial])?;
        Ok(changed > 0)
    }

    /// 仅供测试和后续阶段使用。
    pub fn device_name(&self, serial: &str) -> Result<Option<String>, StorageError> {
        let name = self
            .conn
            .query_row(
                "SELECT name FROM printers WHERE serial = ?1",
                params![serial],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(name)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(serial: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            serial: serial.to_string(),
            name: Some(name.to_string()),
            model: PrinterModel::X2D,
            region: Region::ChinaMainland,
            online: false,
        }
    }

    #[test]
    fn insert_list_rename_remove() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_device(&device("A", "一号")).unwrap();
        db.upsert_device(&device("B", "二号")).unwrap();

        let list = db.list_devices().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].model, PrinterModel::X2D);

        assert!(db.rename_device("A", Some("车间一号")).unwrap());
        assert_eq!(db.device_name("A").unwrap().as_deref(), Some("车间一号"));

        assert!(db.remove_device("A").unwrap());
        assert_eq!(db.list_devices().unwrap().len(), 1);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_device(&device("A", "一号")).unwrap();
        db.upsert_device(&device("A", "改名了")).unwrap();
        assert_eq!(db.device_name("A").unwrap().as_deref(), Some("改名了"));
        assert_eq!(db.list_devices().unwrap().len(), 1);
    }
}
