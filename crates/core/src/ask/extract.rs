//! EXTRACT operation（结构化抽取，spec 十六 CASE 1「我的简历里面有哪些项目」）。
//!
//! EXTRACT 不是独立 intent：它搭乘 DOCUMENT_QA / LIBRARY_QA / MULTI_DOCUMENT_QA
//! 的检索管线（Query Parser 输出 `intent: document_qa, operation: extract`），
//! 只是把「已验证的 grounded 回答」在展示层重组为「条目 + 每项证据」列表。
//!
//! 纪律：模型只决定**条目内容**（JSON Schema 约束输出）；每条目的引用证据由
//! 编排层用 `longest_common_substr_len` 确定性对齐到检索验证过的真实 chunk
//! 原文（达到 `EXTRACT_MATCH_MIN_LEN` 字符才算命中），绝不接受模型自报的
//! 证据编号。抽取生成失败或空条目 → 原样保留已验证的普通回答（绝不 crash、
//! 不劣化既有结果）。
//!
//! 本模块只含确定性纯函数（prompt / JSON Schema / 宽容解析 / 子串对齐），
//! 编排在桌面应用层（restructure_as_extract）。

use serde::{Deserialize, Serialize};

/// 结构化列表的条数上限（防止模型输出超长清单）。
pub const EXTRACT_MAX_ITEMS: usize = 12;
/// 单条材料（事实句 + 证据摘引）进 prompt 的最大字符数。
pub const EXTRACT_MATERIAL_CHARS: usize = 600;
/// 条目文本与证据原文的最长公共子串达到该长度才算确定性命中。
pub const EXTRACT_MATCH_MIN_LEN: usize = 6;

/// 单个抽取条目（模型输出）。`evidence` 只作展示摘引，不作为引用证据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractItem {
    pub item: String,
    #[serde(default)]
    pub evidence: String,
}

/// 结构化抽取结果（模型输出）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractResults {
    pub items: Vec<ExtractItem>,
}

/// 抽取输出的 JSON Schema（llama.cpp 侧约束解码）。
pub fn extract_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["items"],
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["item"],
                    "properties": {
                        "item": {"type": "string", "maxLength": 200},
                        "evidence": {"type": "string", "maxLength": 300}
                    }
                }
            }
        }
    })
}

/// 构建抽取 prompt。materials 为已核验的事实句 + 证据原文摘引
/// （编排层负责按 `EXTRACT_MATERIAL_CHARS` 截断）。
pub fn extract_prompt(question: &str, materials: &[String]) -> (String, String) {
    let system = "你是翻翻的结构化抽取助手。用户的问题要求从本地资料中抽取一份条目清单（如项目列表、技能列表、条款、日期、联系方式等）。只从提供的材料中抽取，每条目只保留材料里出现的内容；不得编造、不得合并材料中不存在的细节。只输出规定 JSON，不要输出 Markdown、代码块或解释。"
        .into();
    let mut user = String::new();
    user.push_str(&format!("【抽取问题】{question}\n\n【材料】\n"));
    if materials.is_empty() {
        user.push_str("（无材料）\n");
    } else {
        for (index, material) in materials.iter().enumerate() {
            user.push_str(&format!("[M{}] {}\n", index + 1, material));
        }
    }
    user.push_str(
        "\n请抽取材料中与问题相关的全部条目，输出 items 数组；每条目给出 item（条目内容）与 evidence（材料中支持该条目的最短原文片段）。",
    );
    (system, user)
}

/// 宽容解析抽取结果；解析失败/空条目返回 None（调用方保持已验证回答）。
pub fn parse_extract_results(raw: &str) -> Option<ExtractResults> {
    let cleaned = raw
        .trim()
        .strip_prefix("```json")
        .or_else(|| raw.trim().strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(raw.trim());
    let parsed: Option<serde_json::Value> = serde_json::from_str(cleaned).ok();
    let value = match parsed {
        Some(value) if value.is_object() => value,
        _ => tolerant_json_object(raw)?,
    };
    parse_extract_results_lenient(&value)
}

/// 逐条目宽容解析：缺失/非法条目丢弃，其余保留（宁缺毋滥）。
fn parse_extract_results_lenient(value: &serde_json::Value) -> Option<ExtractResults> {
    let items = value
        .get("items")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let item_text = item
                        .get("item")
                        .and_then(|text| text.as_str())
                        .map(str::trim)
                        .filter(|text| !text.is_empty())?;
                    Some(ExtractItem {
                        item: item_text.to_owned(),
                        evidence: item
                            .get("evidence")
                            .and_then(|evidence| evidence.as_str())
                            .map(str::trim)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if items.is_empty() {
        return None;
    }
    Some(ExtractResults { items })
}

/// 容错 JSON 对象提取：剥 ```json fence → 直接 parse → 平衡花括号回退。
/// 与 document_summary / compare 的 tolerant_json_object 同语义（模型输出噪声宽容）。
fn tolerant_json_object(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        if let Some(stripped) = rest.strip_suffix("```") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(stripped.trim())
                && value.is_object()
            {
                return Some(value);
            }
        }
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    // 平衡花括号提取（模型常在 JSON 前后夹带说明文字）。
    let mut depth = 0i32;
    let mut start = None;
    for (index, ch) in trimmed.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = start
                {
                    let candidate = &trimmed[start..=index];
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate)
                        && value.is_object()
                    {
                        return Some(value);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// 最长公共子串长度（按 Unicode 字符计，忽略空白）。用于把模型抽取出的
/// 条目确定性对齐到真实证据 chunk——对齐只看逐字符重叠，不信任模型编号。
pub fn longest_common_substr_len(left: &str, right: &str) -> usize {
    let left = left.chars().filter(|ch| !ch.is_whitespace()).collect::<Vec<_>>();
    let right = right.chars().filter(|ch| !ch.is_whitespace()).collect::<Vec<_>>();
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let mut dp = vec![0usize; right.len()];
    let mut best = 0usize;
    for i in 0..left.len() {
        for j in (0..right.len()).rev() {
            if left[i] == right[j] {
                dp[j] = if i == 0 || j == 0 { 1 } else { dp[j - 1] + 1 };
                best = best.max(dp[j]);
            } else {
                dp[j] = 0;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_extract_results() {
        let raw = r#"{"items":[{"item":"项目一：大模型应用开发","evidence":"参与大模型应用开发"},
            {"item":"项目二：LangGraph 知识库问答","evidence":"基于 LangGraph 构建知识库问答"}]}"#;
        let parsed = parse_extract_results(raw).expect("parses");
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].item, "项目一：大模型应用开发");
        assert_eq!(parsed.items[0].evidence, "参与大模型应用开发");
        assert_eq!(parsed.items[1].item, "项目二：LangGraph 知识库问答");
    }

    #[test]
    fn tolerates_fences_and_noise() {
        let raw = "好的，抽取结果如下：\n```json\n{\"items\":[{\"item\":\"技能一\",\"evidence\":\"原文\"}]}\n```\n完";
        let parsed = parse_extract_results(raw).expect("fenced parse");
        assert_eq!(parsed.items[0].item, "技能一");
    }

    #[test]
    fn drops_invalid_items_but_keeps_valid_ones() {
        // 缺 item 的条目丢弃；缺 evidence 的条目保留空摘引
        let raw = r#"{"items":[{"item":"有效条目"},{}],"extra":1}"#;
        let parsed = parse_extract_results(raw).expect("tolerant parse");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].item, "有效条目");
        assert_eq!(parsed.items[0].evidence, "");
    }

    #[test]
    fn rejects_empty_results() {
        assert!(parse_extract_results("{}").is_none());
        assert!(parse_extract_results(r#"{"items":[]}"#).is_none());
        assert!(parse_extract_results(r#"{"items":[{"item":"  "}]}"#).is_none());
        assert!(parse_extract_results("not json").is_none());
    }

    #[test]
    fn schema_requires_items() {
        let schema = extract_schema();
        let required: Vec<_> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(required.contains(&"items"));
    }

    #[test]
    fn prompt_injects_question_and_materials() {
        let (system, user) = extract_prompt(
            "我的简历里面有哪些项目？",
            &["项目经历：大模型应用开发".to_owned(), "项目经历：知识库问答".to_owned()],
        );
        assert!(system.contains("结构化抽取助手"));
        assert!(user.contains("我的简历里面有哪些项目？"));
        assert!(user.contains("[M1] 项目经历：大模型应用开发"));
        assert!(user.contains("[M2] 项目经历：知识库问答"));
    }

    #[test]
    fn empty_materials_still_produce_usable_prompt() {
        let (_, user) = extract_prompt("有哪些技能？", &[]);
        assert!(user.contains("（无材料）"));
    }

    #[test]
    fn longest_common_substr_basics() {
        assert_eq!(longest_common_substr_len("abc", "bc"), 2);
        assert_eq!(longest_common_substr_len("abc", "xyz"), 0);
        assert_eq!(longest_common_substr_len("大模型应用开发", "大模型应用开发"), 7);
        assert_eq!(longest_common_substr_len("", "abc"), 0);
    }

    #[test]
    fn longest_common_substr_ignores_whitespace() {
        // 换行/空格不影响对齐（证据摘引常有行内换行）
        assert_eq!(longest_common_substr_len("大 模型应用", "大模型应用开发"), 5);
        assert_eq!(longest_common_substr_len("LangGraph 知识库", "LangGraph知识库问答"), 12);
    }
}
