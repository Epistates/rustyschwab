//! Reports what Windows records for files created the old and new way.
#![cfg(windows)]
use std::{os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::*, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_OWNER,
        TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
    },
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

const ALL: u32 = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;

fn sddl_of(path: &Path) -> String {
    let mut descriptor = ptr::null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            ALL,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return format!("<GetNamedSecurityInfoW error {code}>");
    }
    let mut output = ptr::null_mut();
    let mut len = 0;
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            ALL,
            &mut output,
            &mut len,
        )
    };
    assert_ne!(ok, 0);
    let text = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(output, len.saturating_sub(1) as usize)
    });
    unsafe {
        LocalFree(output.cast());
        LocalFree(descriptor);
    }
    text
}

fn sid_text(sid: PSID) -> String {
    let mut text = ptr::null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    out
}

fn token_sids() -> (String, String) {
    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let read = |class| {
        let mut len = 0u32;
        unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut len) };
        let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token,
                    class,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                    &mut len,
                )
            },
            0
        );
        buffer
    };
    let user = read(TokenUser);
    let owner = read(TokenOwner);
    let user = sid_text(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid });
    let owner = sid_text(unsafe { (*owner.as_ptr().cast::<TOKEN_OWNER>()).Owner });
    unsafe { CloseHandle(token) };
    (user, owner)
}

fn with_descriptor<T>(sddl: &str, f: impl FnOnce(&SECURITY_ATTRIBUTES) -> T) -> T {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        },
        0
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let out = f(&attributes);
    unsafe { LocalFree(descriptor) };
    out
}

fn move_over(source: &Path, destination: &Path) -> String {
    let ok = unsafe {
        MoveFileExW(
            wide(source).as_ptr(),
            wide(destination).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok != 0 {
        "ok".into()
    } else {
        format!("failed: {}", std::io::Error::last_os_error())
    }
}

#[test]
fn report() {
    let mut out = String::from("\n");
    let mut say = |label: &str, value: String| out.push_str(&format!("{label}: {value}\n"));
    let (user, owner) = token_sids();
    say("token user", user);
    say("token default owner", owner);
    say("temp_dir", format!("{:?}", std::env::temp_dir()));
    say("temp_dir sddl", sddl_of(&std::env::temp_dir()));

    let dir = tempfile::tempdir().unwrap();
    say("tempdir", format!("{:?}", dir.path()));
    say("tempdir sddl", sddl_of(dir.path()));

    let plain = dir.path().join("plain.txt");
    std::fs::write(&plain, b"x").unwrap();
    say("default file sddl", sddl_of(&plain));

    // The descriptor releases up to now passed at creation, and the text
    // they then compared the file's DACL against.
    let old = dir.path().join("old.json");
    let handle = with_descriptor("D:P(A;;FA;;;OW)(A;;FA;;;SY)", |attributes| unsafe {
        CreateFileW(
            wide(&old).as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    });
    assert_ne!(handle, INVALID_HANDLE_VALUE);
    unsafe { CloseHandle(handle) };
    say("old file sddl", sddl_of(&old));

    let old_dir = dir.path().join("old-dir");
    let made = with_descriptor("D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)", |attributes| unsafe {
        CreateDirectoryW(wide(&old_dir).as_ptr(), attributes)
    });
    assert_ne!(made, 0);
    say("old directory sddl", sddl_of(&old_dir));
    let child = old_dir.join("child.txt");
    say("write into old directory", format!("{:?}", std::fs::write(&child, b"x")));
    say("old directory child sddl", sddl_of(&child));

    let new = dir.path().join("new.json");
    say(
        "secure_file_write",
        format!("{:?}", rustyschwab::security::secure_file_write(&new, b"x")),
    );
    say("new file sddl", sddl_of(&new));
    let new_dir = dir.path().join("new-dir");
    say(
        "ensure_private_directory(new)",
        format!("{:?}", rustyschwab::security::ensure_private_directory(&new_dir)),
    );
    say("new directory sddl", sddl_of(&new_dir));
    say(
        "ensure_private_directory(tempdir)",
        format!("{:?}", rustyschwab::security::ensure_private_directory(dir.path())),
    );

    // Replacing a file another handle has open.
    let destination = dir.path().join("destination");
    std::fs::write(&destination, b"old").unwrap();
    let source = dir.path().join("source");
    std::fs::write(&source, b"new").unwrap();
    say("MoveFileExW over a closed file", move_over(&source, &destination));
    std::fs::write(&source, b"newer").unwrap();
    let reader = std::fs::File::open(&destination).unwrap();
    say("MoveFileExW over an open file", move_over(&source, &destination));
    say(
        "std::fs::rename over an open file",
        format!("{:?}", std::fs::rename(&source, &destination)),
    );
    drop(reader);
    let named = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(&source, b"newest").unwrap();
    say("MoveFileExW over a NamedTempFile", move_over(&source, named.path()));

    for target in [dir.path(), &std::env::temp_dir()] {
        let icacls = std::process::Command::new("icacls").arg(target).output().unwrap();
        say("icacls", String::from_utf8_lossy(&icacls.stdout).into_owned());
    }
    panic!("{out}");
}
