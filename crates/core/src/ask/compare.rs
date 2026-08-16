//! COMPARE_DOCUMENTS：两篇文档对比（spec 十一.6）。
//!
//! 管线：先解析两个目标文件（Query Parser 输出 `primary target` +
//! `secondary_target`）→ 两侧分别取证（chunk 检索）→ 比较生成 →
//! 逐比较点 claims（每点引用两侧真实 chunk 原文，通过
//! `validate_answer_evidence` 精确引用校验）。
//!
//! 本模块只含确定性纯函数（prompt / JSON Schema / 宽容解析），
//! 编排在桌面应用层（run_compare_answer）。模型输出中的左右证据摘引
//! 只作展示（`left_evidence` / `right_evidence`），证据永远来自
//! 检索到的真实 chunk 原文。

use serde::{Deserialize, Serialize};

/// 单侧取证材料进 prompt 的条数上限。
pub const COMPARE_MATERIAL_ITEMS: usize = 5;
/// 单条取证材料进 prompt 的最大字符数。
pub const COMPARE_MATERIAL_CHARS: usize = 500;
/// 生成失败时的确定性回退：逐点展示条数。
pub const COMPARE_FALLBACK_ITEMS: usize = 4;

/// 单个比较点（相似点）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparePoint {
    pub point: String,
}

/// 单个差异点：模型摘引的左右证据只作展示，不作为引用证据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareDifference {
    pub point: String,
    #[serde(default)]
    pub left_evidence: String,
    #[serde(default)]
    pub right_evidence: String,
}

/// 结构化比较结果（模型输出）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareResults {
    pub similarities: Vec<ComparePoint>,
    pub differences: Vec<CompareDifference>,
    pub conclusion: String,
}

/// 比较生成的输出 JSON Schema（llama.cpp 侧约束解码）。
pub fn compare_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["similarities", "differences", "conclusion"],
        "properties": {
            "similarities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["point"],
                    "properties": {
                        "point": {"type": "string", "maxLength": 400}
                    }
                }
            },
            "differences": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["point", "left_evidence", "right_evidence"],
                    "properties": {
                        "point": {"type": "string", "maxLength": 400},
                        "left_evidence": {"type": "string", "maxLength": 600},
                        "right_evidence": {"type": "string", "maxLength": 600}
                    }
                }
            },
            "conclusion": {"type": "string", "maxLength": 800}
        }
    })
}

/// 构建比较生成 prompt。两侧材料按文件分组传入（先左侧后右侧）。
/// system 说明角色与证据纪律；user 含原问题与两侧材料。
pub fn compare_prompt(
    file_a_name: &str,
    material_a: &[String],
    file_b_name: &str,
    material_b: &[String],
    question: &str,
) -> (String, String) {
    let system = "你是翻翻的「文档对比助手」。你的任务是比较两份本地文档，找出它们的相同点与差异点，并给出结论。必须基于提供给你的材料，不得编造材料里没有的内容；引用材料时直接摘录原文片段。只输出规定 JSON，不要输出 Markdown、代码块或解释。"
        .into();
    let mut user = String::new();
    user.push_str(&format!(
        "【对比问题】{question}\n\n【文档一：{file_a_name}】\n{}\n\n【文档二：{file_b_name}】\n{}\n\n\
         请从材料中提取：\n\
         1. similarities：两份文档的相同点，每点一句话；\n\
         2. differences：两份文档的差异点，每点一句话，并在 left_evidence / right_evidence 中分别摘录左右两侧支持这句话的原文片段；\n\
         3. conclusion：综合结论，不超过 100 字。\n\
         只输出符合 JSON Schema 的对象。",
        material_a.join("\n---\n"),
        material_b.join("\n---\n"),
    ));
    (system, user)
}

/// 宽容解析比较结果；解析失败返回 None（调用方走确定性回退）。
pub fn parse_compare_results(raw: &str) -> Option<CompareResults> {
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
    parse_compare_results_lenient(&value)
}

/// 逐字段宽容解析：缺失/非法条目丢弃，其余保留（宁缺毋滥）。
fn parse_compare_results_lenient(value: &serde_json::Value) -> Option<CompareResults> {
    let similarities = value
        .get("similarities")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("point")
                        .and_then(|point| point.as_str())
                        .map(|point| point.trim())
                        .filter(|point| !point.is_empty())
                        .map(|point| ComparePoint { point: point.to_owned() })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let differences = value
        .get("differences")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let point = item
                        .get("point")
                        .and_then(|point| point.as_str())
                        .map(str::trim)
                        .filter(|point| !point.is_empty())?;
                    Some(CompareDifference {
                        point: point.to_owned(),
                        left_evidence: item
                            .get("left_evidence")
                            .and_then(|evidence| evidence.as_str())
                            .map(str::trim)
                            .unwrap_or_default()
                            .to_owned(),
                        right_evidence: item
                            .get("right_evidence")
                            .and_then(|evidence| evidence.as_str())
                            .map(str::trim)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let conclusion = value
        .get("conclusion")
        .and_then(|conclusion| conclusion.as_str())
        .map(str::trim)
        .filter(|conclusion| !conclusion.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    if similarities.is_empty() && differences.is_empty() && conclusion.is_empty() {
        return None;
    }
    Some(CompareResults { similarities, differences, conclusion })
}

/// 容错 JSON 对象提取：剥 ```json fence → 直接 parse → 平衡花括号回退。
/// 与 document_summary 的 tolerant_json_object 同语义（模型输出噪声宽容）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_compare_results() {
        let raw = r#"{"similarities":[{"point":"两份文档都包含项目经历"}],
            "differences":[{"point":"A 有三个项目，B 只有两个","left_evidence":"项目一、项目二、项目三","right_evidence":"项目一、项目二"}],
            "conclusion":"B 是 A 的精简版"}"#;
        let parsed = parse_compare_results(raw).expect("parses");
        assert_eq!(parsed.similarities.len(), 1);
        assert_eq!(parsed.differences.len(), 1);
        assert_eq!(parsed.differences[0].left_evidence, "项目一、项目二、项目三");
        assert_eq!(parsed.conclusion, "B 是 A 的精简版");
    }

    #[test]
    fn tolerates_fences_and_noise() {
        let raw = "好的，以下是比较结果：\n```json\n{\"similarities\":[{\"point\":\"相同点一\"}],\"differences\":[],\"conclusion\":\"结论\"}\n```\n完";
        let parsed = parse_compare_results(raw).expect("fenced parse");
        assert_eq!(parsed.similarities[0].point, "相同点一");
    }

    #[test]
    fn drops_invalid_items_but_keeps_valid_ones() {
        // 缺 point 的条目丢弃；缺 evidence 的差异点保留空摘引
        let raw = r#"{"similarities":[{"point":"有效点"},{}],
            "differences":[{"point":"","left_evidence":"x"},{"point":"差异点","left_evidence":"左","right_evidence":"右"}],
            "conclusion":"结论"}"#;
        let parsed = parse_compare_results(raw).expect("tolerant parse");
        assert_eq!(parsed.similarities.len(), 1);
        assert_eq!(parsed.differences.len(), 1);
        assert_eq!(parsed.differences[0].point, "差异点");
    }

    #[test]
    fn rejects_empty_results() {
        assert!(parse_compare_results("{}").is_none());
        assert!(parse_compare_results(r#"{"similarities":[],"differences":[],"conclusion":""}"#).is_none());
        assert!(parse_compare_results("not json").is_none());
    }

    #[test]
    fn schema_requires_all_sections() {
        let schema = compare_schema();
        let required: Vec<_> = schema["required"].as_array().unwrap().iter().map(|value| value.as_str().unwrap()).collect();
        assert!(required.contains(&"similarities"));
        assert!(required.contains(&"differences"));
        assert!(required.contains(&"conclusion"));
    }

    #[test]
    fn prompt_names_both_files_and_injects_materials() {
        let (system, user) = compare_prompt(
            "简历第一版",
            &["项目一：A".to_owned(), "项目二：B".to_owned()],
            "简历第二版",
            &["项目一：A（更新）".to_owned()],
            "两个版本有什么不同？",
        );
        assert!(system.contains("文档对比助手"));
        assert!(user.contains("简历第一版"));
        assert!(user.contains("简历第二版"));
        assert!(user.contains("项目二：B"));
        assert!(user.contains("项目一：A（更新）"));
        assert!(user.contains("两个版本有什么不同？"));
    }

    #[test]
    fn compare_point_materials_are_deterministic() {
        // 空材料不 panic、prompt 仍可用（编排层先做空材料防护，这里是兜底）
        let (_, user) = compare_prompt("A", &[], "B", &[], "对比");
        assert!(user.contains("【文档一：A】"));
        assert!(user.contains("【文档二：B】"));
    }
}
