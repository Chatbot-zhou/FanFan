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
        // 标题来源：结构 `heading_path` 优先；缺失时从正文首行兜底识别标题行。
        // 兜底保证「无 heading_path 的 OCR/纯文本文档」大纲能列出真实章节标题，
        // 而不是全部退化成「未命名内容」。通用句式判定，不针对具体文件/case。
        let struct_heading = node_heading_paths
            .get(&chunk.node_id)
            .and_then(|path| path.iter().last())
            .filter(|text| !text.trim().is_empty())
            .map(|text| text.trim().to_owned());
        let heading = struct_heading
            .clone()
            .or_else(|| extract_heading_candidate(&chunk.text));
        let heading_key = heading.as_deref().unwrap_or("").trim().to_ascii_lowercase();
        let split_oversize = sections
            .last()
            .is_some_and(|section| section.char_count() >= max_section_chars);
        // 新节触发：首 chunk；超长拆分；或「有标题信号且标题键发生变化」。
        // 无标题正文 chunk 并入当前节（不因 heading_key 为空而另开「未命名」节），
        // 消除「标题节 + 未命名节」交错，保证正文标题能聚合为一条章节线。
        let new_section = sections.is_empty()
            || split_oversize
            || (heading.is_some()
                && sections.last().is_some_and(|section| section.heading_key != heading_key));
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

/// 从 chunk 正文首行尝试提取「章节标题行」（无结构 heading_path 时的兜底）。
///
/// 动机：OCR/纯文本派生文档的 `document_nodes.heading_path` 常为全空结构，导致
/// `build_document_sections` 只能产出「未命名内容」，大纲/结构枚举无法给出真实
/// 章节。这类文档常用「编号 / 章节标记」行作标题（如「1. 现状」「2.1 原理」
/// 「第3章 部署」「一、背景」）。本函数从该 chunk 首行判断是否为标题：特征为
/// 首行、较短、不以句末标点结尾、且带编号或章节词信号。无标记的普通正文段落
/// 不认，避免误切分。只做通用句式判定，不针对任何具体文件/关键词/case 特判。
fn extract_heading_candidate(text: &str) -> Option<String> {
    let first_line = text.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }
    // 1) 行内章节标题兜底（先于整行判定，长正文行同样适用）：OCR/纯文本速记文档常把
    //    「第N 章 <标题>」内嵌在正文行中而不是单独成行，整行判定无法命中。命中且
    //    标题被强约束收尾时，直接返回提取出的标题。
    if let Some(title) = extract_inline_chapter_title(first_line) {
        return Some(title);
    }
    // 2) 整行标题判定（原逻辑）：较短、不以句末标点结尾、带编号/章节词信号。
    //    标题行一般较短（含符号 2..=48 字符）；超长行视作正文段落。
    let char_count = first_line.chars().count();
    if !(2..=48).contains(&char_count) {
        return None;
    }
    // 标题通常不以句末标点结尾（。！？；…），正文成句才带句号。
    if first_line.ends_with(['。', '！', '？', '；', '.', '，']) {
        return None;
    }
    if has_heading_signal(first_line) {
        Some(first_line.to_owned())
    } else {
        None
    }
}

/// 行内章节标题的硬边界：标题命中这些符号即视为在此终止（强约束，避免把正文章节
/// 引用误判成标题）。
const INLINE_TITLE_ENDERS: &[char] = &[
    '●', '■', '◆', '○', '·', '・', '、', '。', '；', '！', '？', '．', '，', '｜', '/', '|',
    '：', ':',
];

/// 中文数字（合数用，用于「第<编号> 章」的编号判定）。
fn is_cjk_numerical(c: char) -> bool {
    matches!(
        c,
        '一' | '二'
            | '三'
            | '四'
            | '五'
            | '六'
            | '七'
            | '八'
            | '九'
            | '十'
            | '百'
            | '千'
            | '万'
    )
}

/// 从正文行内提取强约束的章节标题（无结构 heading_path 时的行内兜底）。
///
/// 动机：OCR/纯文本速记文档的 `document_nodes.heading_path` 常为全空结构，且章节
/// 标题常嵌在正文行中间（如「备考精华第1 章计算机硬件基础 CPU 中的相关组件」），
/// `has_heading_signal` 的整行判定无法命中，大纲只能退化为「未命名内容」。
/// 本函数扫描行内「第<编号> 章」标记，读取紧跟其后的章节标题，并仅当标题被
/// **硬边界**（项目符号 / 句末标点 / 行尾）或 **ASCII 字母**（正文常以英文缩写
/// 起步，如「CPU 中的相关组件」）收尾时才返回——强约束，正文里「详见第5 章…」
/// 这类引用不命中；标题长度上限 `MAX_INLINE_TITLE_CHARS`（超出判为正文）。通用
/// 句式判定，不针对任何具体文件/关键词/case。
fn extract_inline_chapter_title(line: &str) -> Option<String> {
    const MAX_INLINE_TITLE_CHARS: usize = 14;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '第' {
            i += 1;
            continue;
        }
        // 编号：阿拉伯数字或中文数字，其间允许空白（如「第1 章」「第十二 章」）。
        let mut j = i + 1;
        let mut found_digit = false;
        while j < chars.len()
            && (chars[j].is_ascii_digit() || is_cjk_numerical(chars[j]) || chars[j] == ' ')
        {
            if chars[j] != ' ' {
                found_digit = true;
            }
            j += 1;
        }
        if !found_digit {
            i += 1;
            continue;
        }
        // 跳过编号后空白，必须紧跟「章」单位。
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        if j >= chars.len() || chars[j] != '章' {
            i += 1;
            continue;
        }
        j += 1;
        // 跳过标题前空白。
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        // 读取有界标题：字母/汉字（不含空白与符号），上限 `MAX_INLINE_TITLE_CHARS`。
        let mut title: Vec<char> = Vec::new();
        while j < chars.len() && title.len() < MAX_INLINE_TITLE_CHARS && chars[j].is_alphabetic() {
            title.push(chars[j]);
            j += 1;
        }
        if title.len() < 2 {
            i += 1;
            continue;
        }
        // 标题终止语境判定（强约束）：
        // - 行尾：可接受；
        // - 硬边界符号：可接受；
        // - 空白后接 ASCII 字母（英文缩写起步的正文）或硬边界或行尾：可接受；
        // - 其余（空白后接汉字、标题被上限截断等）：不置信，跳过此标记。
        let accept = if j >= chars.len() {
            true
        } else {
            let nc = chars[j];
            if INLINE_TITLE_ENDERS.contains(&nc) {
                true
            } else if nc == ' ' {
                let after = chars[j..]
                    .iter()
                    .copied()
                    .find(|c| *c != ' ')
                    .unwrap_or(' ');
                chars[j..].iter().all(|c| *c == ' ')
                    || INLINE_TITLE_ENDERS.contains(&after)
                    || after.is_ascii_alphabetic()
            } else {
                false
            }
        };
        if accept {
            return Some(title.into_iter().collect());
        }
        // 该标记不能被置信，继续向后查找下一个「第」。
        i += 1;
    }
    None
}

/// 标题行「信号」判定：带编号前缀或章节词前缀。
fn has_heading_signal(line: &str) -> bool {
    let line = line.trim_start();
    // 1) 章节词前缀：「第<编号><章|节|部分|篇|讲|回>」。
    //    「第」必须后接数字（阿拉伯或中文）再跟章节单位，避免「第一步先…」级正文误判。
    if let Some(rest) = line.strip_prefix('第') {
        let rest = rest.trim_start();
        let digit_run: String = rest
            .chars()
            .take_while(|c| {
                c.is_ascii_digit()
                    || matches!(c, '一'|'二'|'三'|'四'|'五'|'六'|'七'|'八'|'九'|'十'|'百'|'千'|'万')
            })
            .collect();
        if !digit_run.is_empty() {
            let after = rest[digit_run.len()..].trim_start();
            const CHAPTER_UNITS: &[&str] = &["章", "节", "部分", "篇", "讲", "回"];
            if CHAPTER_UNITS.iter().any(|unit| after.starts_with(unit)) {
                return true;
            }
        }
        // 固定的具名章节词。
        const NAMED_SECTIONS: &[&str] = &["附录", "前言", "目录", "绪论", "结语", "导言", "后记", "引言"];
        return NAMED_SECTIONS.iter().any(|named| line.starts_with(named));
    }
    // 2) 编号前缀：阿拉伯数字（如「1」「1.1」「2-1」）后跟分隔符/空白；或中文序号
    //    「一、」「（一）」。
    if let Some(first) = line.chars().next() {
        if first.is_ascii_digit() {
            return true;
        }
        if matches!(first, '一'|'二'|'三'|'四'|'五'|'六'|'七'|'八'|'九'|'十') {
            let after = line[first.len_utf8()..].trim_start();
            return after.starts_with('、') || after.starts_with('．') || after.starts_with('.');
        }
        if first == '（' || first == '(' {
            return true;
        }
    }
    false
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

/// 把模型产出的摘要批次对齐到输入小节（保持小节顺序）。
///
/// 背景：无结构化标题的文档会把整段正文按长度拆成多节「未命名内容」，
/// 标题键在批次内重复，纯标题匹配会互相挤占、大量回退为节内摘录
/// （r31 实测 5/10 批次回退）。这里采用「唯一标题优先 + 位置补齐」：
/// - 标题键在批次内唯一的节，优先按标题精确/基准键匹配（结构化文档主路径）；
/// - 其余（重复标题或唯一键未命中）按模型输出顺序位置补齐——模型被要求
///   按输入顺序逐节输出，顺序本身即最可靠的对应关系。
/// 仍对不上的节（模型少输出/解析丢项）回退为确定性节内摘录。
///
/// 返回 `(digests, fallback_count)`，`digests` 与 `sections` 一一对应。
pub fn match_section_digests(
    sections: &[DocumentSection],
    parsed: Vec<SectionSummary>,
    fallback_chars: usize,
) -> (Vec<SectionSummary>, usize) {
    // 标题键在批次内的出现次数：唯一键才走标题匹配，重复键视为不可靠标题。
    let mut key_counts = HashMap::<String, usize>::new();
    for section in sections {
        let key = section.title.trim().to_ascii_lowercase();
        *key_counts.entry(key).or_insert(0) += 1;
    }
    // 模型输出顺序保留在 pool 中，标题命中时从 pool 移除，避免重复占用。
    let mut pool = parsed;
    let mut matched = Vec::<Option<SectionSummary>>::with_capacity(sections.len());
    for section in sections {
        let key = section.title.trim().to_ascii_lowercase();
        let base_key = key
            .trim_end_matches(|ch: char| ch.is_ascii_digit())
            .trim_end_matches("（续")
            .trim();
        let unique = key_counts.get(&key).copied().unwrap_or(0) == 1;
        let digest = if unique {
            pool.iter()
                .position(|digest| {
                    let dk = digest.title.trim().to_ascii_lowercase();
                    dk == key || dk == base_key
                })
                .map(|pos| pool.remove(pos))
        } else {
            None
        };
        matched.push(digest);
    }
    // 未按标题匹配上的节按模型输出顺序位置补齐（先进先出）。
    let mut digests = Vec::with_capacity(sections.len());
    let mut fallbacks = 0_usize;
    for (section, digest) in sections.iter().zip(matched) {
        let digest = digest.or_else(|| (!pool.is_empty()).then(|| pool.remove(0)));
        match digest {
            Some(digest) => digests.push(digest),
            None => {
                fallbacks += 1;
                digests.push(SectionSummary {
                    title: section.title.clone(),
                    summary: compact_text(&section.text(), fallback_chars),
                    key_points: Vec::new(),
                });
            }
        }
    }
    (digests, fallbacks)
}

/// 折叠空白后按字符数截断的确定性压缩（确定性节内摘录回退用）。
fn compact_text(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(limit).collect()
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
