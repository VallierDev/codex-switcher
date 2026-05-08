//! Codex Switcher - Shell Tool 输出压缩（Phase 1：截断）
//!
//! 目标：在 proxy 把上游响应转发回客户端之前，把超长的 shell tool 输出
//! （`exec_command` / `Bash` / `write_stdin` 等）压成「头 50 行 + 尾 20 行」，
//! 以减少送回模型的 token。
//!
//! 这是纯逻辑模块，不做任何 IO / 网络 / tokio。被 `proxy.rs` 调用。

use std::borrow::Cow;

/// 默认保留的头部行数
pub const DEFAULT_HEAD_LINES: usize = 50;
/// 默认保留的尾部行数
pub const DEFAULT_TAIL_LINES: usize = 20;
/// 总行数低于该值时走「短文本但超阈值」的分支（首半 + 尾四分之一）
const SHORT_OUTPUT_LINE_THRESHOLD: usize = 70;
/// 二进制内容判定：非可打印字符占比超过该比例 → 视为二进制，不截断
const BINARY_RATIO_THRESHOLD: f32 = 0.10;
/// 检测二进制时最多采样的字节数（避免对大文件做 O(n) 扫描）
const BINARY_SAMPLE_BYTES: usize = 8 * 1024;

/// 压缩结果
#[derive(Debug)]
pub struct Compressed<'a> {
    /// 压缩后内容；未压缩时是 Borrowed 原文
    pub compressed: Cow<'a, str>,
    /// 原始字节数（UTF-8）
    pub original_bytes: usize,
    /// 输出字节数（截断后；未压缩时 == original_bytes）
    pub output_bytes: usize,
    /// 是否真的做了压缩
    pub was_compressed: bool,
    /// 被丢弃的行数（未压缩时为 0）
    pub lines_dropped: u32,
}

/// 对一段文本做截断压缩。
///
/// - `text.len() <= threshold_bytes` → 返回 `Borrowed`，`was_compressed=false`
/// - 二进制内容（非可打印 > 10%） → 返回 `Borrowed`，`was_compressed=false`
/// - 总行数 ≥ 70 → 保留头 50 行 + 尾 20 行
/// - 总行数 < 70 但字节数超阈值（极少数：少量超长行，例如一行 50KB 的 base64） →
///   保留前一半 + 后四分之一行
pub fn compress(text: &str, threshold_bytes: usize) -> Compressed<'_> {
    let original_bytes = text.len();

    if original_bytes <= threshold_bytes {
        return Compressed {
            compressed: Cow::Borrowed(text),
            original_bytes,
            output_bytes: original_bytes,
            was_compressed: false,
            lines_dropped: 0,
        };
    }

    if looks_binary(text) {
        return Compressed {
            compressed: Cow::Borrowed(text),
            original_bytes,
            output_bytes: original_bytes,
            was_compressed: false,
            lines_dropped: 0,
        };
    }

    // 行切分：保留行为基础（split_terminator('\n') 不会在末尾产生空行；
    // split('\n') 会，对我们的截断更友好——头/尾切片都按"含可能的最后空行"算）
    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();

    let (head_n, tail_n) = if total_lines >= SHORT_OUTPUT_LINE_THRESHOLD {
        (DEFAULT_HEAD_LINES, DEFAULT_TAIL_LINES)
    } else {
        // 短文本兜底：少量超长行的场景，按比例切，至少各保 1 行
        let head = (total_lines / 2).max(1);
        let tail = (total_lines / 4).max(1);
        (head, tail)
    };

    if head_n + tail_n >= total_lines {
        // 截后总行数还覆盖原文 → 没必要截，按"未压缩"返回
        return Compressed {
            compressed: Cow::Borrowed(text),
            original_bytes,
            output_bytes: original_bytes,
            was_compressed: false,
            lines_dropped: 0,
        };
    }

    let dropped = total_lines - head_n - tail_n;
    let head_slice = &lines[..head_n];
    let tail_slice = &lines[total_lines - tail_n..];

    let dropped_bytes = dropped_byte_count(text, head_slice, tail_slice);

    let mut out = String::with_capacity(original_bytes / 4 + 128);
    out.push_str(&head_slice.join("\n"));
    out.push('\n');
    out.push_str(&format!(
        "[... {} lines ({} bytes) truncated by codex-switcher ...]",
        dropped, dropped_bytes
    ));
    out.push('\n');
    out.push_str(&tail_slice.join("\n"));

    let output_bytes = out.len();
    Compressed {
        compressed: Cow::Owned(out),
        original_bytes,
        output_bytes,
        was_compressed: true,
        lines_dropped: dropped as u32,
    }
}

/// 估算被丢弃部分的字节数（原长 - 头 - 尾 - 拼接换行符）。
/// 不要求精确，只用于人类可读的告知信息。
fn dropped_byte_count(text: &str, head: &[&str], tail: &[&str]) -> usize {
    let head_bytes: usize = head.iter().map(|s| s.len()).sum::<usize>() + head.len();
    let tail_bytes: usize = tail.iter().map(|s| s.len()).sum::<usize>() + tail.len();
    text.len().saturating_sub(head_bytes + tail_bytes)
}

/// 二进制内容启发式检测：采样前 8KB，统计非可打印字节比例。
/// `\t` `\n` `\r` 算可打印；`< 0x20` 的其它控制符 + `0x7F` 算非可打印。
/// UTF-8 多字节序列的高位字节 (>= 0x80) 也算可打印（中文/emoji shell 输出常见）。
fn looks_binary(text: &str) -> bool {
    let bytes = text.as_bytes();
    let sample_len = bytes.len().min(BINARY_SAMPLE_BYTES);
    if sample_len == 0 {
        return false;
    }
    let sample = &bytes[..sample_len];
    let mut non_print: usize = 0;
    for &b in sample {
        let is_print = matches!(b, b'\t' | b'\n' | b'\r') || (0x20..=0x7E).contains(&b) || b >= 0x80;
        if !is_print {
            non_print += 1;
        }
    }
    let ratio = non_print as f32 / sample_len as f32;
    ratio > BINARY_RATIO_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_threshold_passes_through_borrowed() {
        let text = "hello\nworld";
        let r = compress(text, 1024);
        assert!(!r.was_compressed);
        assert_eq!(r.lines_dropped, 0);
        assert_eq!(r.original_bytes, text.len());
        assert_eq!(r.output_bytes, text.len());
        // 必须是 Borrowed —— 不发生分配
        assert!(matches!(r.compressed, Cow::Borrowed(_)));
        assert_eq!(r.compressed.as_ref(), text);
    }

    #[test]
    fn long_output_keeps_head_50_and_tail_20() {
        // 200 行，每行 100 字节 → 总 ~20KB，远超 8KB 阈值
        let lines: Vec<String> = (0..200).map(|i| format!("LINE-{:0>3}-{}", i, "x".repeat(80))).collect();
        let text = lines.join("\n");
        let r = compress(&text, 8 * 1024);
        assert!(r.was_compressed, "20KB 应触发截断");
        assert_eq!(r.lines_dropped, 200 - 50 - 20);
        let out = r.compressed.as_ref();
        // 头 50 行：必须能找到 LINE-049
        assert!(out.contains("LINE-049"), "head 50 应包含第 50 行");
        // 不应包含第 50 行之后中间的行（如 LINE-099）
        assert!(!out.contains("LINE-099"), "中间行应被丢弃");
        // 尾 20 行：必须能找到 LINE-199（最后一行）和 LINE-180
        assert!(out.contains("LINE-199"), "tail 20 应含最后一行");
        assert!(out.contains("LINE-180"), "tail 20 应覆盖到 LINE-180");
        // 截断说明
        assert!(out.contains("truncated by codex-switcher"));
        assert!(r.output_bytes < r.original_bytes);
    }

    #[test]
    fn binary_content_is_not_truncated() {
        // 构造 16KB 全 0x00 的内容（绝对二进制） → 必须原样返回
        let mut bin = String::new();
        // 用 \u{0001} 这类不可打印字符（合法 UTF-8 但非可打印）
        for _ in 0..16 * 1024 {
            bin.push('\u{0001}');
        }
        let r = compress(&bin, 1024);
        assert!(!r.was_compressed, "二进制内容必须原样透传");
        assert_eq!(r.lines_dropped, 0);
        assert_eq!(r.output_bytes, r.original_bytes);
    }

    #[test]
    fn short_text_with_huge_single_line_uses_fallback_split() {
        // 30 行，但其中一行非常长 → 总字节超阈值，行数 < 70
        // 0..29 共 29 行（L0..L28），insert 1 行 → 共 30 行，末行 L28
        let mut lines: Vec<String> = (0..29).map(|i| format!("L{}", i)).collect();
        lines.insert(15, "X".repeat(20 * 1024)); // 中间一行 20KB
        let text = lines.join("\n");
        let r = compress(&text, 4 * 1024);
        assert!(r.was_compressed, "总字节超阈值必须截断");
        // 30 行：head = 15, tail = 7
        // dropped = 30 - 15 - 7 = 8
        assert_eq!(r.lines_dropped, 8);
        let out = r.compressed.as_ref();
        // 第 15 行（巨长行）应该被丢弃
        assert!(!out.contains(&"X".repeat(1024)), "巨长行应被丢弃");
        // 头部 L0..L13 在；L14 是 head 第 15 行（index 0..15 → L0..L13 + 第14 个元素是 X 巨长行了？）
        // 不对：lines 排布 = [L0, L1, ..., L14, X..X, L15, ..., L28]
        // index 14 是 L14，index 15 是 X..X，index 16 是 L15。
        // head_n=15 → 取 index 0..15 = L0..L14；正好把 X..X 切到了 dropped。
        assert!(out.contains("L0\n"), "head 应含 L0");
        assert!(out.contains("L14\n"), "head 应含 L14");
        // tail_n=7 → 取 index 23..30 = L22..L28
        assert!(out.contains("L28"), "tail 必须含最后一行 L28");
        assert!(out.contains("L22"), "tail 应含 L22");
    }
}
