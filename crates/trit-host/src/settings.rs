//! 设置文件：前端持久化的 JSON，内核只读取自己认识的键。
//!
//! 路径固定为 `<root>/.agent/settings.json`。缺文件时用内置默认值；
//! 文件存在但解析失败或 schema 不匹配时 fail closed，不静默回退。
//! 键与 `docs/FRONTEND-CONTRACT.md` 第 10 节一致。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AgentError;

pub const SETTINGS_SCHEMA: u32 = 1;
pub const SETTINGS_PATH: &str = ".agent/settings.json";
/// 价目表仍按美元维护，展示与预算默认按此汇率折算人民币。
pub const DEFAULT_USD_CNY: f64 = 7.2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub startup: StartupSettings,
    pub concurrency: ConcurrencySettings,
    pub budget: BudgetSettings,
    pub cost: CostSettings,
    pub browser: BrowserSettings,
    pub diagnostics: DiagnosticsSettings,
    /// 记录与信号开关；`verification_signals` 控制是否统计并注入校验摘要。
    pub recording: RecordingSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupSettings {
    /// 应用随系统登录启动；由前端/安装器实现，内核不写注册表。
    pub app_autostart: bool,
    /// 应用启动后自动恢复 agent 会话/服务。
    pub agent_autostart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConcurrencySettings {
    /// 总并发上限；0 表示不限。
    pub max: u32,
    /// 单次 fleet 的 worker 数。
    pub workers: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetSettings {
    /// token 预算；0 表示不限。
    pub tokens: u64,
    /// 人民币预算；0 表示不限。
    pub cny: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CostSettings {
    pub usd_cny: f64,
    /// 覆盖内置价目表的 JSON 文件；None 用内置表。
    pub model_prices: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserSettings {
    pub enabled: bool,
    pub chrome_path: Option<PathBuf>,
    pub headless: bool,
    pub screenshot_allow_write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticsSettings {
    pub background_tasks: bool,
    pub token_rate: bool,
}

/// 记录层开关：控制是否把校验摘要注入模型请求（训练信号）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// 默认开启：注入校验摘要，并在出现重复校验时提示模型。
    /// 关闭后只停止注入与提示；`TurnMetrics` 仍记录三个计数（排障与采样需要）。
    pub verification_signals: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA,
            startup: StartupSettings::default(),
            concurrency: ConcurrencySettings::default(),
            budget: BudgetSettings::default(),
            cost: CostSettings::default(),
            browser: BrowserSettings::default(),
            diagnostics: DiagnosticsSettings::default(),
            recording: RecordingSettings::default(),
        }
    }
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self {
            app_autostart: false,
            agent_autostart: false,
        }
    }
}

impl Default for ConcurrencySettings {
    fn default() -> Self {
        Self { max: 0, workers: 4 }
    }
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            tokens: 0,
            cny: 0.0,
        }
    }
}

impl Default for CostSettings {
    fn default() -> Self {
        Self {
            usd_cny: DEFAULT_USD_CNY,
            model_prices: None,
        }
    }
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            chrome_path: None,
            headless: true,
            screenshot_allow_write: false,
        }
    }
}

impl Default for DiagnosticsSettings {
    fn default() -> Self {
        Self {
            background_tasks: true,
            token_rate: true,
        }
    }
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            verification_signals: true,
        }
    }
}

/// `<root>/.agent/settings.json`。
pub fn path_for(root: &Path) -> PathBuf {
    root.join(SETTINGS_PATH)
}

/// 读取设置；缺文件返回默认值，损坏/版本不符 fail closed。
pub fn load(root: &Path) -> Result<Settings, AgentError> {
    let path = path_for(root);
    if !path.is_file() {
        return Ok(Settings::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        AgentError::SessionState(format!("settings {}: {error}", path.display()))
    })?;
    let settings: Settings = serde_json::from_str(&text).map_err(|error| {
        AgentError::SessionState(format!("settings {}: {error}", path.display()))
    })?;
    if settings.schema != SETTINGS_SCHEMA {
        return Err(AgentError::SessionState(format!(
            "settings {}: schema {} (expected {SETTINGS_SCHEMA})",
            path.display(),
            settings.schema
        )));
    }
    Ok(settings)
}

/// 写回设置；父目录不存在时创建。
pub fn save(root: &Path, settings: &Settings) -> Result<(), AgentError> {
    let path = path_for(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::Io(format!("settings dir: {error}")))?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|error| AgentError::SessionState(format!("settings encode: {error}")))?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|error| AgentError::Io(format!("settings {}: {error}", path.display())))
}

/// 按 `.` 路径读取设置值。
pub fn get(settings: &Settings, key: &str) -> Result<Value, String> {
    let value = serde_json::to_value(settings).map_err(|error| error.to_string())?;
    let mut current = &value;
    for part in key.split('.').filter(|part| !part.is_empty()) {
        current = current
            .get(part)
            .ok_or_else(|| format!("unknown setting: {key}"))?;
    }
    Ok(current.clone())
}

/// 按 `.` 路径写入设置值；值按 JSON 解析，解析失败时按字符串处理。
pub fn set(settings: &mut Settings, key: &str, raw: &str) -> Result<(), String> {
    let mut value = serde_json::to_value(&*settings).map_err(|error| error.to_string())?;
    let parsed: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
    let parts: Vec<&str> = key.split('.').filter(|part| !part.is_empty()).collect();
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| "empty setting key".to_owned())?;
    let mut current = &mut value;
    for part in parents {
        current = current
            .get_mut(*part)
            .ok_or_else(|| format!("unknown setting: {key}"))?;
    }
    let slot = current
        .get_mut(*last)
        .ok_or_else(|| format!("unknown setting: {key}"))?;
    *slot = parsed;
    *settings = serde_json::from_value(value).map_err(|error| format!("{key}: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_frontend_contract() {
        let settings = Settings::default();
        assert_eq!(settings.schema, SETTINGS_SCHEMA);
        assert!(!settings.startup.app_autostart);
        assert!(!settings.startup.agent_autostart);
        assert_eq!(settings.concurrency.max, 0);
        assert_eq!(settings.concurrency.workers, 4);
        assert_eq!(settings.cost.usd_cny, DEFAULT_USD_CNY);
        assert!(settings.browser.headless);
        assert!(!settings.browser.enabled);
        assert!(settings.diagnostics.background_tasks);
    }

    #[test]
    fn get_and_set_use_dot_paths() {
        let mut settings = Settings::default();
        assert_eq!(get(&settings, "concurrency.workers").unwrap(), 4);
        set(&mut settings, "concurrency.max", "8").unwrap();
        assert_eq!(settings.concurrency.max, 8);
        set(&mut settings, "browser.headless", "false").unwrap();
        assert!(!settings.browser.headless);
        set(&mut settings, "startup.agent_autostart", "true").unwrap();
        assert!(settings.startup.agent_autostart);
        assert!(set(&mut settings, "unknown.key", "1").is_err());
        assert!(set(&mut settings, "concurrency.max", "not-a-number").is_err());
    }

    #[test]
    fn save_and_load_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "trit-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = Settings::default();
        settings.concurrency.max = 2;
        settings.budget.cny = 100.0;
        save(&root, &settings).unwrap();
        let loaded = load(&root).unwrap();
        assert_eq!(loaded, settings);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_settings_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "trit-settings-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".agent")).unwrap();
        std::fs::write(root.join(".agent/settings.json"), "{not json").unwrap();
        assert!(load(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
