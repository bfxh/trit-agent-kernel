//! Windows AppContainer 内置沙箱：profile、workspace ACL 与进程创建。
//!
//! 只在 Windows 上编译。调用方是 `trit-host appcontainer-run`：
//!
//! ```text
//! trit-host appcontainer-run --workspace <dir> -- cmd /C <command>
//! ```
//!
//! 步骤：创建/复用 AppContainer profile → 给 workspace 授予该 SID 的读写执行 ACE →
//! 用 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 创建子进程 → 转发退出码。
//! 任一步失败都返回错误，调用方不得回退到无沙箱命令。

#![cfg(windows)]

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, FreeSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
    STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// 在 AppContainer 内运行 `program args`，返回子进程退出码。
pub fn run(workspace: &Path, program: &str, args: &[String]) -> Result<i32, String> {
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| format!("workspace {}: {error}", workspace.display()))?;
    let name = profile_name(&workspace);
    let sid = ensure_profile(&name)?;
    let result = (|| {
        grant_workspace(&workspace, sid)?;
        spawn_in_container(&workspace, program, args, sid)
    })();
    // SAFETY: sid came from CreateAppContainerProfile / DeriveAppContainerSidFromAppContainerName.
    unsafe { FreeSid(sid) };
    result
}

fn profile_name(workspace: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    workspace
        .to_string_lossy()
        .to_lowercase()
        .replace('\\', "/")
        .hash(&mut hasher);
    format!("trit-sandbox-{:016x}", hasher.finish())
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn plain_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_owned(),
        _ => text.into_owned(),
    }
}

fn ensure_profile(name: &str) -> Result<PSID, String> {
    let name_wide = wide(name);
    let mut sid: PSID = null_mut();
    // SAFETY: name_wide is NUL-terminated; sid points to writable storage.
    let hr = unsafe {
        CreateAppContainerProfile(
            name_wide.as_ptr(),
            name_wide.as_ptr(),
            name_wide.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if hr >= 0 {
        return Ok(sid);
    }
    if hr as u32 == ERROR_ALREADY_EXISTS | 0x8007_0000 {
        // SAFETY: same contract as above.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &mut sid) };
        if hr >= 0 {
            return Ok(sid);
        }
        return Err(format!("derive appcontainer sid: 0x{:08x}", hr as u32));
    }
    Err(format!("create appcontainer profile: 0x{:08x}", hr as u32))
}

fn grant_workspace(workspace: &Path, sid: PSID) -> Result<(), String> {
    let path_wide = wide(&workspace.to_string_lossy());
    let mut old_dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: path_wide is NUL-terminated; output pointers are valid.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if rc != 0 {
        return Err(format!("GetNamedSecurityInfoW: {rc}"));
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };
    let mut new_dacl: *mut ACL = null_mut();
    // SAFETY: entry lives for the call; old_dacl belongs to descriptor.
    let rc = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
    if rc != 0 {
        // SAFETY: descriptor was allocated by GetNamedSecurityInfoW.
        unsafe { LocalFree(descriptor as *mut _) };
        return Err(format!("SetEntriesInAclW: {rc}"));
    }
    // SAFETY: path_wide is NUL-terminated; new_dacl is a valid ACL.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null_mut(),
        )
    };
    // SAFETY: both buffers are owned by us / allocated by the APIs above.
    unsafe {
        LocalFree(new_dacl as *mut _);
        LocalFree(descriptor as *mut _);
    }
    if rc != 0 {
        return Err(format!("SetNamedSecurityInfoW: {rc}"));
    }
    Ok(())
}

fn spawn_in_container(
    workspace: &Path,
    program: &str,
    args: &[String],
    sid: PSID,
) -> Result<i32, String> {
    let mut size = 0usize;
    // SAFETY: first call is expected to fail and report the needed size.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
    if size == 0 {
        return Err("InitializeProcThreadAttributeList: no size".into());
    }
    let mut buffer = vec![0u8; size];
    let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: buffer is at least `size` bytes and properly aligned enough for the API.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err("InitializeProcThreadAttributeList failed".into());
    }
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    // SAFETY: list is initialized; capabilities outlives the call.
    let updated = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &capabilities as *const _ as *const _,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        )
    };
    if updated == 0 {
        // SAFETY: list came from InitializeProcThreadAttributeList.
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err("UpdateProcThreadAttribute failed".into());
    }

    // SAFETY: STARTUPINFOEXW is a plain C struct; zero is the documented init.
    let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.lpAttributeList = list;
    // SAFETY: PROCESS_INFORMATION is a plain C struct.
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut command_line = build_command_line(program, args);
    let cwd = wide(&plain_path(workspace));

    // SAFETY: command_line is mutable NUL-terminated UTF-16; info/process are valid.
    let ok = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null_mut(),
            null_mut(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            null_mut(),
            cwd.as_ptr(),
            &info.StartupInfo,
            &mut process,
        )
    };
    // SAFETY: list came from InitializeProcThreadAttributeList.
    unsafe { DeleteProcThreadAttributeList(list) };
    if ok == 0 {
        return Err(format!(
            "CreateProcessW failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: handles are valid until closed below.
    unsafe {
        WaitForSingleObject(process.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(process.hProcess, &mut code);
        CloseHandle(process.hProcess);
        CloseHandle(process.hThread);
        Ok(code as i32)
    }
}

fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(quote(program));
    parts.extend(args.iter().map(|arg| quote(arg)));
    wide(&parts.join(" "))
}

fn quote(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_owned();
    }
    if !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    let escaped = value.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_name_is_stable_and_workspace_scoped() {
        let a = profile_name(Path::new(r"C:\work\a"));
        let b = profile_name(Path::new(r"C:\WORK\A"));
        let c = profile_name(Path::new(r"C:\work\b"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("trit-sandbox-"));
    }

    #[test]
    fn command_line_quotes_spaces_and_quotes() {
        let line = build_command_line("cmd", &["/C".into(), "echo hi".into()]);
        assert_eq!(
            String::from_utf16_lossy(&line[..line.len() - 1]),
            "cmd /C \"echo hi\""
        );
    }
}
