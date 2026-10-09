//! On-disk key-value store for the SDK.
//!
//! A single redb file ([`FileStore`]): every batch the SDK writes is one ACID transaction, so a
//! crash mid-save cannot leave a truncated tree or pending-POI entry behind.

use std::{io, path::Path};

use kohaku_kv_store::{Store, file::FileStore};

/// The database file in `data_dir`. The SDK's key layout changed with kohaku-rs, so the file
/// name is new: state written by earlier versions (`db-v2/`) is not read and is rebuilt by
/// syncing.
pub fn open(data_dir: &Path) -> io::Result<Store> {
    std::fs::create_dir_all(data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Holds decrypted notes and nullifying keys.
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let store = FileStore::open(data_dir.join("db-v3.redb")).map_err(io::Error::other)?;
    Ok(Store::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn long_keys_round_trip() {
        let dir = std::env::temp_dir().join(format!("rgw-db-test-{}", std::process::id()));
        let db = open(&dir).unwrap();
        let key = vec![0xabu8; 400];
        assert_eq!(db.get(&key).await.unwrap(), None);
        db.put(&key, b"one").await.unwrap();
        db.put(&key, b"two").await.unwrap();
        assert_eq!(db.get(&key).await.unwrap(), Some(b"two".to_vec()));
        db.delete(&key).await.unwrap();
        assert_eq!(db.get(&key).await.unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
