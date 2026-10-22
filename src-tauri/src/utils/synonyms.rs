//! 同义词表与查询扩展（docs/10 §5.4、A2b）。
//!
//! 和 worker 的 VLM 标签规范化共用同一个文件 `ai-worker/src/vlm/synonyms.json`（编译时读入）：
//! 打标时"小猫""猫咪"归一成"猫"、原词进 keywords；搜索时查询里出现某组里的任一说法，同组的
//! 其他说法也参与标签和全文通道的检索（"小猫"也能找到只打了"猫"标签的图）。
//!
//! 单字说法（猫、狗、山……）只在查询本身就是这个字、或空格隔开的一个词时才算出现，
//! 免得"山东""湖南"这类词被误扩展。

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::utils::text_tokens::{self, SynonymTerms, Tokens};

const TABLE: &str = include_str!("../../../ai-worker/src/vlm/synonyms.json");

/// 每组：规范说法在前，后面是同义说法（都已 NFKC + 小写）。
fn groups() -> &'static [Vec<String>] {
    static GROUPS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        let Ok(serde_json::Value::Object(table)) = serde_json::from_str::<serde_json::Value>(TABLE)
        else {
            tracing::warn!("synonyms.json is not a JSON object; query expansion disabled");
            return Vec::new();
        };
        table
            .into_iter()
            .filter(|(canonical, _)| !canonical.starts_with('_'))
            .map(|(canonical, variants)| {
                let mut group = vec![text_tokens::normalize(&canonical)];
                if let serde_json::Value::Array(variants) = variants {
                    group.extend(
                        variants
                            .iter()
                            .filter_map(|variant| variant.as_str())
                            .map(text_tokens::normalize),
                    );
                }
                group
            })
            .collect()
    })
}

/// 查询里出现的一个同义说法，以及同组的其他说法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub matched: String,
    pub alternatives: Vec<String>,
    /// 这个说法在规范化查询里出现的字符区间 `[start, end)`。
    pub spans: Vec<(usize, usize)>,
}

/// 查询扩展：对查询里出现的每个同义说法（长的优先，已被更长说法覆盖的位置不再重复匹配），
/// 给出同组的其他说法。`query` 会先做 NFKC + 小写。
pub fn expansions(query: &str) -> Vec<Expansion> {
    let normalized = text_tokens::normalize(query);
    let words = normalized.split_whitespace().collect::<HashSet<_>>();
    let chars = normalized.chars().collect::<Vec<_>>();
    let mut covered = vec![false; chars.len()];
    let mut candidates = groups()
        .iter()
        .flat_map(|group| group.iter().map(move |member| (member, group)))
        .collect::<Vec<_>>();
    // 长说法优先："小孩子"先于"小孩"。
    candidates.sort_by_key(|(member, _)| std::cmp::Reverse(member.chars().count()));
    let mut out: Vec<Expansion> = Vec::new();
    for (member, group) in candidates {
        let member_chars = member.chars().collect::<Vec<_>>();
        if member_chars.is_empty() || member_chars.len() > chars.len() {
            continue;
        }
        if member_chars.len() == 1 && !words.contains(member.as_str()) {
            continue;
        }
        let mut spans = Vec::new();
        for start in 0..=(chars.len() - member_chars.len()) {
            let end = start + member_chars.len();
            if chars[start..end] == *member_chars && !covered[start..end].iter().any(|c| *c) {
                covered[start..end].iter_mut().for_each(|c| *c = true);
                spans.push((start, end));
            }
        }
        if spans.is_empty() {
            continue;
        }
        if let Some(existing) = out
            .iter_mut()
            .find(|expansion| expansion.matched == *member)
        {
            existing.spans.extend(spans);
        } else {
            out.push(Expansion {
                matched: member.clone(),
                alternatives: group
                    .iter()
                    .filter(|other| *other != member)
                    .cloned()
                    .collect(),
                spans,
            });
        }
    }
    out
}

/// 给查询切出的词附上同义扩展（全文通道据此把同义说法组成 OR 组）。
///
/// 每种说法一支：把查询里的这个说法换成它再切词，取与这个位置有交集的词——落在里面的，以及跨在
/// 边界上的双字词，也就是直接输入这种说法时 AND 会要求的、和这个位置有关的那些词。所以：
/// - 原说法那一支都是查询本来的词，扩展前能命中的文档扩展后一定还能命中（不是把这个说法单独
///   切一遍："QR码扫描"里切不出单字"码"）；
/// - 换了说法的几支也保留与前后文的相邻约束："游戏截图"换成"截屏"要求"戏截""截屏"，而不是
///   "游戏"和"截屏"出现在任意位置。
///
/// 夹在两个同义说法之间的跨界双字词（"聊天记录截图"里的"录截"）在不在取决于两边各用哪种说法，
/// 不再要求。
pub fn expand_tokens(mut tokens: Tokens, query: &str) -> Tokens {
    let chars = text_tokens::normalize(query).chars().collect::<Vec<_>>();
    let found = expansions(query);
    tokens.synonyms = found
        .iter()
        .enumerate()
        .filter_map(|(index, expansion)| {
            let others = found
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .flat_map(|(_, other)| other.spans.iter().copied())
                .collect::<Vec<_>>();
            let (matched_terms, dropped_terms) = region_terms(&chars, &expansion.spans, &others);
            let alternatives = expansion
                .alternatives
                .iter()
                .filter_map(|alternative| {
                    let swapped = substitute(&chars, &expansion.spans, &others, alternative);
                    let (terms, _) = region_terms(&swapped.text, &swapped.spans, &swapped.others);
                    (!terms.is_empty()).then(|| (alternative.clone(), terms))
                })
                .collect::<Vec<_>>();
            (!matched_terms.is_empty() && !alternatives.is_empty()).then(|| SynonymTerms {
                matched: expansion.matched.clone(),
                matched_terms,
                dropped_terms,
                alternatives,
            })
        })
        .collect();
    tokens
}

/// 与 `spans` 有交集的词：要求的（落在里面的、跨在边界上的），以及不要求的（同时碰到 `others`
/// 里别的同义说法的跨界双字词）。各自去重并保持出现顺序。
fn region_terms(
    chars: &[char],
    spans: &[(usize, usize)],
    others: &[(usize, usize)],
) -> (Vec<String>, Vec<String>) {
    let touches = |ranges: &[(usize, usize)], start: usize, end: usize| {
        ranges.iter().any(|&(from, to)| start < to && from < end)
    };
    let mut required = Vec::new();
    let mut dropped = Vec::new();
    for (term, start, end) in text_tokens::term_spans(chars) {
        if !touches(spans, start, end) {
            continue;
        }
        let bucket = if touches(others, start, end) {
            &mut dropped
        } else {
            &mut required
        };
        if !bucket.contains(&term) {
            bucket.push(term);
        }
    }
    (required, dropped)
}

/// 换了一种说法之后的查询。
struct Substituted {
    text: Vec<char>,
    /// 换上去的说法在新文本里的位置。
    spans: Vec<(usize, usize)>,
    /// 别的同义说法在新文本里的位置。
    others: Vec<(usize, usize)>,
}

/// 把 `chars` 里 `spans` 位置的说法都换成 `replacement`；`others`（别的同义说法的位置，与
/// `spans` 不重叠）随之平移。
fn substitute(
    chars: &[char],
    spans: &[(usize, usize)],
    others: &[(usize, usize)],
    replacement: &str,
) -> Substituted {
    let replacement = replacement.chars().collect::<Vec<_>>();
    let mut sorted = spans.to_vec();
    sorted.sort_unstable();
    let mut text = Vec::with_capacity(chars.len() + replacement.len() * sorted.len());
    let mut placed = Vec::with_capacity(sorted.len());
    // 每段替换在旧文本和新文本里的结尾位置，用来换算后面的位置。
    let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    let mut cursor = 0;
    for (start, end) in sorted {
        text.extend_from_slice(&chars[cursor..start]);
        let new_start = text.len();
        text.extend_from_slice(&replacement);
        placed.push((new_start, text.len()));
        boundaries.push((end, text.len()));
        cursor = end;
    }
    text.extend_from_slice(&chars[cursor..]);
    let shift = |position: usize| {
        boundaries
            .iter()
            .rev()
            .find(|(old_end, _)| *old_end <= position)
            .map_or(position, |(old_end, new_end)| position - old_end + new_end)
    };
    let others = others
        .iter()
        .map(|&(start, end)| (shift(start), shift(end)))
        .collect();
    Substituted {
        text,
        spans: placed,
        others,
    }
}

/// 某个说法所在组的全部说法（含自己）；不在表里时返回 `None`。
pub fn group_of(term: &str) -> Option<&'static [String]> {
    let normalized = text_tokens::normalize(term);
    groups()
        .iter()
        .find(|group| group.contains(&normalized))
        .map(Vec::as_slice)
}
