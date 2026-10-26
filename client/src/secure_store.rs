//! User-scoped credential persistence. Never include payloads in errors or logs.
use std::path::Path;

pub(super) async fn read_secret(root: &Path, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = match tokio::fs::read(root.join(format!("{name}.secure"))).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("无法读取已保存的凭据，请检查数据目录权限"),
    };
    #[cfg(windows)]
    { return Ok(Some(protect(&bytes, false)?)); }
    #[cfg(target_os = "macos")]
    {
        let account = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("凭据引用损坏"))?;
        return keychain::read(&account).map(Some);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Ok(Some(bytes))
}

pub(super) async fn write_secret(root: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(windows)]
    let bytes = protect(bytes, true)?;
    #[cfg(target_os = "macos")]
    let bytes = {
        use sha2::{Digest, Sha256};
        let root = std::fs::canonicalize(root)?;
        let account = format!("{:x}", Sha256::digest(format!("{}\0{name}", root.display())));
        keychain::write(&account, bytes)?;
        account.into_bytes()
    };
    let path = root.join(format!("{name}.secure"));
    write_private(&path, &bytes).await
}

pub(super) async fn remove_secret(root: &Path, name: &str) -> anyhow::Result<()> {
    let path = root.join(format!("{name}.secure"));
    #[cfg(target_os = "macos")]
    if let Ok(bytes) = tokio::fs::read(&path).await {
        let account = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("凭据引用损坏"))?;
        keychain::remove(&account)?;
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => anyhow::bail!("无法删除已保存的凭据，请检查数据目录权限"),
    }
}

pub(super) async fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let pending = path.with_extension("pending");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&pending).await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&pending, path).await?;
    Ok(())
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> anyhow::Result<Vec<u8>> {
    #[repr(C)] struct Blob { size: u32, data: *mut u8 }
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(input: *const Blob, description: *const u16, entropy: *const Blob,
            reserved: *mut std::ffi::c_void, prompt: *mut std::ffi::c_void, flags: u32, output: *mut Blob) -> i32;
        fn CryptUnprotectData(input: *const Blob, description: *mut *mut u16, entropy: *const Blob,
            reserved: *mut std::ffi::c_void, prompt: *mut std::ffi::c_void, flags: u32, output: *mut Blob) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" { fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void; }
    let input = Blob { size: u32::try_from(bytes.len())?, data: bytes.as_ptr() as *mut u8 };
    let mut output = Blob { size: 0, data: std::ptr::null_mut() };
    // Current-user DPAPI, with UI forbidden. The machine key is never an authentication key.
    let result = unsafe { if encrypt {
        CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), 1, &mut output)
    } else {
        CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), 1, &mut output)
    }};
    if result == 0 { anyhow::bail!("无法访问当前系统用户的安全凭据，请使用原系统用户运行客户端"); }
    let protected = unsafe { std::slice::from_raw_parts(output.data, output.size as usize).to_vec() };
    unsafe { LocalFree(output.data.cast()); }
    Ok(protected)
}

#[cfg(target_os = "macos")]
mod keychain {
    use std::ffi::c_void;
    const SERVICE: &[u8] = b"net.starbridge.client.credentials.v1";
    const NOT_FOUND: i32 = -25300;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainFindGenericPassword(keychain: *const c_void, service_len: u32, service: *const u8,
            account_len: u32, account: *const u8, length: *mut u32, data: *mut *mut c_void, item: *mut *mut c_void) -> i32;
        fn SecKeychainAddGenericPassword(keychain: *const c_void, service_len: u32, service: *const u8,
            account_len: u32, account: *const u8, length: u32, data: *const c_void, item: *mut *mut c_void) -> i32;
        fn SecKeychainItemModifyAttributesAndData(item: *mut c_void, attributes: *const c_void,
            length: u32, data: *const c_void) -> i32;
        fn SecKeychainItemDelete(item: *mut c_void) -> i32;
        fn SecKeychainItemFreeContent(attributes: *mut c_void, data: *mut c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" { fn CFRelease(item: *const c_void); }
    fn check(status: i32) -> anyhow::Result<()> {
        if status != 0 { anyhow::bail!("无法访问系统钥匙串，请解锁当前用户的登录钥匙串后重试（状态 {status}）"); }
        Ok(())
    }
    pub fn read(account: &str) -> anyhow::Result<Vec<u8>> {
        let mut length = 0;
        let mut data = std::ptr::null_mut();
        check(unsafe { SecKeychainFindGenericPassword(std::ptr::null(), SERVICE.len() as u32, SERVICE.as_ptr(),
            account.len() as u32, account.as_ptr(), &mut length, &mut data, std::ptr::null_mut()) })?;
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize).to_vec() };
        unsafe { SecKeychainItemFreeContent(std::ptr::null_mut(), data); }
        Ok(bytes)
    }
    pub fn write(account: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let mut item = std::ptr::null_mut();
        let status = unsafe { SecKeychainFindGenericPassword(std::ptr::null(), SERVICE.len() as u32, SERVICE.as_ptr(),
            account.len() as u32, account.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), &mut item) };
        if status == NOT_FOUND {
            return check(unsafe { SecKeychainAddGenericPassword(std::ptr::null(), SERVICE.len() as u32, SERVICE.as_ptr(),
                account.len() as u32, account.as_ptr(), bytes.len().try_into()?, bytes.as_ptr().cast(), std::ptr::null_mut()) });
        }
        check(status)?;
        let status = unsafe { SecKeychainItemModifyAttributesAndData(item, std::ptr::null(), bytes.len().try_into()?, bytes.as_ptr().cast()) };
        unsafe { CFRelease(item); }
        check(status)
    }
    pub fn remove(account: &str) -> anyhow::Result<()> {
        let mut item = std::ptr::null_mut();
        let status = unsafe { SecKeychainFindGenericPassword(std::ptr::null(), SERVICE.len() as u32, SERVICE.as_ptr(),
            account.len() as u32, account.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), &mut item) };
        if status == NOT_FOUND { return Ok(()); }
        check(status)?;
        let status = unsafe { SecKeychainItemDelete(item) };
        unsafe { CFRelease(item); }
        check(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn credentials_survive_restart_without_plaintext_on_windows() {
        let root = std::env::temp_dir().join(format!("starbridge-secure-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let payload = b"test-device-secret-do-not-log";
        write_secret(&root, "test", payload).await.unwrap();
        assert_eq!(read_secret(&root, "test").await.unwrap().unwrap(), payload);
        #[cfg(windows)]
        assert!(!tokio::fs::read(root.join("test.secure")).await.unwrap().windows(payload.len()).any(|w| w == payload));
        remove_secret(&root, "test").await.unwrap();
        assert!(read_secret(&root, "test").await.unwrap().is_none());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
