//! 模型化 Document Understanding（建库侧）纯逻辑层。
//!
//! 只承载「强类型 schema + (system,user) prompt + serde_json 解析 + 确定性兜底」
//! 的纯函数，不在这里调用任何模型、不做 IO、也不直接改写 `DocumentProfile`。
//! 生产链调用方负责：选择模型 → 用 `document_understanding_prompt` 组 prompt →
//! 解析模型输出 → 失败时回退 `fallback_document_understanding`，保证索引主链不被阻塞。

use crate::contracts::DocumentType;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// prompt 中代表性正文/摘要的字符上限（折叠空白后按字符截断）。
const MAX_SUMMARY_CHARS: usize = 4_000;

/// 兜底派生的 topics 数量上限，避免标题/关键词过多导致画像过重。
const MAX_FALLBACK_TOPICS: usize = 8;

/// 文档理解的结构化输出：建库时为文档画像生成的受约束语义字段。
///
/// 各字段均带 `#[serde(default)]`，保证模型缺字段时解析宽容、可降级。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentUnderstanding {
    /// 文档目的/用途：这份文档「用来做什么 / 记录什么」，一句中文。
    #[serde(default)]
    pub purpose: String,
    /// 主题列表：这份文档「讲了哪些主题」，语义化短语。
    #[serde(default)]
    pub topics: Vec<String>,
    /// 整体置信度，取值 0.0..=1.0；走确定性兜底时为 None。
    #[serde(default)]
    pub confidence: Option<f32>,
}

/// 返回约束模型结构化输出的 JSON Schema（llama.cpp 侧约束解码）。
///
/// 入参：无。
/// 出参：`serde_json::Value`，声明 `purpose`/`topics` 为必填、`confidence` 为可选 number|null。
pub fn document_understanding_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["purpose", "topics"],
        "properties": {
            "purpose": {
                "type": "string",
                "maxLength": 200,
                "description": "文档用途/记录内容（一句中文）"
            },
            "topics": {
                "type": "array",
                "maxItems": 8,
                "items": {"type": "string", "maxLength": 50},
                "description": "文档涉及的主题列表（简短中文短语）"
            },
            "confidence": {
                "type": ["number", "null"],
                "minimum": 0.0,
                "maximum": 1.0
            }
        }
    })
}

/// 构建 Document Understanding 的 (system, user) prompt。
///
/// 入参：
/// - `file_name`：文件名；
/// - `title`：文档标题；
/// - `section_titles`：章节标题列表；
/// - `summary`：代表性正文/摘要（会折叠空白后截断，避免超长）。
///
/// 出参：`(system, user)` 二元组；system 声明角色与「只用输入做受约束字段提取、
/// 不幻想、只输出 JSON」的约束，user 拼入文件名、标题、章节标题与摘要。
pub fn document_understanding_prompt(
    file_name: &str,
    title: &str,
    section_titles: &[String],
    summary: &str,
) -> (String, String) {
    let system = "你是翻翻的「文档理解」模块，在文档入库时为文档画像生成受约束的语义字段。\
你只依据输入中提供的文件名、标题、章节标题与代表性正文做字段提取，不得幻想或补全文档中不存在的内容，\
不得输出与文档无关的信息，不得回答任何问题。思考已关闭（thinking=false），只输出一个 JSON 对象。"
        .to_string();

    let file_name = if file_name.trim().is_empty() {
        "（未知）".to_string()
    } else {
        file_name.trim().to_string()
    };
    let title = if title.trim().is_empty() {
        "（无标题）".to_string()
    } else {
        title.trim().to_string()
    };

    let sections = if section_titles.is_empty() {
        "（无章节标题）".to_string()
    } else {
        section_titles
            .iter()
            .map(|section| section.trim())
            .filter(|section| !section.is_empty())
            .map(|section| format!("- {section}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let summary = compact_text(summary, MAX_SUMMARY_CHARS);
    let summary = if summary.is_empty() { "（无代表性正文）" } else { &summary };

    let user = format!(
        r#"【文档信息】
文件名：{file_name}
标题：{title}
章节标题：
{sections}

代表性正文/摘要：
{summary}

【任务】基于以上信息，输出一个 JSON 对象，字段说明如下：
- purpose（string，必填）：这份文档「用来做什么 / 记录什么」，用一句中文概括。
- topics（array of string，必填）：这份文档「讲了哪些主题」，每个主题是简短中文短语，去重，最多 8 个。
- confidence（number 或 null）：你对上述判断的整体置信度，取值 0.0 到 1.0。

只输出 JSON，不要任何解释、前后缀或 Markdown 代码围栏。"#
    );

    (system, user)
}

/// 解析模型输出的 Document Understanding JSON。
///
/// 入参：`raw` 原始模型输出（可能带 markdown 代码围栏或夹带说明文本）。
/// 出参：成功解析得到 `Some(DocumentUnderstanding)`（topics 已去空、去重，
/// 并保留首次出现顺序）；解析失败或不是合法 JSON 时返回 `None`。
pub fn parse_document_understanding(raw: &str) -> Option<DocumentUnderstanding> {
    let cleaned = extract_first_json_object(raw)?;
    let mut understanding = serde_json::from_str::<DocumentUnderstanding>(&cleaned).ok()?;
    understanding.purpose = understanding.purpose.trim().to_string();

    let mut seen = HashSet::new();
    understanding.topics = understanding
        .topics
        .into_iter()
        .map(|topic| topic.trim().to_string())
        .filter(|topic| !topic.is_empty())
        .filter(|topic| seen.insert(topic.clone()))
        .collect();

    Some(understanding)
}

/// 确定性兜底：在无模型 / 模型不可用 / 输出非法时派生语义字段（纯函数、无 IO）。
///
/// 入参：
/// - `title`：文档标题（当前兜底路径不依赖，保留用于将来增强）；
/// - `section_titles`：章节标题列表；
/// - `keywords`：关键词列表；
/// - `document_type`：可选的文档类型。
///
/// 出参：`DocumentUnderstanding`。`purpose` 由 `document_type` 映射为中文用途
/// （`None` 时使用通用表述）；`topics` 由 `section_titles` + `keywords` 去空去重合并，
/// 截断到上限；`confidence` 置 `None` 表示走兜底。
pub fn fallback_document_understanding(
    title: &str,
    section_titles: &[String],
    keywords: &[String],
    document_type: Option<DocumentType>,
) -> DocumentUnderstanding {
    // title 参数按契约保留，兜底派生当前不依赖标题，标记避免未使用告警。
    let _ = title;
    let purpose = document_type_purpose(document_type);

    let mut seen = HashSet::new();
    let mut topics = Vec::new();
    for topic in section_titles.iter().chain(keywords.iter()) {
        let topic = topic.trim();
        if topic.is_empty() || !seen.insert(topic.to_string()) {
            continue;
        }
        topics.push(topic.to_string());
        if topics.len() >= MAX_FALLBACK_TOPICS {
            break;
        }
    }

    DocumentUnderstanding {
        purpose,
        topics,
        confidence: None,
    }
}

/// 由文档类型映射为中文用途表述；`None` 或 `Other` 时使用通用表述。
fn document_type_purpose(document_type: Option<DocumentType>) -> String {
    match document_type {
        Some(DocumentType::Resume) => "记录个人简历与求职经历",
        Some(DocumentType::Contract) => "记录合同条款与各方约定",
        Some(DocumentType::Invoice) => "记录发票与费用明细",
        Some(DocumentType::Paper) => "记录论文与学术研究内容",
        Some(DocumentType::ProjectDocument) => "记录项目方案与工程资料",
        Some(DocumentType::Meeting) => "记录会议讨论与纪要",
        Some(DocumentType::LearningMaterial) => "记录学习资料与课程内容",
        Some(DocumentType::Certificate) => "记录证书与资质证明",
        Some(DocumentType::Report) => "记录报告内容与结论",
        Some(DocumentType::Spreadsheet) => "记录表格数据与统计",
        Some(DocumentType::Other) | None => "文档内容记录",
    }
    .to_string()
}

/// 折叠空白后按字符数截断的确定性压缩（prompt 摘要截断用）。
fn compact_text(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(limit).collect()
}

/// 去掉 markdown 代码围栏，若整段不是合法 JSON 再从文本中抠出首个 `{ ... }` 平衡块。
///
/// 入参：原始模型输出文本。
/// 出参：一个尽量干净的 JSON 字符串片段；找不到可解析的 JSON 对象时返回 `None`。
fn extract_first_json_object(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed).trim();

    if serde_json::from_str::<serde_json::Value>(trimmed).map_or(false, |value| value.is_object()) {
        return Some(trimmed.to_string());
    }

    let start = trimmed.find('{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in trimmed[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + index + ch.len_utf8();
                    return Some(trimmed[start..end].to_string());
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

    /// 校验：带 markdown 围栏的合法 JSON 可解析且 topics 去重，垃圾文本返回 None。
    #[test]
    fn parse_handles_fenced_json_and_rejects_garbage() {
        let raw = "```json\n{\"purpose\":\"记录个人简历\",\"topics\":[\"项目\",\"技能\",\"项目\"],\"confidence\":0.9}\n```";
        let parsed = parse_document_understanding(raw).expect("应能解析围栏内的 JSON");
        assert_eq!(parsed.purpose, "记录个人简历");
        assert_eq!(parsed.topics, vec!["项目".to_string(), "技能".to_string()]);
        assert_eq!(parsed.confidence, Some(0.9));

        assert!(parse_document_understanding("这不是 JSON").is_none());
        assert!(parse_document_understanding("").is_none());
    }

    /// 校验：兜底能从文档类型派生合理 purpose，topics 非空且去重，confidence 为 None。
    #[test]
    fn fallback_derives_purpose_and_deduplicated_topics() {
        let section_titles = vec![
            "工作经历".to_string(),
            "项目".to_string(),
            "教育背景".to_string(),
            String::new(),
            "项目".to_string(),
        ];
        let keywords = vec![
            "Rust".to_string(),
            "项目".to_string(),
            "LangGraph".to_string(),
        ];

        let result = fallback_document_understanding(
            "我的简历.docx",
            &section_titles,
            &keywords,
            Some(DocumentType::Resume),
        );

        assert!(result.purpose.contains("简历"));
        assert!(!result.topics.is_empty());
        assert!(result.topics.len() <= MAX_FALLBACK_TOPICS);
        // topics 去重且保留顺序：去重后长度不变证明无重复项。
        let mut unique = result.topics.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), result.topics.len());
        assert!(result.confidence.is_none());
    }
}