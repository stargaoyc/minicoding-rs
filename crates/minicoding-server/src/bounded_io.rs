//! 有界 IO 辅助（ST-R6-2，2026-08-28 R6 审查）。
//!
//! `read_line` 会先把整行缓冲进内存再返回——恶意/异常本地客户端可发无限长
//! 单行使 server 无限缓冲 OOM。`read_line_bounded` 逐块累积，超过上限即丢弃
//! 该行残余并返回超限标记（fail-closed），避免"先全量缓冲再判断长度"的
//! OOM 窗口（R5 FE-8 声称用 `take(MAX+1)` 截断但实现未生效）。

use std::io::Error as IoError;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// 有界读取一行（含换行消费），内存上限 `max` 字节。
///
/// 返回：
/// - `Ok(None)`：EOF 且无缓冲内容；
/// - `Ok(Some(Ok(line)))`：正常行（不含换行符）；
/// - `Ok(Some(Err(())))`：单行超限——该行残余已整体丢弃，流保持对齐，
///   调用方应按超限处理（如报 `FrameTooLarge` 后继续）。
///
/// # Errors
/// 底层读取 IO 错误。
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Result<String, ()>>, IoError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut too_long = false;
    loop {
        // fill_buf 的借用必须在 consume 前结束（可用性检查不可跨借用存活），
        // 故每个分支先取 (pos, len) 后复制内容、再消费。
        let (newline_pos, avail_len) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if buf.is_empty() && !too_long {
                    return Ok(None);
                }
                return Ok(Some(if too_long {
                    Err(())
                } else {
                    Ok(String::from_utf8_lossy(&buf).into_owned())
                }));
            }
            (available.iter().position(|&b| b == b'\n'), available.len())
        };
        if let Some(pos) = newline_pos {
            // 换行在块内：整行长度 = 已累积 + pos，同样受 max 约束
            // （大块缓冲内换行晚出现时此前会漏检，超限行被当作正常行返回）
            if !too_long && buf.len() + pos > max {
                too_long = true;
            }
            if !too_long {
                let available = reader.fill_buf().await?;
                let end = pos.min(available.len());
                buf.extend_from_slice(&available[..end]);
            }
            reader.consume(pos + 1);
            return Ok(Some(if too_long {
                Err(())
            } else {
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }));
        }
        if !too_long && buf.len() + avail_len > max {
            too_long = true;
        }
        if !too_long {
            let available = reader.fill_buf().await?;
            buf.extend_from_slice(available);
        }
        reader.consume(avail_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader_of(bytes: &[u8]) -> tokio::io::BufReader<Cursor<&[u8]>> {
        tokio::io::BufReader::new(Cursor::new(bytes))
    }

    #[tokio::test]
    async fn reads_normal_lines() {
        let mut reader = reader_of(b"hello\nworld\n");
        assert_eq!(
            read_line_bounded(&mut reader, 1024).await.unwrap(),
            Some(Ok("hello".to_string()))
        );
        assert_eq!(
            read_line_bounded(&mut reader, 1024).await.unwrap(),
            Some(Ok("world".to_string()))
        );
        assert_eq!(read_line_bounded(&mut reader, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_line_and_discards_it() {
        // 超限行整体丢弃且流对齐：后续行可正常读取。
        let mut reader = reader_of(b"aaaa\nbbbb\n");
        assert_eq!(
            read_line_bounded(&mut reader, 3).await.unwrap(),
            Some(Err(()))
        );
        assert_eq!(
            read_line_bounded(&mut reader, 1024).await.unwrap(),
            Some(Ok("bbbb".to_string()))
        );
    }

    #[tokio::test]
    async fn oversized_without_trailing_newline_errors() {
        let mut reader = reader_of(b"aaaaaaaa");
        assert_eq!(
            read_line_bounded(&mut reader, 4).await.unwrap(),
            Some(Err(()))
        );
        assert_eq!(read_line_bounded(&mut reader, 4).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_line_with_late_newline_rejected() {
        // 大块缓冲内换行晚出现：整行长度必须计已累积内容 + 块内 pos
        let mut reader = reader_of(b"aaaaaaaa\nbb\n");
        assert_eq!(
            read_line_bounded(&mut reader, 4).await.unwrap(),
            Some(Err(()))
        );
        assert_eq!(
            read_line_bounded(&mut reader, 1024).await.unwrap(),
            Some(Ok("bb".to_string()))
        );
    }

    #[tokio::test]
    async fn line_exactly_at_limit_accepted() {
        let mut reader = reader_of(b"aaaa\n");
        assert_eq!(
            read_line_bounded(&mut reader, 4).await.unwrap(),
            Some(Ok("aaaa".to_string()))
        );
    }
}
