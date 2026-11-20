//! 终端编码：会话级增量解码器。
//!
//! 每个 term 会话的 reader task 持有一个增量 `encoding_rs::Decoder`：
//! SSH channel 收到原始字节 → `decode_to_string` → String → bytes → emit，
//! 避免多字节字符被块边界截断。非 UTF-8 编码在 Rust 侧转码后以 UTF-8 字节
//! 输出（term://data 的 data 是这些字节的 base64，xterm 解码后直接写字节无损）。
use encoding_rs::{Decoder, Encoding};

/// 会话编码状态：标签名 + 增量解码器。切换编码时整体替换，
/// 不动已输出内容、不清屏、不重建通道。
pub struct SessionEncoding {
    pub name: String,
    pub decoder: Decoder,
}

impl SessionEncoding {
    /// 创建默认的 UTF-8 会话解码器（不吞 BOM）。
    pub fn default_utf8() -> Self {
        Self::new("UTF-8").expect("UTF-8 是内置编码")
    }

    /// 按 13 项固定列表映射到 encoding_rs::Encoding。
    /// 注意：按 WHATWG Encoding Standard，"GBK" 与 "ISO-8859-1" 分别是
    /// gb18030 与 windows-1252 编码的 label；"UTF-8-BOM" 用 UTF_8 并做
    /// BOM removal（encoding_rs 没有 replace_bom，BOM 处理以 API 为准）。
    pub fn new(name: &str) -> Option<Self> {
        let (encoding, bom_removal) = match name {
            "UTF-8" => (encoding_rs::UTF_8, false),
            "UTF-8-BOM" => (encoding_rs::UTF_8, true),
            "UTF-16LE" => (encoding_rs::UTF_16LE, false),
            "UTF-16BE" => (encoding_rs::UTF_16BE, false),
            "GB18030" | "GBK" => (encoding_rs::GB18030, false),
            "Big5" => (encoding_rs::BIG5, false),
            "Shift_JIS" => (encoding_rs::SHIFT_JIS, false),
            "EUC-JP" => (encoding_rs::EUC_JP, false),
            "EUC-KR" => (encoding_rs::EUC_KR, false),
            "ISO-8859-1" | "Windows-1252" => (encoding_rs::WINDOWS_1252, false),
            "KOI8-R" => (encoding_rs::KOI8_R, false),
            _ => return None,
        };
        let decoder = if bom_removal {
            encoding.new_decoder_with_bom_removal()
        } else {
            encoding.new_decoder_without_bom_handling()
        };
        Some(Self {
            name: name.to_string(),
            decoder,
        })
    }
}

/// 按 UTF-8 无损取 encoding_rs 编码名（调试/日志用）。
pub fn encoding_name(encoding: &'static Encoding) -> &'static str {
    encoding.name()
}

/// 增量解码一块字节流：调用方持有跨块的 `Decoder`，
/// 多字节字符跨块时由 Decoder 内部状态拼接，不逐块独立 decode。
/// 返回 UTF-8 字节（非 UTF-8 编码在 Rust 侧转码后输出）。
///
/// SSH channel reader 与本地 PTY reader 共用，保证两种会话的编码行为一致。
pub fn decode_chunk(enc: &mut SessionEncoding, data: &[u8]) -> Vec<u8> {
    let mut out = String::with_capacity(data.len() * 4);
    // 第三个参数 last=false：字节流还在继续，流不是终止状态。
    let (_, _, _) = enc.decoder.decode_to_string(data, &mut out, false);
    out.into_bytes()
}

/// 流结束时收尾解码——把仍挂在 Decoder 状态里的半截多字节序列
/// 按 U+FFFD 输出（encoding_rs 在 last=true 时把残留字节替换输出）。
/// 不调用它的话，会话最后一行里被截断的字符会静默丢失。
pub fn finish_decode(enc: &mut SessionEncoding) -> Vec<u8> {
    let mut out = String::with_capacity(8);
    let (_, _, _) = enc.decoder.decode_to_string(b"", &mut out, true);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_mapping() {
        assert!(SessionEncoding::new("UTF-8").is_some());
        assert!(SessionEncoding::new("UTF-8-BOM").is_some());
        assert!(SessionEncoding::new("GBK").is_some());
        assert!(SessionEncoding::new("KOI8-R").is_some());
        assert!(
            SessionEncoding::new("iso-8859-1").is_none(),
            "固定列表精确匹配，不做大小写归一"
        );
        assert!(SessionEncoding::new("auto-detect").is_none());
    }

    /// GB18030「钌」字节序列跨块：增量解码器必须拼接，不能逐块独立解码。
    #[test]
    fn gb18030_multibyte_across_chunks() {
        let mut dec = SessionEncoding::new("GB18030").unwrap();
        let full = encoding_rs::GB18030.encode("钌").0.into_owned();
        assert!(full.len() >= 2);
        let (a, b) = full.split_at(full.len() - 1); // 末字节留给第二块
                                                    // decode_to_string 以 String 的 capacity 为输出上限且不扩容，必须预留
        let mut out = String::with_capacity(full.len() * 4);
        let (_, _, _) = dec.decoder.decode_to_string(a, &mut out, false);
        let (_, _, _) = dec.decoder.decode_to_string(b, &mut out, true);
        assert_eq!(out, "钌");
    }

    /// UTF-16LE 字节序列跨块（2 字节单元被从中间切开）。
    #[test]
    fn utf16le_multibyte_across_chunks() {
        let mut dec = SessionEncoding::new("UTF-16LE").unwrap();
        let text = "AB钌";
        let mut full = Vec::new();
        for unit in text.encode_utf16() {
            full.extend_from_slice(&unit.to_le_bytes());
        }
        let (a, b) = full.split_at(full.len() - 1); // 末字节留给第二块
        let mut out = String::with_capacity(full.len() * 4);
        let (_, _, _) = dec.decoder.decode_to_string(a, &mut out, false);
        let (_, _, _) = dec.decoder.decode_to_string(b, &mut out, true);
        assert_eq!(out, "AB钌");
    }

    /// UTF-8-BOM：BOM 被移除，正文无损。
    #[test]
    fn utf8_bom_removal() {
        let mut dec = SessionEncoding::new("UTF-8-BOM").unwrap();
        let mut out = String::with_capacity(64);
        let bytes = "hello".as_bytes();
        let (_, read, _) = dec.decoder.decode_to_string(bytes, &mut out, true);
        assert_eq!(read, 5, "decode_to_string 无 BOM 处理时不吞正文");
        let mut dec = SessionEncoding::new("UTF-8-BOM").unwrap();
        let with_bom = [b"\xEF\xBB\xBF", "hello".as_bytes()].concat();
        let mut out2 = String::with_capacity(64);
        let (_, _, _) = dec.decoder.decode_to_string(&with_bom, &mut out2, true);
        assert_eq!(out2, "hello", "BOM removal 应吞掉 UTF-8 BOM");
    }

    /// 流结束时必须收尾解码——挂在 Decoder 里的半截多字节序列要按
    /// U+FFFD 输出，而不是随会话结束静默丢失。
    #[test]
    fn finish_decode_emits_truncated_tail() {
        // GB18030 双字节序列只喂第一字节，然后收尾
        let mut dec = SessionEncoding::new("GB18030").unwrap();
        let full = encoding_rs::GB18030.encode("钌").0.into_owned();
        assert!(full.len() >= 2);
        let head = decode_chunk(&mut dec, &full[..1]);
        assert!(head.is_empty(), "半截序列不应在中途输出");
        let tail = finish_decode(&mut dec);
        assert_eq!(tail, "\u{FFFD}".as_bytes(), "收尾应输出替换字符: {tail:?}");

        // 完整序列 + 收尾：无残留，输出为空
        let mut dec2 = SessionEncoding::new("GB18030").unwrap();
        let _ = decode_chunk(&mut dec2, &full);
        assert!(finish_decode(&mut dec2).is_empty(), "无残留时收尾输出为空");
    }
}
