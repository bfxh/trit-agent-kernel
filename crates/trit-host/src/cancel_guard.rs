//! Ctrl-C 取消：把控制台信号接到 fleet 的 `CancelToken`。
//!
//! 信号处理器只安装一次；后续 `execute_fleet` 调用只替换共享槽里的最新 token，
//! 因此同进程多次执行仍能响应 Ctrl-C。取消是协作式的：已运行任务不会被强杀。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use trit_orch::CancelToken;

static CANCEL: OnceLock<Mutex<Option<CancelToken>>> = OnceLock::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// 安装/替换当前取消令牌；信号处理器只注册一次。
pub fn install(cancel: CancelToken) -> Result<(), String> {
    let slot = CANCEL.get_or_init(|| Mutex::new(None));
    *slot.lock().map_err(|_| "cancel slot poisoned".to_owned())? = Some(cancel);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
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
    if let Some(slot) = CANCEL.get()
        && let Ok(guard) = slot.try_lock()
        && let Some(cancel) = guard.as_ref()
    {
        cancel.cancel();
    }
}

#[cfg(windows)]
unsafe extern "system" fn handle_windows(_ctrl_type: u32) -> i32 {
    if let Some(slot) = CANCEL.get()
        && let Ok(guard) = slot.try_lock()
        && let Some(cancel) = guard.as_ref()
    {
        cancel.cancel();
    }
    1
}
