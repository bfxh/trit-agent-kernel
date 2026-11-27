//! 跨进程写入租约（operation lease）与 fencing token。
//!
//! 租约是 `<root>/.agent/write-lease` 上的 **OS 排他锁**：进程退出（包括崩溃）时
//! 内核自动释放，不需要 stale 检测。每次成功获取都会把文件里的 `epoch` 加一，
//! 形成 fencing token；旧持有者的 `check()` 会读到新 epoch 并立即失败。
//!
//! 诚实边界：
//!
//! - 这是**本地文件系统**的租约，不是跨机器写权威；分布式存储需要存储端校验 token。
//! - 它只约束经过 `ClaimingTools` 的写入；绕过包装器直接写文件不在保护范围内。
//! - 锁被占用时返回 [`LeaseError::Busy`]，不排队、不自动等待。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 租约文件相对 root 的路径。
pub const LEASE_PATH: &str = ".agent/write-lease";
/// lease 文件读取上限；lease 只承载小 JSON。
const MAX_LEASE_BYTES: u64 = 1024 * 1024;

/// 获取租约失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// 另一个持有者仍占着锁。
    Busy { owner: String, epoch: u64 },
    /// 打开、加锁或写租约文件失败。
    Io(String),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { owner, epoch } => {
                write!(
                    formatter,
                    "write lease is busy: owner={owner} epoch={epoch}"
                )
            }
            Self::Io(message) => write!(formatter, "write lease io: {message}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// 租约文件内容；`released` 只用于审计，不参与锁判断。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LeaseFile {
    #[serde(default)]
    owner: String,
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    acquired_at: u64,
    #[serde(default)]
    released: bool,
}

/// 一个已持有的跨进程写租约（一个或多个路径锁）；drop 时解锁。
#[derive(Debug)]
pub struct OperationLease {
    owner: String,
    epoch: u64,
    locks: Vec<LeaseLock>,
}

#[derive(Debug)]
struct LeaseLock {
    path: PathBuf,
    epoch: u64,
    file: File,
}

impl OperationLease {
    /// 尝试获取 `<root>/.agent/write-lease` 的 root 级排他锁。
    pub fn acquire(root: impl AsRef<Path>, owner: impl Into<String>) -> Result<Self, LeaseError> {
        Self::acquire_paths(root, owner, &[])
    }

    /// 按声明路径获取租约；`paths` 为空时等价于 root 级租约。
    ///
    /// 每个路径映射到 `.agent/leases/<hash>.lock`；不同路径的租约可以并行持有。
    /// 冲突按**声明字符串**判断，不解析 glob 交集。
    pub fn acquire_paths(
        root: impl AsRef<Path>,
        owner: impl Into<String>,
        paths: &[PathBuf],
    ) -> Result<Self, LeaseError> {
        let root = root.as_ref();
        let targets: Vec<PathBuf> = if paths.is_empty() {
            vec![root.join(LEASE_PATH)]
        } else {
            paths.iter().map(|path| lock_path(root, path)).collect()
        };
        Self::from_targets(owner, targets)
    }

    /// 按**锁文件路径**获取租约（相对 `root` 或绝对）；供恢复工具精确重放
    /// journal 里记录的 lease，而不是重新推导声明路径。
    pub fn acquire_lock_files(
        root: impl AsRef<Path>,
        owner: impl Into<String>,
        lock_paths: &[PathBuf],
    ) -> Result<Self, LeaseError> {
        let root = root.as_ref();
        let targets: Vec<PathBuf> = lock_paths
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                }
            })
            .collect();
        Self::from_targets(owner, targets)
    }

    fn from_targets(owner: impl Into<String>, targets: Vec<PathBuf>) -> Result<Self, LeaseError> {
        let owner = owner.into();
        let mut locks: Vec<LeaseLock> = Vec::with_capacity(targets.len());
        for path in targets {
            match open_and_lock(&path, &owner) {
                Ok(lock) => locks.push(lock),
                Err(error) => {
                    for lock in &locks {
                        let _ = unlock(&lock.file);
                    }
                    return Err(error);
                }
            }
        }
        let epoch = locks.first().map_or(0, |lock| lock.epoch);
        Ok(Self {
            owner,
            epoch,
            locks,
        })
    }

    /// 第一个锁文件路径（root 级或第一个声明路径）。
    pub fn path(&self) -> &Path {
        &self.locks[0].path
    }

    /// 本次租约持有的全部锁文件路径；用于 mutation journal 记录恢复边界。
    pub fn lock_paths(&self) -> Vec<&Path> {
        self.locks.iter().map(|lock| lock.path.as_path()).collect()
    }

    /// 持有者标识。
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// 第一个锁的 fencing token。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 本次租约覆盖的锁数量。
    pub fn lock_count(&self) -> usize {
        self.locks.len()
    }

    /// 自己是否仍持有全部锁：每个文件的 owner/epoch 未被替换，且未标记 released。
    ///
    /// 每次写操作前都应调用；失败意味着另一个进程已经重新获取过其中某个路径。
    pub fn check(&self) -> bool {
        self.locks.iter().all(|lock| match read_lease(&lock.path) {
            Ok(current) => {
                !current.released && current.owner == self.owner && current.epoch == lock.epoch
            }
            Err(_) => false,
        })
    }

    /// 显式释放：把所有锁文件标成 released，再解锁。
    pub fn release(self) -> Result<(), LeaseError> {
        let mut first_error = None;
        for lock in &self.locks {
            let content = LeaseFile {
                owner: self.owner.clone(),
                epoch: lock.epoch,
                acquired_at: now_secs(),
                released: true,
            };
            if let Err(error) = write_lease(&lock.file, &content) {
                first_error.get_or_insert(error);
            }
            if let Err(error) = unlock(&lock.file) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        for lock in &self.locks {
            let _ = unlock(&lock.file);
        }
    }
}

/// 任意文件上的单实例 OS 排他锁；进程退出（包括崩溃）时内核自动释放。
///
/// 与 [`OperationLease`] 不同，它不写 epoch / owner，也不承担 fencing：
/// 只回答“这个文件此刻是否已有另一个持有者”。适合守护进程的实例互斥。
#[derive(Debug)]
pub struct SingleInstanceLock {
    file: File,
    path: PathBuf,
}

impl SingleInstanceLock {
    /// 尝试独占 `path`；已被占用返回 `Ok(None)`，不排队。
    pub fn acquire(path: impl AsRef<Path>) -> Result<Option<Self>, LeaseError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| LeaseError::Io(format!("create {}: {error}", parent.display())))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| LeaseError::Io(format!("open {}: {error}", path.display())))?;
        if !try_lock(&file)? {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            path: path.to_path_buf(),
        }))
    }

    /// 被锁定的文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        let _ = unlock(&self.file);
    }
}

fn open_and_lock(path: &Path, owner: &str) -> Result<LeaseLock, LeaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| LeaseError::Io(format!("create {}: {error}", parent.display())))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| LeaseError::Io(format!("open {}: {error}", path.display())))?;
    if !try_lock(&file)? {
        let current = read_lease(path).unwrap_or_default();
        return Err(LeaseError::Busy {
            owner: current.owner,
            epoch: current.epoch,
        });
    }
    let previous = read_lease(path).unwrap_or_default();
    let epoch = previous.epoch.saturating_add(1);
    let content = LeaseFile {
        owner: owner.to_owned(),
        epoch,
        acquired_at: now_secs(),
        released: false,
    };
    write_lease(&file, &content)?;
    Ok(LeaseLock {
        path: path.to_path_buf(),
        epoch,
        file,
    })
}

/// 声明路径对应的锁文件路径；对外暴露以便 journal 记录同一把锁。
pub fn lease_path_for(root: &Path, path: &Path) -> PathBuf {
    lock_path(root, path)
}

fn lock_path(root: &Path, path: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let normalized = path.to_string_lossy().replace('\\', "/");
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    root.join(".agent/leases")
        .join(format!("{:016x}.lock", hasher.finish()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_lease(path: &Path) -> Result<LeaseFile, LeaseError> {
    let mut file = File::open(path)
        .map_err(|error| LeaseError::Io(format!("open {}: {error}", path.display())))?;
    // byte 0 被 OS 锁占用（Windows）；内容从 offset 1 开始，读时跳过锁区。
    file.seek(SeekFrom::Start(1))
        .map_err(|error| LeaseError::Io(format!("seek {}: {error}", path.display())))?;
    if let Ok(metadata) = file.metadata()
        && metadata.len() > MAX_LEASE_BYTES
    {
        return Err(LeaseError::Io(format!(
            "read {}: file too large",
            path.display()
        )));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| LeaseError::Io(format!("read {}: {error}", path.display())))?;
    if text.trim().is_empty() {
        return Ok(LeaseFile::default());
    }
    serde_json::from_str(&text)
        .map_err(|error| LeaseError::Io(format!("parse {}: {error}", path.display())))
}

fn write_lease(file: &File, content: &LeaseFile) -> Result<(), LeaseError> {
    let mut handle: &File = file;
    handle
        .seek(SeekFrom::Start(0))
        .map_err(|error| LeaseError::Io(format!("seek lease: {error}")))?;
    file.set_len(0)
        .map_err(|error| LeaseError::Io(format!("truncate lease: {error}")))?;
    // byte 0 是锁区占位，内容从 offset 1 开始。
    handle
        .write_all(b" ")
        .map_err(|error| LeaseError::Io(format!("reserve lease byte: {error}")))?;
    let text = serde_json::to_string(content)
        .map_err(|error| LeaseError::Io(format!("encode lease: {error}")))?;
    handle
        .write_all(text.as_bytes())
        .map_err(|error| LeaseError::Io(format!("write lease: {error}")))?;
    handle
        .sync_all()
        .map_err(|error| LeaseError::Io(format!("sync lease: {error}")))?;
    Ok(())
}

#[cfg(unix)]
fn try_lock(file: &File) -> Result<bool, LeaseError> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: `fd` is owned by `file`, which outlives this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(LeaseError::Io(format!("flock: {error}")))
    }
}

#[cfg(unix)]
fn unlock(file: &File) -> Result<(), LeaseError> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: `fd` is owned by `file`, which outlives this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(LeaseError::Io(format!(
            "flock unlock: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(windows)]
fn try_lock(file: &File) -> Result<bool, LeaseError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // SAFETY: zeroed OVERLAPPED is the documented initial value for LockFileEx.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: handle is valid for the lifetime of `file`; overlapped is stack-owned.
    let ok = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    // ERROR_LOCK_VIOLATION (33): another handle already holds the range.
    if error.raw_os_error() == Some(33) {
        Ok(false)
    } else {
        Err(LeaseError::Io(format!("LockFileEx: {error}")))
    }
}

#[cfg(windows)]
fn unlock(file: &File) -> Result<(), LeaseError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // SAFETY: zeroed OVERLAPPED is the documented initial value for UnlockFileEx.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: handle is valid for the lifetime of `file`; overlapped is stack-owned.
    let ok = unsafe { UnlockFileEx(handle, 0, 1, 0, &mut overlapped) };
    if ok != 0 {
        Ok(())
    } else {
        Err(LeaseError::Io(format!(
            "UnlockFileEx: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn try_lock(_file: &File) -> Result<bool, LeaseError> {
    Err(LeaseError::Io(
        "operation lease is not implemented on this platform".to_owned(),
    ))
}

#[cfg(not(any(unix, windows)))]
fn unlock(_file: &File) -> Result<(), LeaseError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JOURNAL_PATH, JournalRecord, MutationJournal};
    use crate::permissions::{ClaimingTools, WriteClaim};
    use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

    fn root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "trit-lease-unit-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    struct EchoTools;

    impl ToolExecutor for EchoTools {
        fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
            Ok(ToolOutcome {
                call_id: request.call_id.clone(),
                tool: request.tool.clone(),
                command: request.command.clone(),
                passed: true,
                assertions: 0,
                written_paths: request.paths.clone(),
                in_doubt: false,
                output: "ok".into(),
            })
        }
    }

    #[test]
    fn single_instance_lock_is_exclusive_and_released_on_drop() {
        let root = root("single-instance");
        let path = root.join("monitor.lock");
        let first = SingleInstanceLock::acquire(&path)
            .unwrap()
            .expect("first lock");
        assert_eq!(first.path(), path.as_path());
        assert!(
            SingleInstanceLock::acquire(&path).unwrap().is_none(),
            "second holder must not acquire the same lock"
        );
        drop(first);
        assert!(
            SingleInstanceLock::acquire(&path).unwrap().is_some(),
            "drop must release the OS lock"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn check_fails_when_epoch_is_replaced() {
        let root = root("fence");
        let lease = OperationLease::acquire(&root, "host-a").unwrap();
        assert!(lease.check());
        // 用持锁句柄改写：模拟另一个持有者已经推进 fencing token。
        write_lease(
            &lease.locks[0].file,
            &LeaseFile {
                owner: "host-b".into(),
                epoch: 99,
                acquired_at: 0,
                released: false,
            },
        )
        .unwrap();
        assert!(!lease.check());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claiming_tools_block_after_lease_loss() {
        let root = root("claim");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "x").unwrap();
        let lease = OperationLease::acquire(&root, "host-a").unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src/a.py")]).unwrap();
        let mut tools = ClaimingTools::new(EchoTools, claim).with_lease(lease);

        let request = ToolRequest::write("w1", "write_file", "src/a.py", "y");
        assert!(tools.execute(&request).unwrap().passed);

        {
            let lease = tools.lease().expect("lease");
            write_lease(
                &lease.locks[0].file,
                &LeaseFile {
                    owner: "host-b".into(),
                    epoch: 2,
                    acquired_at: 0,
                    released: false,
                },
            )
            .unwrap();
        }
        let outcome = tools.execute(&request).unwrap();
        assert!(!outcome.passed);
        assert!(
            outcome.output.contains("write lease is no longer valid"),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn path_leases_run_in_parallel_and_conflict_on_overlap() {
        let root = root("paths");
        let a =
            OperationLease::acquire_paths(&root, "host-a", &[PathBuf::from("src/a.py")]).unwrap();
        let b =
            OperationLease::acquire_paths(&root, "host-b", &[PathBuf::from("src/b.py")]).unwrap();
        assert_eq!(a.lock_count(), 1);
        assert!(a.check());
        assert!(b.check());

        let busy = OperationLease::acquire_paths(&root, "host-c", &[PathBuf::from("src/a.py")])
            .unwrap_err();
        assert!(matches!(busy, LeaseError::Busy { .. }), "{busy:?}");

        // 部分冲突不能遗留已经拿到的 c.py 锁。
        let busy = OperationLease::acquire_paths(
            &root,
            "host-c",
            &[PathBuf::from("src/c.py"), PathBuf::from("src/a.py")],
        )
        .unwrap_err();
        assert!(matches!(busy, LeaseError::Busy { .. }), "{busy:?}");
        let c =
            OperationLease::acquire_paths(&root, "host-c", &[PathBuf::from("src/c.py")]).unwrap();
        assert!(c.check());

        drop(a);
        drop(b);
        drop(c);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claiming_tools_record_lease_locks_in_the_journal() {
        let root = root("journal-locks");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "x").unwrap();
        let lease =
            OperationLease::acquire_paths(&root, "host-a", &[PathBuf::from("src/a.py")]).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src/a.py")]).unwrap();
        let journal = MutationJournal::open(&root).unwrap();
        let mut tools = ClaimingTools::new(EchoTools, claim)
            .with_lease(lease)
            .with_journal(journal);

        let request = ToolRequest::write("w1", "write_file", "src/a.py", "y");
        assert!(tools.execute(&request).unwrap().passed);
        let text = std::fs::read_to_string(root.join(JOURNAL_PATH)).unwrap();
        let first: JournalRecord =
            serde_json::from_str(text.lines().next().expect("begin line")).unwrap();
        assert!(!first.lease_locks.is_empty(), "{first:?}");
        assert!(
            first
                .lease_locks
                .iter()
                .all(|lock| lock.replace('\\', "/").contains(".agent/leases/")),
            "{first:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_lock_files_conflicts_with_a_live_path_lease() {
        let root = root("lock-files");
        let live =
            OperationLease::acquire_paths(&root, "host-a", &[PathBuf::from("src/a.py")]).unwrap();
        let locks: Vec<PathBuf> = live
            .lock_paths()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        let busy =
            OperationLease::acquire_lock_files(&root, "journal-rollback", &locks).unwrap_err();
        assert!(matches!(busy, LeaseError::Busy { .. }), "{busy:?}");
        drop(live);
        let recovered =
            OperationLease::acquire_lock_files(&root, "journal-rollback", &locks).unwrap();
        assert!(recovered.check());
        drop(recovered);
        let _ = std::fs::remove_dir_all(root);
    }
}
