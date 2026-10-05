#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use qbot_ops::{
    BackupConfig, BackupError, PgTarget, check_tools, create_verified_backup, newest_backup_age,
    rotate_backups,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qbot-ops-backup-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A dump tool that writes some bytes to the `-f` file, recording how it was called.
const GOOD_DUMP: &str = r#"
out=""
while [ $# -gt 0 ]; do
  case "$1" in -f) out="$2"; shift;; esac
  echo "$1" >> "$(dirname "$0")/dump-args"
  shift
done
echo "$PGPASSWORD" > "$(dirname "$0")/dump-password"
printf 'PGDMP-bytes' > "$out"
"#;

const GOOD_RESTORE: &str = r#"
echo "215; 1259 16401 TABLE public chat_line qbot"
echo "216; 1259 16402 TABLE public member_number qbot"
"#;

fn config(bin: &Path, out: &Path) -> BackupConfig {
    BackupConfig {
        dir: out.to_path_buf(),
        prefix: "qbot".into(),
        keep: 2,
        timeout: Duration::from_secs(5),
        bin_dir: Some(bin.to_path_buf()),
        target: PgTarget {
            host: "db".into(),
            port: 5432,
            user: "qbot".into(),
            database: "qbot".into(),
            password: "s3cret".into(),
        },
    }
}

#[tokio::test]
async fn a_verified_dump_is_published_and_the_password_stays_off_the_command_line() {
    let (bin, out) = (scratch(), scratch());
    script(&bin, "pg_dump", GOOD_DUMP);
    script(&bin, "pg_restore", GOOD_RESTORE);
    let made = create_verified_backup(&config(&bin, &out)).await.unwrap();
    assert!(
        made.path.starts_with(&out) && made.path.extension().unwrap() == "dump",
        "{made:?}"
    );
    assert_eq!(made.bytes, 11);
    assert_eq!(std::fs::read_to_string(&made.path).unwrap(), "PGDMP-bytes");
    let names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 1, "no partial file left: {names:?}");

    let args = std::fs::read_to_string(bin.join("dump-args")).unwrap();
    assert!(
        args.contains("-Fc")
            && args.contains("--no-password")
            && args.contains("db")
            && !args.contains("s3cret"),
        "{args}"
    );
    assert_eq!(
        std::fs::read_to_string(bin.join("dump-password"))
            .unwrap()
            .trim(),
        "s3cret",
        "passed through the environment"
    );
    assert!(newest_backup_age(&out, "qbot").unwrap() < Duration::from_secs(60));
}

#[tokio::test]
async fn failures_are_reported_and_leave_nothing_behind() {
    let (bin, out) = (scratch(), scratch());
    let leftovers = |out: &Path| std::fs::read_dir(out).unwrap().count();

    script(
        &bin,
        "pg_dump",
        "echo 'FATAL: password authentication failed' >&2; exit 1",
    );
    script(&bin, "pg_restore", GOOD_RESTORE);
    let error = create_verified_backup(&config(&bin, &out))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, BackupError::Dump { code, stderr } if code == "1" && stderr.contains("password authentication")),
        "{error}"
    );
    assert_eq!(leftovers(&out), 0);

    // A dump that cannot be listed, or that lacks the chat archive, is not a backup.
    script(&bin, "pg_dump", GOOD_DUMP);
    script(
        &bin,
        "pg_restore",
        "echo 'pg_restore: error: not a valid archive' >&2; exit 1",
    );
    assert!(
        matches!(create_verified_backup(&config(&bin, &out)).await, Err(BackupError::Verification(m)) if m.contains("not a valid archive"))
    );
    script(
        &bin,
        "pg_restore",
        "echo '1; 1 1 TABLE public something_else x'",
    );
    assert!(
        matches!(create_verified_backup(&config(&bin, &out)).await, Err(BackupError::Verification(m)) if m.contains("chat archive"))
    );
    script(
        &bin,
        "pg_dump",
        "out=\"\"; while [ $# -gt 0 ]; do case \"$1\" in -f) out=\"$2\";; esac; shift; done; : > \"$out\"",
    );
    script(&bin, "pg_restore", GOOD_RESTORE);
    assert!(
        matches!(create_verified_backup(&config(&bin, &out)).await, Err(BackupError::Verification(m)) if m.contains("empty"))
    );
    assert_eq!(leftovers(&out), 0, "no partial or rejected file survives");
}

#[tokio::test]
async fn a_hung_tool_is_stopped_at_the_deadline() {
    let (bin, out) = (scratch(), scratch());
    script(&bin, "pg_dump", "sleep 30");
    script(&bin, "pg_restore", GOOD_RESTORE);
    let mut cfg = config(&bin, &out);
    cfg.timeout = Duration::from_millis(300);
    let started = std::time::Instant::now();
    let error = create_verified_backup(&cfg).await.unwrap_err();
    assert!(
        matches!(
            error,
            BackupError::TimedOut {
                tool: "pg_dump",
                ..
            }
        ),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it did not wait for the sleep"
    );
}

#[tokio::test]
async fn missing_tools_are_found_out_at_startup() {
    let (bin, out) = (scratch(), scratch());
    let error = check_tools(&config(&bin, &out)).await.unwrap_err();
    assert!(
        matches!(
            error,
            BackupError::Spawn {
                tool: "pg_dump",
                ..
            }
        ),
        "{error}"
    );
    script(&bin, "pg_dump", "echo pg_dump 17");
    script(&bin, "pg_restore", "echo pg_restore 17");
    check_tools(&config(&bin, &out)).await.unwrap();
}

#[test]
fn rotation_keeps_the_newest_and_never_touches_other_files() {
    let out = scratch();
    for stamp in [
        "20260101-000000",
        "20260102-000000",
        "20260103-000000",
        "20260104-000000",
    ] {
        std::fs::write(out.join(format!("qbot-{stamp}.dump")), b"x").unwrap();
    }
    std::fs::write(out.join("notes.txt"), b"keep me").unwrap();
    std::fs::write(out.join("other-20260101-000000.dump"), b"another prefix").unwrap();
    assert_eq!(rotate_backups(&out, "qbot", 2), 2);
    let mut left: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "notes.txt",
            "other-20260101-000000.dump",
            "qbot-20260103-000000.dump",
            "qbot-20260104-000000.dump"
        ]
    );
    assert_eq!(
        rotate_backups(&out, "qbot", 0),
        1,
        "at least one is always kept"
    );
    assert!(newest_backup_age(&scratch(), "qbot").is_none());
}
