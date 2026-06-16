//! Root 路径规范化：保证不同形式的相同路径会被识别为同一个 root，避免重复扫描。
//!
//! 规则（按平台）：
//! - **Windows**：
//!   - 盘符统一大写（`d:\photos` → `D:\photos`）
//!   - 反斜杠统一为 `\`
//!   - 尾随分隔符去除，但盘符根 `D:\` 保留（否则 `D:` 没意义）
//!   - UNC 路径（`\\server\share`）尾分隔符去除，host/share 大小写不变（SMB 服务器侧大小写敏感性不确定）
//! - **macOS / Linux**：
//!   - 仅去除尾随 `/`，大小写不变（POSIX 大小写敏感）
//!
//! 不做的事：
//! - 不解析符号链接（`canonicalize` 会触发 IO，且会让"链接到 NAS 的快捷方式"失去意义）
//! - 不展开 `~`（前端选目录返回的就是绝对路径）

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// 把绝对路径规范化为存库形式。**输入必须是绝对路径**，否则返回 `AppError::Config`。
pub fn normalize(input: &Path) -> AppResult<PathBuf> {
    if !input.is_absolute() {
        return Err(AppError::Config(format!(
            "path must be absolute: {}",
            input.display()
        )));
    }
    let s = input.to_string_lossy().to_string();
    let s = normalize_str(&s);
    Ok(PathBuf::from(s))
}

/// 字符串层面的规范化，跨平台分支。提取出来便于测试。
fn normalize_str(s: &str) -> String {
    if is_windows_path(s) {
        normalize_windows(s)
    } else {
        normalize_posix(s)
    }
}

/// 判断字符串是否长得像 Windows 路径（盘符 + 冒号，或 UNC）。
/// 注意：在 macOS 上跑测试也能识别。
fn is_windows_path(s: &str) -> bool {
    if s.starts_with("\\\\") {
        return true;
    }
    let mut chars = s.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), Some(':')) if c.is_ascii_alphabetic()
    )
}

fn normalize_windows(s: &str) -> String {
    // 反斜杠统一
    let s = s.replace('/', "\\");

    if let Some(stripped) = s.strip_prefix("\\\\") {
        // UNC：保留双反斜杠前缀，去除尾随分隔符（保留 host\share 至少一层）
        let body = stripped.trim_end_matches('\\');
        return format!("\\\\{body}");
    }

    // 普通盘符路径：D:\xxx
    let mut chars = s.chars();
    let drive = chars.next().unwrap_or(' ').to_ascii_uppercase();
    let colon = chars.next().unwrap_or(' ');
    debug_assert_eq!(colon, ':');
    let rest: String = chars.collect();

    // rest 是 \xxx\yyy 或 \xxx\yyy\ 或 \
    let rest = if rest == "\\" {
        // 盘符根保留 D:\
        "\\".to_string()
    } else {
        rest.trim_end_matches('\\').to_string()
    };

    format!("{drive}:{rest}")
}

fn normalize_posix(s: &str) -> String {
    if s == "/" {
        return "/".to_string();
    }
    s.trim_end_matches('/').to_string()
}

/// 根据路径形式推断 root 类型（local / network）。
pub fn detect_root_type(input: &Path) -> &'static str {
    let s = input.to_string_lossy();
    if s.starts_with("\\\\") || s.starts_with("//") {
        "network"
    } else {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_drive_uppercases() {
        assert_eq!(normalize_str("d:\\photos"), "D:\\photos");
        assert_eq!(normalize_str("D:\\Photos"), "D:\\Photos");
    }

    #[test]
    fn windows_strips_trailing_separator() {
        assert_eq!(normalize_str("D:\\photos\\"), "D:\\photos");
        assert_eq!(normalize_str("D:\\photos\\\\"), "D:\\photos");
    }

    #[test]
    fn windows_drive_root_keeps_separator() {
        // 盘符根必须保留反斜杠，否则 D: 失去意义
        assert_eq!(normalize_str("D:\\"), "D:\\");
        assert_eq!(normalize_str("d:\\"), "D:\\");
    }

    #[test]
    fn windows_forward_slashes_converted() {
        assert_eq!(normalize_str("D:/photos/2026"), "D:\\photos\\2026");
    }

    #[test]
    fn unc_path_preserved() {
        assert_eq!(normalize_str("\\\\nas\\share"), "\\\\nas\\share");
        assert_eq!(normalize_str("\\\\nas\\share\\"), "\\\\nas\\share");
        assert_eq!(normalize_str("\\\\nas\\share\\photos"), "\\\\nas\\share\\photos");
    }

    #[test]
    fn posix_strips_trailing_slash() {
        assert_eq!(normalize_str("/Users/admin/Pictures/"), "/Users/admin/Pictures");
        assert_eq!(normalize_str("/Users/admin/Pictures"), "/Users/admin/Pictures");
    }

    #[test]
    fn posix_root_kept() {
        assert_eq!(normalize_str("/"), "/");
    }

    #[test]
    fn posix_case_preserved() {
        // POSIX 大小写敏感，不动
        assert_eq!(normalize_str("/Users/Admin"), "/Users/Admin");
        assert_eq!(normalize_str("/users/admin"), "/users/admin");
    }

    #[test]
    fn detect_type_local() {
        assert_eq!(detect_root_type(Path::new("D:\\photos")), "local");
        assert_eq!(detect_root_type(Path::new("/Users/admin")), "local");
    }

    #[test]
    fn detect_type_network() {
        assert_eq!(detect_root_type(Path::new("\\\\nas\\share")), "network");
        assert_eq!(detect_root_type(Path::new("//nas/share")), "network");
    }

    #[test]
    fn relative_path_rejected() {
        assert!(normalize(Path::new("photos")).is_err());
        assert!(normalize(Path::new("./photos")).is_err());
    }
}
