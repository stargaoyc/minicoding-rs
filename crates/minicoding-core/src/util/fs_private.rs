//! 私密文件写入（0600，S7 收敛点）。
//!
//! 凭证/审计等敏感内容的落盘统一走本模块：unix 以 `mode 0o600` 创建（OpenOptions
//! 原子指定，避免"先写后 chmod"的竞态窗口）；windows 沿用默认用户 ACL。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// 写入仅属主可读的私密文件（覆盖写）。
///
/// unix 下以 `mode 0o600` 创建；已存在文件不改变既有权限（保持最小惊讶），
/// 需要收紧时由调用方显式处理。
///
/// # Errors
/// 打开或写入失败时透传 `std::io::Error`。
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    // 已存在文件的权限兜底收紧到 0600（历史文件可能是宽权限创建的）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = f.metadata()?;
        if meta.permissions().mode() & 0o777 != 0o600 {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            f.set_permissions(perm)?;
        }
    }
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_creates_0600_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("secret.toml");
        write_private(&p, b"api_key = \"sk-test\"").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "新文件应为 0600");
        }
        assert_eq!(std::fs::read(&p).expect("read"), b"api_key = \"sk-test\"");
    }

    #[test]
    fn write_private_tightens_existing_wide_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("wide.toml");
        std::fs::write(&p, b"old").expect("seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        }
        write_private(&p, b"new").expect("rewrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "已存在的宽权限文件应被收紧");
        }
        assert_eq!(std::fs::read(&p).expect("read"), b"new");
    }
}
