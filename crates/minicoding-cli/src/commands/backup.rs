//! `minicoding backup` 子命令：打包 `~/.minicoding/` 为 tar.gz（S-05）。
//!
//! 见 `docs/data-model.md` §12 备份与导出、`docs/features.md` S-05。
//!
//! ## 用法
//!
//! ```text
//! minicoding backup create [--output <path>]   # 打包 ~/.minicoding/ 为 tar.gz
//! minicoding backup list                       # 列出 ~/.minicoding/backups/ 下的备份
//! ```
//!
//! 不构建 `Runtime`，无需 API key——仅做文件 IO 与终端渲染。

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::fs::File;
use time::OffsetDateTime;

/// `backup` 顶层子命令。
#[derive(Args, Debug)]
pub struct BackupCommand {
    #[command(subcommand)]
    pub action: BackupAction,
}

/// `backup` 子命令动作。
#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// 创建备份：打包 `~/.minicoding/` 为 tar.gz。
    Create {
        /// 输出路径（默认 `~/.minicoding/backups/minicoding-<timestamp>.tar.gz`）。
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
    },
    /// 列出已有备份（`~/.minicoding/backups/` 下的 tar.gz 文件）。
    List,
}

/// 执行 `backup` 子命令。
///
/// # Errors
/// IO 失败（创建/写入 tar.gz、读取目录）时返回错误。
pub fn run_backup_command(cmd: &BackupCommand) -> Result<()> {
    let home =
        minicoding_core::paths::minicoding_home().context("无法确定 minicoding home 目录")?;
    match &cmd.action {
        BackupAction::Create { output } => {
            let output_path = output.as_deref().map(Utf8PathBuf::from);
            create_backup(&home, output_path)
        }
        BackupAction::List => list_backups(&home),
    }
}

/// 创建 tar.gz 备份。
///
/// 打包 `home` 下所有文件（排除 `backups/` 自身避免递归），
/// 输出到 `--output` 指定路径或默认 `<home>/backups/minicoding-<timestamp>.tar.gz`。
///
/// # Errors
/// 创建/写入备份文件、读取目录失败时返回错误。
fn create_backup(home: &Utf8Path, output: Option<Utf8PathBuf>) -> Result<()> {
    // 默认输出路径：<home>/backups/minicoding-<timestamp>.tar.gz
    let output_path = if let Some(p) = output {
        p
    } else {
        let backups_dir = home.join("backups");
        std::fs::create_dir_all(&backups_dir)
            .with_context(|| format!("创建备份目录失败: {backups_dir}"))?;
        let timestamp = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string())
            .replace(':', "-");
        backups_dir.join(format!("minicoding-{timestamp}.tar.gz"))
    };

    // 创建 tar.gz：GzEncoder 包裹 File，tar::Builder 写入归档条目
    let file =
        File::create(&output_path).with_context(|| format!("创建备份文件失败: {output_path}"))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    // 遍历 home 目录打包所有文件，排除 backups/ 避免递归
    add_dir_to_tar(&mut builder, home, home, &["backups"])?;

    builder.finish()?;
    builder.into_inner()?.finish()?;

    let size = std::fs::metadata(&output_path)?.len();
    println!("备份已创建: {} ({})", output_path, format_size(size));
    Ok(())
}

/// 递归添加目录到 tar builder。
///
/// `base` 为相对路径基准（用于生成归档内条目名），`dir` 为当前遍历目录，
/// `exclude` 为相对于 `base` 的排除项（按路径组件前缀匹配）。
fn add_dir_to_tar(
    builder: &mut tar::Builder<GzEncoder<File>>,
    base: &Utf8Path,
    dir: &Utf8Path,
    exclude: &[&str],
) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("读取目录失败: {dir}"))?;
    for entry in entries {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|e| anyhow::anyhow!("路径非 UTF-8: {}", e.display()))?;
        let path_ref: &Utf8Path = &path;
        let name = path_ref.strip_prefix(base).unwrap_or(path_ref);

        // 排除项：按相对路径组件匹配（"backups" 排除 backups/ 及其子内容）
        if exclude
            .iter()
            .any(|&ex| name.as_str() == ex || name.starts_with(ex))
        {
            continue;
        }

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            builder.append_dir(name, &path)?;
            add_dir_to_tar(builder, base, path_ref, exclude)?;
        } else if file_type.is_file() {
            let mut f = File::open(&path)?;
            builder.append_file(name, &mut f)?;
        }
        // 符号链接等特殊文件跳过（避免递归/越界）
    }
    Ok(())
}

/// 列出已有备份（`<home>/backups/` 下的 .tar.gz 文件）。
///
/// # Errors
/// 读取备份目录失败时返回错误。
fn list_backups(home: &Utf8Path) -> Result<()> {
    let backups_dir = home.join("backups");

    if !backups_dir.exists() {
        println!("暂无备份");
        return Ok(());
    }

    let mut backups: Vec<_> = std::fs::read_dir(&backups_dir)
        .with_context(|| format!("读取备份目录失败: {backups_dir}"))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "gz"))
        .collect();
    backups.sort_by_key(|e| std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));

    if backups.is_empty() {
        println!("暂无备份");
        return Ok(());
    }

    println!("{:<40} {:>12}", "文件名", "大小");
    println!("{}", "-".repeat(54));
    for entry in &backups {
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = entry.metadata().map_or(0, |m| m.len());
        println!("{:<40} {:>12}", name, format_size(size));
    }
    Ok(())
}

/// 格式化文件大小为人类可读字符串。
#[allow(clippy::cast_precision_loss)] // u64 → f64 精度损失可接受（文件大小远小于 2^53）
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    #[test]
    fn create_and_list_backup() {
        let tmp = TempDir::new().expect("创建临时目录");
        let home =
            Utf8PathBuf::from_path_buf(tmp.path().to_owned()).expect("tempdir 路径应为 UTF-8");

        // 准备测试文件
        std::fs::write(home.join("config.toml"), "test = true\n").expect("写 config.toml");
        std::fs::create_dir_all(home.join("sessions")).expect("创建 sessions 目录");
        std::fs::write(home.join("sessions/test.jsonl"), "{}\n").expect("写 test.jsonl");

        // 创建备份（默认输出路径）
        create_backup(&home, None).expect("创建备份失败");

        // 验证备份文件存在
        let backups_dir = home.join("backups");
        let mut entries: Vec<_> = std::fs::read_dir(&backups_dir)
            .expect("读取 backups 目录")
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "应恰好有一个备份文件");

        let archive_path = entries.remove(0).path();
        assert_eq!(
            archive_path.extension().and_then(|e| e.to_str()),
            Some("gz"),
            "备份文件应为 .tar.gz"
        );

        // 读取 tar.gz 验证内容
        let file = std::fs::File::open(&archive_path).expect("打开备份文件");
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .expect("读取 tar 条目")
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.path().ok().and_then(|p| p.to_str().map(String::from)))
            .collect();
        assert!(
            names.iter().any(|n| n == "config.toml"),
            "应包含 config.toml，实际: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "sessions/test.jsonl"),
            "应包含 sessions/test.jsonl，实际: {names:?}"
        );
        // backups/ 自身不应被打包（避免递归）
        assert!(
            !names.iter().any(|n| n.starts_with("backups")),
            "不应包含 backups/ 目录，实际: {names:?}"
        );

        // 验证 list_backups 不报错
        list_backups(&home).expect("列出备份失败");
    }
}
