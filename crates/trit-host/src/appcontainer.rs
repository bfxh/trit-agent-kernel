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
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, REVOKE_ACCESS, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
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

/// 本 workspace 记录过的 profile 清单（相对路径）。
const PROFILE_LIST: &str = ".agent/appcontainer-profiles";
/// profile 清单读取上限；清单只承载 profile 名与时间戳。
const MAX_PROFILE_LIST_BYTES: u64 = 1024 * 1024;
/// profile 的默认 TTL（秒）：超过后下次运行自动回收。
const PROFILE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// 在 AppContainer 内运行 `program args`，返回子进程退出码。
pub fn run(workspace: &Path, program: &str, args: &[String]) -> Result<i32, String> {
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| format!("workspace {}: {error}", workspace.display()))?;
    let _ = prune_expired(&workspace, PROFILE_TTL_SECS);
    let name = profile_name(&workspace);
    let sid = ensure_profile(&name)?;
    remember_profile(&workspace, &name)?;
    let result = (|| {
        grant_workspace(&workspace, sid)?;
        spawn_in_container(&workspace, program, args, sid)
    })();
    // SAFETY: sid came from CreateAppContainerProfile / DeriveAppContainerSidFromAppContainerName.
    unsafe { FreeSid(sid) };
    result
}

/// 删除本 workspace 记录过的全部 AppContainer profile，并撤销它们在 workspace 上的 ACE。
pub fn prune(workspace: &Path) -> Result<usize, String> {
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| format!("workspace {}: {error}", workspace.display()))?;
    let entries = read_profiles(&workspace)?;
    let mut removed = 0usize;
    for (name, _) in &entries {
        if remove_profile(&workspace, name).is_ok() {
            removed += 1;
        }
    }
    write_profiles(&workspace, &[])?;
    Ok(removed)
}

/// 回收超过 `ttl_secs` 未使用的 profile，并撤销其遗留 ACE。
fn prune_expired(workspace: &Path, ttl_secs: u64) -> Result<usize, String> {
    let entries = read_profiles(workspace)?;
    if entries.is_empty() {
        return Ok(0);
    }
    let now = unix_now();
    let total = entries.len();
    let mut kept = Vec::with_capacity(entries.len());
    let mut removed = 0usize;
    for (name, stamp) in entries {
        if now.saturating_sub(stamp) >= ttl_secs {
            if remove_profile(workspace, &name).is_ok() {
                removed += 1;
            }
        } else {
            kept.push((name, stamp));
        }
    }
    if kept.len() != total {
        write_profiles(workspace, &kept)?;
    }
    Ok(removed)
}

/// 删除一个 profile 并撤销它在 workspace 上的 ACE。
///
/// SID 由 profile 名确定性派生，所以即使 profile 早已被删掉，遗留在 workspace
/// 上的孤儿 ACE 仍然能按同一个 SID 撤销。派生必须先于
/// `DeleteAppContainerProfile`，这里保持这个顺序以免依赖实现细节。
fn remove_profile(workspace: &Path, name: &str) -> Result<(), String> {
    let name_wide = wide(name);
    let mut sid: PSID = null_mut();
    // SAFETY: name_wide is NUL-terminated; sid points to writable storage.
    let derived =
        unsafe { DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &mut sid) };
    // SAFETY: name_wide is NUL-terminated.
    unsafe { DeleteAppContainerProfile(name_wide.as_ptr()) };
    if derived < 0 {
        return Err(format!("derive appcontainer sid: 0x{:08x}", derived as u32));
    }
    let revoked = update_workspace_ace(workspace, sid, REVOKE_ACCESS);
    // SAFETY: sid came from DeriveAppContainerSidFromAppContainerName.
    unsafe { FreeSid(sid) };
    revoked
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// profile 清单格式：`<name>\t<unix_secs>`，一行一个；旧版无时间戳的行按“刚用过”处理。
fn read_profiles(workspace: &Path) -> Result<Vec<(String, u64)>, String> {
    let path = workspace.join(PROFILE_LIST);
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_PROFILE_LIST_BYTES) {
        return Err(format!("profile list too large: {}", path.display()));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let now = unix_now();
    let mut entries: Vec<(String, u64)> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (name, stamp) = match line.split_once('\t') {
            Some((name, stamp)) => (name.trim(), stamp.trim().parse::<u64>().unwrap_or(now)),
            None => (line, now),
        };
        if !name.is_empty() {
            entries.push((name.to_owned(), stamp));
        }
    }
    Ok(entries)
}

fn write_profiles(workspace: &Path, entries: &[(String, u64)]) -> Result<usize, String> {
    let path = workspace.join(PROFILE_LIST);
    if entries.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", path.display())),
        }
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let mut text = String::new();
    for (name, stamp) in entries {
        text.push_str(name);
        text.push('\t');
        text.push_str(&stamp.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(entries.len())
}

/// 记录/刷新 profile 的最近使用时间，供 TTL 回收判断。
fn remember_profile(workspace: &Path, name: &str) -> Result<(), String> {
    let mut entries = read_profiles(workspace)?;
    let now = unix_now();
    match entries.iter_mut().find(|(existing, _)| existing == name) {
        Some(entry) => entry.1 = now,
        None => entries.push((name.to_owned(), now)),
    }
    write_profiles(workspace, &entries)?;
    Ok(())
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
    update_workspace_ace(workspace, sid, GRANT_ACCESS)
}

/// 以 `mode`（`GRANT_ACCESS` / `REVOKE_ACCESS`）修改 workspace 根目录 DACL。
///
/// `REVOKE_ACCESS` 会移除该 trustee 在这个 ACL 上的全部 ACE，用于 profile 删除后
/// 清理孤儿授权。
fn update_workspace_ace(workspace: &Path, sid: PSID, mode: i32) -> Result<(), String> {
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
        grfAccessMode: mode,
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

    fn workspace(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "trit-appcontainer-unit-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn profile_list_round_trips_with_timestamps() {
        let root = workspace("list");
        remember_profile(&root, "trit-sandbox-aaaa").unwrap();
        remember_profile(&root, "trit-sandbox-bbbb").unwrap();
        let entries = read_profiles(&root).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|(name, stamp)| name.starts_with("trit-sandbox-") && *stamp > 0)
        );
        // 重复记录只刷新时间戳，不新增行。
        remember_profile(&root, "trit-sandbox-aaaa").unwrap();
        assert_eq!(read_profiles(&root).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_list_accepts_legacy_names_without_timestamps() {
        let root = workspace("legacy");
        let path = root.join(PROFILE_LIST);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "trit-sandbox-legacy\n").unwrap();
        let entries = read_profiles(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "trit-sandbox-legacy");
        // 旧格式没有时间戳：按“刚用过”处理，不会被立刻判过期。
        assert!(unix_now().saturating_sub(entries[0].1) < 60);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_expired_drops_only_stale_entries() {
        let root = workspace("ttl");
        let path = root.join(PROFILE_LIST);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let now = unix_now();
        let stale = now.saturating_sub(PROFILE_TTL_SECS + 60);
        std::fs::write(
            &path,
            format!("trit-sandbox-stale\t{stale}\ntrit-sandbox-fresh\t{now}\n"),
        )
        .unwrap();
        // SID 由 profile 名派生，过期条目对应的 ACE 总能撤销；计数是回收的条目数。
        let removed = prune_expired(&root, PROFILE_TTL_SECS).unwrap();
        assert_eq!(removed, 1);
        let entries = read_profiles(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "trit-sandbox-fresh");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_without_list_is_a_noop() {
        let root = workspace("empty");
        assert_eq!(prune(&root).unwrap(), 0);
        assert!(!root.join(PROFILE_LIST).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
