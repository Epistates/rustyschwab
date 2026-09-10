//! Windows token files use a protected owner/SYSTEM DACL at creation, then
//! atomic replacement. Inherited directory permissions never grant token access.
use crate::error::{AuthError, Error, Result};
use std::{
    io::Write,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{INVALID_HANDLE_VALUE, LocalFree},
    Security::{Authorization::*, DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES},
    Storage::FileSystem::*,
};

const DACL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)";
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn last_error() -> Error {
    Error::Io(std::io::Error::last_os_error())
}

pub(crate) fn secure_create_new(path: &Path) -> Result<std::fs::File> {
    let mut descriptor = ptr::null_mut();
    let sddl: Vec<u16> = DACL.encode_utf16().chain(Some(0)).collect();
    // SAFETY: SDDL is NUL-terminated; Windows allocates the descriptor and
    // Local owns it until CreateFileW has copied the security attributes.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let _descriptor = Local(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: successful CreateFileW returns a uniquely owned file handle.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

pub fn secure_file_write(path: &Path, data: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temp = path.with_file_name(name);
    let temp_wide = wide(&temp);
    let mut file = secure_create_new(&temp)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        if unsafe {
            MoveFileExW(
                temp_wide.as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(last_error());
        }
        verify_file_permissions(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn verify_file_permissions(path: &Path) -> Result<()> {
    // Reject links before obtaining their target's descriptor.
    let metadata = std::fs::symlink_metadata(path)?;
    use std::os::windows::fs::MetadataExt;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::Auth(AuthError::TokenFileInsecure(
            "Token file must not be a reparse point".into(),
        )));
    }
    let mut descriptor = ptr::null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(Error::Io(std::io::Error::from_raw_os_error(code as i32)));
    }
    let _descriptor = Local(descriptor);
    let mut output = ptr::null_mut();
    let mut len = 0;
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut output,
            &mut len,
        )
    } == 0
    {
        return Err(last_error());
    }
    let _output = Local(output.cast());
    // SAFETY: Windows returned `len` UTF-16 units including the terminal NUL.
    let text = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(output, len.saturating_sub(1) as usize)
    });
    if text != DACL && text != "D:P(A;;FA;;;SY)(A;;FA;;;OW)" {
        return Err(Error::Auth(AuthError::TokenFileInsecure(
            "Token file requires a protected owner/SYSTEM DACL".into(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protected_file_is_created_and_atomically_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        secure_file_write(&path, b"first").unwrap();
        verify_file_permissions(&path).unwrap();
        secure_file_write(&path, b"replacement").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        verify_file_permissions(&path).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

/// Explicit inheritable ACL protects SQLite WAL/SHM files at creation.
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let _descriptor = Local(descriptor);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) } == 0 && !path.exists() {
            return Err(last_error());
        }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    use std::os::windows::fs::MetadataExt;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::Storage(
            "State directory must be a real directory".into(),
        ));
    }
    let mut current = ptr::null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut current,
        )
    };
    if code != 0 {
        return Err(Error::Io(std::io::Error::from_raw_os_error(code as i32)));
    }
    let _current = Local(current);
    let mut output = ptr::null_mut();
    let mut len = 0;
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            current,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut output,
            &mut len,
        )
    } == 0
    {
        return Err(last_error());
    }
    let _output = Local(output.cast());
    let text = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(output, len.saturating_sub(1) as usize)
    });
    if text != "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        && text != "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)"
    {
        return Err(Error::Storage(
            "State directory needs a protected inheritable owner/SYSTEM ACL".into(),
        ));
    }
    Ok(())
}
