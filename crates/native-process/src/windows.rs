use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr::{null, null_mut},
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::ConvertSidToStringSidW, *},
    System::{JobObjects::*, Threading::*},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub sid: String,
    pub is_system: bool,
    pub elevated: bool,
    pub integrity: String,
    pub session_id: u32,
}

/// Sent only over the mutually authenticated pairing bridge, never accepted
/// from a public run request. Creation time protects against PID recycling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserContext {
    pub pid: u32,
    pub created_at: u64,
    pub identity: Identity,
    pub environment: BTreeMap<String, String>,
    pub cwd: String,
}

fn error(message: &str) -> io::Error {
    io::Error::other(message.to_owned())
}
fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.as_ref().encode_wide().collect();
    if result.contains(&0) {
        return Err(error("NUL is not allowed in process parameters"));
    }
    result.push(0);
    Ok(result)
}
fn check(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn own(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: callers pass a newly owned Win32 handle, never a pseudo handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
fn token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut handle = null_mut();
    // SAFETY: handle points to writable storage; process remains open.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut handle) })?;
    own(handle)
}
fn token_data(handle: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut required = 0;
    // SAFETY: zero-sized query obtains required size only.
    unsafe {
        GetTokenInformation(handle, class, null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut data = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned allocation is at least required bytes and lives through parsing.
    check(unsafe {
        GetTokenInformation(
            handle,
            class,
            data.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    })?;
    Ok(data)
}
fn sid_text(sid: PSID) -> io::Result<String> {
    let mut raw = null_mut();
    // SAFETY: SID is supplied by GetTokenInformation and remains allocated.
    check(unsafe { ConvertSidToStringSidW(sid, &mut raw) })?;
    let result = unsafe { read_wide(raw) };
    // SAFETY: ConvertSidToStringSidW requires LocalFree.
    unsafe {
        LocalFree(raw.cast());
    }
    Ok(result)
}
unsafe fn read_wide(raw: *const u16) -> String {
    let mut len = 0;
    // SAFETY: caller supplies a valid NUL-terminated Win32 string.
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(raw, len))
    }
}
fn identity(handle: HANDLE) -> io::Result<Identity> {
    let user_data = token_data(handle, TokenUser)?;
    // SAFETY: class determines struct layout, allocation is aligned and valid.
    let user = unsafe { &*user_data.as_ptr().cast::<TOKEN_USER>() };
    let sid = sid_text(user.User.Sid)?;
    let mut name_len = 0;
    let mut domain_len = 0;
    let mut kind = 0;
    unsafe {
        LookupAccountSidW(
            null(),
            user.User.Sid,
            null_mut(),
            &mut name_len,
            null_mut(),
            &mut domain_len,
            &mut kind,
        );
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    let account = if unsafe {
        LookupAccountSidW(
            null(),
            user.User.Sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    } != 0
    {
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        }
    } else {
        sid.clone()
    };
    let elevation = token_data(handle, TokenElevation)?;
    let elevated = unsafe { (*elevation.as_ptr().cast::<TOKEN_ELEVATION>()).TokenIsElevated != 0 };
    let session = token_data(handle, TokenSessionId)?;
    let session_id = unsafe { *session.as_ptr().cast::<u32>() };
    let mandatory = token_data(handle, TokenIntegrityLevel)?;
    let label = unsafe { &*mandatory.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
    if count == 0 {
        return Err(error("invalid token integrity SID"));
    }
    let rid = unsafe { *GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)) };
    let integrity = match rid {
        0..0x1000 => "Untrusted",
        0x1000..0x2000 => "Low",
        0x2000..0x3000 => "Medium",
        0x3000..0x4000 => "High",
        0x4000..0x5000 => "System",
        _ => "Protected",
    }
    .to_owned();
    Ok(Identity {
        user: account,
        is_system: sid == "S-1-5-18",
        sid,
        elevated,
        integrity,
        session_id,
    })
}
pub fn current_identity() -> io::Result<Identity> {
    let handle = token(unsafe { GetCurrentProcess() })?;
    identity(handle.as_raw_handle())
}
fn creation_time(process: HANDLE) -> io::Result<u64> {
    let mut times = [FILETIME::default(); 4];
    check(unsafe {
        GetProcessTimes(
            process,
            &mut times[0],
            &mut times[1],
            &mut times[2],
            &mut times[3],
        )
    })?;
    Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
}
pub fn current_user_context() -> io::Result<UserContext> {
    let identity = current_identity()?;
    if identity.elevated || identity.is_system {
        return Err(error("the paired user daemon must be non-elevated"));
    }
    Ok(UserContext {
        pid: std::process::id(),
        created_at: creation_time(unsafe { GetCurrentProcess() })?,
        identity,
        environment: std::env::vars().collect(),
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
    })
}
fn administrator_token(context: &UserContext) -> io::Result<OwnedHandle> {
    let process = own(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, context.pid) })?;
    if creation_time(process.as_raw_handle())? != context.created_at {
        return Err(error("paired user process changed; retry authorization"));
    }
    let original = token(process.as_raw_handle())?;
    let actual = identity(original.as_raw_handle())?;
    // Display names may resolve differently in SYSTEM and user contexts (for
    // example a Microsoft account alias). Authenticate the SID/session/token,
    // never the localized or context-dependent display name.
    if actual.sid != context.identity.sid
        || actual.session_id != context.identity.session_id
        || actual.integrity != context.identity.integrity
        || actual.elevated
        || actual.is_system
    {
        return Err(error(&format!(
            "paired user process identity does not match (sid={}, session={}, integrity={}, elevated={}, system={})",
            actual.sid == context.identity.sid,
            actual.session_id == context.identity.session_id,
            actual.integrity == context.identity.integrity,
            actual.elevated,
            actual.is_system
        )));
    }
    let data = token_data(original.as_raw_handle(), TokenLinkedToken)
        .map_err(|_| error("the paired current user has no linked administrator token; no identity fallback is permitted"))?;
    let linked = own(unsafe { (*data.as_ptr().cast::<TOKEN_LINKED_TOKEN>()).LinkedToken })?;
    let elevated = identity(linked.as_raw_handle())?;
    if elevated.sid != actual.sid || !elevated.elevated || elevated.is_system {
        return Err(error(
            "linked token is not the same user's administrator identity",
        ));
    }
    let mut primary = null_mut();
    check(unsafe {
        DuplicateTokenEx(
            linked.as_raw_handle(),
            TOKEN_ALL_ACCESS,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    })?;
    own(primary)
}
pub fn administrator_identity(context: &UserContext) -> io::Result<Identity> {
    let token = administrator_token(context)?;
    let mut identity = identity(token.as_raw_handle())?;
    identity.session_id = current_identity()?.session_id;
    Ok(identity)
}

pub struct Spawned {
    pub child: Child,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}
pub struct Child {
    process: OwnedHandle,
    job: OwnedHandle,
    pub identity: Identity,
    pub id: u32,
}
impl Child {
    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
                Ok(Some(code as i32))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn kill_tree(&self) -> io::Result<()> {
        check(unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) })
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.kill_tree();
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    check(unsafe {
        windows_sys::Win32::System::Pipes::CreatePipe(&mut read, &mut write, &attributes, 0)
    })?;
    Ok((own(read)?, own(write)?))
}
fn quote(argument: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in argument.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            result.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            result.push_str(&"\\".repeat(slashes));
        }
        result.push(ch);
        slashes = 0;
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}
struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast());
        }
    }
}

/// `administrator` must originate from the authenticated paired user daemon.
/// Normal spawns preserve the caller's identity. Administrator spawns preserve
/// the user's environment and logon identity, but run without a desktop in the
/// broker's session so redirected handles can be inherited safely.
pub fn spawn(
    executable: &str,
    arguments: &[&str],
    administrator: Option<&UserContext>,
) -> io::Result<Spawned> {
    spawn_with_environment(executable, arguments, administrator, &BTreeMap::new())
}

pub fn spawn_with_environment(
    executable: &str,
    arguments: &[&str],
    administrator: Option<&UserContext>,
    overrides: &BTreeMap<String, String>,
) -> io::Result<Spawned> {
    let admin_token = administrator.map(administrator_token).transpose()?;
    let (input_read, input_write) = pipe()?;
    let (output_read, output_write) = pipe()?;
    let (error_read, error_write) = pipe()?;
    for handle in [&input_write, &output_read, &error_read] {
        check(unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) })?;
    }
    let job = own(unsafe { CreateJobObjectW(null(), null()) })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    })?;
    let mut attribute_size = 0;
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size);
    }
    let mut raw_attributes = vec![0usize; attribute_size.div_ceil(size_of::<usize>())];
    check(unsafe {
        InitializeProcThreadAttributeList(
            raw_attributes.as_mut_ptr().cast(),
            1,
            0,
            &mut attribute_size,
        )
    })?;
    let mut attributes = Attributes(raw_attributes);
    let handles = [
        input_read.as_raw_handle(),
        output_write.as_raw_handle(),
        error_write.as_raw_handle(),
    ];
    check(unsafe {
        UpdateProcThreadAttribute(
            attributes.0.as_mut_ptr().cast(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(&handles),
            null_mut(),
            null(),
        )
    })?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input_read.as_raw_handle();
    startup.StartupInfo.hStdOutput = output_write.as_raw_handle();
    startup.StartupInfo.hStdError = error_write.as_raw_handle();
    startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
    let mut environment: BTreeMap<String, String> = administrator
        .map(|context| context.environment.clone())
        .unwrap_or_else(|| std::env::vars().collect());
    overlay_environment(&mut environment, overrides);
    if let Some(path) = environment
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())
    {
        environment.insert("LOCALSHELLD_INITIAL_PATH".into(), path);
    }
    environment.insert("LOCALSHELLD_SESSION".into(), "1".into());
    let mut env = zeroize::Zeroizing::new(Vec::<u16>::new());
    // Windows environment names are case insensitive. Sort without changing values.
    let mut entries: Vec<_> = environment.into_iter().collect();
    entries.sort_by_key(|(key, _)| key.to_uppercase());
    for (key, value) in entries {
        if key.contains('=') {
            return Err(error("invalid environment variable name"));
        }
        env.extend(wide(format!("{key}={value}"))?);
    }
    env.push(0);
    let cwd = administrator
        .map(|context| wide(&context.cwd))
        .transpose()?;
    let cmd = std::iter::once(executable)
        .chain(arguments.iter().copied())
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd = wide(cmd)?;
    if cmd.len() >= 32767 {
        return Err(error(
            "shell bootstrap command exceeds Windows command-line limit",
        ));
    }
    let mut information: PROCESS_INFORMATION = unsafe { zeroed() };
    let flags = CREATE_NO_WINDOW
        | CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT;
    let cwd = cwd.as_ref().map_or(null(), |value| value.as_ptr());
    let ok = if let Some(token) = &admin_token {
        // Inheritable handles cannot cross Windows sessions. This changes only
        // the window-station session, not the account/logon token or HKCU.
        let session = current_identity()?.session_id;
        if identity(token.as_raw_handle())?.session_id != session {
            check(unsafe {
                SetTokenInformation(
                    token.as_raw_handle(),
                    TokenSessionId,
                    (&session as *const u32).cast(),
                    size_of::<u32>() as u32,
                )
            })?;
        }
        unsafe {
            CreateProcessAsUserW(
                token.as_raw_handle(),
                null(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_ptr().cast(),
                cwd,
                &startup.StartupInfo,
                &mut information,
            )
        }
    } else {
        unsafe {
            CreateProcessW(
                null(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_ptr().cast(),
                cwd,
                &startup.StartupInfo,
                &mut information,
            )
        }
    };
    check(ok)?;
    let process = own(information.hProcess)?;
    let thread = own(information.hThread)?;
    if let Err(failure) =
        check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) })
    {
        unsafe {
            TerminateProcess(process.as_raw_handle(), 1);
        }
        return Err(failure);
    }
    let actual_identity = identity(token(process.as_raw_handle())?.as_raw_handle())?;
    let child = Child {
        process,
        job,
        identity: actual_identity,
        id: information.dwProcessId,
    };
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    drop((input_read, output_write, error_write));
    Ok(Spawned {
        child,
        stdin: File::from(input_write),
        stdout: File::from(output_read),
        stderr: File::from(error_read),
    })
}

fn overlay_environment(
    environment: &mut BTreeMap<String, String>,
    overrides: &BTreeMap<String, String>,
) {
    for (key, value) in overrides {
        environment.retain(|name, _| !name.eq_ignore_ascii_case(key));
        environment.insert(key.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn overrides_replace_case_insensitive_inherited_names_without_mutating_the_parent() {
        let original = BTreeMap::from([
            ("Test_Token".into(), "inherited".into()),
            ("OTHER".into(), "preserved".into()),
        ]);
        let mut environment = original.clone();
        let overrides = BTreeMap::from([("TEST_TOKEN".into(), "selected".into())]);
        overlay_environment(&mut environment, &overrides);
        assert_eq!(environment.len(), 2);
        assert_eq!(environment["TEST_TOKEN"], "selected");
        assert!(!environment.contains_key("Test_Token"));
        assert_eq!(environment["OTHER"], "preserved");
        assert_eq!(original["Test_Token"], "inherited");
    }

    #[test]
    fn token_identity_is_real_and_command_arguments_are_quoted() {
        let identity = current_identity().unwrap();
        assert!(identity.sid.starts_with("S-1-"));
        assert!(!identity.user.is_empty());
        assert_eq!(quote("a b\\"), "\"a b\\\\\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn child_has_separate_streams_and_is_contained() {
        let mut spawned = spawn("pwsh", &["-NoProfile", "-NonInteractive", "-Command", "[Console]::Out.Write([Console]::In.ReadLine()); [Console]::Error.Write('err'); exit 7"], None).unwrap();
        spawned.stdin.write_all(b"hello\n").unwrap();
        drop(spawned.stdin);
        let mut stdout = String::new();
        let mut stderr = String::new();
        spawned.stdout.read_to_string(&mut stdout).unwrap();
        spawned.stderr.read_to_string(&mut stderr).unwrap();
        assert_eq!(stdout, "hello");
        assert_eq!(stderr, "err");
        for _ in 0..100 {
            if let Some(code) = spawned.child.try_wait().unwrap() {
                assert_eq!(code, 7);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("child did not exit");
    }
}
