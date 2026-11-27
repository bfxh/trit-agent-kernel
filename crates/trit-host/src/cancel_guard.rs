//! Ctrl-C 取消：把控制台信号接到 fleet 的 `CancelToken`。
//!
//! 只做协作式取消：已运行的任务不会被强杀，fleet runtime 在下一个检查点停止启动
//! 新任务。安装失败（例如已安装过）由调用方决定是否忽略。

use std::sync::OnceLock;

use trit_orch::CancelToken;

static CANCEL: OnceLock<CancelToken> = OnceLock::new();

/// 安装一次 Ctrl-C 处理器；重复安装返回错误，但不改变已有处理器。
pub fn install(cancel: CancelToken) -> Result<(), String> {
    if CANCEL.set(cancel).is_err() {
        return Err("cancel handler already installed".to_owned());
    }
    #[cfg(unix)]
    {
        unsafe {
            libc::signal(libc::SIGINT, handle_unix as libc::sighandler_t);
        }
    }
    #[cfg(windows)]
    {
        unsafe {
            if windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handle_windows), 1)
                == 0
            {
                return Err("SetConsoleCtrlHandler failed".to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
extern "C" fn handle_unix(_signal: libc::c_int) {
    if let Some(cancel) = CANCEL.get() {
        cancel.cancel();
    }
}

#[cfg(windows)]
unsafe extern "system" fn handle_windows(_ctrl_type: u32) -> i32 {
    if let Some(cancel) = CANCEL.get() {
        cancel.cancel();
    }
    1
}
