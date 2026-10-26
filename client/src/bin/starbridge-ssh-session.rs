//! Preserve OpenSSH's inherited session handles while providing a hidden console.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, io, os::windows::ffi::OsStrExt, path::PathBuf, ptr};

    #[repr(C)]
    #[derive(Default)]
    struct StartupInfo {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32, y: u32, width: u32, height: u32,
        columns: u32, rows: u32, fill: u32, flags: u32,
        show: u16, reserved_size: u16,
        reserved_data: *mut u8,
        stdin: *mut c_void, stdout: *mut c_void, stderr: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcessInfo {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStartupInfoW(info: *mut StartupInfo);
        fn CreateProcessW(application: *const u16, command: *mut u16,
            process_attributes: *mut c_void, thread_attributes: *mut c_void,
            inherit: i32, flags: u32, environment: *mut c_void, directory: *const u16,
            startup: *const StartupInfo, process: *mut ProcessInfo) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
        fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn ExitProcess(code: u32) -> !;
    }

    // Windows CRT argument quoting, without passing through another shell.
    fn quote(argument: &std::ffi::OsStr, command: &mut Vec<u16>) {
        command.push(b'"' as u16);
        let mut slashes = 0;
        for unit in argument.encode_wide() {
            if unit == b'\\' as u16 {
                slashes += 1;
            } else {
                command.extend(std::iter::repeat_n(b'\\' as u16,
                    if unit == b'"' as u16 { slashes * 2 + 1 } else { slashes }));
                command.push(unit);
                slashes = 0;
            }
        }
        command.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        command.push(b'"' as u16);
    }

    pub fn run() -> io::Result<u32> {
        let executable = PathBuf::from(std::env::var_os("STARBRIDGE_SSHD_SESSION_PATH")
            .ok_or_else(|| io::Error::other("missing SSH session executable"))?);
        if !executable.is_absolute() || executable.file_name().and_then(|name| name.to_str()) != Some("sshd-session.exe") {
            return Err(io::Error::other("invalid SSH session executable"));
        }
        let application: Vec<u16> = executable.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command = Vec::new();
        // OpenSSH re-executes saved argv[0] after authentication. Keep the
        // launcher there so that the post-authentication session is hidden too.
        quote(std::env::current_exe()?.as_os_str(), &mut command);
        for argument in std::env::args_os().skip(1) {
            command.push(b' ' as u16);
            quote(&argument, &mut command);
        }
        command.push(0);
        let mut startup = StartupInfo::default();
        let mut process = ProcessInfo::default();
        // Keep stdio AND OpenSSH's reserved CRT descriptor table. Rebuilding only
        // stdin/stdout/stderr loses the authentication/socket/logging handles.
        unsafe { GetStartupInfoW(&mut startup); }
        startup.cb = std::mem::size_of::<StartupInfo>() as u32;
        startup.flags |= 0x00000001; // STARTF_USESHOWWINDOW
        startup.show = 0; // SW_HIDE, applied before the console can become visible.
        if unsafe { CreateProcessW(application.as_ptr(), command.as_mut_ptr(),
            ptr::null_mut(), ptr::null_mut(), 1,
            0x00000010 | 0x00000400, // CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT
            ptr::null_mut(), ptr::null(), &startup, &mut process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { CloseHandle(process.thread); }
        let wait = unsafe { WaitForSingleObject(process.process, u32::MAX) };
        let mut code = 255;
        let result = if wait == 0 && unsafe { GetExitCodeProcess(process.process, &mut code) } != 0 {
            Ok(code)
        } else {
            Err(io::Error::last_os_error())
        };
        unsafe { CloseHandle(process.process); }
        result
    }

    pub fn exit(code: u32) -> ! {
        unsafe { ExitProcess(code) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_spaces_quotes_and_trailing_backslashes() {
            for (input, expected) in [
                ("", "\"\""), ("a b", "\"a b\""),
                ("a\"b", "\"a\\\"b\""), ("a\\", "\"a\\\\\""),
                ("a\\\"b", "\"a\\\\\\\"b\""),
            ] {
                let mut command = Vec::new();
                quote(input.as_ref(), &mut command);
                assert_eq!(String::from_utf16(&command).unwrap(), expected);
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    match windows::run() {
        Ok(code) => windows::exit(code),
        Err(error) => {
            eprintln!("StarBridge SSH session startup failed: {error}");
            windows::exit(255);
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("This SSH session launcher is only used on Windows.");
        std::process::exit(255);
    }
}
