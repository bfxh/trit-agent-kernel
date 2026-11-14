//! 无头模式的本地工具：read_file / write_file / bash。
//!
//! 文件操作通过 `cap-std` 的目录能力完成：路径相对 root 解析，最终组件 no-follow，
//! 打开后在校验过的句柄上计数硬链接；文件创建/覆盖不再依赖“先检查路径再写路径”。
//! shell 默认关闭；开启后优先走宿主提供的 OS 沙箱，否则必须显式允许无沙箱。

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl SandboxCommand {
    /// 外部沙箱的最终 argv：program + args + 平台 shell 调用。
    ///
    /// 宿主负责提供真正做写根约束的程序（bwrap / sandbox-exec / 自定义 wrapper）；
    /// 内核不假装 `Command` 本身能限制写入。
    pub fn argv(&self, command: &str) -> (PathBuf, Vec<String>) {
        let mut args = self.args.clone();
        append_shell(&mut args, command);
        (self.program.clone(), args)
    }
}

fn append_shell(args: &mut Vec<String>, command: &str) {
    #[cfg(windows)]
    {
        args.push("cmd".into());
        args.push("/C".into());
    }
    #[cfg(not(windows))]
    {
        args.push("sh".into());
        args.push("-c".into());
    }
    args.push(command.to_owned());
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub allow_write: bool,
    pub allow_shell: bool,
    /// 没有外部沙箱时，是否允许无约束 shell。默认 false。
    pub allow_unsandboxed_shell: bool,
    pub sandbox: Option<SandboxCommand>,
    /// 拒绝硬链接文件；默认 true。
    pub forbid_hardlinks: bool,
    pub forbid: Vec<PathBuf>,
    pub max_output_bytes: usize,
}

impl WorkspaceConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            allow_write: false,
            allow_shell: false,
            allow_unsandboxed_shell: false,
            sandbox: None,
            forbid_hardlinks: true,
            forbid: Vec::new(),
            max_output_bytes: 32 * 1024,
        }
    }
}

pub struct LocalTools {
    root: PathBuf,
    dir: Dir,
    allow_write: bool,
    allow_shell: bool,
    allow_unsandboxed_shell: bool,
    sandbox: Option<SandboxCommand>,
    forbid_hardlinks: bool,
    forbid: Vec<PathBuf>,
    max_output_bytes: usize,
}

impl LocalTools {
    pub fn new(config: WorkspaceConfig) -> Result<Self, AgentError> {
        let root = std::fs::canonicalize(&config.root)
            .map_err(|error| AgentError::Io(format!("workspace root: {error}")))?;
        if !root.is_dir() {
            return Err(AgentError::Tool("workspace root is not a directory".into()));
        }
        let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|error| AgentError::Io(format!("open workspace: {error}")))?;
        let forbid = config
            .forbid
            .into_iter()
            .map(|path| {
                let path = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                normalize_absolute(&path)
            })
            .collect();
        Ok(Self {
            root,
            dir,
            allow_write: config.allow_write,
            allow_shell: config.allow_shell,
            allow_unsandboxed_shell: config.allow_unsandboxed_shell,
            sandbox: config.sandbox,
            forbid_hardlinks: config.forbid_hardlinks,
            forbid,
            max_output_bytes: config.max_output_bytes,
        })
    }

    fn resolve_relative(&self, raw: &str) -> Result<PathBuf, AgentError> {
        if raw.trim().is_empty() {
            return Err(AgentError::Tool("path is blank".into()));
        }
        let relative = normalize_relative(Path::new(raw))
            .ok_or_else(|| AgentError::Tool(format!("path escapes workspace: {raw}")))?;
        let joined = self.root.join(&relative);
        if self.forbid.iter().any(|path| joined.starts_with(path)) {
            return Err(AgentError::Tool(format!("path is forbidden: {raw}")));
        }
        Ok(relative)
    }

    fn reject_symlink(&self, relative: &Path, raw: &str) -> Result<(), AgentError> {
        let is_symlink = self
            .dir
            .symlink_metadata(relative)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            return Err(AgentError::Tool(format!("refusing symlink: {raw}")));
        }
        Ok(())
    }

    fn first_path<'a>(&self, request: &'a ToolRequest) -> Result<&'a str, AgentError> {
        request
            .paths
            .first()
            .map(String::as_str)
            .ok_or_else(|| AgentError::Tool("path is required".into()))
    }

    fn read_file(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        let raw = self.first_path(request)?;
        let relative = self.resolve_relative(raw)?;
        self.reject_symlink(&relative, raw)?;

        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = self
            .dir
            .open_with(&relative, &options)
            .map_err(|error| AgentError::Io(format!("read {raw}: {error}")))?;
        let metadata = file
            .metadata()
            .map_err(|error| AgentError::Io(format!("stat {raw}: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(AgentError::Tool(format!("refusing symlink: {raw}")));
        }
        if metadata.is_file() && self.forbid_hardlinks && link_count(&file)? > 1 {
            return Err(AgentError::Tool(format!("refusing hardlinked file: {raw}")));
        }
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|error| AgentError::Io(format!("read {raw}: {error}")))?;
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: truncate(content, self.max_output_bytes),
        })
    }

    fn write_file(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !self.allow_write {
            return Err(AgentError::Tool("write_file is not allowed".into()));
        }
        let raw = self.first_path(request)?;
        let relative = self.resolve_relative(raw)?;
        self.reject_symlink(&relative, raw)?;
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
        {
            self.dir
                .create_dir_all(parent)
                .map_err(|error| AgentError::Io(format!("mkdir {raw}: {error}")))?;
        }

        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .truncate(false)
            .follow(FollowSymlinks::No);
        let mut file = self
            .dir
            .open_with(&relative, &options)
            .map_err(|error| AgentError::Io(format!("write {raw}: {error}")))?;
        let metadata = file
            .metadata()
            .map_err(|error| AgentError::Io(format!("stat {raw}: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(AgentError::Tool(format!("refusing symlink: {raw}")));
        }
        if !metadata.is_file() {
            return Err(AgentError::Tool(format!("not a regular file: {raw}")));
        }
        if self.forbid_hardlinks && link_count(&file)? > 1 {
            return Err(AgentError::Tool(format!("refusing hardlinked file: {raw}")));
        }
        let content = request.content.clone().unwrap_or_default();
        file.set_len(0)
            .map_err(|error| AgentError::Io(format!("truncate {raw}: {error}")))?;
        file.write_all(content.as_bytes())
            .map_err(|error| AgentError::Io(format!("write {raw}: {error}")))?;
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: vec![raw.to_owned()],
            in_doubt: false,
            output: format!("wrote {} bytes to {raw}", content.len()),
        })
    }

    fn bash(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !self.allow_shell {
            return Err(AgentError::Tool("bash is not allowed".into()));
        }
        let command = request
            .command
            .as_deref()
            .ok_or_else(|| AgentError::Tool("command is required".into()))?;
        let mut process = match &self.sandbox {
            Some(sandbox) => {
                let (program, args) = sandbox.argv(command);
                let mut process = Command::new(program);
                process.args(args);
                process
            }
            None if self.allow_unsandboxed_shell => shell_command(command),
            None => {
                return Err(AgentError::Tool(
                    "bash requires an OS sandbox (configure sandbox or allow_unsandboxed_shell)"
                        .into(),
                ));
            }
        };
        let output = process
            .current_dir(&self.root)
            .output()
            .map_err(|error| AgentError::Io(format!("bash: {error}")))?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: Some(command.to_owned()),
            passed: output.status.success(),
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: truncate(text, self.max_output_bytes),
        })
    }
}

impl ToolExecutor for LocalTools {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        match request.tool.as_str() {
            "read_file" => self.read_file(request),
            "write_file" => self.write_file(request),
            "bash" => self.bash(request),
            other => Err(AgentError::Tool(format!("unknown tool: {other}"))),
        }
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd");
        process.args(["/C", command]);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    }
}

#[cfg(unix)]
fn link_count(file: &cap_std::fs::File) -> Result<u64, AgentError> {
    use cap_fs_ext::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| AgentError::Io(format!("stat handle: {error}")))?;
    Ok(metadata.nlink())
}

#[cfg(windows)]
fn link_count(file: &cap_std::fs::File) -> Result<u64, AgentError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return Err(AgentError::Io(format!(
            "link count: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(u64::from(info.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
fn link_count(_file: &cap_std::fs::File) -> Result<u64, AgentError> {
    Ok(1)
}

fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("...(truncated)");
    text
}
