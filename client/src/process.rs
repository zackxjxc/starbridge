/// Windows jobs own the complete FRP/SSHD child tree, including sshd-session children.
/// Closing this non-inherited handle on process death releases all app-owned services.
pub(super) struct ProcessJob {
    #[cfg(windows)] handle: usize,
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    #[repr(C)] pub struct BasicLimits {
        process_time:i64, job_time:i64, flags:u32, min_working_set:usize, max_working_set:usize,
        active_processes:u32, affinity:usize, priority:u32, scheduling:u32,
    }
    #[repr(C)] pub struct ExtendedLimits {
        basic:BasicLimits, io:[u64;6], process_memory:usize, job_memory:usize,
        peak_process_memory:usize, peak_job_memory:usize,
    }
    #[link(name="kernel32")]
    unsafe extern "system" {
        pub fn CreateJobObjectW(attributes:*mut c_void,name:*const u16)->*mut c_void;
        pub fn SetInformationJobObject(job:*mut c_void,class:i32,info:*const c_void,size:u32)->i32;
        pub fn AssignProcessToJobObject(job:*mut c_void,process:*mut c_void)->i32;
        pub fn CloseHandle(handle:*mut c_void)->i32;
    }
    pub fn limits()->ExtendedLimits {
        ExtendedLimits { basic:BasicLimits {process_time:0,job_time:0,flags:0x2000,
            min_working_set:0,max_working_set:0,active_processes:0,affinity:0,priority:0,scheduling:0},
            io:[0;6],process_memory:0,job_memory:0,peak_process_memory:0,peak_job_memory:0 }
    }
}

impl ProcessJob {
    pub fn new()->anyhow::Result<Self> {
        #[cfg(windows)] {
            use windows::*;
            let handle=unsafe{CreateJobObjectW(std::ptr::null_mut(),std::ptr::null())};
            if handle.is_null(){return Err(std::io::Error::last_os_error().into());}
            let limits=limits();
            if unsafe{SetInformationJobObject(handle,9,(&limits as *const ExtendedLimits).cast(),std::mem::size_of::<ExtendedLimits>() as u32)}==0 {
                let error=std::io::Error::last_os_error();unsafe{CloseHandle(handle);}
                return Err(error.into());
            }
            Ok(Self{handle:handle as usize})
        }
        #[cfg(not(windows))] {Ok(Self{})}
    }

    pub fn attach(&self,child:&tokio::process::Child)->anyhow::Result<()> {
        #[cfg(windows)] {
            let handle=child.raw_handle().ok_or_else(||anyhow::anyhow!("子进程已经退出"))?;
            if unsafe{windows::AssignProcessToJobObject(self.handle as *mut _,handle)}==0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))] let _=child;
        Ok(())
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        #[cfg(windows)] unsafe{windows::CloseHandle(self.handle as *mut _);}
    }
}
