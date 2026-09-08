//! `~/.git-ai/config.json` 的"合并而非覆盖"读写。
//! git-ai 自己写这个文件,我们只动几个字段并保留其它键。
//! 修改前会先备份到 `~/.git-ai-studio/backups/git-ai-config-<ts>.json`。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, Result};
use crate::paths::{git_ai_config_json, studio_backups_dir};

/// git-ai 配置中由 Studio 管理的可选更新字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitAiConfigPatch {
    pub disable_auto_updates: Option<bool>,
    pub update_channel: Option<String>,
}

/// git-ai 配置的展示结果，其余配置键保持原有值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitAiConfig {
    pub disable_auto_updates: bool,
    pub update_channel: String,
    /// 保留其它字段供前端展示
    #[serde(flatten)]
    pub other: serde_json::Map<String, Value>,
}

impl Default for GitAiConfig {
    fn default() -> Self {
        Self {
            disable_auto_updates: false,
            update_channel: "stable".to_string(),
            other: serde_json::Map::new(),
        }
    }
}

/// 读取 git-ai 配置；仅文件不存在时使用首次安装默认值。
pub fn read() -> Result<GitAiConfig> {
    // 1. 读取实际配置位置
    read_from_path(&git_ai_config_json())
}

/// 读取指定位置的原始字节，区分文件缺失和读取失败。
fn read_existing(path: &Path) -> Result<Option<Vec<u8>>> {
    // 1. 仅 NotFound 表示没有原配置
    match fs::read(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// 严格解析指定文件，保留非 Studio 管理的配置字段。
fn read_from_path(path: &Path) -> Result<GitAiConfig> {
    // 1. 缺失文件允许使用首次安装默认配置
    let Some(raw) = read_existing(path)? else {
        return Ok(GitAiConfig::default());
    };

    // 2. 解析对象并提取 Studio 管理字段
    let v: Value = serde_json::from_slice(&raw)?;
    let obj = v
        .as_object()
        .ok_or_else(|| AppError::Other(format!("{} 不是 JSON 对象", path.display())))?;
    let disable_auto_updates = obj
        .get("disable_auto_updates")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let update_channel = obj
        .get("update_channel")
        .and_then(|x| x.as_str())
        .unwrap_or("stable")
        .to_string();
    let mut other = obj.clone();
    other.remove("disable_auto_updates");
    other.remove("update_channel");
    Ok(GitAiConfig {
        disable_auto_updates,
        update_channel,
        other,
    })
}

/// 合并 Studio 管理字段；原配置读取、解析或备份失败时停止，保留原文件。
pub fn write(patch: &GitAiConfigPatch) -> Result<GitAiConfig> {
    // 1. 使用实际配置与备份目录执行更新
    write_to_path(patch, &git_ai_config_json(), &studio_backups_dir())
}

/// 合并指定文件并在替换前保存原始字节，供正式写入与隔离文件测试共用。
fn write_to_path(patch: &GitAiConfigPatch, path: &Path, backup_dir: &Path) -> Result<GitAiConfig> {
    // 1. 严格解析已有配置，仅缺失文件允许创建空对象
    let raw = read_existing(path)?;
    let mut value: Value = match raw.as_deref() {
        Some(bytes) => serde_json::from_slice(bytes)?,
        None => Value::Object(Default::default()),
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| AppError::Other("已存在的 config.json 不是 JSON 对象".to_string()))?;

    // 2. 合并本次更新，保持其它键不变
    if let Some(disabled) = patch.disable_auto_updates {
        obj.insert("disable_auto_updates".into(), Value::Bool(disabled));
    }
    if let Some(channel) = &patch.update_channel {
        obj.insert("update_channel".into(), Value::String(channel.clone()));
    }
    let serialized = serde_json::to_vec_pretty(&value)?;

    // 3. 确认原始字节备份成功，才允许替换配置
    if let Some(bytes) = raw {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| AppError::Other(format!("生成配置备份时间失败: {error}")))?
            .as_secs();
        fs::create_dir_all(backup_dir)?;
        let backup = backup_dir.join(format!("git-ai-config-{timestamp}.json"));
        fs::write(backup, bytes)?;
    }

    // 4. 写入临时文件后替换目标文件
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = with_extension(path, "json.tmp");
    fs::write(&temporary, serialized)?;
    fs::rename(&temporary, path)?;
    read_from_path(path)
}

/// 在完整路径后追加临时文件扩展名，避免更改原文件名主体。
fn with_extension(path: &Path, extension: &str) -> PathBuf {
    // 1. 追加扩展名并保留原路径编码
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺失配置可读取默认值并通过正常 patch 创建，不产生无来源的备份。
    #[test]
    fn missing_config_initializes_on_write() {
        // 1. 读取缺失文件不创建配置
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git-ai/config.json");
        let backups = dir.path().join("backups");
        let config = read_from_path(&path).unwrap();
        assert!(!config.disable_auto_updates);
        assert_eq!(config.update_channel, "stable");
        assert!(!path.exists());

        // 2. 首次写入 patch 后可正常读取，且无需原文件备份
        let updated = write_to_path(
            &GitAiConfigPatch {
                disable_auto_updates: Some(true),
                update_channel: None,
            },
            &path,
            &backups,
        )
        .unwrap();
        assert!(updated.disable_auto_updates);
        assert_eq!(updated.update_channel, "stable");
        assert!(!backups.exists());
    }

    /// 正常更新保留其它键，并按字节保存含原格式的备份。
    #[test]
    fn valid_config_update_preserves_other_keys_and_original_backup() {
        // 1. 准备具有原始空白格式与非托管字段的有效配置
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let backups = dir.path().join("backups");
        let original =
            b"{\n  \"existing_field\": \"preserved\", \"update_channel\": \"stable\"\n}\n";
        fs::write(&path, original).unwrap();

        // 2. 合并受管字段并验证原始字节备份
        let updated = write_to_path(
            &GitAiConfigPatch {
                disable_auto_updates: Some(true),
                update_channel: Some("none".into()),
            },
            &path,
            &backups,
        )
        .unwrap();
        assert!(updated.disable_auto_updates);
        assert_eq!(updated.update_channel, "none");
        assert_eq!(
            updated.other.get("existing_field").and_then(Value::as_str),
            Some("preserved")
        );
        let files = fs::read_dir(&backups)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(files[0].path()).unwrap(), original);
    }

    /// JSON 损坏、非法 UTF-8 和非对象根值均不得被覆盖。
    #[test]
    fn invalid_config_fails_without_changing_original_bytes() {
        // 1. 分别准备不能被合并的已有文件
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let backups = dir.path().join("backups");
        for original in [b"{invalid".as_slice(), b"{\"key\":\"\xff\"}", b"[]"] {
            fs::write(&path, original).unwrap();

            // 2. 读写均须失败，原文件及备份目录保持不变
            assert!(read_from_path(&path).is_err());
            assert!(write_to_path(&GitAiConfigPatch::default(), &path, &backups).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
            assert!(!backups.exists());
        }
    }

    /// 读取异常不能被解释为没有已有配置。
    #[test]
    fn config_read_error_does_not_initialize_defaults() {
        // 1. 配置路径为目录时，读取和写入均明确失败
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        assert!(read_from_path(dir.path()).is_err());
        assert!(write_to_path(&GitAiConfigPatch::default(), dir.path(), &backups).is_err());
        assert!(dir.path().is_dir());
        assert!(!backups.exists());
    }

    /// 备份目录不可创建时，中止修改并保留已有配置字节。
    #[test]
    fn backup_failure_leaves_original_config_unchanged() {
        // 1. 用普通文件占据备份目录位置，稳定制造备份失败
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let backups = dir.path().join("backups");
        let original = b"{\"update_channel\":\"stable\"}";
        fs::write(&path, original).unwrap();
        fs::write(&backups, b"blocked").unwrap();

        // 2. 不得跳过失败备份继续写入配置
        let patch = GitAiConfigPatch {
            disable_auto_updates: Some(true),
            update_channel: None,
        };
        assert!(write_to_path(&patch, &path, &backups).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(&backups).unwrap(), b"blocked");
    }
}
