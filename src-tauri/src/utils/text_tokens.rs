//! 检索分词（docs/10 §5.4，A2）：不依赖词典的"CJK 双字 + 单字"切分，与评测原型
//! `scripts/search_eval/common.py::tokenize` 同一规则（原型上描述全文 nDCG@20 0.692）。
//!
//! - 先做 NFKC 规范化（全角转半角等）并转小写；
//! - 连续的 CJK 汉字切成相邻双字（"海边日落" → 海边 / 边日 / 日落），含停用字的双字丢掉；
//! - 每个汉字另出一个单字（停用字除外），写进 `chars` 列、权重很低，用来兜住"猫""车"这类单字查询；
//! - 只有一个汉字的片段（例如标签"猫"、被标点隔开的"狗"）本身就是一个词，也作为词项；
//! - 连续的 ASCII 字母数字是一个词；其余字符（标点、空白、其他文字）都是分隔符。
//!
//! 写入 FTS5 时词与词之间用空格连接，`unicode61` 只按空格切开，双字词保持完整。

use unicode_normalization::UnicodeNormalization;

/// 单字里过于常见、不带检索意义的字；含这些字的双字也丢掉（"的小""了一"之类）。
pub const STOP_CHARS: &str = "的了和与及或是在有着很也都就而又被把从对为这那个一些";

/// 一段文本切出的词。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    /// 双字词、单字片段、ASCII 词（建索引时保留重复，用于词频）。
    pub terms: Vec<String>,
    /// 单字（停用字除外）。
    pub chars: Vec<String>,
}

impl Tokens {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.chars.is_empty()
    }

    /// 去重并保持首次出现的顺序（查询用）。
    pub fn dedup(mut self) -> Self {
        dedup_in_place(&mut self.terms);
        dedup_in_place(&mut self.chars);
        self
    }
}

fn dedup_in_place(items: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

/// NFKC + 小写。
pub fn normalize(text: &str) -> String {
    text.nfkc().collect::<String>().to_lowercase()
}

pub fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

fn is_stop(c: char) -> bool {
    STOP_CHARS.contains(c)
}

/// 切词（建索引用，保留重复）。
pub fn tokenize(text: &str) -> Tokens {
    let normalized = normalize(text);
    let mut tokens = Tokens::default();
    let mut run: Vec<char> = Vec::new();
    let mut ascii = String::new();
    for c in normalized.chars() {
        if is_cjk(c) {
            flush_ascii(&mut ascii, &mut tokens);
            run.push(c);
        } else if c.is_ascii_alphanumeric() {
            flush_cjk(&mut run, &mut tokens);
            ascii.push(c);
        } else {
            flush_cjk(&mut run, &mut tokens);
            flush_ascii(&mut ascii, &mut tokens);
        }
    }
    flush_cjk(&mut run, &mut tokens);
    flush_ascii(&mut ascii, &mut tokens);
    tokens
}

/// 切词（查询用，去重）。
pub fn query_tokens(text: &str) -> Tokens {
    tokenize(text).dedup()
}

fn flush_ascii(word: &mut String, tokens: &mut Tokens) {
    if !word.is_empty() {
        tokens.terms.push(std::mem::take(word));
    }
}

fn flush_cjk(run: &mut Vec<char>, tokens: &mut Tokens) {
    if run.is_empty() {
        return;
    }
    if run.len() == 1 {
        let c = run[0];
        if !is_stop(c) {
            tokens.terms.push(c.to_string());
        }
    }
    for &c in run.iter() {
        if !is_stop(c) {
            tokens.chars.push(c.to_string());
        }
    }
    for pair in run.windows(2) {
        if !is_stop(pair[0]) && !is_stop(pair[1]) {
            tokens.terms.push(pair.iter().collect());
        }
    }
    run.clear();
}

/// FTS5 的字符串字面量：用双引号括起来，内部双引号写两次。
pub fn fts_quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}
