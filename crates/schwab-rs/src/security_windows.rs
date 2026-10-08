//! Windows token files use a protected owner/SYSTEM DACL at creation, then
//! atomic replacement. Inherited directory permissions never grant token access.
use crate::error::{AuthError, Error, Result};
use std::{
    io::Write,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::*, DACL_SECURITY_INFORMATION, EqualSid, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const DACL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)";
const DIRECTORY_DACL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";
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
/// Closes a kernel handle this module opened.
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn last_error() -> Error {
    Error::Io(std::io::Error::last_os_error())
}
fn insecure(why: &str) -> Error {
    Error::Auth(AuthError::TokenFileInsecure(format!("Private file {why}")))
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

/// See [`crate::security::open_private_file`]. The file is opened without
/// following a reparse point, and the open handle (not the path) is then
/// checked: no reparse point, an owner that is this user (or SYSTEM, or
/// the Administrators group), and the protected owner/SYSTEM DACL. A path
/// swapped after the open cannot change what was checked.
pub fn open_private_file(path: &Path) -> Result<std::fs::File> {
    open_checked(path, false)
}

/// [`open_private_file`], opened for writing too (lock files).
pub(crate) fn open_private_file_rw(path: &Path) -> Result<std::fs::File> {
    open_checked(path, true)
}

fn open_checked(path: &Path, write: bool) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(insecure("must not be a reparse point"));
    }
    let (trusted, dacl) = handle_security(file.as_raw_handle() as HANDLE)?;
    if !trusted {
        return Err(insecure(
            "must be owned by this user, SYSTEM or Administrators",
        ));
    }
    if dacl != DACL && dacl != "D:P(A;;FA;;;SY)(A;;FA;;;OW)" {
        return Err(insecure("requires a protected owner/SYSTEM DACL"));
    }
    Ok(file)
}

pub(crate) fn verify_file_permissions(path: &Path) -> Result<()> {
    open_private_file(path).map(drop)
}

/// Whether `owner` is this process's user, SYSTEM or Administrators.
fn trusted_owner(owner: PSID) -> Result<bool> {
    // SAFETY: `owner` points into a security descriptor the caller holds.
    let well_known = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid) != 0
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
    };
    if well_known {
        return Ok(true);
    }
    let mut token = ptr::null_mut();
    // SAFETY: the process pseudo-handle needs no closing; `token` is closed
    // by `Handle`.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let _token = Handle(token);
    let mut len = 0u32;
    // SAFETY: a size query; the first call fails and reports the length.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len) };
    // u64 units keep the TOKEN_USER (which holds a pointer) aligned.
    let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut len,
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start
    // of `buffer`, whose SID lives in the same buffer.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    Ok(unsafe { EqualSid(owner, user.User.Sid) } != 0)
}

/// The owner verdict and the DACL (as SDDL) of an open handle.
fn handle_security(handle: HANDLE) -> Result<(bool, String)> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
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
    Ok((trusted_owner(owner)?, dacl_text(descriptor)?))
}

/// The DACL of `descriptor` as SDDL.
fn dacl_text(descriptor: *mut std::ffi::c_void) -> Result<String> {
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
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(output, len.saturating_sub(1) as usize)
    }))
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
        open_private_file_rw(&path).unwrap();
    }
}

/// Explicit inheritable ACL protects SQLite WAL/SHM files at creation.
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    let sddl: Vec<u16> = DIRECTORY_DACL.encode_utf16().chain(Some(0)).collect();
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
    let mut owner: PSID = ptr::null_mut();
    let mut current = ptr::null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
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
    if !trusted_owner(owner)? {
        return Err(Error::Storage(
            "State directory must be owned by this user, SYSTEM or Administrators".into(),
        ));
    }
    let text = dacl_text(current)?;
    if text != DIRECTORY_DACL && text != "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)" {
        return Err(Error::Storage(
            "State directory needs a protected inheritable owner/SYSTEM ACL".into(),
        ));
    }
    Ok(())
}
