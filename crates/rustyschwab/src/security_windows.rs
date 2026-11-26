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
//! Every check is made on an open handle, never on a path, so the file
//! checked is the file read. A reparse point as the last part of a path is
//! refused rather than followed (`FILE_FLAG_OPEN_REPARSE_POINT`); one
//! earlier in the path is followed, which is why a trusted file is opened
//! inside the directory that was judged, as described below.
//!
//! # Trusted files
//!
//! A file that decides authorization (an MCP policy, grants, identity or
//! risk file) is opened by `open_trusted_file`. It holds no secret, so
//! others may read it, and it may be reached through a symbolic link. What
//! it must not be is changeable by anyone else, because whoever can edit it
//! can grant themselves trading, as on Unix.
//!
//! The path is resolved first, following links as `std::fs::canonicalize`
//! does. The directory holding the result is then opened, without following
//! a reparse point and without `FILE_SHARE_DELETE`, and judged. The file is
//! opened by its name relative to that directory's handle (`NtCreateFile`
//! with the handle as `RootDirectory`, as `openat` does on Unix), with
//! `FILE_OPEN_REPARSE_POINT`, and judged. The path is never resolved a
//! second time, so no part of it can be swapped for a link between the
//! check and the open: the file read is the file checked, and the directory
//! judged is the directory it was opened in. The directory's handle stays
//! open until the file is open, and since it does not share delete access,
//! Windows refuses to rename or remove that directory meanwhile.
//!
//! As on Unix, only the directory that holds the file is judged, not its
//! ancestors. An account that can delete children of an ancestor can rename
//! the directory away, and a replacement it creates there is refused
//! because it owns it.
//!
//! On a network share, SYSTEM and Administrators are the file server's
//! groups and the server evaluates the access list, so the check says
//! nothing about this machine's accounts. Keep authorization files on a
//! local disk.
//!
//! Each of the file and the directory is judged as follows:
//!
//! - **Owner.** The same rule as for a private file.
//! - **Access.** The same principals are trusted. An entry that allows
//!   anyone else a right that changes the object (`FILE_CHANGE` for a
//!   file, `DIRECTORY_CHANGE` for a directory) is refused. An entry that
//!   allows only reading is fine, as is one for `OWNER RIGHTS` or `CREATOR
//!   OWNER`. A missing DACL is refused.
//! - **Entries that do not apply to the object itself.** An entry marked
//!   inherit-only (`INHERIT_ONLY_ACE`) is skipped: Windows does not consult
//!   it when deciding access to the object it sits on, only when it passes
//!   it to what is created there. A private directory must still refuse such
//!   an entry, because the private files created in it would inherit it. A
//!   trusted file already exists, with a DACL of its own.
//! - **Entries this check cannot evaluate** (object, callback and
//!   conditional entries) are refused even when they only grant reading:
//!   whom they admit depends on the caller or on a condition.
//! - **Kind.** The file must be an ordinary file that is not a reparse
//!   point, and the directory an ordinary directory.
//! - **Entries that deny access** never cause a refusal and never excuse an
//!   entry that allows, as for a private file: whether a deny wins depends
//!   on the order of the entries and on group membership, so the check
//!   relies on neither.
use crate::error::{Error, Result};
use crate::security::{insecure, untrusted};
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
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    },
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_ALL, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, Authorization::*, DACL_SECURITY_INFORMATION, EqualSid,
        GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
        INHERIT_ONLY_ACE, IsWellKnownSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinCreatorOwnerSid,
        WinLocalSystemSid,
    },
    Storage::FileSystem::*,
    System::{
        IO::IO_STATUS_BLOCK,
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
            ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
            ACCESS_SYSTEM_SECURITY,
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

/// The rights that let an account change a file that is not its own: its
/// content (`FILE_WRITE_DATA`, `FILE_APPEND_DATA`), its extended attributes
/// and attributes (`FILE_WRITE_EA`, `FILE_WRITE_ATTRIBUTES`), its existence
/// (`DELETE`; a file is also deleted by `FILE_DELETE_CHILD` on its
/// directory, which [`DIRECTORY_CHANGE`] covers), and its security
/// (`WRITE_DAC` for the permissions, `WRITE_OWNER` for the owner,
/// `ACCESS_SYSTEM_SECURITY` for the audit list).
///
/// `GENERIC_WRITE` and `GENERIC_ALL` stand for sets that include those. The
/// file system maps generic rights to specific ones when an entry applies to
/// the object, but leaves them as written in an inherit-only entry (see "ACE
/// Inheritance Rules"), so they are listed too. `GENERIC_READ` and
/// `GENERIC_EXECUTE` map to `FILE_READ_DATA`, `FILE_READ_EA`,
/// `FILE_READ_ATTRIBUTES`, `READ_CONTROL`, `FILE_EXECUTE` and `SYNCHRONIZE`,
/// none of which changes the file. `FILE_DELETE_CHILD` is defined only for
/// a directory, and `FILE_ALL_ACCESS` and `FILE_GENERIC_WRITE` are sums of
/// the rights above, so a mask holding either also holds one of these bits.
///
/// See "File Access Rights Constants" and "File Security and Access Rights"
/// in the Win32 documentation.
const FILE_CHANGE: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | ACCESS_SYSTEM_SECURITY
    | GENERIC_WRITE
    | GENERIC_ALL;

/// The rights that let an account change which files a directory holds:
/// create a file (`FILE_ADD_FILE`, the same bit as `FILE_WRITE_DATA`),
/// create a subdirectory (`FILE_ADD_SUBDIRECTORY`, the bit of
/// `FILE_APPEND_DATA`), delete or rename any file in it whatever that file
/// allows (`FILE_DELETE_CHILD`), remove or rename the directory itself
/// (`DELETE`), and change who may do any of that (`WRITE_DAC`,
/// `WRITE_OWNER`, `ACCESS_SYSTEM_SECURITY`), with `GENERIC_WRITE` and
/// `GENERIC_ALL` as for [`FILE_CHANGE`].
///
/// `FILE_WRITE_EA` and `FILE_WRITE_ATTRIBUTES` are left out: on a directory
/// they change its own extended attributes and timestamps, not what it
/// holds.
const DIRECTORY_CHANGE: u32 = FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | ACCESS_SYSTEM_SECURITY
    | GENERIC_WRITE
    | GENERIC_ALL;

/// What a descriptor must satisfy.
#[derive(Clone, Copy)]
enum Rule {
    /// No allow entry names anyone but the trusted principals. With
    /// `directory`, files created in the directory must inherit access too.
    Private { directory: bool },
    /// No allow entry that applies to the object gives anyone but the
    /// trusted principals a right that changes it, or, with `directory`,
    /// what the directory holds.
    Trusted { directory: bool },
}

impl Rule {
    /// What the rule judges, for the operator log.
    fn object(self) -> &'static str {
        match self {
            Rule::Private { directory: true } | Rule::Trusted { directory: true } => "directory",
            Rule::Private { .. } | Rule::Trusted { .. } => "file",
        }
    }
}

/// Why `descriptor` does not make its file (or, with `directory`, its
/// directory) private under the module policy, or `None` if it does.
/// `path` is only for the operator log.
fn violation(
    descriptor: PSECURITY_DESCRIPTOR,
    directory: bool,
    path: &Path,
) -> Result<Option<&'static str>> {
    judge(descriptor, Rule::Private { directory }, path)
}

/// Why `descriptor` does not satisfy `rule`, or `None` if it does. `path` is
/// the path of the object the descriptor belongs to, only for the operator
/// log.
fn judge(
    descriptor: PSECURITY_DESCRIPTOR,
    rule: Rule,
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
                if owner_alias || user.trusts(trustee) {
                    passed_on |= u32::from(header.AceFlags) & OBJECT_INHERIT_ACE != 0;
                    continue;
                }
                // SAFETY: an access-allowed entry is an ACCESS_ALLOWED_ACE.
                let mask = unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask };
                let why = match rule {
                    Rule::Private { .. } => Some("grants access to another account"),
                    Rule::Trusted { directory } => {
                        let change = if directory {
                            DIRECTORY_CHANGE
                        } else {
                            FILE_CHANGE
                        };
                        let applies = u32::from(header.AceFlags) & INHERIT_ONLY_ACE == 0;
                        (applies && mask & change != 0)
                            .then_some("can be changed by another account")
                    }
                };
                if let Some(why) = why {
                    tracing::error!(
                        ?path,
                        object = rule.object(),
                        account = %sid_text(trustee).unwrap_or_default(),
                        rights = format_args!("{mask:#x}"),
                        "an access control entry allows another account"
                    );
                    return Ok(Some(why));
                }
            }
            _ => return Ok(Some("has an access control entry that cannot be checked")),
        }
    }
    if matches!(rule, Rule::Private { directory: true }) && !passed_on {
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
    let descriptor = private_descriptor(false)?;
    crate::storage::diag_here("scn_descriptor");
    let created = create_new_with(path, &descriptor);
    crate::storage::diag_here("scn_create_new_tried");
    created
}

/// See [`crate::security::rename_noclobber`].
pub(crate) fn rename_noclobber(from: &Path, to: &Path) -> std::io::Result<()> {
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let moved = unsafe {
        MoveFileExW(
            wide(from).as_ptr(),
            wide(to).as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Windows has no call that flushes a directory: `rename_noclobber` writes
/// through, and the caller flushes the file once more under its new name.
pub(crate) fn sync_directory(_dir: &Path) -> std::io::Result<()> {
    Ok(())
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

/// The file at `path`, opened without following a reparse point, or why it
/// is not an ordinary file.
fn open_regular(path: &Path, write: bool) -> Result<std::result::Result<File, &'static str>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Ok(match not_regular(&file)? {
        Some(why) => Err(why),
        None => Ok(file),
    })
}

/// Why `file`, opened without following a reparse point, is not an ordinary
/// file, or `None` if it is.
fn not_regular(file: &File) -> Result<Option<&'static str>> {
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Some("is a symbolic link or other reparse point"));
    }
    // SAFETY: the handle is open for the lifetime of `file`.
    if metadata.is_dir() || unsafe { GetFileType(file.as_raw_handle() as HANDLE) } != FILE_TYPE_DISK
    {
        return Ok(Some("is not a regular file"));
    }
    Ok(None)
}

fn open_checked(path: &Path, write: bool) -> Result<File> {
    let file = open_regular(path, write)?.map_err(|why| insecure(path, why))?;
    crate::storage::diag_here("ocf_opened");
    let security = security_of_handle(file.as_raw_handle() as HANDLE)?;
    crate::storage::diag_here("ocf_security_read");
    let judged = violation(security.0, false, path);
    crate::storage::diag_here("ocf_judged");
    match judged? {
        Some(why) => Err(insecure(path, why)),
        None => Ok(file),
    }
}

/// An authorization file opened for reading, or an error that keeps `path`
/// out of its text. See [`crate::security::read_trusted_file`] and the
/// module documentation, which say what holds between the check and the
/// read.
pub(crate) fn open_trusted_file(path: &Path) -> Result<File> {
    open_trusted_between(path, || {})
}

/// [`open_trusted_file`], running `between` once the directory has been
/// judged and is held, and before the file is opened. It lets a test act in
/// the window in which a path could be swapped.
fn open_trusted_between(path: &Path, between: impl FnOnce()) -> Result<File> {
    let resolved = std::fs::canonicalize(path)?;
    let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
        return Err(untrusted(path, "is not a regular file"));
    };
    let Some(directory) = open_directory(parent)? else {
        return Err(untrusted(
            path,
            "is in something other than an ordinary directory",
        ));
    };
    let security = security_of_handle(directory.as_raw_handle() as HANDLE)?;
    if let Some(why) = judge(security.0, Rule::Trusted { directory: true }, parent)? {
        return Err(untrusted(path, &format!("is in a directory that {why}")));
    }
    between();
    // The file is named relative to the directory that was judged, which is
    // held (and cannot be renamed or removed) until it is open.
    let file = open_in(&directory, name)?;
    drop(directory);
    if let Some(why) = not_regular(&file)? {
        return Err(untrusted(path, why));
    }
    let security = security_of_handle(file.as_raw_handle() as HANDLE)?;
    match judge(security.0, Rule::Trusted { directory: false }, path)? {
        Some(why) => Err(untrusted(path, why)),
        None => Ok(file),
    }
}

/// The file `name` in `directory`, opened for reading without following a
/// reparse point, as `openat` does on Unix: `NtCreateFile` with the
/// directory's handle as the root of the name, so the path to the directory
/// is not resolved again. Opening is by the name alone, which must be a
/// single path component.
///
/// The handle is the one `CreateFileW` would give for `GENERIC_READ` with
/// the default sharing: synchronous, so the standard library can read from
/// it without an `OVERLAPPED` structure. A directory is opened too, not
/// refused here, so that [`not_regular`] can name it. `OBJ_CASE_INSENSITIVE`
/// matches the case rules of `CreateFileW`.
fn open_in(directory: &File, name: &std::ffi::OsStr) -> Result<File> {
    let units: Vec<u16> = name.encode_wide().collect();
    let bytes = u16::try_from(units.len() * 2)
        .ok()
        .filter(|_| !units.is_empty() && !units.contains(&u16::from(b'\\')))
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a file name is one path component",
            ))
        })?;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle() as HANDLE,
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    // SAFETY: a zeroed IO_STATUS_BLOCK is the documented initial state.
    let mut status_block = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    // SAFETY: `attributes` and the name it points to outlive the call, and
    // the root is an open directory handle.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_READ,
            &attributes,
            &mut status_block,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: a plain conversion of an NTSTATUS.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(Error::Io(std::io::Error::from_raw_os_error(code as i32)));
    }
    // SAFETY: a successful NtCreateFile returns a uniquely owned file handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// The directory at `path`, opened without following a reparse point, or
/// `None` if it is not a real directory.
///
/// Opened for `READ_CONTROL`, to read its security, and `FILE_LIST_DIRECTORY`
/// (a read right, so that its sharing counts), and without
/// `FILE_SHARE_DELETE`: while the handle is open Windows refuses to rename
/// or remove the directory. Sharing is only enforced between opens that ask
/// for data or delete access, so a handle with `READ_CONTROL` alone would
/// not hold it.
fn open_directory(path: &Path) -> Result<Option<File>> {
    let directory = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_LIST_DIRECTORY)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    let real = metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    Ok(real.then_some(directory))
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
    let why = match open_directory(path)? {
        None => Some("is not a real directory"),
        Some(directory) => {
            let security = security_of_handle(directory.as_raw_handle() as HANDLE)?;
            violation(security.0, true, path)?
        }
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
    use crate::security::{read_private_file, read_trusted_file, verify_file_permissions};
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
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

    // Trusted files: authorization files read by `read_trusted_file`.

    /// The verdict on the descriptor `sddl` describes, as a trusted file or
    /// as the directory holding one.
    fn trusted_verdict(sddl: &str, directory: bool) -> Option<&'static str> {
        let descriptor = descriptor(sddl).unwrap();
        judge(descriptor.0, Rule::Trusted { directory }, Path::new(sddl)).unwrap()
    }

    /// A directory only this account and SYSTEM can change, as the
    /// directory of a trusted file must be, inside `parent`.
    fn trusted_directory(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        create_directory_with(&path, &format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)", me()));
        path
    }

    /// The file `name` in `directory` with exactly the entries in `dacl`.
    fn trusted_file(directory: &Path, name: &str, dacl: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"[]").unwrap();
        set_dacl(&path, dacl);
        path
    }

    /// The DACL of a file only this account and SYSTEM can change, with
    /// `others` appended.
    fn closed(others: &str) -> String {
        format!("D:P(A;;FA;;;{})(A;;FA;;;SY){others}", me())
    }

    fn accepted(path: &Path) {
        assert_eq!(read_trusted_file(path, 16).unwrap(), b"[]", "{path:?}");
    }

    /// The text of the refusal, which must not name the path.
    fn refusal(path: &Path) -> String {
        match read_trusted_file(path, 16) {
            Err(Error::Config(message)) => {
                assert!(message.ends_with("; see server logs for the path"));
                assert!(!message.contains(&*path.to_string_lossy()), "{message}");
                message
            }
            other => panic!("{path:?} should be refused, got {other:?}"),
        }
    }

    const FILE_REFUSED: &str = "A configuration file can be changed by another account; \
                                see server logs for the path";
    const DIRECTORY_REFUSED: &str = "A configuration file is in a directory that can be \
                                     changed by another account; see server logs for the path";

    /// World-readable is fine, as on Unix, however the read right is
    /// spelled, and the size limit still applies.
    #[test]
    fn a_trusted_file_anyone_may_read_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        for readers in [
            "(A;;FR;;;WD)",
            "(A;;FRFX;;;BU)",
            "(A;;GR;;;WD)",
            "(A;;GX;;;AU)",
            "(A;;0x1;;;BU)",
            "(A;;0x80;;;AU)",
            "(A;;RC;;;BG)",
            "",
        ] {
            let path = trusted_file(&etc, "policy.json", &closed(readers));
            accepted(&path);
            assert!(
                matches!(read_trusted_file(&path, 1), Err(Error::Config(m)) if m.contains("exceeds")),
                "{readers}"
            );
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A file Everyone, Users or any other account can write is refused
    /// whichever right does it, and an entry for the file's own owner or for
    /// an owner alias is not another account.
    #[test]
    fn a_trusted_file_another_account_can_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        for writer in [
            "(A;;FW;;;WD)",
            "(A;;FA;;;WD)",
            "(A;;FA;;;BU)",
            "(A;;FW;;;AU)",
            "(A;;GW;;;WD)",
            "(A;;GA;;;BU)",
            "(A;;0x2;;;BU)",
            "(A;;0x4;;;BU)",
            "(A;;0x10;;;BU)",
            "(A;;0x100;;;BU)",
            "(A;;0x10000;;;BU)",
            "(A;;0x40000;;;BU)",
            "(A;;0x80000;;;BU)",
            // Read access beside a right that changes the file.
            "(A;;FR;;;BU)(A;;0x2;;;BU)",
            "(A;;FR;;;WD)(A;;FW;;;BG)",
            "(A;;FW;;;LS)",
        ] {
            let path = trusted_file(&etc, "policy.json", &closed(writer));
            assert_eq!(refusal(&path), FILE_REFUSED, "{writer}");
            std::fs::remove_file(&path).unwrap();
        }
        // No DACL: every account has full access.
        let path = trusted_file(&etc, "policy.json", "D:NO_ACCESS_CONTROL");
        assert!(refusal(&path).contains("has no access control list"));
        std::fs::remove_file(&path).unwrap();
        // The owner's own entries are not another account.
        for own in ["(A;;FA;;;OW)", "(A;;FA;;;BA)", "(A;;FA;;;CO)"] {
            let path = trusted_file(&etc, "policy.json", &closed(own));
            accepted(&path);
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// The rights that count, one bit at a time, against a list written
    /// out in full, so that a right added to or dropped from
    /// `FILE_CHANGE` or `DIRECTORY_CHANGE` shows up here.
    #[test]
    fn exactly_the_rights_that_change_an_object_are_refused() {
        // WRITE_DATA, APPEND_DATA, WRITE_EA, WRITE_ATTRIBUTES, DELETE,
        // WRITE_DAC, WRITE_OWNER, ACCESS_SYSTEM_SECURITY, GENERIC_ALL and
        // GENERIC_WRITE.
        const FILE: [u32; 10] = [
            0x2,
            0x4,
            0x10,
            0x100,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x100_0000,
            0x1000_0000,
            0x4000_0000,
        ];
        // ADD_FILE, ADD_SUBDIRECTORY, DELETE_CHILD, DELETE, WRITE_DAC,
        // WRITE_OWNER, ACCESS_SYSTEM_SECURITY, GENERIC_ALL and GENERIC_WRITE.
        const DIRECTORY: [u32; 9] = [
            0x2,
            0x4,
            0x40,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x100_0000,
            0x1000_0000,
            0x4000_0000,
        ];
        let me = me();
        for bit in 0..32 {
            let mask = 1u32 << bit;
            let sddl = format!("O:{me}D:P(A;;FA;;;{me})(A;;{mask:#x};;;BU)");
            for (directory, refused) in [(false, &FILE[..]), (true, &DIRECTORY[..])] {
                let expected = refused
                    .contains(&mask)
                    .then_some("can be changed by another account");
                assert_eq!(
                    trusted_verdict(&sddl, directory),
                    expected,
                    "{mask:#x} on a {}",
                    if directory { "directory" } else { "file" }
                );
            }
        }
    }

    /// The owner is judged as for a private file. A real file owned by an
    /// account that is neither this one, SYSTEM nor Administrators needs the
    /// privilege to restore files to be built (a process may not give a file
    /// away to an arbitrary owner, nor create one owned by another account),
    /// so the real file here is one Windows ships: `cmd.exe` is owned by
    /// TrustedInstaller and readable by everyone.
    #[test]
    fn a_trusted_file_owned_by_another_account_is_refused() {
        let me = me();
        for sddl in [
            format!("O:BUD:P(A;;FA;;;{me})"),
            format!("O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;{me})"),
            "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:P(A;;FA;;;SY)"
                .to_string(),
        ] {
            for directory in [false, true] {
                assert_eq!(
                    trusted_verdict(&sddl, directory),
                    Some("is owned by another account"),
                    "{sddl}"
                );
            }
        }
        for sddl in [
            format!("O:{me}D:P(A;;FA;;;{me})"),
            format!("O:BAD:P(A;;FA;;;{me})"),
            "O:SYD:P(A;;FA;;;SY)".to_string(),
        ] {
            for directory in [false, true] {
                assert_eq!(trusted_verdict(&sddl, directory), None, "{sddl}");
            }
        }

        let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let file = open_regular(&cmd, false).unwrap().unwrap();
        let security = security_of_handle(file.as_raw_handle() as HANDLE).unwrap();
        assert_eq!(
            judge(security.0, Rule::Trusted { directory: false }, &cmd).unwrap(),
            Some("is owned by another account")
        );
        // `read_trusted_file(&cmd, ..)` is not asserted here: System32's own
        // owner is another account too, so it would be refused for the
        // directory whatever the file's owner is. The judgment above is of
        // the file alone.
    }

    /// Nobody else may add, delete or replace files in the directory.
    #[test]
    fn a_trusted_file_in_a_directory_another_account_can_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let me = me();
        for entry in [
            "(A;;0x2;;;BU)",
            "(A;;0x4;;;BU)",
            "(A;;0x40;;;BU)",
            "(A;;0x10000;;;AU)",
            "(A;;0x40000;;;WD)",
            "(A;;0x80000;;;BU)",
            "(A;;FW;;;BU)",
            "(A;;FA;;;WD)",
            "(A;OICI;FW;;;BU)",
            "(A;OICI;FA;;;WD)",
            "(A;OI;0x2;;;BU)(A;CI;0x2;;;BU)",
            "(A;;GW;;;BU)",
        ] {
            let shared = dir.path().join("shared");
            create_directory_with(
                &shared,
                &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY){entry}"),
            );
            let path = trusted_file(&shared, "policy.json", &closed("(A;;FR;;;WD)"));
            assert_eq!(refusal(&path), DIRECTORY_REFUSED, "{entry}");
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_dir(&shared).unwrap();
        }
        // A directory others may list and traverse, and no more.
        for entry in ["(A;OICI;FRFX;;;BU)", "(A;;FR;;;WD)", "(A;OICI;0x1;;;AU)"] {
            let shared = dir.path().join("shared");
            create_directory_with(
                &shared,
                &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY){entry}"),
            );
            accepted(&trusted_file(
                &shared,
                "policy.json",
                &closed("(A;;FR;;;WD)"),
            ));
            std::fs::remove_file(shared.join("policy.json")).unwrap();
            std::fs::remove_dir(&shared).unwrap();
        }
        // The same owner rule as the file's.
        assert_eq!(
            trusted_verdict("O:BUD:P(A;OICI;FA;;;SY)", true),
            Some("is owned by another account")
        );
        assert!(
            trusted_verdict("O:SYD:NO_ACCESS_CONTROL", true)
                .unwrap()
                .contains("no access control list")
        );
    }

    /// An entry marked inherit-only does not control access to the object
    /// it is on: Windows consults it only for what is created there. An
    /// ordinary folder carries such entries, for `CREATOR OWNER` and for
    /// others, so they are not counted. A private directory, in contrast,
    /// refuses them, as the files created in it would inherit them.
    #[test]
    fn inherit_only_entries_do_not_count_against_a_trusted_directory() {
        let me = me();
        let sddl = format!(
            "O:{me}D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICIIO;GA;;;CO)(A;OICIIO;FW;;;BU)"
        );
        assert_eq!(trusted_verdict(&sddl, true), None);
        assert_eq!(
            verdict(&sddl, true),
            Some("grants access to another account"),
            "a private directory still refuses it"
        );
        // The same entries on a directory that also applies them to itself.
        let effective = sddl.replace("OICIIO;FW", "OICI;FW");
        assert_eq!(
            trusted_verdict(&effective, true),
            Some("can be changed by another account")
        );
        // An inherit-only entry on a file is skipped too.
        assert_eq!(
            trusted_verdict(&format!("O:{me}D:P(A;;FA;;;{me})(A;OIIO;FW;;;BU)"), false),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        create_directory_with(&folder, &sddl.replace(&format!("O:{me}"), ""));
        accepted(&trusted_file(&folder, "policy.json", &closed("")));
    }

    /// Entries that deny access are never a reason to refuse. An allow
    /// entry for a principal the check does not trust is, and a deny entry
    /// for the same principal does not excuse it: whether the deny wins
    /// depends on the order of the entries and on which groups the caller
    /// is in, so the check relies on neither, as OpenSSH for Windows does.
    #[test]
    fn a_deny_entry_neither_causes_nor_excuses_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        // Rights are written as bits, not as `FW`, which includes
        // READ_CONTROL: denying it to a group this account is in would lock
        // the test out of its own file.
        let me = me();
        for dacl in [
            // Full control for the trusted three and a deny-write for
            // Administrators, before and after the entries that allow.
            format!("D:P(D;;0x2;;;BA)(A;;FA;;;{me})(A;;FA;;;SY)(A;;FA;;;BA)"),
            closed("(A;;FA;;;BA)(D;;0x2;;;BA)"),
            // A deny for another account takes access away from it only.
            closed("(D;;0x2;;;BU)"),
        ] {
            let path = trusted_file(&etc, "policy.json", &dacl);
            accepted(&path);
            std::fs::remove_file(&path).unwrap();
        }
        for (name, dacl) in [
            ("deny first", closed("(D;;0x2;;;BU)(A;;0x2;;;BU)")),
            ("deny last", closed("(A;;0x2;;;BU)(D;;0x2;;;BU)")),
        ] {
            let path = trusted_file(&etc, "policy.json", &dacl);
            assert_eq!(refusal(&path), FILE_REFUSED, "{name}");
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(
            trusted_verdict(&format!("O:{me}D:P(D;;0x2;;;BU)(A;;0x2;;;BU)"), true),
            Some("can be changed by another account")
        );
    }

    /// A file saved in the user's own profile with the ACLs it inherits is
    /// accepted: the profile grants only SYSTEM, Administrators and the
    /// user, and the file is owned by the user or, when an elevated process
    /// created it, by Administrators.
    #[test]
    fn a_trusted_file_in_the_users_profile_with_inherited_access_is_accepted() {
        let profile = std::env::var_os("USERPROFILE").expect("a user profile");
        let dir = tempfile::Builder::new()
            .prefix("rustyschwab-trusted")
            .tempdir_in(profile)
            .unwrap();
        let path = dir.path().join("policy.json");
        std::fs::write(&path, b"[]").unwrap();
        let security =
            security_of_handle(std::fs::File::open(&path).unwrap().as_raw_handle() as HANDLE)
                .unwrap();
        // It does inherit, rather than carry a DACL of its own.
        let (mut control, mut revision) = (0, 0);
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(security.0, &mut control, &mut revision) },
            0
        );
        assert_eq!(control & SE_DACL_PROTECTED, 0);
        accepted(&path);
    }

    /// A link is resolved first, then the file it resolves to is checked.
    #[test]
    fn a_trusted_file_may_be_reached_through_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        let good = trusted_file(&etc, "policy.json", &closed("(A;;FR;;;WD)"));
        let link = dir.path().join("current.json");
        std::os::windows::fs::symlink_file(&good, &link).unwrap();
        accepted(&link);
        // A link to a link, as a ConfigMap mount is.
        let again = dir.path().join("again.json");
        std::os::windows::fs::symlink_file(&link, &again).unwrap();
        accepted(&again);
        // A link to a directory in the path.
        let current = dir.path().join("current");
        std::os::windows::fs::symlink_dir(&etc, &current).unwrap();
        accepted(&current.join("policy.json"));

        // What the link points at is judged, not the link.
        let bad = trusted_file(&etc, "writable.json", &closed("(A;;FW;;;BU)"));
        let to_bad = dir.path().join("to-bad.json");
        std::os::windows::fs::symlink_file(&bad, &to_bad).unwrap();
        assert_eq!(refusal(&to_bad), FILE_REFUSED);

        // The file is opened without following a reparse point: handed a
        // link, as when the path is swapped after it was resolved, the open
        // refuses it, whether by path or by name inside its directory.
        assert_eq!(
            open_regular(&link, false).unwrap().unwrap_err(),
            "is a symbolic link or other reparse point"
        );
        let held = open_directory(dir.path()).unwrap().unwrap();
        let by_name = open_in(&held, std::ffi::OsStr::new("current.json")).unwrap();
        assert_eq!(
            not_regular(&by_name).unwrap(),
            Some("is a symbolic link or other reparse point")
        );
    }

    #[test]
    fn a_trusted_file_must_be_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        let inner = etc.join("inner");
        std::fs::create_dir(&inner).unwrap();
        assert_eq!(
            refusal(&inner),
            "A configuration file is not a regular file; see server logs for the path"
        );
        let missing = read_trusted_file(&etc.join("missing.json"), 16).unwrap_err();
        assert!(matches!(missing, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound));
        // A name relative to a directory is one path component.
        let held = open_directory(&etc).unwrap().unwrap();
        for name in ["", "a\\b"] {
            assert!(
                matches!(open_in(&held, std::ffi::OsStr::new(name)),
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput),
                "{name:?}"
            );
        }
        assert!(
            matches!(open_in(&held, std::ffi::OsStr::new("missing.json")),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound)
        );
    }

    /// Entries of a kind this check cannot evaluate are refused whatever
    /// they grant: an object entry applies to some property of an object,
    /// and a callback or conditional entry depends on the caller. A file
    /// system takes none of them on a real file, so these are judged as
    /// descriptors. The same kinds that deny are ignored, as other deny
    /// entries are.
    #[test]
    fn an_entry_that_cannot_be_evaluated_is_refused_even_when_it_only_reads() {
        let me = me();
        const OBJECT: &str = "bf967a86-0de6-11d0-a285-00aa006c33d6";
        for entry in [
            format!("(OA;;0x1;{OBJECT};;WD)"),
            format!("(OA;;0x1;{OBJECT};;{me})"),
            "(XA;;FR;;;WD;(Member_of {SID(BA)}))".to_string(),
            "(XA;;0x1;;;BU;(Member_of {SID(BA)}))".to_string(),
        ] {
            for directory in [false, true] {
                assert_eq!(
                    trusted_verdict(&format!("O:{me}D:P(A;;FA;;;{me}){entry}"), directory),
                    Some("has an access control entry that cannot be checked"),
                    "{entry}"
                );
            }
        }
        for entry in [
            format!("(OD;;0x2;{OBJECT};;BU)"),
            "(XD;;FW;;;BU;(Member_of {SID(BA)}))".to_string(),
        ] {
            for directory in [false, true] {
                assert_eq!(
                    trusted_verdict(&format!("O:{me}D:P(A;;FA;;;{me}){entry}"), directory),
                    None,
                    "{entry}"
                );
            }
        }
    }

    /// A hard link is another name for the same file, and the file's own
    /// access list is what decides who may change it, so a name for an
    /// acceptable file is accepted. It is opened in, and judged with, the
    /// directory of the name it is read by: a link in a directory others
    /// can write to is refused for that directory, however acceptable the
    /// file is.
    #[test]
    fn a_hard_link_to_an_acceptable_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        let good = trusted_file(&etc, "policy.json", &closed("(A;;FR;;;WD)"));
        let elsewhere = trusted_directory(dir.path(), "elsewhere");
        let link = elsewhere.join("current.json");
        std::fs::hard_link(&good, &link).unwrap();
        accepted(&link);
        accepted(&good);

        let open = dir.path().join("open");
        create_directory_with(
            &open,
            &format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)(A;;0x2;;;BU)", me()),
        );
        let in_open = open.join("current.json");
        std::fs::hard_link(&good, &in_open).unwrap();
        assert_eq!(refusal(&in_open), DIRECTORY_REFUSED);
        accepted(&good);
    }

    /// The directory that was judged cannot be renamed or removed until
    /// the file is open, so a path to it cannot be pointed somewhere else
    /// in between. `between` runs exactly in that window. Once the read is
    /// over, the same rename is allowed, so the refusal is the hold.
    #[test]
    fn the_directory_cannot_be_renamed_while_the_file_opens() {
        let dir = tempfile::tempdir().unwrap();
        let etc = trusted_directory(dir.path(), "etc");
        let path = trusted_file(&etc, "policy.json", &closed("(A;;FR;;;WD)"));
        let moved = dir.path().join("moved");
        let mut rename = None;
        let file = open_trusted_between(&path, || {
            rename = Some(std::fs::rename(&etc, &moved));
        })
        .unwrap();
        let error = rename.expect("between ran").unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32),
            "{error}"
        );
        assert!(etc.exists() && !moved.exists());
        // The file that was opened is the one that was judged.
        assert_eq!(std::io::Read::bytes(file).count(), 2);
        std::fs::rename(&etc, &moved).unwrap();
        assert!(!etc.exists() && moved.join("policy.json").exists());
    }
}
