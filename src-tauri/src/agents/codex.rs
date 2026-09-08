//! Codex (OpenAI):`~/.codex/config.toml` 内嵌 `[[hooks.*]]` 段(git-ai 1.4.8+ 主路径)。
//! 显式设置 `codex_hooks_format = "hooks_json"` 时使用 `~/.codex/hooks.json`。
//!
//! # 权威 schema 来源
//! 上游 `git-ai/src/mdm/agents/codex.rs:164-208, 705-720` 与
//! `git-ai/src/config.rs:85-110`：
//! - `[features].hooks = true`(legacy: `[features].codex_hooks = true`)
//! - `[[hooks.<Event>]]` 三段:`PreToolUse / PostToolUse / Stop`
//!   - matcher 缺省或 `"*"`(catch-all)
//!   - 内含 `hooks = [{ type = "command", command = "<bin> checkpoint codex --hook-input stdin" }]`
//! - `[hooks.state."<config_path>:<event_snake>:<group>:<handler>"]` 用 SHA-256 trust 绕开 TUI 审批
//!
//! 命令字面 = `<git-ai bin> checkpoint codex --hook-input stdin`,反伪造校验对齐 Claude probe:
//! 命令首 token 必须是 git-ai 可执行,且字符串不含 shell 短路 / 注释符。

use async_trait::async_trait;
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

use crate::paths::{git_ai_config_json, home_dir};

use super::{AgentHookStatus, AgentKind, AgentProbe, HookType};

const CODEX_HOOK_EVENTS: [&str; 3] = ["PreToolUse", "PostToolUse", "Stop"];

/// 按 git-ai 选择的配置格式检查 Codex hooks。
pub struct CodexProbe;

#[async_trait]
impl AgentProbe for CodexProbe {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }
    fn config_path(&self) -> PathBuf {
        home_dir().join(".codex").join("config.toml")
    }
    /// 读取上游选择的格式并诊断实际 Codex hook 配置。
    async fn probe(&self) -> AgentHookStatus {
        // 1. 使用实际配置路径读取所选格式与 hooks。
        probe_paths(
            home_dir().join(".codex").join("config.toml"),
            home_dir().join(".codex").join("hooks.json"),
            &git_ai_config_json(),
        )
    }
}

/// 在指定路径检查配置，避免诊断与测试依赖进程级 HOME 修改。
fn probe_paths(toml_path: PathBuf, json_path: PathBuf, git_config_path: &Path) -> AgentHookStatus {
    // 1. 先读取上游选择的格式，损坏配置不能被当作默认格式。
    let uses_json = match uses_hooks_json(git_config_path) {
        Ok(value) => value,
        Err(message) => return config_error(git_config_path.to_path_buf(), message),
    };

    // 2. 两种格式都依赖 config.toml 中的 hooks 功能开关。
    if toml_path.exists() {
        let parsed = std::fs::read_to_string(&toml_path)
            .map_err(|e| format!("读 config.toml 失败: {e}"))
            .and_then(|raw| {
                toml::from_str::<TomlValue>(&raw).map_err(|e| format!("config.toml 解析失败: {e}"))
            });
        return match parsed {
            Ok(value) if uses_json => {
                let mut status = probe_json(json_path);
                if !is_hooks_feature_enabled(&value) {
                    status.configured = false;
                    status.hook_type = None;
                    status.issues.push("[features].hooks = true 未启用(或 legacy [features].codex_hooks = true 缺失)".into());
                }
                status
            }
            Ok(value) => probe_toml(value, toml_path, &json_path),
            Err(message) => config_error(toml_path, message),
        };
    }

    // 3. 显式 JSON 模式缺少功能开关时报告未配置；保留旧版配置的诊断。
    if uses_json {
        return config_error(
            toml_path,
            "缺少 config.toml，无法确认 Codex hooks 功能已启用".into(),
        );
    }
    if json_path.exists() {
        return probe_legacy_json(json_path);
    }
    missing(toml_path)
}

/// 对齐上游 CodexHooksFormat 的两个格式及其连字符别名；无配置时默认 TOML。
fn uses_hooks_json(path: &Path) -> Result<bool, String> {
    // 1. 首次安装允许配置文件不存在；其它读取和解析错误保留原因。
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("读 git-ai 配置失败: {error}")),
    };
    let config: JsonValue =
        serde_json::from_str(&raw).map_err(|error| format!("git-ai 配置解析失败: {error}"))?;
    let object = config
        .as_object()
        .ok_or_else(|| "git-ai 配置必须是 JSON 对象".to_string())?;

    // 2. 按上游字段选择格式，未知值明确提示配置错误。
    match object.get("codex_hooks_format") {
        None | Some(JsonValue::Null) => Ok(false),
        Some(JsonValue::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "config_toml" | "config-toml" => Ok(false),
            "hooks_json" | "hooks-json" => Ok(true),
            _ => Err(format!("未知 codex_hooks_format: {value}")),
        },
        Some(_) => Err("codex_hooks_format 必须是字符串".into()),
    }
}

fn missing(toml_path: PathBuf) -> AgentHookStatus {
    AgentHookStatus {
        agent: AgentKind::Codex,
        detected: false,
        configured: false,
        config_path: Some(toml_path.display().to_string()),
        hook_type: None,
        raw_excerpt: None,
        issues: vec![
            "未检测到 ~/.codex/config.toml 或 ~/.codex/hooks.json(Codex 未配置 hooks)".into(),
        ],
    }
}

/// 将配置读取错误映射为未配置状态，保留失败路径与原因。
fn config_error(path: PathBuf, msg: String) -> AgentHookStatus {
    // 1. 配置不可用时不能推断 hooks 已生效。
    AgentHookStatus {
        agent: AgentKind::Codex,
        detected: true,
        configured: false,
        config_path: Some(path.display().to_string()),
        hook_type: None,
        raw_excerpt: None,
        issues: vec![msg],
    }
}

/// 解析 config.toml,按 git-ai 1.4.8 `config_has_inline_hooks` + `config_hooks_feature_enabled` 等价规则判断。
fn probe_toml(
    toml_v: TomlValue,
    toml_path: PathBuf,
    legacy_json_path: &std::path::Path,
) -> AgentHookStatus {
    let mut issues = Vec::new();
    let mut excerpt: Option<String> = None;

    let feature_enabled = is_hooks_feature_enabled(&toml_v);
    let mut configured_events: u8 = 0;

    for which in CODEX_HOOK_EVENTS {
        if event_has_git_ai_hook(&toml_v, which, &mut excerpt) {
            configured_events += 1;
        } else {
            issues.push(format!(
                "[[hooks.{which}]] 中未找到 catch-all matcher + git-ai checkpoint codex 命令"
            ));
        }
    }

    if !feature_enabled {
        issues.push(
            "[features].hooks = true 未启用(或 legacy [features].codex_hooks = true 缺失)".into(),
        );
    }

    if legacy_json_path.exists() {
        issues.push(
            "检测到残留的 ~/.codex/hooks.json(legacy 格式),重跑 git-ai install-hooks 会清理".into(),
        );
    }

    let configured = configured_events == 3 && feature_enabled;
    if !configured && legacy_json_path.exists() {
        let legacy_status = probe_legacy_json(legacy_json_path.to_path_buf());
        if legacy_status.configured {
            issues.push(
                "检测到 ~/.codex/hooks.json 里仍有 legacy git-ai Codex hooks,但 ~/.codex/config.toml 缺少新版 inline hooks;通常是 git-ai 版本过旧或仍在写 legacy 格式,请升级 git-ai 到 1.4.8+ 后重新修复".into(),
            );
            if excerpt.is_none() {
                excerpt = legacy_status.raw_excerpt;
            }
        }
    }

    AgentHookStatus {
        agent: AgentKind::Codex,
        detected: true,
        configured,
        config_path: Some(toml_path.display().to_string()),
        hook_type: if configured {
            Some(HookType::Command)
        } else {
            None
        },
        raw_excerpt: excerpt,
        issues,
    }
}

fn event_has_git_ai_hook(toml_v: &TomlValue, event: &str, excerpt: &mut Option<String>) -> bool {
    let Some(blocks) = toml_v
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|v| v.as_array())
    else {
        return false;
    };
    for block in blocks {
        // matcher 缺省 / "*" 都算 catch-all (上游 codex.rs:185-190)
        let matcher_ok = block.get("matcher").is_none()
            || block.get("matcher").and_then(|v| v.as_str()) == Some("*");
        if !matcher_ok {
            continue;
        }
        let Some(hooks_arr) = block.get("hooks").and_then(|v| v.as_array()) else {
            continue;
        };
        for h in hooks_arr {
            let cmd = h.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if is_git_ai_codex_hook(cmd) {
                if excerpt.is_none() {
                    *excerpt = Some(format!("[[hooks.{event}]] command: {cmd}"));
                }
                return true;
            }
        }
    }
    false
}

fn is_hooks_feature_enabled(toml_v: &TomlValue) -> bool {
    let features = toml_v.get("features");
    let new_flag = features
        .and_then(|v| v.get("hooks"))
        .and_then(|v| v.as_bool())
        == Some(true);
    let legacy_flag = features
        .and_then(|v| v.get("codex_hooks"))
        .and_then(|v| v.as_bool())
        == Some(true);
    new_flag || legacy_flag
}

/// 为默认 TOML 模式下发现的旧版 JSON hooks 补充迁移提示。
fn probe_legacy_json(json_path: PathBuf) -> AgentHookStatus {
    // 1. 保留实际 hooks 检测结果，仅补充格式迁移信息。
    let mut status = probe_json(json_path);
    status.issues.insert(
        0,
        "使用 legacy ~/.codex/hooks.json 格式，当前选择 config_toml，可重跑 install-hooks 迁移"
            .into(),
    );
    status
}

/// 检查 JSON hooks 的三个必需事件，供显式 JSON 模式与旧版诊断共用。
fn probe_json(json_path: PathBuf) -> AgentHookStatus {
    // 1. 读取并解析目标 JSON 文件。
    let raw = match std::fs::read_to_string(&json_path) {
        Ok(s) => s,
        Err(e) => {
            return AgentHookStatus {
                agent: AgentKind::Codex,
                detected: true,
                configured: false,
                config_path: Some(json_path.display().to_string()),
                hook_type: None,
                raw_excerpt: None,
                issues: vec![format!("读 hooks.json 失败: {e}")],
            };
        }
    };
    let json: JsonValue = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return AgentHookStatus {
                agent: AgentKind::Codex,
                detected: true,
                configured: false,
                config_path: Some(json_path.display().to_string()),
                hook_type: None,
                raw_excerpt: None,
                issues: vec![format!("hooks.json 解析失败: {e}")],
            };
        }
    };

    // 2. 逐个检查上游要求的事件与命令。
    let mut issues = Vec::new();
    let mut excerpt: Option<String> = None;
    let hooks = json.get("hooks");
    let mut configured_events: u8 = 0;

    for which in CODEX_HOOK_EVENTS {
        let arr = hooks.and_then(|h| h.get(which)).and_then(|v| v.as_array());
        let Some(arr) = arr else {
            issues.push(format!("hooks.{which} 缺失"));
            continue;
        };
        let mut event_configured = false;
        for matcher_block in arr {
            let Some(inner) = matcher_block.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for hook in inner {
                let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if is_git_ai_codex_hook(command) {
                    event_configured = true;
                    if excerpt.is_none() {
                        excerpt = Some(format!("hooks.json command: {command}"));
                    }
                    break;
                }
            }
            if event_configured {
                break;
            }
        }
        if event_configured {
            configured_events += 1;
        }
    }

    let configured = configured_events == 3;
    AgentHookStatus {
        agent: AgentKind::Codex,
        detected: true,
        configured,
        config_path: Some(json_path.display().to_string()),
        hook_type: if configured {
            Some(HookType::Command)
        } else {
            None
        },
        raw_excerpt: excerpt,
        issues,
    }
}

/// 反伪造:首 token 是 git-ai 可执行 + 含 `checkpoint codex` + 不含 shell 短路 / 注释符。
fn is_git_ai_codex_hook(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains('#')
    {
        return false;
    }
    if !trimmed.contains("checkpoint codex") {
        return false;
    }
    if let Some(first) = trimmed.split_whitespace().next() {
        let lower = first.to_ascii_lowercase();
        if lower.ends_with("git-ai") || lower.ends_with("git-ai.exe") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_toml_path() -> PathBuf {
        PathBuf::from("config.toml")
    }
    fn fake_json_path() -> PathBuf {
        // tests 用不存在的路径,触发 legacy_json_path.exists() == false 分支
        PathBuf::from("/nonexistent/.codex/hooks.json")
    }

    #[test]
    fn official_codex_hook_in_config_toml_is_configured() {
        let raw = r#"
[features]
hooks = true

[[hooks.PreToolUse]]
hooks = [{ type = "command", command = "/home/u/.git-ai/bin/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "/home/u/.git-ai/bin/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.Stop]]
hooks = [{ type = "command", command = "/home/u/.git-ai/bin/git-ai checkpoint codex --hook-input stdin" }]
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(s.configured);
        assert_eq!(s.hook_type, Some(HookType::Command));
    }

    #[test]
    fn legacy_codex_hooks_feature_flag_also_accepted() {
        let raw = r#"
[features]
codex_hooks = true

[[hooks.PreToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.Stop]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(s.configured);
    }

    #[test]
    fn missing_features_hooks_flag_unconfigured() {
        let raw = r#"
[[hooks.PreToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.Stop]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(!s.configured);
        assert!(s.issues.iter().any(|i| i.contains("[features].hooks")));
    }

    #[test]
    fn missing_one_event_unconfigured() {
        let raw = r#"
[features]
hooks = true

[[hooks.PreToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(!s.configured);
        assert!(s.issues.iter().any(|i| i.contains("Stop")));
    }

    #[test]
    fn empty_hooks_table_reports_unconfigured() {
        let raw = r#"
[features]
hooks = true
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(!s.configured);
    }

    #[test]
    fn legacy_hooks_json_with_existing_config_reports_version_hint() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("hooks.json");
        std::fs::write(
            &json_path,
            r#"
{
  "hooks": {
    "PreToolUse": [{ "hooks": [{ "type": "command", "command": "/h/g/git-ai checkpoint codex --hook-input stdin" }] }],
    "PostToolUse": [{ "hooks": [{ "type": "command", "command": "/h/g/git-ai checkpoint codex --hook-input stdin" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "/h/g/git-ai checkpoint codex --hook-input stdin" }] }]
  }
}
"#,
        )
        .unwrap();
        let raw = r#"
[features]
hooks = true
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &json_path);
        assert!(!s.configured);
        assert!(
            s.issues
                .iter()
                .any(|issue| issue.contains("legacy git-ai Codex hooks")),
            "应提示 legacy hooks.json 与 git-ai 版本问题,实际 issues: {:?}",
            s.issues
        );
    }

    #[test]
    fn non_catchall_matcher_is_rejected() {
        // matcher = "Edit" 不是 catch-all,该事件不配
        let raw = r#"
[features]
hooks = true

[[hooks.PreToolUse]]
matcher = "Edit"
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]

[[hooks.Stop]]
hooks = [{ type = "command", command = "/h/g/git-ai checkpoint codex --hook-input stdin" }]
"#;
        let v: TomlValue = toml::from_str(raw).unwrap();
        let s = probe_toml(v, fake_toml_path(), &fake_json_path());
        assert!(!s.configured);
    }

    #[test]
    fn shell_short_circuit_is_rejected() {
        assert!(!is_git_ai_codex_hook(
            "echo skip; git-ai checkpoint codex --hook-input stdin"
        ));
        assert!(!is_git_ai_codex_hook(
            "true && git-ai checkpoint codex --hook-input stdin"
        ));
    }

    #[test]
    fn wrong_subcommand_rejected() {
        assert!(!is_git_ai_codex_hook(
            "/u/.git-ai/bin/git-ai checkpoint claude --hook-input stdin"
        ));
    }

    #[test]
    fn first_token_must_be_git_ai_binary() {
        assert!(is_git_ai_codex_hook(
            "/home/u/.git-ai/bin/git-ai checkpoint codex --hook-input stdin"
        ));
        let win = r"C:\Users\u\.git-ai\bin\git-ai.exe checkpoint codex --hook-input stdin";
        assert!(is_git_ai_codex_hook(win));
        assert!(!is_git_ai_codex_hook(
            "echo 'checkpoint codex' && touch /tmp/x"
        ));
    }
    /// 创建上游显式 JSON 模式安装生成的配置，不修改真实用户目录。
    fn write_json_mode_fixture(dir: &Path) {
        // 1. 写入所选格式、功能开关和三个官方事件。
        std::fs::write(
            dir.join("git-ai.json"),
            r#"{"codex_hooks_format":"hooks_json"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("config.toml"), "[features]\nhooks = true\n").unwrap();
        let hook = serde_json::json!([{"hooks": [{"type": "command", "command": "git-ai checkpoint codex --hook-input stdin"}]}]);
        std::fs::write(
            dir.join("hooks.json"),
            serde_json::to_string(&serde_json::json!({"hooks": {
                "PreToolUse": hook, "PostToolUse": hook, "Stop": hook
            }}))
            .unwrap(),
        )
        .unwrap();
    }

    /// 显式 JSON 模式不得误报缺少 inline hooks 或要求迁移格式。
    #[test]
    fn selected_hooks_json_is_configured_without_legacy_warning() {
        // 1. 按真实入口读取隔离配置并检查完整诊断。
        let dir = tempfile::tempdir().unwrap();
        write_json_mode_fixture(dir.path());
        let status = probe_paths(
            dir.path().join("config.toml"),
            dir.path().join("hooks.json"),
            &dir.path().join("git-ai.json"),
        );
        assert!(status.configured, "{:?}", status.issues);
        assert!(status.issues.is_empty(), "{:?}", status.issues);
        assert_eq!(status.hook_type, Some(HookType::Command));
    }

    /// JSON hooks 存在仍必须启用 config.toml 中的 hooks 功能。
    #[test]
    fn selected_hooks_json_requires_features_flag() {
        // 1. 删除真实必要条件，确认不会假报已配置。
        let dir = tempfile::tempdir().unwrap();
        write_json_mode_fixture(dir.path());
        std::fs::write(
            dir.path().join("config.toml"),
            "[features]\nhooks = false\n",
        )
        .unwrap();
        let status = probe_paths(
            dir.path().join("config.toml"),
            dir.path().join("hooks.json"),
            &dir.path().join("git-ai.json"),
        );
        assert!(!status.configured);
        assert!(status
            .issues
            .iter()
            .any(|issue| issue.contains("[features].hooks")));
    }

    /// 损坏的上游格式配置与 Codex TOML 都不能被其它文件掩盖。
    #[test]
    fn malformed_format_config_or_toml_is_reported() {
        // 1. 分别破坏决定实际通道的两份配置。
        let dir = tempfile::tempdir().unwrap();
        for file in ["git-ai.json", "config.toml"] {
            write_json_mode_fixture(dir.path());
            std::fs::write(dir.path().join(file), "{").unwrap();
            let status = probe_paths(
                dir.path().join("config.toml"),
                dir.path().join("hooks.json"),
                &dir.path().join("git-ai.json"),
            );
            assert!(!status.configured);
            assert!(status.issues.iter().any(|issue| issue.contains("解析失败")));
        }
    }

    /// 所选格式支持上游既有别名；未设置时使用默认 TOML。
    #[test]
    fn format_selection_matches_upstream_names() {
        // 1. 覆盖缺省与上游接受的格式拼写。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git-ai.json");
        assert!(!uses_hooks_json(&path).unwrap());
        for (value, expected) in [
            ("hooks_json", true),
            (" HOOKS-JSON ", true),
            ("config_toml", false),
            ("config-toml", false),
        ] {
            std::fs::write(
                &path,
                serde_json::to_string(&serde_json::json!({"codex_hooks_format": value})).unwrap(),
            )
            .unwrap();
            assert_eq!(uses_hooks_json(&path).unwrap(), expected);
        }
    }
}
