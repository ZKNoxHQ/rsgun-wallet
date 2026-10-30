//! On-disk snapshot of the SDK state.
//!
//! The SDK keeps everything in memory ([`RailgunState`]) and leaves persistence to the
//! application: the wallet writes a snapshot to `<data_dir>/railgun-state.bin` after every
//! command. A snapshot is always a consistent checkpoint (the SDK publishes each validated
//! batch atomically), and the file is replaced atomically, so a crash mid-save leaves the
//! previous checkpoint in place rather than a truncated tree or pending-POI entry.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use railgun::RailgunState;

/// The snapshot file in `data_dir`. State written by earlier versions of the wallet
/// (`db-v2/`, `db-v3.redb`) is not read, those files are left alone and rebuilt by syncing.
pub const SNAPSHOT_FILE: &str = "railgun-state.bin";

pub fn snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SNAPSHOT_FILE)
}

fn temp_path(data_dir: &Path) -> PathBuf {
    data_dir.join(format!("{SNAPSHOT_FILE}.tmp"))
}

/// Restores the state from the snapshot in `data_dir`, or starts empty when there is none. A
/// snapshot that exists but cannot be read is an error, never a silent resync: the file is
/// named so the user can decide what to do with it.
pub fn open(data_dir: &Path) -> Result<RailgunState> {
    fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Holds decrypted notes and nullifying keys.
        fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = snapshot_path(data_dir);
    if !path.exists() {
        return Ok(RailgunState::default());
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot = postcard::from_bytes(&bytes)
        .with_context(|| format!("{} is not a valid wallet snapshot", path.display()))?;
    RailgunState::from_snapshot(snapshot)
        .with_context(|| format!("{} cannot be restored", path.display()))
}

/// Writes a snapshot of `state` to `data_dir`, see [`save_bytes`].
pub fn save(data_dir: &Path, state: &RailgunState) -> Result<()> {
    let snapshot = state.snapshot().context("exporting the wallet state")?;
    let bytes = postcard::to_allocvec(&snapshot).context("encoding the wallet snapshot")?;
    save_bytes(data_dir, &bytes)
}

/// Writes an already encoded snapshot to `data_dir`, replacing the previous one atomically: the
/// bytes go to a temporary file, are flushed to disk, then renamed over the snapshot.
pub fn save_bytes(data_dir: &Path, bytes: &[u8]) -> Result<()> {
    let path = snapshot_path(data_dir);
    let tmp = temp_path(data_dir);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, &path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

/// Deletes the snapshot of `data_dir` (and a temporary file left by an interrupted save).
/// Returns the names of the files removed.
pub fn delete(data_dir: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for path in [snapshot_path(data_dir), temp_path(data_dir)] {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                removed.push(name.to_string());
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let dir = std::env::temp_dir().join(format!("rgw-db-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // No file yet: an empty state, and nothing written until asked.
        let state = open(&dir).unwrap();
        assert!(!snapshot_path(&dir).exists());
        save(&dir, &state).unwrap();
        assert!(snapshot_path(&dir).exists());
        assert!(!temp_path(&dir).exists());

        // What was saved comes back as the same snapshot.
        let restored = open(&dir).unwrap();
        let before = postcard::to_allocvec(&state.snapshot().unwrap()).unwrap();
        let after = postcard::to_allocvec(&restored.snapshot().unwrap()).unwrap();
        assert_eq!(before, after);

        // A corrupt file is reported, naming it, instead of being replaced by an empty state.
        fs::write(snapshot_path(&dir), b"not a snapshot").unwrap();
        let Err(err) = open(&dir) else {
            panic!("a corrupt snapshot must not open");
        };
        assert!(format!("{err:#}").contains(SNAPSHOT_FILE), "{err:#}");

        assert_eq!(delete(&dir).unwrap(), vec![SNAPSHOT_FILE.to_string()]);
        assert!(!snapshot_path(&dir).exists());
        assert!(delete(&dir).unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
