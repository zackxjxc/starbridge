//! Stable user profile, portable-profile migration, and a process-lifetime lock.
use fs2::FileExt;
use std::{fs::{File, OpenOptions}, path::{Path, PathBuf}};

pub struct DataDirectory {
    pub root: PathBuf,
    _instance: File,
}

pub fn default_root() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("无法确定系统 LocalAppData 目录，请检查当前系统用户环境"))?;
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("无法确定当前用户目录"))?.join("Library/Application Support");
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| anyhow::anyhow!("无法确定当前用户数据目录，请使用 --data-dir 指定独立实例"))?;
    anyhow::ensure!(base.is_absolute(), "系统用户数据目录必须是绝对路径");
    Ok(base.join("StarBridge"))
}

fn same_path(one: &Path, two: &Path) -> bool {
    let one = std::fs::canonicalize(one).unwrap_or_else(|_| one.to_path_buf());
    let two = std::fs::canonicalize(two).unwrap_or_else(|_| two.to_path_buf());
    #[cfg(windows)]
    { one.to_string_lossy().eq_ignore_ascii_case(&two.to_string_lossy()) }
    #[cfg(not(windows))]
    { one == two }
}

pub(crate) fn is_default(root: &Path) -> bool {
    default_root().is_ok_and(|default| same_path(root, &default))
}

pub fn prepare(explicit: Option<PathBuf>, legacy_bases: &[PathBuf]) -> anyhow::Result<DataDirectory> {
    prepare_with_import(explicit, None, legacy_bases)
}

pub fn prepare_with_import(explicit: Option<PathBuf>, import: Option<PathBuf>, legacy_bases: &[PathBuf]) -> anyhow::Result<DataDirectory> {
    let isolated = explicit.is_some();
    if isolated && import.is_some() { anyhow::bail!("--data-dir 与 --import-data-dir 不能同时使用"); }
    let default = default_root();
    let root = match explicit { Some(path) => std::path::absolute(path)?, None => default.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?.clone() };
    if isolated && default.as_ref().is_ok_and(|default| same_path(&root, default)) {
        anyhow::bail!("--data-dir 用于独立测试实例，请选择默认用户数据目录以外的目录");
    }
    let import = import.map(std::path::absolute).transpose()?;
    prepare_at_with_import(root, isolated, legacy_bases, import.as_deref())
}

#[cfg(test)]
fn prepare_at(root: PathBuf, isolated: bool, legacy_bases: &[PathBuf]) -> anyhow::Result<DataDirectory> {
    prepare_at_with_import(root, isolated, legacy_bases, None)
}

fn prepare_at_with_import(root: PathBuf, isolated: bool, legacy_bases: &[PathBuf], import: Option<&Path>) -> anyhow::Result<DataDirectory> {
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let instance = lock(&root)?;
    if root.join("migrated-to.json").is_file() {
        anyhow::bail!("此旧数据目录已迁移到系统用户目录，请省略 --data-dir 启动；不要重新使用旧副本");
    }
    if let Some(source) = import {
        if has_identity(&root) { anyhow::bail!("默认目录已有设备身份，不能覆盖导入；请先退出并备份、移走现有默认数据目录，再选择要保留的旧身份"); }
        if same_path(&root, source) || !has_identity(source) {
            anyhow::bail!("请选择包含旧设备身份的其他数据目录进行导入");
        }
        migrate_selected(&root, source)?;
    } else if isolated {
        if !root.join("instance-scope.json").is_file() {
            std::fs::write(root.join("instance-scope.json"), b"{\"version\":1,\"isolated\":true}\n")?;
        }
    } else if !has_identity(&root) {
        migrate_legacy(&root, legacy_bases)?;
    }
    Ok(DataDirectory { root, _instance: instance })
}

fn lock(root: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(root.join("instance.lock"))?;
    file.try_lock_exclusive().map_err(|_| anyhow::anyhow!("此数据目录已有客户端运行，请先退出原实例"))?;
    Ok(file)
}

pub(crate) fn is_instance_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.to_string().contains("此数据目录已有客户端运行"))
}

fn has_identity(root: &Path) -> bool {
    root.join("identity.json").is_file() || root.join("identity.secure").is_file()
}

fn migrate_legacy(root: &Path, bases: &[PathBuf]) -> anyhow::Result<()> {
    let mut candidates = Vec::new();
    for base in bases {
        for name in ["node", "controller", "executor"] {
            let candidate = base.join("state").join(name);
            if !has_identity(&candidate) || candidate.join("migrated-to.json").is_file() || same_path(root, &candidate) { continue; }
            if !candidates.iter().any(|previous: &PathBuf| same_path(previous, &candidate)) {
                candidates.push(candidate);
            }
        }
    }
    if candidates.len() > 1 {
        anyhow::bail!("发现多个旧设备身份，未自动合并。请先备份各旧 state 目录，再使用 --import-data-dir 指定要保留的旧目录迁移到默认用户目录；其他身份保留供后续核对");
    }
    let Some(source) = candidates.first() else { return Ok(()); };
    migrate_selected(root, source)
}

fn migrate_selected(root: &Path, source: &Path) -> anyhow::Result<()> {
    if source.join("migrated-to.json").is_file() {
        anyhow::bail!("所选旧目录已经迁移，请使用默认用户目录中的身份；需要恢复时先核对并备份原迁移结果");
    }
    let _source_instance = lock(source).map_err(|_| anyhow::anyhow!("旧客户端仍在运行，请退出后重试迁移"))?;
    // Source remains recoverable. Credentials are sealed by load_identity/load_auth before use.
    // Identity files are copied last, so an interrupted import can be retried safely.
    copy_profile(source, root, true)?;
    let marker = serde_json::to_vec(&serde_json::json!({"version":1,"destination":root}))?;
    std::fs::write(source.join("migrated-to.json"), marker)?;
    std::fs::write(root.join("legacy-migration.json"), b"{\"version\":1,\"complete\":true}\n")?;
    Ok(())
}

fn copy_profile(source: &Path, destination: &Path, top: bool) -> anyhow::Result<()> {
    let mut entries = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| matches!(entry.file_name().to_str(), Some("identity.json" | "identity.secure")));
    for entry in entries {
        let name = entry.file_name();
        if top && matches!(name.to_str(), Some("instance.lock" | "instance-scope.json" | "migrated-to.json" | "legacy-migration.json")) { continue; }
        let kind = entry.file_type()?;
        if kind.is_symlink() { anyhow::bail!("旧数据目录包含符号链接，请先备份并手动选择需要保留的数据"); }
        let target = destination.join(name);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_profile(&entry.path(), &target, false)?;
            std::fs::set_permissions(&target, entry.metadata()?.permissions())?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        let root = std::env::temp_dir().join(format!("starbridge-directory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap(); root
    }
    #[test]
    fn program_directory_does_not_change_default_path() {
        let default = default_root().unwrap();
        assert!(default.is_absolute());
        assert!(default.ends_with("StarBridge"));
        assert_eq!(default_root().unwrap(), default);
    }
    #[test]
    fn data_directory_lock_excludes_concurrent_instances() {
        let root = temp();
        let first = prepare_at(root.clone(), true, &[]).unwrap();
        assert!(prepare_at(root.clone(), true, &[]).is_err());
        drop(first);
        drop(prepare_at(root.clone(), true, &[]).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn one_legacy_profile_preserves_configuration_keys_and_identity() {
        let base = temp(); let old = base.join("state/controller"); let stable = base.join("stable");
        std::fs::create_dir_all(old.join("ssh")).unwrap();
        std::fs::write(old.join("identity.json"), b"legacy-identity").unwrap();
        std::fs::write(old.join("config.json"), b"legacy-name-config").unwrap();
        std::fs::write(old.join("ssh/key"), b"legacy-ssh-key").unwrap();
        let profile = prepare_at(stable.clone(), false, &[base.clone()]).unwrap();
        assert_eq!(std::fs::read(stable.join("identity.json")).unwrap(), b"legacy-identity");
        assert_eq!(std::fs::read(stable.join("config.json")).unwrap(), b"legacy-name-config");
        assert_eq!(std::fs::read(stable.join("ssh/key")).unwrap(), b"legacy-ssh-key");
        assert!(prepare_at(old.clone(), true, &[]).is_err());
        drop(profile);
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn multiple_legacy_profiles_require_explicit_resolution() {
        let base = temp(); let stable = base.join("stable");
        for name in ["controller", "executor"] {
            let old = base.join("state").join(name); std::fs::create_dir_all(&old).unwrap();
            std::fs::write(old.join("identity.json"), name).unwrap();
        }
        assert!(prepare_at(stable.clone(), false, &[base.clone()]).is_err());
        assert!(!has_identity(&stable));
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn explicit_import_selects_one_profile_and_cannot_replace_an_existing_identity() {
        let base = temp(); let stable = base.join("stable"); let selected = base.join("state/controller");
        for name in ["controller", "executor"] {
            let old = base.join("state").join(name); std::fs::create_dir_all(&old).unwrap();
            std::fs::write(old.join("identity.json"), name).unwrap();
        }
        let imported = prepare_at_with_import(stable.clone(), false, &[base.clone()], Some(&selected)).unwrap();
        assert_eq!(std::fs::read(stable.join("identity.json")).unwrap(), b"controller");
        assert!(base.join("state/executor/identity.json").is_file());
        drop(imported);
        let other = base.join("state/executor");
        assert!(prepare_at_with_import(stable.clone(), false, &[], Some(&other)).is_err());
        assert_eq!(std::fs::read(stable.join("identity.json")).unwrap(), b"controller");
        std::fs::remove_dir_all(base).unwrap();
    }
}
