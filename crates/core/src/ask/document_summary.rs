//! DOCUMENT_SUMMARY 管线（纯函数部分）。
//!
//! 约束（spec 十一.3 / 二十 CASE 7）：整文摘要必须按文档结构分层处理——
//! 章节分组 → 逐节摘要 → 聚合总览，禁止只拿 rerank top-3 chunk 生成。
//!
//! 本模块只做确定性逻辑（分组、Prompt、Schema、宽容解析），模型调用与
//! 存储读取由桌面侧编排（generation runtime 与 CatalogStore 不在核心
//! 模块的职责内，与 memory_writer 同构）。

use std::collections::HashMap;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::contracts::SourceLocator;

/// 摘要可引用的最小证据单元：文档里真实存在的一个 chunk。
#[derive(Debug, Clone)]
pub struct SectionChunk {
    pub chunk_id: Uuid,
    pub node_id: Uuid,
    pub revision_id: Uuid,
    pub ordinal: u64,
    pub text: String,
    pub locator: SourceLocator,
}

/// 按标题结构归组的章节：同一 heading 下的连续 chunk 为一节；
/// 无标题（纯正文流）全部落入默认节。
#[derive(Debug, Clone)]
pub struct DocumentSection {
    /// 章节标题（heading_path 完整路径，如「第2章 / 2.1 现状」；无标题 → "未命名内容"）
    pub title: String,
    /// 分组键：原始 heading 的小写形态；无标题 chunk 为 ""。
    /// 与 title 分离——「未命名内容」的节必须仍按 "" 匹配连续无标题流。
    pub heading_key: String,
    /// 节内首个 chunk 的 ordinal（保持文档顺序）
    pub ordinal: u64,
    pub chunks: Vec<SectionChunk>,
}

impl DocumentSection {
    pub fn text(&self) -> String {
        self.chunks
            .iter()
            .map(|chunk| chunk.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn char_count(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.text.len()).sum()
    }
}

/// 模型产出的单节摘要（宽容解析产物）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SectionSummary {
    pub title: String,
    pub summary: String,
    pub key_points: Vec<String>,
}

/// 模型产出的文档总览（最后一层聚合）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DocumentOverview {
    pub overview: String,
    pub overall_summary: String,
    pub structure: Vec<StructureEntry>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StructureEntry {
    pub title: String,
    pub key_points: Vec<String>,
}

/// 单节文本超过该长度时强制拆分（模型上下文与摘要粒度约束）。
pub const MAX_SECTION_CHARS: usize = 6_000;
/// 节数超过该值时把尾部合并进「其余内容」一节（约束生成成本与输出体积）。
pub const MAX_SECTIONS: usize = 30;

/// 把 chunk 流按节点 heading 归组成章节。
///
/// `node_heading_paths`: node_id → heading_path（来自 document_nodes 列；
/// 缺失的节点视为无标题，并入当前节）。分组规则：
/// - heading 路径变化（与上一节不同）→ 新节；
/// - 当前节字符数超过 `max_section_chars` → 拆出新节（标题沿用，标记「（续）」）。
pub fn build_document_sections(
    chunks: &[SectionChunk],
    node_heading_paths: &HashMap<Uuid, Vec<String>>,
    max_section_chars: usize,
) -> Vec<DocumentSection> {
    let mut sections: Vec<DocumentSection> = Vec::new();
    let mut current_title: Option<String> = None;
    let mut title_seen = 0_usize;
    for chunk in chunks {
        let heading = node_heading_paths
            .get(&chunk.node_id)
            .and_then(|path| path.iter().last())
            .filter(|text| !text.trim().is_empty())
            .map(|text| text.trim().to_owned());
        let heading_key = heading.as_deref().unwrap_or("").trim().to_ascii_lowercase();
        let split_oversize = sections
            .last()
            .is_some_and(|section| section.char_count() >= max_section_chars);
        let new_section = split_oversize
            || sections
                .last()
                .is_none_or(|section| section.heading_key != heading_key);
        if new_section {
            let base_title = heading.clone().unwrap_or_else(|| "未命名内容".to_owned());
            if !split_oversize || heading.is_some() {
                current_title = Some(base_title);
                title_seen = 0;
            }
            let mut title = current_title
                .clone()
                .unwrap_or_else(|| "未命名内容".to_owned());
            if split_oversize {
                title_seen += 1;
                title = format!("{title}（续 {title_seen}）");
            }
            sections.push(DocumentSection {
                title,
                heading_key: heading_key.clone(),
                ordinal: chunk.ordinal,
                chunks: Vec::new(),
            });
        }
        sections
            .last_mut()
            .expect("section created above")
            .chunks
            .push(chunk.clone());
    }
    sections
}

/// 节数超过 `max_sections` 时，把尾部小节并入「其余内容」一节（保留分节边界）。
pub fn merge_tail_sections(sections: &mut Vec<DocumentSection>, max_sections: usize) -> usize {
    if sections.len() <= max_sections {
        return sections.len();
    }
    let kept = max_sections.saturating_sub(1);
    let tail = sections.split_off(kept);
    let merged_title = "其余内容".to_owned();
    let mut ordinal = u64::MAX;
    let mut chunks = Vec::new();
    for section in tail {
        ordinal = ordinal.min(section.ordinal);
        chunks.extend(section.chunks);
    }
    sections.push(DocumentSection {
        title: merged_title,
        heading_key: "其余内容".to_owned(),
        ordinal,
        chunks,
    });
    sections.len()
}

/// 单节摘要批次输出 Schema（一次调用覆盖一批小节）。
pub fn section_summary_schema() -> Value {
    json!({
        "type": "object",
        "required": ["sections"],
        "properties": {
            "sections": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["title", "summary", "key_points"],
                    "properties": {
                        "title": { "type": "string", "description": "与原章节标题完全一致" },
                        "summary": { "type": "string", "description": "本节内容的摘要（只概括本节原文，不补充外部知识）" },
                        "key_points": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "本节要点，每点一句话"
                        }
                    }
                }
            }
        }
    })
}

/// 文档总览输出 Schema（最后一层聚合）。
pub fn overview_schema() -> Value {
    json!({
        "type": "object",
        "required": ["overview", "overall_summary", "structure"],
        "properties": {
            "overview": { "type": "string", "description": "一段话总览：这份文档是什么、主要讲什么" },
            "overall_summary": { "type": "string", "description": "全文整体摘要（覆盖各节要点）" },
            "structure": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["title", "key_points"],
                    "properties": {
                        "title": { "type": "string" },
                        "key_points": { "type": "array", "items": { "type": "string" } }
                    }
                }
            }
        }
    })
}

/// 构建单节摘要批次 Prompt。`sections_json` 由调用方用
/// `section_batch_json` 序列化；一次调用只处理一批小节。
pub fn document_summary_prompt(
    file_name: &str,
    document_type_hint: Option<&str>,
    sections_json: &str,
) -> (String, String) {
    let system = "你是翻翻的本地文档摘要器。你的任务是把给定文档的章节逐一概括。\
每个章节必须严格依据该章节原文概括，只提取原文存在的信息，不得补充外部知识、\
不得臆测。输出严格的 JSON，不要输出 JSON 以外的任何文字。";
    let user = format!(
        "文档名称：{file_name}{}\n\
\n\
下面是这份文档按章节切分后的内容（每个章节可能较长，可以适当压缩，\
但要保留章节内的关键事实、数字、结论）。\n\
\n\
请为每一节输出：\n\
- title：与原章节标题完全一致；\n\
- summary：本节内容的连贯摘要（3~8 句话）；\n\
- key_points：本节要点列表，每点一句话。\n\
\n\
章节内容：\n\
{sections_json}",
        document_type_hint
            .map(|hint| format!("（文档类型：{hint}）"))
            .unwrap_or_default()
    );
    (system.to_owned(), user)
}

/// 构建文档总览（聚合层）Prompt。`digests_json` 为各节摘要的序列化数组。
pub fn document_overview_prompt(
    file_name: &str,
    document_type_hint: Option<&str>,
    digests_json: &str,
) -> (String, String) {
    let system = "你是翻翻的本地文档总览器。你的任务是基于各章节的摘要，\
给出一份文档级总览。只能使用提供的章节摘要信息，不得补充外部知识，\
不得凭「这类文档通常有什么」补齐章节摘要里不存在的章节或主题。\
输出严格的 JSON，不要输出 JSON 以外的任何文字。";
    let user = format!(
        "文档名称：{file_name}{}\n\
\n\
以下是各章节的摘要：\n\
{digests_json}\n\
\n\
请输出：\n\
- overview：一段话总览（这份文档是什么、整体讲什么、面向谁）；\n\
- overall_summary：全文整体摘要（只覆盖上面真实存在的章节要点，5~10 句话）；\n\
- structure：章节结构表，只列上面真实出现的章节，每节给出标题与要点。",
        document_type_hint
            .map(|hint| format!("（文档类型：{hint}）"))
            .unwrap_or_default()
    );
    (system.to_owned(), user)
}

/// 宽容解析单节摘要批次：剥 ```json 围栏 → 取首个 JSON 对象 → 逐条提取；
/// 任一步失败都只丢弃坏项，不整体失败。无法解析时返回空数组
/// （调用方按确定性回退处理）。
pub fn parse_section_summaries(raw: &str) -> Vec<SectionSummary> {
    let Some(value) = tolerant_json_object(raw) else {
        return Vec::new();
    };
    let Some(items) = value.get("sections").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)?;
            let summary = item
                .get("summary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .unwrap_or_default();
            let key_points = item
                .get("key_points")
                .and_then(Value::as_array)
                .map(|points| {
                    points
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(SectionSummary {
                title,
                summary,
                key_points,
            })
        })
        .collect()
}

/// 宽容解析文档总览：剥围栏取首个 JSON 对象；失败返回 None。
pub fn parse_overview(raw: &str) -> Option<DocumentOverview> {
    let value = tolerant_json_object(raw)?;
    let overview = value
        .get("overview")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    let overall_summary = value
        .get("overall_summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    let structure = value
        .get("structure")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)?;
                    let key_points = item
                        .get("key_points")
                        .and_then(Value::as_array)
                        .map(|points| {
                            points
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::trim)
                                .filter(|text| !text.is_empty())
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Some(StructureEntry { title, key_points })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(DocumentOverview {
        overview,
        overall_summary,
        structure,
    })
}

/// 把一批节序列化为模型输入的 JSON（title + 正文）。
pub fn section_batch_json(sections: &[DocumentSection]) -> Value {
    Value::Array(
        sections
            .iter()
            .map(|section| {
                json!({
                    "title": section.title,
                    "content": section.text(),
                })
            })
            .collect(),
    )
}

/// 把各节摘要序列化为总览层的输入 JSON。
pub fn digests_json(digests: &[SectionSummary]) -> Value {
    Value::Array(
        digests
            .iter()
            .map(|digest| {
                json!({
                    "title": digest.title,
                    "summary": digest.summary,
                    "key_points": digest.key_points,
                })
            })
            .collect(),
    )
}

/// 剥 ```json / ``` 围栏后取首个顶层 JSON 对象（容忍前后杂文）。
fn tolerant_json_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed).trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    // 整段不是合法 JSON（或不是对象）：尝试从文本中抠出第一个 { ... } 平衡块
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
                    return serde_json::from_str(&trimmed[start..end]).ok();
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

    fn chunk(text: &str) -> SectionChunk {
        SectionChunk {
            chunk_id: Uuid::now_v7(),
            node_id: Uuid::now_v7(),
            revision_id: Uuid::now_v7(),
            ordinal: 0,
            text: text.to_owned(),
            locator: SourceLocator::default(),
        }
    }

    #[test]
    fn sections_split_by_heading_change() {
        let mut paths = HashMap::new();
        let node_a = Uuid::now_v7();
        let node_b = Uuid::now_v7();
        paths.insert(node_a, vec!["第1章".to_owned()]);
        paths.insert(node_b, vec!["第2章".to_owned()]);
        let chunks = vec![
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_a,
                revision_id: Uuid::now_v7(),
                ordinal: 0,
                text: "第一章内容甲".into(),
                locator: SourceLocator::default(),
            },
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_a,
                revision_id: Uuid::now_v7(),
                ordinal: 1,
                text: "第一章内容乙".into(),
                locator: SourceLocator::default(),
            },
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_b,
                revision_id: Uuid::now_v7(),
                ordinal: 2,
                text: "第二章内容".into(),
                locator: SourceLocator::default(),
            },
        ];
        let sections = build_document_sections(&chunks, &paths, MAX_SECTION_CHARS);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "第1章");
        assert_eq!(sections[0].chunks.len(), 2);
        assert_eq!(sections[1].title, "第2章");
        assert_eq!(sections[1].chunks.len(), 1);
    }

    #[test]
    fn sections_split_oversize() {
        let mut paths = HashMap::new();
        let node_a = Uuid::now_v7();
        paths.insert(node_a, vec!["只有一个大节".to_owned()]);
        let chunks = vec![
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_a,
                revision_id: Uuid::now_v7(),
                ordinal: 0,
                text: "a".repeat(1200),
                locator: SourceLocator::default(),
            },
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_a,
                revision_id: Uuid::now_v7(),
                ordinal: 1,
                text: "b".repeat(1200),
                locator: SourceLocator::default(),
            },
            SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: node_a,
                revision_id: Uuid::now_v7(),
                ordinal: 2,
                text: "c".repeat(1200),
                locator: SourceLocator::default(),
            },
        ];
        // 前两个 chunk 合并进同一节（1200+1200 < 1500 时触发不了拆分，
        // 拆分发生在「节已超限后下一个 chunk 到达」时）
        let sections = build_document_sections(&chunks, &paths, 1500);
        assert_eq!(sections.len(), 2);
        assert!(sections[1].title.contains("续"));
        assert_eq!(sections[0].chunks.len(), 2);
        assert_eq!(sections[1].chunks.len(), 1);
        assert_eq!(sections[0].heading_key, "只有一个大节");
    }

    #[test]
    fn sections_untitled_flow_single_section() {
        let sections = build_document_sections(
            &[chunk("无标题正文一"), chunk("无标题正文二")],
            &HashMap::new(),
            MAX_SECTION_CHARS,
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "未命名内容");
        assert_eq!(sections[0].chunks.len(), 2);
    }

    #[test]
    fn empty_chunks_produce_no_sections() {
        let sections = build_document_sections(&[], &HashMap::new(), MAX_SECTION_CHARS);
        assert!(sections.is_empty());
    }

    #[test]
    fn merge_tail_sections_bounds_count() {
        // 12 个独立小节 → 合并后保留 5 节（4 节 + 其余内容）
        let mut sections = (0..12)
            .map(|index| DocumentSection {
                title: format!("节{index}"),
                heading_key: format!("节{index}"),
                ordinal: index as u64,
                chunks: vec![chunk("内容")],
            })
            .collect::<Vec<_>>();
        let count = merge_tail_sections(&mut sections, 5);
        assert_eq!(count, 5);
        assert_eq!(sections.last().expect("last").title, "其余内容");
        assert_eq!(sections.last().expect("last").chunks.len(), 8);
    }

    #[test]
    fn parse_section_summaries_valid() {
        let raw = r#"{"sections":[
            {"title":"第1章","summary":"讲第一章。","key_points":["要点一"]},
            {"title":"第2章","summary":"讲第二章。","key_points":["要点二","要点三"]}
        ]}"#;
        let parsed = parse_section_summaries(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "第1章");
        assert_eq!(
            parsed[1].key_points,
            vec!["要点二".to_owned(), "要点三".to_owned()]
        );
    }

    #[test]
    fn parse_section_summaries_strips_fence_and_trailing_text() {
        let raw = "好的，这是摘要：\n```json\n{\"sections\":[{\"title\":\"T\",\"summary\":\"S\",\"key_points\":[]}]}\n```\n希望对你有帮助";
        let parsed = parse_section_summaries(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "T");
        assert_eq!(parsed[0].summary, "S");
    }

    #[test]
    fn parse_section_summaries_drops_invalid_items() {
        let raw = r#"{"sections":[
            {"title":"有效","summary":"S","key_points":[]},
            {"summary":"缺标题"},
            "字符串不是对象"
        ]}"#;
        let parsed = parse_section_summaries(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "有效");
    }

    #[test]
    fn parse_section_summaries_garbage_yields_empty() {
        assert!(parse_section_summaries("完全没有 JSON 的输出").is_empty());
        assert!(parse_section_summaries("").is_empty());
    }

    #[test]
    fn parse_overview_valid_and_tolerant() {
        let raw = r#"{"overview":"总体","overall_summary":"全文","structure":[{"title":"T","key_points":["K"]}]}"#;
        let overview = parse_overview(raw).expect("parse");
        assert_eq!(overview.overview, "总体");
        assert_eq!(overview.structure.len(), 1);
        assert!(parse_overview("不是 JSON").is_none());
    }

    #[test]
    fn section_batch_json_round_trip_shape() {
        let section = DocumentSection {
            title: "章节".into(),
            heading_key: "章节".into(),
            ordinal: 0,
            chunks: vec![SectionChunk {
                chunk_id: Uuid::now_v7(),
                node_id: Uuid::now_v7(),
                revision_id: Uuid::now_v7(),
                ordinal: 0,
                text: "正文".into(),
                locator: SourceLocator::default(),
            }],
        };
        let value = section_batch_json(&[section]);
        assert_eq!(value[0]["title"], "章节");
        assert_eq!(value[0]["content"], "正文");
    }

    #[test]
    fn tolerant_json_object_finds_object_in_noise() {
        let raw =
            "前言\n{\"sections\":[{\"title\":\"T\",\"summary\":\"S\",\"key_points\":[]}]}\n后记";
        let parsed = parse_section_summaries(raw);
        assert_eq!(parsed.len(), 1);
    }
}
