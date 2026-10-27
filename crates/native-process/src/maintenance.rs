//! OS-owned paths, exact filesystem ACLs and a narrow UAC launcher. No scripts
//! or environment-variable path lookups are needed for these trust boundaries.
use std::{
    ffi::OsStr,
    io,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, WAIT_OBJECT_0},
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        SetFileSecurityW,
    },
    System::{
        Com::CoTaskMemFree,
        SystemInformation::GetSystemDirectoryW,
        Threading::{GetExitCodeProcess, WaitForSingleObject},
    },
    UI::{
        Shell::{
            FOLDERID_ProgramData, FOLDERID_ProgramFiles, SEE_MASK_NOCLOSEPROCESS,
            SHELLEXECUTEINFOW, SHGetKnownFolderPath, ShellExecuteExW, ShellExecuteW,
        },
        WindowsAndMessaging::SW_HIDE,
    },
};

fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut result: Vec<_> = value.as_ref().encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::other("NUL in Windows path/argument"));
    }
    result.push(0);
    Ok(result)
}

fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    let mut path = null_mut();
    // SAFETY: the known-folder GUID and output pointer are valid; returned
    // memory belongs to the COM allocator and is released below.
    let status = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut path) };
    if status < 0 {
        return Err(io::Error::other(format!(
            "known folder lookup failed: {status:#x}"
        )));
    }
    let mut length = 0;
    let result = unsafe {
        while *path.add(length) != 0 {
            length += 1;
        }
        PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
            path, length,
        )))
    };
    unsafe {
        CoTaskMemFree(path.cast());
    }
    Ok(result)
}

pub fn program_files_directory() -> io::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramFiles)
}
pub fn program_data_directory() -> io::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramData)
}
pub fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    // SAFETY: buffer is writable and its size is passed in UTF-16 units.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

#[derive(Clone, Copy)]
pub enum FileAccess {
    Private,
    System,
    Program,
}

/// Replace the entire DACL, not just the ACEs of a few named principals.
/// Program files allow ordinary users RX; system state allows only BA/SYSTEM.
pub fn restrict_path(path: &Path, directory: bool, access: FileAccess) -> io::Result<()> {
    let inheritance = if directory { "OICI" } else { "" };
    let mut acl = format!("D:P(A;{inheritance};FA;;;SY)(A;{inheritance};FA;;;BA)");
    let owner = match access {
        FileAccess::Private => {
            let sid = crate::current_identity()?.sid;
            acl.push_str(&format!("(A;{inheritance};FA;;;{sid})"));
            sid
        }
        FileAccess::Program => {
            acl.push_str(&format!("(A;{inheritance};GRGX;;;BU)"));
            "BA".into()
        }
        FileAccess::System => "BA".into(),
    };
    let descriptor = wide(format!("O:{owner}{acl}"))?;
    let path = wide(path)?;
    let mut raw = null_mut();
    // SAFETY: valid terminated SDDL and output storage; conversion allocates
    // the descriptor, which remains alive through SetFileSecurityW.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut raw,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let applied = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            raw,
        )
    };
    let error = io::Error::last_os_error();
    unsafe {
        LocalFree(raw);
    }
    if applied == 0 {
        return Err(error);
    }
    Ok(())
}

fn quote(argument: &str) -> String {
    let mut output = String::from("\"");
    let mut slashes = 0;
    for c in argument.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        output.push_str(&"\\".repeat(if c == '"' { slashes * 2 + 1 } else { slashes }));
        output.push(c);
        slashes = 0;
    }
    output.push_str(&"\\".repeat(slashes * 2));
    output.push('"');
    output
}

pub fn elevate_and_wait(executable: &Path, arguments: &[String]) -> io::Result<u32> {
    let file = wide(executable)?;
    let parameters = wide(
        arguments
            .iter()
            .map(|value| quote(value))
            .collect::<Vec<_>>()
            .join(" "),
    )?;
    let verb = wide("runas")?;
    let mut request: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    request.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    request.fMask = SEE_MASK_NOCLOSEPROCESS;
    request.lpVerb = verb.as_ptr();
    request.lpFile = file.as_ptr();
    request.lpParameters = parameters.as_ptr();
    request.nShow = SW_HIDE;
    // SAFETY: all pointer fields reference live, terminated UTF-16 allocations.
    if unsafe { ShellExecuteExW(&mut request) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if request.hProcess.is_null() {
        return Err(io::Error::other("UAC did not return a helper process"));
    }
    // The UI thread is not blocked; this executes on an installation worker.
    let waited = unsafe { WaitForSingleObject(request.hProcess, 10 * 60 * 1000) };
    let mut code = 1;
    let status = unsafe { GetExitCodeProcess(request.hProcess, &mut code) };
    unsafe {
        CloseHandle(request.hProcess);
    }
    if waited != WAIT_OBJECT_0 || status == 0 {
        return Err(io::Error::other(
            "installation helper did not complete within ten minutes",
        ));
    }
    Ok(code)
}

pub fn open_browser(url: &str) -> io::Result<()> {
    let verb = wide("open")?;
    let target = wide(url)?;
    // SAFETY: this is called only with an application-generated local HTTP URL.
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            1,
        )
    } as isize;
    if result <= 32 {
        return Err(io::Error::other(format!(
            "could not open browser ({result})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_arguments_are_quoted_without_shell_interpolation() {
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("C:\\end\\"), "\"C:\\end\\\\\"");
    }
    #[test]
    fn windows_paths_are_absolute_os_locations() {
        assert!(program_files_directory().unwrap().is_absolute());
        assert!(program_data_directory().unwrap().is_absolute());
        assert!(system_directory().unwrap().join("cmd.exe").is_file());
    }
}
