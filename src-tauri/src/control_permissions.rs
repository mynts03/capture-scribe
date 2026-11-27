//! Capability discovery must be private to the OS user, never a shared /tmp file.
use std::{fs, io, path::Path};
#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}
#[cfg(unix)]
pub fn check_private(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink()
        || m.is_dir() != directory
        || (!directory && !m.is_file())
        || m.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "control endpoint must be private and not a symlink",
        ));
    }
    Ok(())
}
#[cfg(unix)]
pub fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::{ffi::OsStrExt, fs::MetadataExt},
        ptr,
    };
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            s: *const u16,
            revision: u32,
            out: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn GetFileSecurityW(
            path: *const u16,
            info: u32,
            descriptor: *mut c_void,
            length: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd: *const c_void,
            revision: u32,
            info: u32,
            out: *mut *mut u16,
            length: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    pub fn create(path: &Path) -> io::Result<()> {
        // Protected DACL; only owner rights and SYSTEM. Children inherit it.
        let s: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let mut sd = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                s.as_ptr(),
                1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let a = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                descriptor: sd,
                inherit: 0,
            };
            let ok = CreateDirectoryW(wide(path).as_ptr(), &a);
            let error = io::Error::last_os_error();
            LocalFree(sd);
            if ok == 0 {
                Err(error)
            } else {
                Ok(())
            }
        }
    }
    pub fn check(path: &Path, directory: bool) -> io::Result<()> {
        let m = fs::symlink_metadata(path)?;
        if m.file_attributes() & 0x400 != 0
            || m.is_dir() != directory
            || (!directory && !m.is_file())
        {
            return Err(io::Error::other("invalid endpoint file type/reparse point"));
        }
        unsafe {
            let mut needed = 0;
            let path = wide(path);
            GetFileSecurityW(path.as_ptr(), 4, ptr::null_mut(), 0, &mut needed);
            if needed == 0 || needed > 65536 {
                return Err(io::Error::other("cannot inspect endpoint DACL"));
            }
            let mut sd = vec![0u8; needed as usize];
            if GetFileSecurityW(
                path.as_ptr(),
                4,
                sd.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut text = ptr::null_mut();
            let mut len = 0;
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd.as_ptr().cast(),
                1,
                4,
                &mut text,
                &mut len,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(
                text,
                len.saturating_sub(1) as usize,
            ));
            LocalFree(text.cast());
            // No public/inherited additional principals. Fail closed on unknown ACLs.
            let aces: Vec<_> = s.split('(').skip(1).collect();
            if aces.len() != 2
                || !aces.iter().all(|ace| {
                    ace.starts_with("A;")
                        && ace.contains(";FA;;;")
                        && (ace.ends_with(";OW)") || ace.ends_with(";SY)"))
                })
            {
                return Err(io::Error::other("endpoint DACL is not owner/SYSTEM only"));
            }
            Ok(())
        }
    }
}
#[cfg(windows)]
pub use windows::{check as check_private, create as create_private_dir};
#[cfg(windows)]
pub fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
