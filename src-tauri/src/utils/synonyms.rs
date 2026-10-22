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
        let mut found = false;
        for start in 0..=(chars.len() - member_chars.len()) {
            let end = start + member_chars.len();
            if chars[start..end] == *member_chars && !covered[start..end].iter().any(|c| *c) {
                covered[start..end].iter_mut().for_each(|c| *c = true);
                found = true;
            }
        }
        if found && !out.iter().any(|expansion| expansion.matched == *member) {
            out.push(Expansion {
                matched: member.clone(),
                alternatives: group
                    .iter()
                    .filter(|other| *other != member)
                    .cloned()
                    .collect(),
            });
        }
    }
    out
}

/// 给查询切出的词附上同义扩展（全文通道据此把同义说法组成 OR 组）。
pub fn expand_tokens(mut tokens: Tokens, query: &str) -> Tokens {
    tokens.synonyms = expansions(query)
        .into_iter()
        .map(|expansion| SynonymTerms {
            matched_terms: text_tokens::query_tokens(&expansion.matched).terms,
            matched: expansion.matched,
            alternatives: expansion
                .alternatives
                .into_iter()
                .map(|alternative| {
                    let terms = text_tokens::query_tokens(&alternative).terms;
                    (alternative, terms)
                })
                .filter(|(_, terms)| !terms.is_empty())
                .collect(),
        })
        .filter(|synonym| !synonym.matched_terms.is_empty() && !synonym.alternatives.is_empty())
        .collect();
    tokens
}

/// 某个说法所在组的全部说法（含自己）；不在表里时返回 `None`。
pub fn group_of(term: &str) -> Option<&'static [String]> {
    let normalized = text_tokens::normalize(term);
    groups()
        .iter()
        .find(|group| group.contains(&normalized))
        .map(Vec::as_slice)
}
