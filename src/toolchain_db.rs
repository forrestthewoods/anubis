use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

// ============================================================================
// Global Toolchain Database
// ============================================================================

/// Database for tracking globally installed toolchains in ~/.anubis/toolchains.db
pub struct GlobalToolchainDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct GlobalToolchainRecord {
    pub id: i64,
    pub toolchain_type: String,
    pub version: String,
    pub platform: String,
    pub archive_sha256: String,
    pub install_path: String,
    pub installed_at: i64,
}

impl GlobalToolchainDb {
    /// Open or create the global toolchain database
    pub fn open(db_path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)?;

        // Create table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS global_toolchains (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                toolchain_type TEXT NOT NULL,
                version TEXT NOT NULL,
                platform TEXT NOT NULL,
                archive_sha256 TEXT NOT NULL,
                install_path TEXT NOT NULL,
                installed_at INTEGER NOT NULL,
                UNIQUE(toolchain_type, version, platform)
            )",
            [],
        )?;

        Ok(Self { conn })
    }

    /// Check if a specific version of a toolchain is installed globally
    pub fn is_installed(
        &self,
        toolchain_type: &str,
        version: &str,
        platform: &str,
        archive_sha256: &str,
    ) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT archive_sha256 FROM global_toolchains
             WHERE toolchain_type = ?1 AND version = ?2 AND platform = ?3",
        )?;

        let mut rows = stmt.query(params![toolchain_type, version, platform])?;

        if let Some(row) = rows.next()? {
            let stored_sha256: String = row.get(0)?;
            Ok(stored_sha256 == archive_sha256)
        } else {
            Ok(false)
        }
    }

    /// Get the installation path for a specific toolchain version
    pub fn get_install_path(
        &self,
        toolchain_type: &str,
        version: &str,
        platform: &str,
    ) -> Result<Option<PathBuf>> {
        let mut stmt = self.conn.prepare(
            "SELECT install_path FROM global_toolchains
             WHERE toolchain_type = ?1 AND version = ?2 AND platform = ?3",
        )?;

        let mut rows = stmt.query(params![toolchain_type, version, platform])?;

        if let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            Ok(Some(PathBuf::from(path)))
        } else {
            Ok(None)
        }
    }

    /// Get the full record for a specific toolchain version
    pub fn get_toolchain(
        &self,
        toolchain_type: &str,
        version: &str,
        platform: &str,
    ) -> Result<Option<GlobalToolchainRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, toolchain_type, version, platform, archive_sha256, install_path, installed_at
             FROM global_toolchains
             WHERE toolchain_type = ?1 AND version = ?2 AND platform = ?3",
        )?;

        let mut rows = stmt.query(params![toolchain_type, version, platform])?;

        if let Some(row) = rows.next()? {
            Ok(Some(GlobalToolchainRecord {
                id: row.get(0)?,
                toolchain_type: row.get(1)?,
                version: row.get(2)?,
                platform: row.get(3)?,
                archive_sha256: row.get(4)?,
                install_path: row.get(5)?,
                installed_at: row.get(6)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Record a global toolchain installation
    pub fn record_installation(
        &self,
        toolchain_type: &str,
        version: &str,
        platform: &str,
        archive_sha256: &str,
        install_path: &str,
    ) -> Result<()> {
        let installed_at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO global_toolchains
             (toolchain_type, version, platform, archive_sha256, install_path, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                toolchain_type,
                version,
                platform,
                archive_sha256,
                install_path,
                installed_at
            ],
        )?;

        Ok(())
    }

    /// List all globally installed toolchains
    pub fn list_toolchains(&self) -> Result<Vec<GlobalToolchainRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, toolchain_type, version, platform, archive_sha256, install_path, installed_at
             FROM global_toolchains
             ORDER BY installed_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(GlobalToolchainRecord {
                id: row.get(0)?,
                toolchain_type: row.get(1)?,
                version: row.get(2)?,
                platform: row.get(3)?,
                archive_sha256: row.get(4)?,
                install_path: row.get(5)?,
                installed_at: row.get(6)?,
            })
        })?;

        let mut toolchains = Vec::new();
        for record in rows {
            toolchains.push(record?);
        }

        Ok(toolchains)
    }
}

// ============================================================================
// Project Toolchain Database
// ============================================================================

/// Database for tracking symlinks in a project's toolchains/.anubis_db
pub struct ProjectToolchainDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct ProjectSymlinkRecord {
    pub symlink_name: String,
    pub toolchain_type: String,
    pub version: String,
    pub platform: String,
    pub target_path: String,
    pub created_at: i64,
}

impl ProjectToolchainDb {
    /// Open or create the project toolchain database
    pub fn open(db_path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)?;

        // Create table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS project_symlinks (
                symlink_name TEXT PRIMARY KEY,
                toolchain_type TEXT NOT NULL,
                version TEXT NOT NULL,
                platform TEXT NOT NULL,
                target_path TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            [],
        )?;

        Ok(Self { conn })
    }

    /// Get a symlink record by name
    pub fn get_symlink(&self, symlink_name: &str) -> Result<Option<ProjectSymlinkRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT symlink_name, toolchain_type, version, platform, target_path, created_at
             FROM project_symlinks
             WHERE symlink_name = ?1",
        )?;

        let mut rows = stmt.query(params![symlink_name])?;

        if let Some(row) = rows.next()? {
            Ok(Some(ProjectSymlinkRecord {
                symlink_name: row.get(0)?,
                toolchain_type: row.get(1)?,
                version: row.get(2)?,
                platform: row.get(3)?,
                target_path: row.get(4)?,
                created_at: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Check if a symlink exists and points to the expected version/platform
    pub fn is_symlink_current(
        &self,
        symlink_name: &str,
        version: &str,
        platform: &str,
    ) -> Result<bool> {
        if let Some(record) = self.get_symlink(symlink_name)? {
            Ok(record.version == version && record.platform == platform)
        } else {
            Ok(false)
        }
    }

    /// Record a symlink creation
    pub fn record_symlink(
        &self,
        symlink_name: &str,
        toolchain_type: &str,
        version: &str,
        platform: &str,
        target_path: &str,
    ) -> Result<()> {
        let created_at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO project_symlinks
             (symlink_name, toolchain_type, version, platform, target_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                symlink_name,
                toolchain_type,
                version,
                platform,
                target_path,
                created_at
            ],
        )?;

        Ok(())
    }

    /// Remove a symlink record
    pub fn remove_symlink(&self, symlink_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM project_symlinks WHERE symlink_name = ?1",
            params![symlink_name],
        )?;
        Ok(())
    }

    /// List all symlinks in this project
    pub fn list_symlinks(&self) -> Result<Vec<ProjectSymlinkRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT symlink_name, toolchain_type, version, platform, target_path, created_at
             FROM project_symlinks
             ORDER BY created_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ProjectSymlinkRecord {
                symlink_name: row.get(0)?,
                toolchain_type: row.get(1)?,
                version: row.get(2)?,
                platform: row.get(3)?,
                target_path: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;

        let mut symlinks = Vec::new();
        for record in rows {
            symlinks.push(record?);
        }

        Ok(symlinks)
    }
}

// ============================================================================
// Legacy ToolchainDb (kept for backward compatibility during migration)
// ============================================================================

#[deprecated(note = "Use GlobalToolchainDb or ProjectToolchainDb instead")]
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

#[allow(deprecated)]
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
             WHERE name = ?1",
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
        let installed_at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;

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
        self.conn.execute("DELETE FROM toolchains WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// List all installed toolchains
    pub fn list_toolchains(&self) -> Result<Vec<ToolchainRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, archive_filename, archive_sha256, install_path, installed_hash, installed_at
             FROM toolchains
             ORDER BY installed_at DESC",
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
