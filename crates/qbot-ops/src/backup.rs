//! Verified database backups.
//!
//! A dump is written next to its final name, checked by listing it with `pg_restore` (it must
//! parse and contain the chat archive), and only then renamed into place, so a half-written or
//! unreadable file is never mistaken for a backup. Old dumps are rotated away after a new one has
//! succeeded, never before.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use jiff::Timestamp;
use tokio::process::Command;

/// The table a usable backup must contain.
const MARKER_TABLE: &str = " public chat_line ";
/// How much of a tool's complaint is kept in the error.
const STDERR_KEPT: usize = 500;

#[derive(Debug, Clone)]
pub struct PgTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    /// Passed to the tool through its environment, never on the command line.
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    pub prefix: String,
    /// Dumps kept after rotation (at least one).
    pub keep: usize,
    /// How long one dump or verification may take.
    pub timeout: Duration,
    /// Where `pg_dump` and `pg_restore` live; `None` finds them on `PATH`.
    pub bin_dir: Option<PathBuf>,
    pub target: PgTarget,
}

impl BackupConfig {
    /// Dumps of `target` into `dir`, keeping `keep`, with the client tools on `PATH`. A dump or
    /// verification of a group-chat database finishes in minutes; fifteen is a hung tool.
    pub fn new(dir: PathBuf, keep: usize, target: PgTarget) -> Self {
        Self {
            dir,
            prefix: "qbot".into(),
            keep,
            timeout: Duration::from_secs(15 * 60),
            bin_dir: None,
            target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackupError {
    #[error("cannot prepare the backup directory {path}: {reason}")]
    Directory { path: PathBuf, reason: String },
    #[error("could not run {tool}: {reason}")]
    Spawn { tool: &'static str, reason: String },
    #[error("{tool} did not finish within {after:?} and was stopped")]
    TimedOut { tool: &'static str, after: Duration },
    #[error("pg_dump failed ({code}): {stderr}")]
    Dump { code: String, stderr: String },
    #[error("the dump did not pass verification: {0}")]
    Verification(String),
    #[error("backup file {0} already exists")]
    Exists(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBackup {
    pub path: PathBuf,
    pub bytes: u64,
}

impl BackupConfig {
    fn tool(&self, name: &str) -> PathBuf {
        self.bin_dir
            .as_ref()
            .map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
    }
}

struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run(
    tool: &'static str,
    mut command: Command,
    timeout: Duration,
) -> Result<Finished, BackupError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|e| BackupError::Spawn {
        tool,
        reason: e.to_string(),
    })?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| BackupError::TimedOut {
            tool,
            after: timeout,
        })?
        .map_err(|e| BackupError::Spawn {
            tool,
            reason: e.to_string(),
        })?;
    let tail = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        text.chars().take(STDERR_KEPT).collect::<String>()
    };
    Ok(Finished {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: tail(&output.stderr),
    })
}

/// Check that the tools a backup needs can be run. Used at startup so a missing client is an
/// error then, not at two in the morning.
pub async fn check_tools(cfg: &BackupConfig) -> Result<(), BackupError> {
    for tool in ["pg_dump", "pg_restore"] {
        let mut command = Command::new(cfg.tool(tool));
        command.arg("--version");
        let finished = run(tool, command, Duration::from_secs(10)).await?;
        if finished.code != Some(0) {
            return Err(BackupError::Spawn {
                tool,
                reason: format!("`--version` exited with {:?}", finished.code),
            });
        }
    }
    Ok(())
}

pub async fn create_verified_backup(cfg: &BackupConfig) -> Result<VerifiedBackup, BackupError> {
    tokio::fs::create_dir_all(&cfg.dir)
        .await
        .map_err(|e| BackupError::Directory {
            path: cfg.dir.clone(),
            reason: e.to_string(),
        })?;
    let stamp = Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
    let target = cfg.dir.join(format!("{}-{stamp}.dump", cfg.prefix));
    let partial = cfg.dir.join(format!("{}-{stamp}.partial", cfg.prefix));
    if target.exists() {
        return Err(BackupError::Exists(target));
    }
    let outcome = dump_and_verify(cfg, &partial).await;
    let result = match outcome {
        Ok(bytes) => tokio::fs::rename(&partial, &target)
            .await
            .map(|()| VerifiedBackup {
                path: target.clone(),
                bytes,
            })
            .map_err(|e| BackupError::Directory {
                path: target.clone(),
                reason: e.to_string(),
            }),
        Err(error) => Err(error),
    };
    // Whatever happened, no partial file is left behind.
    let _ = tokio::fs::remove_file(&partial).await;
    result
}

async fn dump_and_verify(cfg: &BackupConfig, partial: &Path) -> Result<u64, BackupError> {
    let t = &cfg.target;
    let mut dump = Command::new(cfg.tool("pg_dump"));
    dump.args([
        "-Fc",
        "--no-password",
        "-h",
        &t.host,
        "-p",
        &t.port.to_string(),
        "-U",
        &t.user,
        "-d",
        &t.database,
        "-f",
    ])
    .arg(partial)
    .env("PGPASSWORD", &t.password);
    let finished = run("pg_dump", dump, cfg.timeout).await?;
    if finished.code != Some(0) {
        return Err(BackupError::Dump {
            code: finished
                .code
                .map_or_else(|| "killed by a signal".to_owned(), |c| c.to_string()),
            stderr: finished.stderr,
        });
    }
    let bytes = tokio::fs::metadata(partial).await.map_or(0, |m| m.len());
    if bytes == 0 {
        return Err(BackupError::Verification("the dump file is empty".into()));
    }
    let mut list = Command::new(cfg.tool("pg_restore"));
    list.arg("--list").arg(partial);
    let listed = run("pg_restore", list, cfg.timeout).await?;
    if listed.code != Some(0) {
        return Err(BackupError::Verification(format!(
            "pg_restore --list failed: {}",
            listed.stderr
        )));
    }
    if !listed.stdout.contains(MARKER_TABLE) {
        return Err(BackupError::Verification(
            "the dump does not contain the chat archive".into(),
        ));
    }
    Ok(bytes)
}

fn dumps(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{prefix}-")) && n.ends_with(".dump"))
        })
        .collect();
    // Names carry a sortable UTC stamp, newest last.
    found.sort();
    found
}

/// Delete all but the newest `keep` dumps. Returns how many were removed.
pub fn rotate_backups(dir: &Path, prefix: &str, keep: usize) -> usize {
    let all = dumps(dir, prefix);
    let surplus = all.len().saturating_sub(keep.max(1));
    all.into_iter()
        .take(surplus)
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// How long ago the newest dump was written, or `None` if there is none.
pub fn newest_backup_age(dir: &Path, prefix: &str) -> Option<Duration> {
    let newest = dumps(dir, prefix).pop()?;
    let modified = std::fs::metadata(newest).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(modified).ok()
}
