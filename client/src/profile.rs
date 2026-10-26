//! Typed local-profile failures and private, reversible credential quarantine.
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKind { Authorization, CorruptCredential, Identity, Machine, Storage }

#[derive(Debug)]
struct ProfileError { kind: FailureKind, message: String }

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { formatter.write_str(&self.message) }
}
impl std::error::Error for ProfileError {}

pub(crate) fn failure(kind: FailureKind, message: impl Into<String>) -> anyhow::Error {
    ProfileError { kind, message: message.into() }.into()
}

pub(crate) fn kind(error: &anyhow::Error) -> FailureKind {
    error.downcast_ref::<ProfileError>().map_or(FailureKind::Storage, |error| error.kind)
}

#[cfg(feature = "desktop")]
#[derive(Clone, Debug, Serialize)]
pub(crate) struct StartupIssue {
    pub kind: FailureKind,
    pub message: String,
    pub account_recovery: bool,
}

#[cfg(feature = "desktop")]
impl StartupIssue {
    pub fn from_error(error: &anyhow::Error) -> Self {
        let kind = kind(error);
        Self { kind, message: error.to_string(), account_recovery: matches!(kind, FailureKind::Identity | FailureKind::Machine | FailureKind::CorruptCredential) }
    }
}

pub(crate) struct ProfileBackup { root: PathBuf, directory: PathBuf, moved: Vec<String>, config: bool }

impl ProfileBackup {
    /// Only fixed profile basenames are accepted; never traverse arbitrary paths.
    pub async fn create(root: &Path, names: &[&str], copy_config: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(names.iter().all(|name| matches!(*name, "identity.secure" | "identity.json" | "auth.secure" | "auth.json" | "machine.json")), "不允许移动此恢复文件");
        let parent = root.join("recovery-backups");
        match tokio::fs::symlink_metadata(&parent).await {
            Ok(metadata) => anyhow::ensure!(metadata.file_type().is_dir(), "恢复备份目录无效，请检查数据目录"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => tokio::fs::create_dir(&parent).await?,
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        tokio::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o700)).await?;
        let directory = parent.join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir(&directory).await?;
        #[cfg(unix)]
        tokio::fs::set_permissions(&directory, std::os::unix::fs::PermissionsExt::from_mode(0o700)).await?;
        let mut backup = Self { root: root.into(), directory, moved: Vec::new(), config: false };
        if copy_config && root.join("config.json").is_file() {
            tokio::fs::copy(root.join("config.json"), backup.directory.join("config.json")).await?;
            backup.config = true;
        }
        for name in names {
            let source = root.join(name);
            match tokio::fs::symlink_metadata(&source).await {
                Ok(metadata) if metadata.file_type().is_file() => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {
                    backup.rollback().await?;
                    anyhow::bail!("无法备份原设备资料，请检查数据目录权限");
                }
            }
            if let Err(error) = tokio::fs::rename(&source, backup.directory.join(name)).await {
                backup.rollback().await?;
                return Err(error.into());
            }
            backup.moved.push((*name).into());
        }
        Ok(backup)
    }

    pub async fn rollback(&self) -> anyhow::Result<()> {
        for name in ["identity.secure", "identity.json", "auth.secure", "auth.json", "machine.json"] {
            // Leave files outside the reset scope untouched.
            if !self.moved.iter().any(|moved| moved == name) { continue; }
            tokio::fs::copy(self.directory.join(name), self.root.join(name)).await?;
        }
        if self.config { tokio::fs::copy(self.directory.join("config.json"), self.root.join("config.json")).await?; }
        Ok(())
    }

    #[cfg(feature = "desktop")]
    pub async fn rollback_recovery(&self, reset_machine: bool) -> anyhow::Result<()> {
        let mut names = vec!["identity.secure", "identity.json", "auth.secure", "auth.json"];
        if reset_machine { names.push("machine.json"); }
        for name in ["identity", "auth"] { crate::secure_store::remove_secret(&self.root, name).await?; }
        for name in names {
            match tokio::fs::remove_file(self.root.join(name)).await {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        if !self.config {
            match tokio::fs::remove_file(self.root.join("config.json")).await {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        self.rollback().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn partial_backup_failure_restores_original_files_and_keeps_private_backup() {
        let root = std::env::temp_dir().join(format!("starbridge-backup-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(root.join("auth.secure")).await.unwrap();
        tokio::fs::write(root.join("identity.secure"), b"original-reference").await.unwrap();
        assert!(ProfileBackup::create(&root, &["identity.secure", "auth.secure"], false).await.is_err());
        assert_eq!(tokio::fs::read(root.join("identity.secure")).await.unwrap(), b"original-reference");
        let backup = std::fs::read_dir(root.join("recovery-backups")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(tokio::fs::read(backup.join("identity.secure")).await.unwrap(), b"original-reference");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o700);
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
