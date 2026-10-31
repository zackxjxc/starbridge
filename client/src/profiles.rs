//! Automatic multi-login profile allocation for desktop launches.
use crate::data_directory::{self, DataDirectory};
use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const INDEX_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
struct ProfileIndex {
    version: u32,
    last_profile_id: Option<String>,
    profiles: Vec<ProfileRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProfileRecord {
    id: String,
    relative_root: String,
    last_username: String,
    last_used_at: u64,
}

impl Default for ProfileIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            last_profile_id: None,
            profiles: Vec::new(),
        }
    }
}

pub struct ProfileLaunch {
    pub profile_id: String,
    pub directory: DataDirectory,
}

struct RegistryLock {
    _file: File,
}

pub fn prepare_auto(profile_id: Option<&str>, new_profile: bool) -> anyhow::Result<ProfileLaunch> {
    prepare_auto_at(&data_directory::default_root()?, profile_id, new_profile)
}

fn prepare_auto_at(
    base: &Path,
    profile_id: Option<&str>,
    new_profile: bool,
) -> anyhow::Result<ProfileLaunch> {
    std::fs::create_dir_all(&base)?;
    let _registry_lock = registry_lock(&base).context("无法锁定登录档案索引")?;
    let mut index = load_or_initialize(&base).context("无法读取登录档案索引")?;

    if let Some(id) = profile_id {
        let record = index
            .profiles
            .iter()
            .find(|record| record.id == id)
            .ok_or_else(|| anyhow::anyhow!("找不到指定的 StarBridge 登录档案"))?
            .clone();
        return open_record(&base, &mut index, record);
    }

    if !new_profile {
        let mut records = index.profiles.clone();
        records.sort_by_key(|record| std::cmp::Reverse(record.last_used_at));
        for record in records {
            match open_record(&base, &mut index, record) {
                Ok(launch) => return Ok(launch),
                Err(error) => {
                    if data_directory::is_instance_busy(&error) {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    let record = create_record(&base)?;
    open_record(&base, &mut index, record)
}

fn registry_lock(base: &Path) -> anyhow::Result<RegistryLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(base.join("profiles.lock"))
        .context("无法打开登录档案锁文件")?;
    file.lock_exclusive().context("无法取得登录档案锁")?;
    Ok(RegistryLock { _file: file })
}

fn index_path(base: &Path) -> PathBuf {
    base.join("profiles.json")
}

fn load_or_initialize(base: &Path) -> anyhow::Result<ProfileIndex> {
    let path = index_path(base);
    if !path.is_file() {
        purge_legacy_contents(base).context("无法清理旧 StarBridge 数据")?;
        let mut index = ProfileIndex::default();
        index.profiles.push(ProfileRecord {
            id: "default".into(),
            relative_root: "profiles/default".into(),
            last_username: String::new(),
            last_used_at: 0,
        });
        std::fs::create_dir_all(base.join("profiles/default")).context("无法创建默认登录档案")?;
        write_index(base, &index).context("无法写入登录档案索引")?;
        return Ok(index);
    }
    let index: ProfileIndex =
        serde_json::from_slice(&std::fs::read(path).context("无法读取登录档案索引文件")?)?;
    anyhow::ensure!(
        index.version == INDEX_VERSION,
        "不支持的 StarBridge 登录档案版本"
    );
    anyhow::ensure!(!index.profiles.is_empty(), "StarBridge 登录档案索引为空");
    Ok(index)
}

fn purge_legacy_contents(base: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("profiles.lock") {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::create_dir_all(base.join("profiles"))?;
    Ok(())
}

fn create_record(base: &Path) -> anyhow::Result<ProfileRecord> {
    let id = uuid::Uuid::new_v4().to_string();
    let relative_root = format!("profiles/{id}");
    std::fs::create_dir_all(base.join(&relative_root))?;
    Ok(ProfileRecord {
        id,
        relative_root,
        last_username: String::new(),
        last_used_at: 0,
    })
}

fn open_record(
    base: &Path,
    index: &mut ProfileIndex,
    record: ProfileRecord,
) -> anyhow::Result<ProfileLaunch> {
    let root = safe_profile_root(base, &record.relative_root)?;
    let directory = data_directory::prepare_with_import(Some(root.clone()), None, &[])
        .with_context(|| format!("无法打开登录档案目录 {}", root.display()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Some(existing) = index.profiles.iter_mut().find(|item| item.id == record.id) {
        existing.last_used_at = now;
    } else {
        let mut updated = record.clone();
        updated.last_used_at = now;
        index.profiles.push(updated);
    }
    index.last_profile_id = Some(record.id.clone());
    write_index(base, index)?;
    Ok(ProfileLaunch {
        profile_id: record.id,
        directory,
    })
}

fn safe_profile_root(base: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let root = base.join(relative);
    let profiles = base
        .join("profiles")
        .canonicalize()
        .unwrap_or_else(|_| base.join("profiles"));
    let parent = root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("登录档案路径无效"))?;
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    anyhow::ensure!(parent == profiles, "登录档案路径无效");
    Ok(root)
}

fn write_index(base: &Path, index: &ProfileIndex) -> anyhow::Result<()> {
    let path = index_path(base);
    std::fs::write(path, serde_json::to_vec_pretty(index)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("starbridge-profiles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn first_launch_creates_default_profile_and_second_launch_gets_new_profile_when_busy() {
        let root = base();
        let first = prepare_auto_at(&root.join("StarBridge"), None, false).unwrap();
        let second = prepare_auto_at(&root.join("StarBridge"), None, false).unwrap();
        assert_ne!(first.profile_id, second.profile_id);
        drop(second.directory);
        drop(first.directory);
        let reopened = prepare_auto_at(&root.join("StarBridge"), None, false).unwrap();
        assert_eq!(reopened.profile_id, first.profile_id);
        drop(reopened.directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_new_profile_is_independent() {
        let root = base();
        let base = root.join("StarBridge");
        let first = prepare_auto_at(&base, None, false).unwrap();
        let second = prepare_auto_at(&base, None, true).unwrap();
        assert_ne!(first.profile_id, second.profile_id);
        drop(second.directory);
        drop(first.directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn three_concurrent_launches_get_three_independent_profiles() {
        let root = base();
        let base = root.join("StarBridge");
        let first = prepare_auto_at(&base, None, false).unwrap();
        let second = prepare_auto_at(&base, None, false).unwrap();
        let third = prepare_auto_at(&base, None, false).unwrap();
        assert_ne!(first.profile_id, second.profile_id);
        assert_ne!(first.profile_id, third.profile_id);
        assert_ne!(second.profile_id, third.profile_id);
        assert_ne!(first.directory.root, second.directory.root);
        assert_ne!(second.directory.root, third.directory.root);
        drop(third.directory);
        drop(second.directory);
        drop(first.directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_launch_removes_legacy_profile_files_before_creating_store() {
        let root = base();
        let base = root.join("StarBridge");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.json"), b"legacy").unwrap();
        let launch = prepare_auto_at(&base, None, false).unwrap();
        assert!(!base.join("config.json").exists());
        assert!(base.join("profiles.json").is_file());
        drop(launch.directory);
        std::fs::remove_dir_all(root).unwrap();
    }
}
