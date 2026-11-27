//! 无头模式的本地工具：read_file / write_file / bash。
//!
//! 文件操作通过 `cap-std` 的目录能力完成：路径相对 root 解析，最终组件 no-follow，
//! 打开后在校验过的句柄上计数硬链接；文件创建/覆盖不再依赖“先检查路径再写路径”。
//! shell 默认关闭；开启后优先走宿主提供的 OS 沙箱，否则必须显式允许无沙箱。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;

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

    /// 内置 Linux 后端：用 `bwrap` 构造只读系统 + 可写 workspace 的沙箱。
    ///
    /// 仍要求系统安装了 `bubblewrap`；内核负责参数构造与 fail-closed。
    /// 非 Linux 平台直接返回错误，不回退到无沙箱。
    pub fn builtin(root: &std::path::Path) -> Result<Self, AgentError> {
        #[cfg(target_os = "linux")]
        {
            let root = std::fs::canonicalize(root)
                .map_err(|error| AgentError::Io(format!("sandbox root: {error}")))?;
            let root_text = root.to_string_lossy().to_string();
            Ok(Self {
                program: PathBuf::from("bwrap"),
                args: vec![
                    "--ro-bind".into(),
                    "/".into(),
                    "/".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--proc".into(),
                    "/proc".into(),
                    "--tmpfs".into(),
                    "/tmp".into(),
                    "--bind".into(),
                    root_text.clone(),
                    root_text.clone(),
                    "--chdir".into(),
                    root_text,
                    "--unshare-all".into(),
                    "--unshare-net".into(),
                    "--die-with-parent".into(),
                    "--".into(),
                ],
            })
        }
        #[cfg(target_os = "macos")]
        {
            let root = std::fs::canonicalize(root)
                .map_err(|error| AgentError::Io(format!("sandbox root: {error}")))?;
            let root_text = root.to_string_lossy();
            let profile = format!(
                "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n\
                 (allow file-write* (subpath \"{root_text}\") (subpath \"/tmp\") (subpath \"/private/tmp\"))\n"
            );
            Ok(Self {
                program: PathBuf::from("sandbox-exec"),
                args: vec!["-p".into(), profile],
            })
        }
        #[cfg(windows)]
        {
            let root = std::fs::canonicalize(root)
                .map_err(|error| AgentError::Io(format!("sandbox root: {error}")))?;
            let exe = std::env::current_exe()
                .map_err(|error| AgentError::Io(format!("current exe: {error}")))?;
            Ok(Self {
                program: exe,
                args: vec![
                    "appcontainer-run".into(),
                    "--workspace".into(),
                    root.to_string_lossy().to_string(),
                    "--".into(),
                ],
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = root;
            Err(AgentError::Tool(
                "builtin sandbox is only available on Linux/macOS; pass --sandbox <program>".into(),
            ))
        }
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
    /// 无沙箱 shell 使用持久 bash 会话；沙箱模式仍逐条执行。
    pub persistent_shell: bool,
    pub max_search_file_bytes: usize,
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
            persistent_shell: true,
            max_search_file_bytes: 1024 * 1024,
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
    persistent_shell: bool,
    max_search_file_bytes: usize,
    shell_session: Mutex<Option<ShellSession>>,
    browser: Mutex<Option<crate::browser::Browser>>,
}

fn persistent_shell_candidates() -> Vec<(PathBuf, Vec<&'static str>)> {
    let mut candidates = Vec::new();
    if let Ok(shell) = std::env::var("TRIT_SHELL") {
        candidates.push((PathBuf::from(shell), vec!["--noprofile", "--norc"]));
    }
    #[cfg(windows)]
    {
        for path in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            let path = PathBuf::from(path);
            if path.is_file() {
                candidates.push((path, vec!["--noprofile", "--norc"]));
            }
        }
    }
    #[cfg(not(windows))]
    {
        candidates.push((PathBuf::from("bash"), vec!["--noprofile", "--norc"]));
        candidates.push((PathBuf::from("sh"), Vec::new()));
    }
    candidates
}

struct ShellSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    counter: u64,
}

impl ShellSession {
    fn spawn(root: &Path) -> Result<Self, AgentError> {
        let mut last_error = None;
        for (program, args) in persistent_shell_candidates() {
            let mut command = Command::new(&program);
            crate::child_env::sanitize(&mut command);
            command
                .args(args)
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            match command.spawn() {
                Ok(mut child) => {
                    let Some(stdin) = child.stdin.take() else {
                        let _ = child.kill();
                        continue;
                    };
                    let Some(stdout) = child.stdout.take() else {
                        let _ = child.kill();
                        continue;
                    };
                    return Ok(Self {
                        child,
                        stdin,
                        stdout: BufReader::new(stdout),
                        counter: 0,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(AgentError::Tool(format!(
            "persistent shell unavailable: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no shell".into())
        )))
    }

    fn run(&mut self, command: &str, max: usize) -> Result<(Option<bool>, String), AgentError> {
        self.counter = self.counter.saturating_add(1);
        let marker = format!(
            "__TRIT_SHELL_DONE_{}_{}__",
            std::process::id(),
            self.counter
        );
        let wrapped = format!(
            "{{ {command}\n}} 2>&1; __trit_status=$?; printf '\\036{marker}%s\\n' \"$__trit_status\""
        );
        writeln!(self.stdin, "{wrapped}")
            .map_err(|error| AgentError::Io(format!("shell write: {error}")))?;
        self.stdin
            .flush()
            .map_err(|error| AgentError::Io(format!("shell flush: {error}")))?;
        let mut output = String::new();
        let mut truncated = false;
        let mut status = None;
        loop {
            let mut line = Vec::new();
            let read = self
                .stdout
                .read_until(b'\n', &mut line)
                .map_err(|error| AgentError::Io(format!("shell read: {error}")))?;
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            let (body, marker_part) = match text.split_once('\u{1e}') {
                Some((body, rest)) => (body, Some(rest)),
                None => (text.as_str(), None),
            };
            if !truncated && !body.is_empty() {
                if output.len() + body.len() <= max.saturating_mul(4) {
                    output.push_str(body);
                } else {
                    let remaining = max.saturating_mul(4).saturating_sub(output.len());
                    if remaining > 0 {
                        let mut end = remaining;
                        while end > 0 && !body.is_char_boundary(end) {
                            end -= 1;
                        }
                        output.push_str(&body[..end]);
                    }
                    truncated = true;
                }
            }
            if let Some(rest) = marker_part {
                let trimmed = rest.trim_end_matches(['\r', '\n']);
                if let Some(value) = trimmed.strip_prefix(&marker) {
                    status = value.trim().parse::<i32>().ok();
                }
                break;
            }
        }
        Ok((status.map(|status| status == 0), truncate(output, max)))
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cached_workspace(root: &Path) -> Result<(PathBuf, Dir), AgentError> {
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<std::collections::BTreeMap<PathBuf, (PathBuf, Dir)>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((canonical, dir)) = cache.get(root) {
        let dir = dir
            .try_clone()
            .map_err(|error| AgentError::Io(format!("open workspace: {error}")))?;
        return Ok((canonical.clone(), dir));
    }
    let canonical = std::fs::canonicalize(root)
        .map_err(|error| AgentError::Io(format!("workspace root: {error}")))?;
    if !canonical.is_dir() {
        return Err(AgentError::Tool("workspace root is not a directory".into()));
    }
    let dir = Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())
        .map_err(|error| AgentError::Io(format!("open workspace: {error}")))?;
    let cached = dir
        .try_clone()
        .map_err(|error| AgentError::Io(format!("open workspace: {error}")))?;
    cache.insert(root.to_path_buf(), (canonical.clone(), cached));
    Ok((canonical, dir))
}

impl LocalTools {
    pub fn new(config: WorkspaceConfig) -> Result<Self, AgentError> {
        let (root, dir) = cached_workspace(&config.root)?;
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
            persistent_shell: config.persistent_shell,
            max_search_file_bytes: config.max_search_file_bytes,
            shell_session: Mutex::new(None),
            browser: Mutex::new(None),
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

    fn resolve_search_base(&self, raw: Option<&str>) -> Result<PathBuf, AgentError> {
        let raw = raw.unwrap_or(".").trim();
        if raw.is_empty() || raw == "." {
            return Ok(PathBuf::new());
        }
        let relative = self.resolve_relative(raw)?;
        self.reject_symlink(&relative, raw)?;
        if self.is_forbidden_absolute(&self.root.join(&relative)) {
            return Err(AgentError::Tool(format!("path is forbidden: {raw}")));
        }
        Ok(relative)
    }

    fn is_forbidden_absolute(&self, path: &Path) -> bool {
        let absolute = normalize_absolute(path);
        self.forbid
            .iter()
            .any(|forbidden| absolute.starts_with(forbidden))
    }

    fn read_search_text(&self, relative: &Path) -> Option<String> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.dir.open_with(relative, &options).ok()?;
        let metadata = file.metadata().ok()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return None;
        }
        if self.forbid_hardlinks && link_count(&file).ok()? > 1 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(self.max_search_file_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > self.max_search_file_bytes
            || bytes.iter().take(1024).any(|byte| *byte == 0)
        {
            return None;
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn search_code(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        let arguments = request.arguments.as_ref();
        let query = arguments
            .and_then(|arguments| arguments.get("query"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| AgentError::Tool("search_code requires a non-empty query".into()))?;
        let regex_mode = arguments
            .and_then(|arguments| arguments.get("regex"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let case_sensitive = arguments
            .and_then(|arguments| arguments.get("case_sensitive"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let max_results = arguments
            .and_then(|arguments| arguments.get("max_results"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 1000) as usize;
        let path = arguments
            .and_then(|arguments| arguments.get("path"))
            .and_then(serde_json::Value::as_str);
        let include_glob = arguments
            .and_then(|arguments| arguments.get("glob"))
            .and_then(serde_json::Value::as_str)
            .map(Glob::new)
            .transpose()
            .map_err(|error| AgentError::Tool(format!("bad glob filter: {error}")))?
            .map(|glob| glob.compile_matcher());
        let pattern = if regex_mode {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let matcher = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| AgentError::Tool(format!("bad regex: {error}")))?;
        let base = self.resolve_search_base(path)?;
        let mut walker = WalkBuilder::new(self.root.join(&base));
        walker
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false);
        let mut results = Vec::new();
        for entry in walker.build().flatten() {
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if self.is_forbidden_absolute(path) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            if let Some(include_glob) = &include_glob
                && !glob_matches(include_glob, relative)
            {
                continue;
            }
            let Some(text) = self.read_search_text(relative) else {
                continue;
            };
            let display = relative.to_string_lossy().replace('\\', "/");
            for (index, line) in text.lines().enumerate() {
                if matcher.is_match(line) {
                    results.push(format!(
                        "{display}:{}:{}",
                        index + 1,
                        line.trim_end().chars().take(500).collect::<String>()
                    ));
                    if results.len() >= max_results {
                        break;
                    }
                }
            }
            if results.len() >= max_results {
                break;
            }
        }
        let output = if results.is_empty() {
            format!("no matches for {query}")
        } else {
            results.join("\n")
        };
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: truncate(output, self.max_output_bytes),
        })
    }

    fn glob(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        let arguments = request.arguments.as_ref();
        let pattern = arguments
            .and_then(|arguments| arguments.get("pattern"))
            .and_then(serde_json::Value::as_str)
            .filter(|pattern| !pattern.trim().is_empty())
            .ok_or_else(|| AgentError::Tool("glob requires a pattern".into()))?;
        let max_results = arguments
            .and_then(|arguments| arguments.get("max_results"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(200)
            .clamp(1, 2000) as usize;
        let path = arguments
            .and_then(|arguments| arguments.get("path"))
            .and_then(serde_json::Value::as_str);
        let matcher = Glob::new(pattern)
            .map_err(|error| AgentError::Tool(format!("bad glob: {error}")))?
            .compile_matcher();
        let base = self.resolve_search_base(path)?;
        let mut walker = WalkBuilder::new(self.root.join(&base));
        walker
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false);
        let mut paths = Vec::new();
        for entry in walker.build().flatten() {
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if self.is_forbidden_absolute(path) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            if glob_matches(&matcher, relative) {
                paths.push(relative.to_string_lossy().replace('\\', "/"));
                if paths.len() >= max_results {
                    break;
                }
            }
        }
        paths.sort();
        let output = if paths.is_empty() {
            format!("no files match {pattern}")
        } else {
            paths.join("\n")
        };
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: truncate(output, self.max_output_bytes),
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

    fn move_path(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !self.allow_write {
            return Err(AgentError::Tool("move_path is not allowed".into()));
        }
        if request.paths.len() != 2 {
            return Err(AgentError::Tool(
                "move_path requires exactly two paths (from, to)".into(),
            ));
        }
        let from_raw = &request.paths[0];
        let to_raw = &request.paths[1];
        let from = self.resolve_relative(from_raw)?;
        let to = self.resolve_relative(to_raw)?;
        self.reject_symlink(&from, from_raw)?;
        self.reject_symlink(&to, to_raw)?;
        let metadata = self
            .dir
            .symlink_metadata(&from)
            .map_err(|error| AgentError::Io(format!("stat {from_raw}: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(AgentError::Tool(format!("refusing symlink: {from_raw}")));
        }
        if !metadata.is_file() {
            return Err(AgentError::Tool(format!("not a regular file: {from_raw}")));
        }
        if let Some(parent) = to.parent()
            && !parent.as_os_str().is_empty()
        {
            self.dir
                .create_dir_all(parent)
                .map_err(|error| AgentError::Io(format!("mkdir {}: {error}", parent.display())))?;
        }
        match self.dir.rename(&from, &self.dir, &to) {
            Ok(()) => {}
            Err(error) if is_cross_device(&error) => {
                self.copy_then_remove(&from, &to).map_err(|error| {
                    AgentError::Io(format!("move {from_raw} -> {to_raw}: {error}"))
                })?;
            }
            Err(error) => {
                return Err(AgentError::Io(format!(
                    "move {from_raw} -> {to_raw}: {error}"
                )));
            }
        }
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: vec![from_raw.clone(), to_raw.clone()],
            in_doubt: false,
            output: format!("moved {from_raw} -> {to_raw}"),
        })
    }

    fn copy_then_remove(&self, from: &Path, to: &Path) -> Result<(), AgentError> {
        let source = self.root.join(from);
        let target = self.root.join(to);
        std::fs::copy(&source, &target)
            .map_err(|error| AgentError::Io(format!("copy {}: {error}", source.display())))?;
        self.dir
            .remove_file(from)
            .map_err(|error| AgentError::Io(format!("remove {}: {error}", source.display())))?;
        Ok(())
    }

    fn bash(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !self.allow_shell {
            return Err(AgentError::Tool("bash is not allowed".into()));
        }
        let command = request
            .command
            .as_deref()
            .ok_or_else(|| AgentError::Tool("command is required".into()))?;
        if self.persistent_shell && self.sandbox.is_none() && self.allow_unsandboxed_shell {
            let mut guard = self
                .shell_session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.is_none() {
                *guard = ShellSession::spawn(&self.root).ok();
            }
            let run = guard
                .as_mut()
                .map(|session| session.run(command, self.max_output_bytes))
                .transpose()?;
            if let Some((status, output)) = run {
                let in_doubt = status.is_none();
                if in_doubt {
                    *guard = None;
                }
                return Ok(ToolOutcome {
                    call_id: request.call_id.clone(),
                    tool: request.tool.clone(),
                    command: Some(command.to_owned()),
                    passed: status.unwrap_or(false),
                    assertions: 0,
                    written_paths: Vec::new(),
                    in_doubt,
                    output,
                });
            }
        }
        let mut process = match &self.sandbox {
            Some(sandbox) => {
                let (program, args) = sandbox.argv(command);
                let mut process = Command::new(program);
                process.args(args);
                crate::child_env::sanitize(&mut process);
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
            "move_path" => self.move_path(request),
            "search_code" => self.search_code(request),
            "glob" => self.glob(request),
            "bash" => self.bash(request),
            "browser" => self.browser(request),
            other => Err(AgentError::Tool(format!("unknown tool: {other}"))),
        }
    }

    /// 同一批全是只读工具时并行执行；含写工具时退回逐条串行。
    fn execute_batch(&mut self, requests: &[ToolRequest]) -> Vec<Result<ToolOutcome, AgentError>> {
        if requests.len() < 2
            || !requests
                .iter()
                .all(|request| is_readonly_tool(&request.tool))
        {
            return requests
                .iter()
                .map(|request| self.execute(request))
                .collect();
        }
        let tools: &LocalTools = self;
        std::thread::scope(|scope| {
            let handles: Vec<_> = requests
                .iter()
                .map(|request| scope.spawn(move || tools.execute_readonly(request)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(AgentError::Tool("readonly tool worker panicked".into()))
                    })
                })
                .collect()
        })
    }
}

impl LocalTools {
    /// 只读工具的无副作用执行入口；并行 batch 只允许走这里。
    fn execute_readonly(&self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        match request.tool.as_str() {
            "read_file" => self.read_file(request),
            "search_code" => self.search_code(request),
            "glob" => self.glob(request),
            other => Err(AgentError::Tool(format!("tool {other} is not read-only"))),
        }
    }

    /// 浏览器动作：open / eval / screenshot / close；按需启动 headless Chrome。
    fn browser(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        let args = request
            .arguments
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let action = args["action"].as_str().unwrap_or("open");
        let mut guard = self
            .browser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if action == "close" {
            *guard = None;
            return Ok(self.browser_outcome(request, "browser closed".into(), Vec::new()));
        }
        if guard.is_none() {
            *guard = Some(crate::browser::Browser::launch(
                &crate::browser::BrowserConfig::default(),
            )?);
        }
        let browser = guard.as_mut().expect("browser was just launched");
        let (output, written_paths) = match action {
            "open" => {
                let url = args["url"]
                    .as_str()
                    .ok_or_else(|| AgentError::Tool("browser open requires url".into()))?;
                browser.open(url)?;
                (format!("opened {url}"), Vec::new())
            }
            "eval" => {
                let expression = args["expression"]
                    .as_str()
                    .ok_or_else(|| AgentError::Tool("browser eval requires expression".into()))?;
                let value = browser.eval(expression)?;
                (
                    serde_json::to_string(&value).unwrap_or_default(),
                    Vec::new(),
                )
            }
            "screenshot" => {
                if !self.allow_write {
                    return Err(AgentError::Tool(
                        "browser screenshot requires allow-write".into(),
                    ));
                }
                let raw = args["path"].as_str().unwrap_or("screenshot.png");
                let relative = self.resolve_relative(raw)?;
                browser.screenshot(&self.root.join(&relative))?;
                let written = relative.display().to_string();
                (format!("screenshot {written}"), vec![written])
            }
            other => {
                return Err(AgentError::Tool(format!("unknown browser action: {other}")));
            }
        };
        drop(guard);
        Ok(self.browser_outcome(request, output, written_paths))
    }

    fn browser_outcome(
        &self,
        request: &ToolRequest,
        output: String,
        written_paths: Vec<String>,
    ) -> ToolOutcome {
        ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths,
            in_doubt: false,
            output: truncate(output, self.max_output_bytes),
        }
    }
}

/// 只读工具可以在同一批内并行执行（无写副作用）。
pub fn is_readonly_tool(tool: &str) -> bool {
    matches!(tool, "read_file" | "search_code" | "glob")
}

fn is_cross_device(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(windows)]
    {
        // ERROR_NOT_SAME_DEVICE
        error.raw_os_error() == Some(17)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd");
        process.args(["/C", command]);
        crate::child_env::sanitize(&mut process);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        crate::child_env::sanitize(&mut process);
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

fn glob_matches(matcher: &GlobMatcher, path: &Path) -> bool {
    matcher.is_match(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trit-tools-unit-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn copy_then_remove_moves_file() {
        let root = temp("copy");
        let mut config = WorkspaceConfig::new(&root);
        config.allow_write = true;
        let tools = LocalTools::new(config).unwrap();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        tools
            .copy_then_remove(Path::new("a.txt"), Path::new("b.txt"))
            .unwrap();
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "hello"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cross_device_detection_uses_platform_error() {
        #[cfg(windows)]
        assert!(is_cross_device(&std::io::Error::from_raw_os_error(17)));
        #[cfg(unix)]
        {
            assert!(is_cross_device(&std::io::Error::from_raw_os_error(
                libc::EXDEV
            )));
            assert!(!is_cross_device(&std::io::Error::from_raw_os_error(17)));
        }
    }

    #[test]
    fn readonly_batch_preserves_request_order() {
        let root = temp("readonly-batch");
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("b.txt"), "beta").unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut tools = LocalTools::new(config).unwrap();
        let request = |call_id: &str, path: &str| ToolRequest {
            call_id: call_id.into(),
            tool: "read_file".into(),
            read_only: true,
            command: None,
            paths: vec![path.into()],
            operation: None,
            content: None,
            span: None,
            arguments: None,
        };
        let outcomes =
            tools.execute_batch(&[request("call-1", "a.txt"), request("call-2", "b.txt")]);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].as_ref().unwrap().output.contains("alpha"));
        assert!(outcomes[1].as_ref().unwrap().output.contains("beta"));
        let _ = std::fs::remove_dir_all(root);
    }
}
