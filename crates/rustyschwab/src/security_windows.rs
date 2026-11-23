//! Private files and directories on Windows.
//!
//! Windows has no file mode, so whether a file is private is read from its
//! security descriptor: its owner and its discretionary access control list
//! (DACL).
//!
//! # Policy
//!
//! A file or directory is private when all of these hold:
//!
//! - **Owner.** It is owned by the account this process runs as, by SYSTEM
//!   or by the built-in Administrators group. Windows makes Administrators
//!   the owner of whatever an elevated process creates, so that is the
//!   normal owner of an administrator's own files.
//! - **Access.** It has a DACL, and every entry in it that allows access
//!   names one of those three principals. `OWNER RIGHTS` and `CREATOR OWNER`
//!   entries are accepted as well: they stand for an owner, and the owner
//!   has already been checked. An entry for anyone else (Everyone, Users,
//!   Authenticated Users, another account) is refused whatever it grants,
//!   and so are a missing DACL, which lets every account in, and an entry
//!   of a kind this check cannot evaluate. Entries that deny access are
//!   ignored, because they can only take access away.
//! - **Directories pass their access on.** A directory must have at least
//!   one entry that files created in it inherit. Without one, Windows gives
//!   a new file the default DACL of the process that creates it instead,
//!   and a database creates its journal files beside its main file.
//!
//! SYSTEM and Administrators are allowed because neither can be kept out:
//! SYSTEM is the operating system itself, and an administrator can take
//! ownership of any file. Windows also grants both on every user profile,
//! so refusing them would refuse a file saved in the user's own home
//! directory. OpenSSH for Windows applies the same rule to private keys.
//!
//! Inherited entries are judged exactly as explicit ones are. A file that
//! inherits access for this account, SYSTEM and Administrators only is
//! private; one that inherits access for anyone else is not. The DACL is
//! not required to be protected from inheritance, only to pass as it is.
//!
//! # What this module creates
//!
//! A new private file or directory is created with its security descriptor
//! in the same call, so it never exists with the permissions it would have
//! inherited from its directory. The descriptor names this account as the
//! owner, protects the DACL from inheritance, and grants full control to
//! this account and to SYSTEM and to no one else. On a directory the two
//! entries are inheritable, so the files a database creates beside its main
//! file are private from their first byte.
//!
//! A file is replaced by writing a new file with that descriptor in the
//! same directory and renaming it over the old one. A rename within a
//! volume keeps the file's own descriptor, so the replacement is private
//! whatever the file it replaced allowed.
//!
//! Every check is made on an open handle, never on a path, and the handle
//! is opened without following a reparse point, so the file checked is the
//! file read.
use crate::error::{Error, Result};
use crate::security::insecure;
use std::{
    fs::File,
    io::Write,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, Authorization::*, DACL_SECURITY_INFORMATION, EqualSid,
        GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
        IsWellKnownSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
        WinCreatorOwnerRightsSid, WinCreatorOwnerSid, WinLocalSystemSid,
    },
    Storage::FileSystem::*,
    System::{
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
            ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
/// Frees memory Windows allocated with `LocalAlloc`.
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

/// The account this process runs as, read from its access token.
struct CurrentUser(Vec<u64>);

impl CurrentUser {
    fn new() -> Result<Self> {
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
        // SAFETY: `buffer` is writable for the length passed.
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
        Ok(Self(buffer))
    }

    fn sid(&self) -> PSID {
        // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start
        // of the buffer, whose SID lives in the same buffer.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// Whether `sid` is this account, SYSTEM or the Administrators group:
    /// the principals a private file may be owned by and grant access to.
    fn trusts(&self, sid: PSID) -> bool {
        // SAFETY: both SIDs are valid for the duration of the call.
        unsafe {
            EqualSid(sid, self.sid()) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }
}

/// `sid` in its `S-1-…` form.
fn sid_text(sid: PSID) -> Result<String> {
    let mut text = ptr::null_mut();
    // SAFETY: `sid` is valid; Windows allocates `text`, which `Local` frees.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last_error());
    }
    let _text = Local(text.cast());
    // SAFETY: Windows returned a NUL-terminated string.
    let len = (0..)
        .take_while(|&unit| unsafe { *text.add(unit) } != 0)
        .count();
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}

/// The security descriptor `sddl` describes.
fn descriptor(sddl: &str) -> Result<Local> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: the SDDL is NUL-terminated; Windows allocates the descriptor,
    // which `Local` frees.
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
    Ok(Local(descriptor))
}

/// The descriptor private files and directories are created with: owned by
/// this account, closed to inherited entries (`P`), and granting full
/// control to this account and SYSTEM. A directory's entries are inherited
/// by the files and directories created in it (`OICI`).
fn private_descriptor(directory: bool) -> Result<Local> {
    let user = sid_text(CurrentUser::new()?.sid())?;
    let inherit = if directory { "OICI" } else { "" };
    descriptor(&format!(
        "O:{user}D:P(A;{inherit};FA;;;{user})(A;{inherit};FA;;;SY)"
    ))
}

fn attributes(descriptor: &Local) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

/// The owner and DACL of the object behind `handle`.
fn security_of_handle(handle: HANDLE) -> Result<Local> {
    let mut descriptor = ptr::null_mut();
    // SAFETY: `handle` is open; Windows allocates the descriptor, which
    // `Local` frees.
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
    Ok(Local(descriptor))
}

/// The SID of the access-allowed entry at `ace`. It starts at the entry's
/// `SidStart` field and runs past the end of the declared struct, so it is
/// reached by offset from the entry's own pointer.
fn allowed_ace_sid(ace: *mut std::ffi::c_void) -> PSID {
    ace.wrapping_byte_add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
}

/// Why `descriptor` does not make its file (or, with `directory`, its
/// directory) private under the module policy, or `None` if it does.
/// `path` is only for the operator log.
fn violation(
    descriptor: PSECURITY_DESCRIPTOR,
    directory: bool,
    path: &Path,
) -> Result<Option<&'static str>> {
    let user = CurrentUser::new()?;
    let (mut owner, mut defaulted) = (ptr::null_mut(), 0);
    // SAFETY: `descriptor` is valid; `owner` points into it.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) } == 0 {
        return Err(last_error());
    }
    if owner.is_null() || !user.trusts(owner) {
        return Ok(Some("is owned by another account"));
    }
    let (mut present, mut dacl): (_, *mut ACL) = (0, ptr::null_mut());
    // SAFETY: `descriptor` is valid; `dacl` points into it.
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        == 0
    {
        return Err(last_error());
    }
    // SAFETY: a DACL present in a valid descriptor is null or a valid ACL.
    let acl = match unsafe { dacl.as_ref() } {
        Some(acl) if present != 0 => acl,
        _ => {
            return Ok(Some(
                "has no access control list, so any account may use it",
            ));
        }
    };
    let mut passed_on = false;
    for index in 0..acl.AceCount {
        let mut ace = ptr::null_mut();
        // SAFETY: `index` is below the ACL's entry count.
        if unsafe { GetAce(dacl, index.into(), &mut ace) } == 0 {
            return Err(last_error());
        }
        // SAFETY: GetAce succeeded, so `ace` points at an entry inside the
        // ACL, and every entry starts with an ACE_HEADER.
        let Some(header) = (unsafe { ace.cast::<ACE_HEADER>().as_ref() }) else {
            return Ok(Some("has an access control entry that cannot be checked"));
        };
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                let trustee = allowed_ace_sid(ace);
                // SAFETY: `trustee` is a valid SID inside `descriptor`.
                let owner_alias = unsafe {
                    IsWellKnownSid(trustee, WinCreatorOwnerRightsSid) != 0
                        || IsWellKnownSid(trustee, WinCreatorOwnerSid) != 0
                };
                if !owner_alias && !user.trusts(trustee) {
                    tracing::error!(
                        ?path,
                        account = %sid_text(trustee).unwrap_or_default(),
                        "an access control entry allows another account"
                    );
                    return Ok(Some("grants access to another account"));
                }
                passed_on |= u32::from(header.AceFlags) & OBJECT_INHERIT_ACE != 0;
            }
            _ => return Ok(Some("has an access control entry that cannot be checked")),
        }
    }
    if directory && !passed_on {
        return Ok(Some(
            "passes none of its access on to the files created in it",
        ));
    }
    Ok(None)
}

/// Create a new file at `path` with `descriptor`, failing if one exists.
fn create_new_with(path: &Path, descriptor: &Local) -> Result<File> {
    // SAFETY: the path is NUL-terminated, and `descriptor` outlives the call,
    // which copies the security attributes.
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes(descriptor),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: successful CreateFileW returns a uniquely owned file handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Create a new private file, failing if one exists.
pub(crate) fn secure_create_new(path: &Path) -> Result<File> {
    create_new_with(path, &private_descriptor(false)?)
}

pub fn secure_file_write(path: &Path, data: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let temp = path.with_file_name(name);
    let file = secure_create_new(&temp)?;
    let result = write_and_replace(file, &temp, path, data);
    if result.is_err() {
        // The temp file holds the secret too: never leave it behind.
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Write `data` to `file`, just created at `temp`, and rename it over `path`.
fn write_and_replace(mut file: File, temp: &Path, path: &Path, data: &[u8]) -> Result<()> {
    file.write_all(data)?;
    file.sync_all()?;
    // Where a plain rename is refused because `path` is open elsewhere,
    // `std::fs::rename` renames with POSIX semantics, which replaces it as a
    // rename does on Unix; readers that have it open keep the old file.
    std::fs::rename(temp, path)?;
    // Flushed again so the file is on disk under its new name before success
    // is reported.
    file.sync_all()?;
    drop(file);
    open_private_file(path).map(drop)
}

/// See [`crate::security::open_private_file`]. The file is opened without
/// following a reparse point, and the open handle (not the path) is then
/// checked against the policy in the module documentation: an ordinary
/// file, not a reparse point, owned by and open to no one but this account,
/// SYSTEM and Administrators. A path swapped after the open cannot change
/// what was checked.
pub fn open_private_file(path: &Path) -> Result<File> {
    open_checked(path, false)
}

/// [`open_private_file`], opened for writing too (lock files).
pub(crate) fn open_private_file_rw(path: &Path) -> Result<File> {
    open_checked(path, true)
}

fn open_checked(path: &Path, write: bool) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let handle = file.as_raw_handle() as HANDLE;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(insecure(path, "is a symbolic link or other reparse point"));
    }
    // SAFETY: `handle` is open for the lifetime of `file`.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return Err(insecure(path, "is not a regular file"));
    }
    let security = security_of_handle(handle)?;
    match violation(security.0, false, path)? {
        Some(why) => Err(insecure(path, why)),
        None => Ok(file),
    }
}

/// Create a private directory before any database or auxiliary file is
/// opened: see [`crate::security::ensure_private_directory`].
///
/// A directory created here gets the inheritable private descriptor
/// described in the module documentation, so the journal files a database
/// creates in it are private at creation. An existing one must be a real
/// directory (not a reparse point) that passes the same policy as a private
/// file and passes its access on to the files created in it.
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let descriptor = private_descriptor(true)?;
        // SAFETY: the path is NUL-terminated, and `descriptor` outlives the
        // call, which copies the security attributes.
        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes(&descriptor)) } == 0 {
            let error = last_error();
            // Another process may have created it first; it is checked below.
            if !path.exists() {
                return Err(error);
            }
        }
    }
    // Opened without following a reparse point and checked through the
    // handle, after creation: a link planted between the existence check
    // and the creation is refused, not written through.
    let directory = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    let why =
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Some("is not a real directory")
        } else {
            let security = security_of_handle(directory.as_raw_handle() as HANDLE)?;
            violation(security.0, true, path)?
        };
    if let Some(why) = why {
        tracing::error!(?path, "state directory {why}");
        return Err(Error::Storage(
            "State directory must be private (open to this account, SYSTEM and \
             Administrators only, and passing that on to new files), owned by one \
             of them, and not a reparse point"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AuthError;
    use crate::security::{read_private_file, verify_file_permissions};
    use windows_sys::Win32::Security::{
        CONTAINER_INHERIT_ACE, CreateWellKnownSid, GetSecurityDescriptorControl, INHERITED_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED, SECURITY_MAX_SID_SIZE,
        WELL_KNOWN_SID_TYPE, WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid,
    };

    const SYSTEM: &str = "S-1-5-18";
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    const INHERITABLE: u32 = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

    fn me() -> String {
        sid_text(CurrentUser::new().unwrap().sid()).unwrap()
    }

    /// What Windows holds for a path: its owner, whether its DACL is
    /// protected from inheritance, and each entry as (trustee, flags, mask).
    #[derive(Debug, PartialEq)]
    struct Security {
        owner: String,
        protected: bool,
        allows: Vec<(String, u32, u32)>,
    }

    fn security_of(path: &Path) -> Security {
        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let code = unsafe {
            GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(code, 0);
        let _descriptor = Local(descriptor);
        let (mut control, mut revision) = (0, 0);
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        let acl = unsafe { dacl.as_ref() }.expect("a DACL");
        let allows = (0..acl.AceCount)
            .map(|index| {
                let mut ace = ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, index.into(), &mut ace) }, 0);
                let entry = unsafe { ace.cast::<ACCESS_ALLOWED_ACE>().as_ref() }.unwrap();
                assert_eq!(u32::from(entry.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
                (
                    sid_text(allowed_ace_sid(ace)).unwrap(),
                    u32::from(entry.Header.AceFlags),
                    entry.Mask,
                )
            })
            .collect();
        Security {
            owner: sid_text(owner).unwrap(),
            protected: control & SE_DACL_PROTECTED != 0,
            allows,
        }
    }

    /// Replace the DACL of `path` with the one in `sddl`, closed to
    /// inheritance, as its owner could with `icacls`.
    fn set_dacl(path: &Path, sddl: &str) {
        let descriptor = descriptor(sddl).unwrap();
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl = ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        let code = unsafe {
            SetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        assert_eq!(code, 0);
    }

    /// A directory created with the descriptor in `sddl`.
    fn create_directory_with(path: &Path, sddl: &str) {
        let descriptor = descriptor(sddl).unwrap();
        assert_ne!(
            unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes(&descriptor)) },
            0
        );
    }

    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Vec<u32> {
        let mut sid = vec![0u32; SECURITY_MAX_SID_SIZE as usize / 4];
        let mut len = SECURITY_MAX_SID_SIZE;
        assert_ne!(
            unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid.as_mut_ptr().cast(), &mut len) },
            0
        );
        sid
    }

    fn refused(path: &Path) -> bool {
        matches!(
            verify_file_permissions(path),
            Err(Error::Auth(AuthError::TokenFileInsecure(_)))
        ) && read_private_file(path).is_err()
    }

    /// The verdict on the descriptor `sddl` describes, as a file or as a
    /// directory.
    fn verdict(sddl: &str, directory: bool) -> Option<&'static str> {
        let descriptor = descriptor(sddl).unwrap();
        violation(descriptor.0, directory, Path::new(sddl)).unwrap()
    }

    /// The descriptor every private file is created with.
    fn private_file() -> Security {
        Security {
            owner: me(),
            protected: true,
            allows: vec![
                (me(), 0, FILE_ALL_ACCESS),
                (SYSTEM.into(), 0, FILE_ALL_ACCESS),
            ],
        }
    }

    #[test]
    fn a_file_this_module_writes_is_private_and_passes_its_own_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        secure_file_write(&path, b"first").unwrap();
        verify_file_permissions(&path).unwrap();
        assert_eq!(security_of(&path), private_file());
        secure_file_write(&path, b"replacement").unwrap();
        assert_eq!(&*read_private_file(&path).unwrap(), b"replacement");
        assert_eq!(security_of(&path), private_file());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        open_private_file_rw(&path).unwrap();

        let lock = dir.path().join("tokens.lock");
        drop(secure_create_new(&lock).unwrap());
        assert_eq!(security_of(&lock), private_file());
        open_private_file_rw(&lock).unwrap();
        assert!(secure_create_new(&lock).is_err(), "creation is exclusive");
    }

    /// A replacement is private whatever it replaces, and goes through
    /// while a reader still has the old file open.
    #[test]
    fn a_replacement_is_private_even_over_an_open_file_that_was_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        std::fs::write(&path, b"old").unwrap();
        set_dacl(&path, &format!("D:P(A;;FA;;;{})(A;;FR;;;WD)", me()));
        assert!(refused(&path));
        let reader = std::fs::File::open(&path).unwrap();
        secure_file_write(&path, b"new").unwrap();
        drop(reader);
        assert_eq!(&*read_private_file(&path).unwrap(), b"new");
        assert_eq!(security_of(&path), private_file());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_file_that_lets_another_principal_in_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let me = me();
        // Everyone, Users, Authenticated Users and Guests, whatever they get.
        for other in [
            "(A;;FR;;;WD)",
            "(A;;FA;;;BU)",
            "(A;;FR;;;AU)",
            "(A;;RC;;;BG)",
        ] {
            secure_file_write(&path, b"secret").unwrap();
            set_dacl(&path, &format!("D:P(A;;FA;;;{me})(A;;FA;;;SY){other}"));
            assert!(refused(&path), "{other}");
        }
        // No DACL at all: every account has full access.
        secure_file_write(&path, b"secret").unwrap();
        set_dacl(&path, "D:NO_ACCESS_CONTROL");
        assert!(refused(&path));
        // Administrators may be let in, an entry that denies only takes
        // access away, and `OWNER RIGHTS` is the owner, checked already.
        for accepted in ["(A;;FA;;;BA)", "(D;;FA;;;BG)", "(A;;FA;;;OW)"] {
            secure_file_write(&path, b"secret").unwrap();
            set_dacl(&path, &format!("D:P{accepted}(A;;FA;;;{me})(A;;FA;;;SY)"));
            verify_file_permissions(&path).unwrap();
        }
    }

    /// An entry a file inherits from its directory counts like any other.
    #[test]
    fn inherited_access_for_another_principal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let me = me();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        set_dacl(
            &shared,
            &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FR;;;BU)"),
        );
        assert!(matches!(
            ensure_private_directory(&shared),
            Err(Error::Storage(_))
        ));
        let inherits = shared.join("tokens.json");
        std::fs::write(&inherits, b"secret").unwrap();
        let security = security_of(&inherits);
        assert!(!security.protected);
        assert!(
            security
                .allows
                .iter()
                .any(|(trustee, flags, _)| trustee == "S-1-5-32-545" && flags & INHERITED_ACE != 0),
            "{security:?}"
        );
        assert!(refused(&inherits));
        // The same directory still holds a private file once it is written
        // through this module, which does not inherit.
        secure_file_write(&inherits, b"secret").unwrap();
        assert_eq!(security_of(&inherits), private_file());
    }

    /// A file saved in a directory shaped like a user profile (SYSTEM,
    /// Administrators and the user, inherited by everything in it) is
    /// private, whoever of those owns it. An elevated administrator's
    /// process, such as a CI runner's, creates it owned by Administrators.
    #[test]
    fn a_file_in_the_users_own_profile_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let me = me();
        let profile = dir.path().join("profile");
        create_directory_with(
            &profile,
            &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{me})"),
        );
        ensure_private_directory(&profile).unwrap();
        let saved = profile.join("tokens.json");
        std::fs::write(&saved, b"secret").unwrap();
        let security = security_of(&saved);
        assert!(
            security.owner == me || security.owner == ADMINISTRATORS,
            "{security:?}"
        );
        assert_eq!(&*read_private_file(&saved).unwrap(), b"secret");
    }

    /// Earlier releases created files and directories granting `OWNER
    /// RIGHTS` and SYSTEM, owned by whoever the process's default owner
    /// was. They stay readable after an upgrade.
    #[test]
    fn files_and_directories_from_earlier_releases_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        create_directory_with(&state, "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)");
        ensure_private_directory(&state).unwrap();
        let tokens = state.join("tokens.json");
        let mut file =
            create_new_with(&tokens, &descriptor("D:P(A;;FA;;;OW)(A;;FA;;;SY)").unwrap()).unwrap();
        file.write_all(b"secret").unwrap();
        drop(file);
        assert_eq!(&*read_private_file(&tokens).unwrap(), b"secret");
        let journal = state.join("state.sqlite3-wal");
        std::fs::write(&journal, b"").unwrap();
        verify_file_permissions(&journal).unwrap();
    }

    #[test]
    fn a_new_private_directory_keeps_what_is_created_in_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nested").join("state");
        ensure_private_directory(&state).unwrap();
        ensure_private_directory(&state).unwrap();
        assert_eq!(
            security_of(&state),
            Security {
                owner: me(),
                protected: true,
                allows: vec![
                    (me(), INHERITABLE, FILE_ALL_ACCESS),
                    (SYSTEM.into(), INHERITABLE, FILE_ALL_ACCESS),
                ],
            }
        );
        // What a database would create beside its main file.
        let journal = state.join("state.sqlite3-wal");
        std::fs::write(&journal, b"").unwrap();
        let inherited: Vec<String> = security_of(&journal)
            .allows
            .into_iter()
            .map(|(trustee, flags, _)| {
                assert_ne!(flags & INHERITED_ACE, 0);
                trustee
            })
            .collect();
        assert_eq!(inherited, [me(), SYSTEM.to_string()]);
        verify_file_permissions(&journal).unwrap();

        assert!(ensure_private_directory(&journal).is_err(), "a file");
    }

    /// The policy on descriptors no test process could give a real file
    /// without privileges, such as an owner other than itself.
    #[test]
    fn the_owner_and_every_entry_that_allows_access_are_judged() {
        let me = me();
        for private in [
            // What this module creates.
            format!("O:{me}D:P(A;;FA;;;{me})(A;;FA;;;SY)"),
            // An elevated administrator's file.
            "O:BAD:P(A;;FA;;;BA)(A;;FA;;;SY)".into(),
            "O:SYD:P(A;;FA;;;SY)".into(),
            // An earlier release's file, with the flags Windows reports.
            "O:BAD:PAI(A;;FA;;;OW)(A;;FA;;;SY)".into(),
            // A file inheriting a user profile's entries.
            format!("O:{me}D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FA;;;{me})"),
            // Entries that deny only take access away.
            format!("O:{me}D:P(D;;FA;;;BG)(A;;FA;;;{me})"),
        ] {
            assert_eq!(verdict(&private, false), None, "{private}");
        }
        for (sddl, why) in [
            (
                "O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;SY)".into(),
                "is owned by another account",
            ),
            ("O:BUD:P(A;;FA;;;SY)".into(), "is owned by another account"),
            (
                format!("O:{me}D:P(A;;FA;;;{me})(A;;FR;;;WD)"),
                "grants access to another account",
            ),
            (
                format!("O:{me}D:P(A;;FA;;;{me})(A;;FA;;;BU)"),
                "grants access to another account",
            ),
            (
                format!("O:{me}D:P(A;;FA;;;{me})(A;;FR;;;S-1-5-21-1-2-3-1001)"),
                "grants access to another account",
            ),
            // Inherited, and only the right to read the permissions.
            (
                format!("O:{me}D:AI(A;ID;FA;;;{me})(A;ID;RC;;;AU)"),
                "grants access to another account",
            ),
            (
                format!("O:{me}D:NO_ACCESS_CONTROL"),
                "has no access control list, so any account may use it",
            ),
            (
                format!("O:{me}"),
                "has no access control list, so any account may use it",
            ),
            // A conditional entry: whom it admits depends on the caller.
            (
                format!("O:{me}D:P(A;;FA;;;{me})(XA;;FR;;;WD;(Member_of {{SID(BA)}}))"),
                "has an access control entry that cannot be checked",
            ),
        ] {
            assert_eq!(verdict(&sddl, false), Some(why), "{sddl}");
        }
    }

    #[test]
    fn a_directory_must_pass_private_access_on_to_new_files() {
        let me = me();
        for private in [
            format!("O:{me}D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)"),
            "O:BAD:PAI(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)".into(),
            format!("O:{me}D:P(A;;FA;;;{me})(A;OICIIO;FA;;;CO)"),
        ] {
            assert_eq!(verdict(&private, true), None, "{private}");
        }
        // Files created in it would get the process's default DACL.
        let closed = format!("O:{me}D:P(A;;FA;;;{me})(A;;FA;;;SY)");
        assert_eq!(verdict(&closed, false), None);
        assert_eq!(
            verdict(&closed, true),
            Some("passes none of its access on to the files created in it")
        );
        // Passed on to new files only, and still refused.
        assert_eq!(
            verdict(
                &format!("O:{me}D:P(A;OICI;FA;;;{me})(A;OIIO;FR;;;BU)"),
                true
            ),
            Some("grants access to another account")
        );
    }

    #[test]
    fn only_this_account_system_and_administrators_are_trusted() {
        let user = CurrentUser::new().unwrap();
        assert!(user.trusts(user.sid()));
        for trusted in [WinLocalSystemSid, WinBuiltinAdministratorsSid] {
            assert!(user.trusts(well_known(trusted).as_mut_ptr().cast()));
        }
        for other in [WinWorldSid, WinBuiltinUsersSid, WinAuthenticatedUserSid] {
            assert!(!user.trusts(well_known(other).as_mut_ptr().cast()));
        }
    }
}
