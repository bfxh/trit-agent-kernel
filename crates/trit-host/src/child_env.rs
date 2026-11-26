//! 子进程环境白名单：子进程默认 `env_clear`，只透传运行必需、不含凭据的宿主变量。
//!
//! 宿主进程常带着 `OPENAI_API_KEY` 等 provider 凭据启动；`bash`、子 agent、MCP
//! supervisor 与 git 插件若直接继承完整环境，任何能执行命令的工具都能把 key
//! 读走。需要额外变量的调用方必须显式传入（如 `ProcessSpec::env`）。

use std::process::Command;

/// 允许透传的宿主环境变量。只放路径、临时目录、locale 与 Windows 进程启动所需项。
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "SHELL",
    "TERM",
    "USER",
    "LOGNAME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "COMSPEC",
    "PATHEXT",
    "OS",
    "LOCALAPPDATA",
    "APPDATA",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "USERNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// 清空继承环境，再按白名单透传。
pub fn sanitize(command: &mut Command) {
    command.env_clear();
    for key in BASE_ENV {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

/// 清空继承环境，透传白名单，并叠加调用方显式给定的变量。
pub fn sanitize_with(command: &mut Command, vars: impl IntoIterator<Item = (String, String)>) {
    sanitize(command);
    for (key, value) in vars {
        command.env(key, value);
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    fn probe(var: &str) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd");
            command.args(["/C", &format!("echo %{var}%")]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("printf %s \"${var}\"")]);
            command
        }
    }

    #[test]
    fn unlisted_secret_is_not_inherited() {
        const KEY: &str = "TRIT_CHILD_ENV_PROBE_SECRET";
        // SAFETY: 测试内设置唯一变量名，并在断言后恢复；不与其他测试共享该键。
        unsafe { std::env::set_var(KEY, "probe-secret-value") };
        let mut command = probe(KEY);
        super::sanitize(&mut command);
        let output = command.output().expect("probe child");
        let text = String::from_utf8_lossy(&output.stdout);
        // SAFETY: 与上面的 set_var 成对，键名唯一。
        unsafe { std::env::remove_var(KEY) };
        assert!(
            !text.contains("probe-secret-value"),
            "secret leaked to child: {text}"
        );
    }

    #[test]
    fn path_survives_sanitize() {
        let mut command = probe("PATH");
        super::sanitize(&mut command);
        let output = command.output().expect("probe child");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.trim().is_empty(), "PATH should survive: {text}");
    }
}
