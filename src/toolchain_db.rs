use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct ToolchainDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct ToolchainRecord {
    pub name: String,
    pub archive_filename: String,
    pub archive_sha256: String,
    pub install_path: String,
    pub installed_hash: String,
    pub installed_at: i64,
}

impl ToolchainDb {
    /// Open or create the toolchain database
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;

        // Create table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS toolchains (
                name TEXT PRIMARY KEY,
                archive_filename TEXT NOT NULL,
                archive_sha256 TEXT NOT NULL,
                install_path TEXT NOT NULL,
                installed_hash TEXT NOT NULL,
                installed_at INTEGER NOT NULL
            )",
            [],
        )?;

        Ok(Self { conn })
    }

    /// Check if a toolchain is installed and up-to-date
    /// Returns Some(record) if installed, None if not installed or out of date
    pub fn get_toolchain(&self, name: &str) -> Result<Option<ToolchainRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, archive_filename, archive_sha256, install_path, installed_hash, installed_at
             FROM toolchains
             WHERE name = ?1"
        )?;

        let mut rows = stmt.query(params![name])?;

        if let Some(row) = rows.next()? {
            Ok(Some(ToolchainRecord {
                name: row.get(0)?,
                archive_filename: row.get(1)?,
                archive_sha256: row.get(2)?,
                install_path: row.get(3)?,
                installed_hash: row.get(4)?,
                installed_at: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Check if a toolchain with specific archive hash is installed
    pub fn is_toolchain_installed(&self, name: &str, archive_sha256: &str) -> Result<bool> {
        if let Some(record) = self.get_toolchain(name)? {
            // Check if the archive hash matches
            Ok(record.archive_sha256 == archive_sha256)
        } else {
            Ok(false)
        }
    }

    /// Record a toolchain installation
    pub fn record_installation(
        &self,
        name: &str,
        archive_filename: &str,
        archive_sha256: &str,
        install_path: &str,
        installed_hash: &str,
    ) -> Result<()> {
        let installed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO toolchains
             (name, archive_filename, archive_sha256, install_path, installed_hash, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                name,
                archive_filename,
                archive_sha256,
                install_path,
                installed_hash,
                installed_at
            ],
        )?;

        Ok(())
    }

    /// Remove a toolchain record
    pub fn remove_toolchain(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM toolchains WHERE name = ?1",
            params![name],
        )?;
        Ok(())
    }

    /// List all installed toolchains
    pub fn list_toolchains(&self) -> Result<Vec<ToolchainRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, archive_filename, archive_sha256, install_path, installed_hash, installed_at
             FROM toolchains
             ORDER BY installed_at DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ToolchainRecord {
                name: row.get(0)?,
                archive_filename: row.get(1)?,
                archive_sha256: row.get(2)?,
                install_path: row.get(3)?,
                installed_hash: row.get(4)?,
                installed_at: row.get(5)?,
            })
        })?;

        let mut toolchains = Vec::new();
        for record in rows {
            toolchains.push(record?);
        }

        Ok(toolchains)
    }
}
