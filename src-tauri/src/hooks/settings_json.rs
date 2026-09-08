//! `~/.claude/settings.json` 的合并语义。
//!
//! - 保留 hooks 段之外的所有字段(permissions / theme / mcpServers 等)。
//! - 在 PreToolUse / PostToolUse 中:
//!   - 仅识别"git-ai owned"条目(含 `checkpoint claude`)
//!   - 已有 git-ai owned 条目时只替换内层 hook;无 → 在头部追加 matcher 分组
//!   - 其它 hook 条目(cc-switch / 用户写的)**不动**

use std::fs;

use serde_json::{json, Value};

use crate::error::{AppError, Result};
use crate::paths::claude_settings_json;

use super::backups;
use super::model::HooksMode;

#[derive(Debug, Clone)]
pub struct MergeReport {
    pub changed: bool,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

const MATCHER: &str = "Write|Edit|MultiEdit";

/// 把当前 settings.json 合并到目标模式。
/// `command_path` 仅在 mode=Official 时使用 — 通常是 git-ai 二进制的绝对路径。
pub fn merge_to_mode(mode: HooksMode, command_path: Option<&str>) -> Result<MergeReport> {
    let path = claude_settings_json();
    let raw = if path.exists() {
        fs::read_to_string(&path).map_err(AppError::Io)?
    } else {
        "{}".to_string()
    };
    let mut v: Value = serde_json::from_str(&raw)
        .map_err(|e| AppError::Other(format!("settings.json JSON 解析失败: {e}")))?;
    if !v.is_object() {
        return Err(AppError::Other(
            "settings.json 根不是对象,拒绝写入".to_string(),
        ));
    }

    let mut report = MergeReport {
        changed: false,
        added: vec![],
        updated: vec![],
        removed: vec![],
    };

    // 写入前先备份
    if path.exists() {
        let _ = backups::backup_claude_settings()?;
    }

    // 确保 hooks / PreToolUse / PostToolUse 存在
    let hooks = v
        .as_object_mut()
        .unwrap()
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        return Err(AppError::Other(
            "settings.json 的 hooks 字段不是对象".to_string(),
        ));
    }

    for stage in ["PreToolUse", "PostToolUse"] {
        let stage_arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(stage.to_string())
            .or_insert_with(|| json!([]));
        if !stage_arr.is_array() {
            return Err(AppError::Other(format!(
                "settings.json hooks.{stage} 不是数组"
            )));
        }
        let arr = stage_arr.as_array_mut().unwrap();
        match mode {
            HooksMode::Official => write_official(arr, stage, command_path, &mut report)?,
            HooksMode::None => remove_git_ai_owned(arr, stage, &mut report),
        }
    }

    // 原子写
    let tmp = with_extension(&path, "json.tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&v).map_err(AppError::Json)?,
    )
    .map_err(AppError::Io)?;
    fs::rename(&tmp, &path).map_err(AppError::Io)?;
    Ok(report)
}

fn write_official(
    arr: &mut Vec<Value>,
    stage: &str,
    command_path: Option<&str>,
    report: &mut MergeReport,
) -> Result<()> {
    let cmd_path = command_path
        .ok_or_else(|| AppError::Other("Official 模式需要提供 git-ai 命令路径".to_string()))?;
    let command = format!("{cmd_path} checkpoint claude --hook-input stdin");
    let new_block = json!({
        "matcher": MATCHER,
        "hooks": [{ "type": "command", "command": command }]
    });
    upsert_git_ai_owned(arr, stage, new_block, report);
    Ok(())
}

/// 更新首个已含 git-ai 的分组中的自有 hook,保留分组属性与同组用户 hook。
fn upsert_git_ai_owned(
    arr: &mut Vec<Value>,
    stage: &str,
    new_block: Value,
    report: &mut MergeReport,
) {
    // 1. 定位已包含 git-ai hook 的分组。
    let mut idx_found: Option<usize> = None;
    for (i, block) in arr.iter().enumerate() {
        let inner = block.get("hooks").and_then(|v| v.as_array());
        let Some(inner) = inner else { continue };
        if inner.iter().any(is_git_ai_owned) {
            idx_found = Some(i);
            break;
        }
    }
    // 2. 只更新自有 hook;首次安装时追加完整分组。
    match idx_found {
        Some(i) => {
            let replacement = &new_block["hooks"][0];
            let inner = arr[i]["hooks"].as_array_mut().unwrap();
            let mut updated = false;
            for hook in inner.iter_mut() {
                if is_git_ai_owned(hook) && hook != replacement {
                    *hook = replacement.clone();
                    updated = true;
                }
            }
            if updated {
                report.changed = true;
                report.updated.push(stage.into());
            }
        }
        None => {
            arr.insert(0, new_block);
            report.changed = true;
            report.added.push(stage.into());
        }
    }
}

/// 删除分组内的 git-ai hook,仅移除因本次清理而变空的分组。
fn remove_git_ai_owned(arr: &mut Vec<Value>, stage: &str, report: &mut MergeReport) {
    // 1. 保留每个分组的用户 hook 和附加属性。
    let mut removed = false;
    arr.retain_mut(|block| {
        let Some(inner) = block.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = inner.len();
        inner.retain(|hook| !is_git_ai_owned(hook));
        if inner.len() == before {
            return true;
        }
        removed = true;
        !inner.is_empty()
    });

    // 2. 同组用户 hook 留存时,分组数量未变也需要报告已修改。
    if removed {
        report.changed = true;
        report.removed.push(stage.into());
    }
}

/// 判定 hook 是否由 git-ai 拥有(可被我们 update / 删除)。
fn is_git_ai_owned(h: &Value) -> bool {
    let cmd = h.get("command").and_then(|v| v.as_str()).unwrap_or("");
    cmd.contains("checkpoint claude")
}

fn with_extension(p: &std::path::Path, ext: &str) -> std::path::PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    std::path::PathBuf::from(s)
}

/// 给 UI 看的诊断:当前 settings.json 是 Official / None。
pub fn detect_mode() -> HooksMode {
    let path = claude_settings_json();
    let Ok(raw) = fs::read_to_string(&path) else {
        return HooksMode::None;
    };
    let Ok(v): std::result::Result<Value, _> = serde_json::from_str(&raw) else {
        return HooksMode::None;
    };
    for stage in ["PreToolUse", "PostToolUse"] {
        let arr = v
            .pointer(&format!("/hooks/{stage}"))
            .and_then(|v| v.as_array());
        let Some(arr) = arr else { continue };
        for block in arr {
            let inner = block.get("hooks").and_then(|v| v.as_array());
            let Some(inner) = inner else { continue };
            for h in inner {
                if is_git_ai_owned(h) {
                    return HooksMode::Official;
                }
            }
        }
    }
    HooksMode::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn setup() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_AI_STUDIO_TEST_HOME", tmp.path());
        // 同时初始化 ~/.claude 目录,放空 settings.json
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        tmp
    }

    #[test]
    #[serial]
    fn detect_none_on_empty_settings() {
        let _g = setup();
        assert_eq!(detect_mode(), HooksMode::None);
    }

    #[test]
    #[serial]
    fn merge_to_official_inserts_when_empty() {
        let _g = setup();
        let rep = merge_to_mode(HooksMode::Official, Some("/home/u/.git-ai/bin/git-ai")).unwrap();
        assert!(rep.changed);
        assert_eq!(rep.added.len(), 2);
        assert_eq!(detect_mode(), HooksMode::Official);
    }

    #[test]
    #[serial]
    fn merge_preserves_unrelated_hooks() {
        let _g = setup();
        // 先放一份用户自己写的 hook
        let p = claude_settings_json();
        fs::write(
            &p,
            r#"{ "permissions": {"deny": ["x"]}, "hooks": { "PostToolUse": [
                { "matcher": "X", "hooks": [{ "type": "command", "command": "echo user-hook" }] }
            ]}}"#,
        )
        .unwrap();
        let _ = merge_to_mode(HooksMode::Official, Some("/home/u/.git-ai/bin/git-ai")).unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(
            raw.contains("\"echo user-hook\""),
            "user hook 被吃掉了: {raw}"
        );
        assert!(raw.contains("checkpoint claude"));
        assert!(raw.contains("permissions"));
    }

    #[test]
    #[serial]
    fn none_mode_strips_git_ai_owned_only() {
        let _g = setup();
        let p = claude_settings_json();
        fs::write(
            &p,
            r#"{"hooks": {
                "PostToolUse": [
                  { "matcher": "X", "hooks": [{ "type": "command", "command": "/g/git-ai checkpoint claude" }] },
                  { "matcher": "Y", "hooks": [{ "type": "command", "command": "echo user" }] }
                ]
            }}"#,
        )
        .unwrap();
        let _ = merge_to_mode(HooksMode::None, None).unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("checkpoint claude"));
        assert!(raw.contains("echo user"));
    }

    /// 上游复用通配 matcher 安装时,停用必须保留同组用户 hook 和备份原文。
    #[test]
    #[serial]
    fn none_mode_preserves_mixed_groups_and_backup() {
        // 1. 模拟上游在已有用户分组内追加 git-ai hook 的正常安装结果。
        let _g = setup();
        let p = claude_settings_json();
        let user_hook = json!({"type": "command", "command": "echo user", "timeout": 15});
        let prompt_hook = json!({"type": "prompt", "prompt": "Review the edit"});
        let owned_hook =
            json!({"type": "command", "command": "/g/git-ai checkpoint claude --hook-input stdin"});
        let original = json!({
            "permissions": {"deny": ["x"]},
            "hooks": {
                "PreToolUse": [
                    {"matcher": "*", "custom": {"owner": "user"}, "hooks": [user_hook.clone(), owned_hook.clone(), prompt_hook.clone(), owned_hook.clone()]},
                    {"matcher": "Bash", "hooks": [owned_hook.clone()]},
                    {"matcher": "Read", "hooks": []}
                ],
                "PostToolUse": [
                    {"matcher": "*", "hooks": [owned_hook, user_hook.clone()]}
                ]
            }
        });
        let original_bytes = serde_json::to_vec_pretty(&original).unwrap();
        fs::write(&p, &original_bytes).unwrap();

        // 2. 停用时只移除 git-ai hook,原本就空的用户分组保持原状。
        let report = merge_to_mode(HooksMode::None, None).unwrap();
        let actual: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            actual,
            json!({
                "permissions": {"deny": ["x"]},
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "*", "custom": {"owner": "user"}, "hooks": [user_hook.clone(), prompt_hook]},
                        {"matcher": "Read", "hooks": []}
                    ],
                    "PostToolUse": [{"matcher": "*", "hooks": [user_hook]}]
                }
            })
        );
        assert!(report.changed);
        assert_eq!(report.removed, ["PreToolUse", "PostToolUse"]);

        // 3. 自动备份逐字节保留原始配置。
        let backups = backups::list_backups().unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0].path).unwrap(), original_bytes);
    }

    /// 更新 git-ai 命令时也只能替换自有 hook,不能覆盖同组用户配置。
    #[test]
    #[serial]
    fn official_mode_preserves_mixed_group_when_updating() {
        // 1. 已有分组同时包含用户 hook 和旧 git-ai 路径。
        let _g = setup();
        let p = claude_settings_json();
        let user_hook = json!({"type": "command", "command": "echo user", "timeout": 15});
        fs::write(&p, serde_json::to_vec(&json!({"hooks": {"PostToolUse": [
            {"matcher": "*", "custom": true, "hooks": [
                user_hook.clone(),
                {"type": "command", "command": "/old/git-ai checkpoint claude --hook-input stdin"}
            ]}
        ]}})).unwrap()).unwrap();

        // 2. 更新命令后,分组规则、属性及用户 hook 完全保留。
        let report = merge_to_mode(HooksMode::Official, Some("/new/git-ai")).unwrap();
        let actual: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            actual["hooks"]["PostToolUse"],
            json!([
                {"matcher": "*", "custom": true, "hooks": [
                    user_hook,
                    {"type": "command", "command": "/new/git-ai checkpoint claude --hook-input stdin"}
                ]}
            ])
        );
        assert_eq!(report.updated, ["PostToolUse"]);
        assert!(report.changed);
    }
}
