//! Memory Writer（Step 5）：问答结束后的异步 Memory Candidate Extractor。
//!
//! 铁律（规格书「十五、Memory Writer」）：
//! - **禁止所有问答自动写 Memory**——只有四种事件允许产生候选：
//!   用户明确表达（“这是我的简历”）/ 用户明确确认候选（“对，就是第一份”）/
//!   用户给文件起别名（“以后这个就叫法律项目”）/ 同一关系被多次明确使用；
//! - Writer 是模型 → 推断类来源：**只能产生 candidate**（STRICT：
//!   哪怕模型输出 `status = confirmed`，确定性预写验证也强制降级为 candidate；
//!   确认只能来自用户的显式操作，见 Step 7 澄清选择 / 用户确认链路）；
//! - 普通聊天（“Transformer 是什么？”）必须 `should_write = false`；
//! - 本模块是纯函数、无 IO。模型输出 → [`parse_writer_output`] →
//!   [`prewrite_validate`]（确定性门：结构化约束、别名规范化、置信度钳制、
//!   去重、强制 candidate）→ [`resolve_proposal_targets`]（名字必须能解析到
//!   已知文件/实体/收藏集，**无法解析到合法目标的一律丢弃**——绝不写悬空记忆）。
//!   文件「在场 + 授权根」的合法性检查属于存储层（`memory_file_target_valid`），
//!   由 Step 6 编排层在写入前再验一道；这里拿到的是已加载的合法清单。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::knowledge::{AskMessage, fold_recent_history};
use crate::memory::{
    MemoryEntity, MemoryKind, MemorySource, MemoryStatus, MemoryTargetType, MemoryWriteInput,
    normalize_alias,
};

/// Writer 输出的单条记忆提案（模型侧 JSON，snake_case）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryProposal {
    /// `relation`（关系）或 `alias`（别名）。
    /// alias 时 `subject` 是**目标名字**（文件/实体/收藏集的真名），`alias` 是用户起的别名；
    /// relation 时 `subject` / `object` 是两端名字，`predicate` 是关系谓词。
    pub kind: MemoryKind,
    pub subject: String,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub alias: Option<String>,
    pub confidence: f32,
    /// 模型可填 `confirmed`，但确定性预写验证一律降级为 candidate。
    pub status: MemoryStatus,
}

/// Writer 的顶层输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryWriterOutput {
    pub should_write: bool,
    pub memories: Vec<MemoryProposal>,
}

/// Writer 的输入上下文（编排层组装）。
#[derive(Debug, Clone)]
pub struct MemoryWriterContext<'a> {
    /// 本轮用户消息（判断依据）
    pub question: &'a str,
    /// 本轮回合回答（含引用；「对，就是第一份」这类确认要靠它关联文件）
    pub answer: &'a str,
    /// 最近对话历史（仅作理解上下文参考，不是解析对象）
    pub history: &'a [AskMessage],
    /// 本轮实际参与的文件真名（目标解析的依据，别名/关系必须指向其中名字）
    pub active_files: &'a [String],
    /// 已知实体名（关系两端可解析到的实体）
    pub known_entities: &'a [String],
    /// 已知收藏集名（别名/关系目标可解析到的收藏集）
    pub known_collections: &'a [String],
}

/// Memory Writer 的输出 JSON Schema（llama.cpp 侧约束解码）。
pub fn memory_writer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["should_write", "memories"],
        "properties": {
            "should_write": {"type": "boolean"},
            "memories": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "type", "subject", "predicate", "object", "alias",
                        "confidence", "status"
                    ],
                    "properties": {
                        "type": {"type": "string", "enum": ["relation", "alias"]},
                        "subject": {"type": "string", "maxLength": 200},
                        "predicate": {"type": ["string", "null"], "maxLength": 100},
                        "object": {"type": ["string", "null"], "maxLength": 200},
                        "alias": {"type": ["string", "null"], "maxLength": 100},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "status": {"type": "string", "enum": ["candidate", "confirmed"]}
                    }
                }
            }
        }
    })
}

/// 构建 Memory Writer prompt。
/// system 按规格书「十五」；user 含折叠历史（仅上下文）、本轮问答、
/// 涉及文件清单与示例。
pub fn memory_writer_prompt(context: &MemoryWriterContext<'_>) -> (String, String) {
    let system = "你是翻翻的 Memory Candidate Extractor。你的任务不是总结对话，而是判断本轮用户是否明确提供了值得长期保存的“实体关系或别名”。只记录：1. 用户明确表达的关系；2. 用户明确确认的关系；3. 用户给文件/项目/实体定义的别名。不要根据猜测建立用户身份事实。不要记录普通聊天内容。不要记录临时问题。不要复制文档正文。"
        .into();
    let mut user = String::new();
    let folded = fold_recent_history(context.history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!(
            "【对话历史】以下是最近 5 条对话的历史记录，仅作理解上文的参考，不是解析对象，严禁复读或引用其中的任何内容：\n{folded}\n\n"
        ));
    }
    user.push_str(&format!(
        r#"【本轮问答】
用户：{question}
回答：{answer}

【本轮涉及的文件】（必须使用其中的真实文件名，别名与关系的目标必须指向这里或下面的已知实体/收藏集）
{files}

【已知实体】
{entities}

【已知收藏集】
{collections}

【任务】判断用户是否明确提供了值得长期保存的关系或别名，只输出规定 JSON。

规则：
- 普通问答（“Transformer 是什么？”）→ should_write = false，memories = []。
- 用户明确表达/确认/起别名才记录；“我”不是实体，涉及“我的 X”时 type = alias，subject 用目标文件的真名，alias 填“X”。
- 只允许 type = relation | alias：
  - relation：subject / object 用真名（文件真名、已知实体名或已知收藏集名），predicate 用简短谓词（如 is_about / owns / works_on）；
  - alias：subject = 目标真名，alias = 用户起的别名。
- 目标必须是【本轮涉及的文件】、【已知实体】或【已知收藏集】里的名字；找不到对应目标就放弃这条记忆，不要编造。
- status 只能填 candidate（模型推断永远不能确认事实，确认只能来自用户显式操作）。
- confidence 0.0 ~ 1.0，表达明确的给高值，一般推断给低值。

【示例】
用户：这是我的简历。
回答：好的，已记住周晨.pdf 是你的简历。
输出：{{"should_write":true,"memories":[{{"type":"alias","subject":"周晨.pdf","predicate":null,"object":null,"alias":"我的简历","confidence":0.95,"status":"candidate"}}]}}

用户：以后这个文件就叫法律项目。
回答：已为您记住别名「法律项目」。
输出：{{"should_write":true,"memories":[{{"type":"alias","subject":"合同范本-2026.pdf","predicate":null,"object":null,"alias":"法律项目","confidence":0.9,"status":"candidate"}}]}}

用户：以后把这个收藏集叫做法务库。
回答：已为您记住别名「法务库」。
输出：{{"should_write":true,"memories":[{{"type":"alias","subject":"法律项目","predicate":null,"object":null,"alias":"法务库","confidence":0.9,"status":"candidate"}}]}}

用户：对，就是第一份。
回答：周晨.pdf 中记载周晨毕业于北京大学。
输出：{{"should_write":true,"memories":[{{"type":"relation","subject":"周晨","predicate":"graduated_from","object":"北京大学","alias":null,"confidence":0.85,"status":"candidate"}}]}}

用户：Transformer 是什么？
回答：Transformer 是一种……
输出：{{"should_write":false,"memories":[]}}

只输出符合 JSON Schema 的对象，不要输出 Markdown、代码块或解释。"#,
        question = context.question.trim(),
        answer = context.answer.trim(),
        files = if context.active_files.is_empty() {
            "（无）".to_owned()
        } else {
            context.active_files.join("\n")
        },
        entities = if context.known_entities.is_empty() {
            "（无）".to_owned()
        } else {
            context.known_entities.join("\n")
        },
        collections = if context.known_collections.is_empty() {
            "（无）".to_owned()
        } else {
            context.known_collections.join("\n")
        },
    ));
    (system, user)
}

/// 宽容解析 Writer 输出：容忍 Markdown 围栏/前后噪声/个别字段缺失。
/// 顶层 JSON 解析失败返回 None（调用方按「不写」处理）；
/// 单项非法（type/status 不认识、字段类型错误）丢弃该项。
pub fn parse_writer_output(raw: &str) -> Option<MemoryWriterOutput> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed).trim();
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let should_write = value
        .get("should_write")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut memories = Vec::new();
    if let Some(items) = value.get("memories").and_then(serde_json::Value::as_array) {
        for item in items {
            let Some(proposal) = parse_proposal(item) else {
                continue;
            };
            memories.push(proposal);
        }
    }
    Some(MemoryWriterOutput {
        should_write,
        memories,
    })
}

fn parse_proposal(value: &serde_json::Value) -> Option<MemoryProposal> {
    let kind = match value.get("type").and_then(serde_json::Value::as_str) {
        Some("relation") => MemoryKind::Relation,
        Some("alias") => MemoryKind::Alias,
        _ => return None,
    };
    let subject = value
        .get("subject")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())?;
    let predicate = value
        .get("predicate")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let object = value
        .get("object")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let alias = value
        .get("alias")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let confidence = value
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .map(|value| value as f32)
        .unwrap_or(0.5);
    let status = match value.get("status").and_then(serde_json::Value::as_str) {
        Some("confirmed") => MemoryStatus::Confirmed,
        Some("rejected") => MemoryStatus::Rejected,
        _ => MemoryStatus::Candidate,
    };
    Some(MemoryProposal {
        kind,
        subject,
        predicate,
        object,
        alias,
        confidence,
        status,
    })
}

/// 确定性预写验证（纯函数，无 IO）：
/// 1. `should_write = false` → 一律不产生候选（禁止问答自动写 Memory 的兜底）；
/// 2. 结构化约束：alias 必须有可规范化的别名；relation 必须有 subject /
///    predicate / object；
/// 3. **STRICT**：所有提案强制降级为 `candidate`（模型推断永远不能确认），
///    来源在解析阶段统一为 `model_inference`；
/// 4. confidence 钳制到 [0, 1]；按同一规范化键去重，保留置信度最高者。
pub fn prewrite_validate(output: MemoryWriterOutput) -> Vec<MemoryProposal> {
    if !output.should_write {
        return Vec::new();
    }
    let mut proposals = output.memories;
    // 置信度高者优先，去重时保留最高置信度。
    proposals.sort_by(|left, right| {
        right
            .confidence
            .total_cmp(&left.confidence)
            .then_with(|| left.subject.cmp(&right.subject))
    });
    let mut seen: HashSet<String> = HashSet::new();
    let mut validated = Vec::new();
    for mut proposal in proposals {
        proposal.status = MemoryStatus::Candidate;
        proposal.confidence = proposal.confidence.clamp(0.0, 1.0);
        match proposal.kind {
            MemoryKind::Alias => {
                let Some(alias) = normalize_alias(proposal.alias.as_deref().unwrap_or("")) else {
                    continue; // 别名不能为空
                };
                proposal.alias = Some(alias);
            }
            MemoryKind::Relation => {
                let subject = proposal.subject.trim();
                if subject.is_empty() {
                    continue;
                }
                let Some(predicate) = proposal
                    .predicate
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                let Some(object) = proposal
                    .object
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                proposal.subject = subject.to_owned();
                proposal.predicate = Some(predicate.to_owned());
                proposal.object = Some(object.to_owned());
            }
        }
        let Some(key) = dedup_key(&proposal) else {
            continue;
        };
        if !seen.insert(key) {
            continue; // 同一记忆重复提案：只留置信度最高的
        }
        validated.push(proposal);
    }
    validated
}

fn dedup_key(proposal: &MemoryProposal) -> Option<String> {
    match proposal.kind {
        MemoryKind::Alias => Some(format!(
            "a|{}|{}",
            normalize_alias(&proposal.subject)?,
            normalize_alias(proposal.alias.as_deref()?)?
        )),
        MemoryKind::Relation => Some(format!(
            "r|{}|{}|{}",
            normalize_alias(&proposal.subject)?,
            proposal.predicate.as_deref()?,
            normalize_alias(proposal.object.as_deref()?)?
        )),
    }
}

/// 名字解析依据：编排层已加载的合法目标清单（文件 id 必须已通过
/// 存储层合法性检查——在场 + 授权根；本函数只做纯名字匹配）。
#[derive(Debug, Default)]
pub struct MemoryTargetRegistry<'a> {
    /// (file_id, 文件名)
    pub files: Vec<(uuid::Uuid, &'a str)>,
    /// 已存在的实体
    pub entities: Vec<MemoryEntity>,
    /// (collection_id, 收藏集名)
    pub collections: Vec<(uuid::Uuid, &'a str)>,
}

/// 把通过预写验证的提案解析成可写入的 [`MemoryWriteInput`]。
///
/// 确定性规则：
/// - 名字匹配：规范化（去空白 + ASCII 小写）等值；查找顺序 实体 → 文件 → 收藏集；
/// - **无法解析到合法目标的名字 → 整条丢弃**（不写悬空记忆）；
/// - 统一 source_type = `model_inference`、status = `candidate`。
pub fn resolve_proposal_targets(
    proposals: Vec<MemoryProposal>,
    registry: &MemoryTargetRegistry<'_>,
) -> Vec<MemoryWriteInput> {
    let mut writes = Vec::new();
    for proposal in proposals {
        let Some(subject_target) = resolve_name(&proposal.subject, registry) else {
            continue;
        };
        match proposal.kind {
            MemoryKind::Alias => {
                // 兜底规范化：解析函数不依赖调用方先过预写验证
                let Some(alias) = proposal.alias.as_deref().and_then(normalize_alias) else {
                    continue;
                };
                writes.push(MemoryWriteInput {
                    kind: MemoryKind::Alias,
                    subject_type: subject_target.0,
                    subject_id: subject_target.1,
                    predicate: "alias".to_owned(),
                    object_type: subject_target.0,
                    object_id: subject_target.1,
                    alias: Some(alias),
                    confidence: proposal.confidence,
                    source_type: MemorySource::ModelInference,
                    source_id: None,
                    status: MemoryStatus::Candidate,
                });
            }
            MemoryKind::Relation => {
                let Some(predicate) = proposal.predicate else {
                    continue;
                };
                // 宾语同样必须能解析到合法目标；纯文本值（如“北京大学”）要写
                // 关系必须已存在于实体清单（Step 6 编排层可先用 upsert_memory_entity
                // 登记，再进解析），否则视为无法确定性验证 → 丢弃。
                let Some(object_target) = proposal
                    .object
                    .as_deref()
                    .and_then(|name| resolve_name(name, registry))
                else {
                    continue;
                };
                writes.push(MemoryWriteInput {
                    kind: MemoryKind::Relation,
                    subject_type: subject_target.0,
                    subject_id: subject_target.1,
                    predicate,
                    object_type: object_target.0,
                    object_id: object_target.1,
                    alias: None,
                    confidence: proposal.confidence,
                    source_type: MemorySource::ModelInference,
                    source_id: None,
                    status: MemoryStatus::Candidate,
                });
            }
        }
    }
    writes
}

/// 名字 → 目标（实体 → 文件 → 收藏集，规范化等值匹配）。
fn resolve_name(
    name: &str,
    registry: &MemoryTargetRegistry<'_>,
) -> Option<(MemoryTargetType, uuid::Uuid)> {
    let key = normalize_alias(name)?;
    for entity in &registry.entities {
        if normalize_alias(&entity.canonical_name).as_deref() == Some(key.as_str()) {
            return Some((MemoryTargetType::Entity, entity.entity_id));
        }
    }
    for (file_id, filename) in &registry.files {
        if normalize_alias(filename).as_deref() == Some(key.as_str()) {
            return Some((MemoryTargetType::File, *file_id));
        }
    }
    for (collection_id, collection_name) in &registry.collections {
        if normalize_alias(collection_name).as_deref() == Some(key.as_str()) {
            return Some((MemoryTargetType::Collection, *collection_id));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn proposal(
        kind: MemoryKind,
        subject: &str,
        predicate: Option<&str>,
        object: Option<&str>,
        alias: Option<&str>,
        confidence: f32,
        status: MemoryStatus,
    ) -> MemoryProposal {
        MemoryProposal {
            kind,
            subject: subject.to_owned(),
            predicate: predicate.map(str::to_owned),
            object: object.map(str::to_owned),
            alias: alias.map(str::to_owned),
            confidence,
            status,
        }
    }

    fn entity(name: &str) -> MemoryEntity {
        MemoryEntity {
            entity_id: uuid::Uuid::now_v7(),
            entity_type: "person".to_owned(),
            canonical_name: name.to_owned(),
            metadata_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn should_write_false_yields_no_proposals_even_with_memories() {
        let output = MemoryWriterOutput {
            should_write: false,
            memories: vec![proposal(
                MemoryKind::Alias,
                "周晨.pdf",
                None,
                None,
                Some("我的简历"),
                0.95,
                MemoryStatus::Confirmed,
            )],
        };
        assert!(prewrite_validate(output).is_empty(), "普通问答绝不写 Memory");
    }

    #[test]
    fn model_confirmed_is_downgraded_to_candidate() {
        let output = MemoryWriterOutput {
            should_write: true,
            memories: vec![proposal(
                MemoryKind::Alias,
                "周晨.pdf",
                None,
                None,
                Some("我的简历"),
                0.95,
                MemoryStatus::Confirmed,
            )],
        };
        let validated = prewrite_validate(output);
        assert_eq!(validated.len(), 1);
        assert_eq!(
            validated[0].status,
            MemoryStatus::Candidate,
            "STRICT：模型推断永远只能写 candidate"
        );
    }

    #[test]
    fn alias_kind_requires_normalizable_alias_and_whitespace_is_normalized() {
        let output = MemoryWriterOutput {
            should_write: true,
            memories: vec![
                proposal(MemoryKind::Alias, "周晨.pdf", None, None, Some("   "), 0.9, MemoryStatus::Candidate),
                proposal(MemoryKind::Alias, "周晨.pdf", None, None, Some("我的 简历"), 0.9, MemoryStatus::Candidate),
            ],
        };
        let validated = prewrite_validate(output);
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].alias.as_deref(), Some("我的简历"));
    }

    #[test]
    fn relation_kind_requires_subject_predicate_and_object() {
        let output = MemoryWriterOutput {
            should_write: true,
            memories: vec![
                proposal(MemoryKind::Relation, "周晨", None, None, None, 0.8, MemoryStatus::Candidate),
                proposal(MemoryKind::Relation, "周晨", Some("is_about"), None, None, 0.8, MemoryStatus::Candidate),
                proposal(
                    MemoryKind::Relation,
                    "周晨",
                    Some("graduated_from"),
                    Some("北京大学"),
                    None,
                    0.8,
                    MemoryStatus::Candidate,
                ),
            ],
        };
        let validated = prewrite_validate(output);
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].object.as_deref(), Some("北京大学"));
    }

    #[test]
    fn dedup_keeps_highest_confidence_proposal() {
        let output = MemoryWriterOutput {
            should_write: true,
            memories: vec![
                proposal(
                    MemoryKind::Relation,
                    "周晨",
                    Some("graduated_from"),
                    Some("北京大学"),
                    None,
                    0.6,
                    MemoryStatus::Candidate,
                ),
                proposal(
                    MemoryKind::Relation,
                    "周晨",
                    Some("graduated_from"),
                    Some("北京大学"),
                    None,
                    0.9,
                    MemoryStatus::Candidate,
                ),
            ],
        };
        let validated = prewrite_validate(output);
        assert_eq!(validated.len(), 1);
        assert!((validated[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn confidence_is_clamped_into_range() {
        let output = MemoryWriterOutput {
            should_write: true,
            memories: vec![proposal(
                MemoryKind::Alias,
                "周晨.pdf",
                None,
                None,
                Some("我的简历"),
                1.7,
                MemoryStatus::Candidate,
            )],
        };
        let validated = prewrite_validate(output);
        assert!((validated[0].confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_writer_output_tolerates_fences_noise_and_invalid_items() {
        let raw = "```json\n{\"should_write\": true, \"memories\": [\n  {\"type\": \"alias\", \"subject\": \"周晨.pdf\", \"predicate\": null, \"object\": null, \"alias\": \"我的简历\", \"confidence\": 0.9, \"status\": \"candidate\"},\n  {\"type\": \"nonsense\", \"subject\": \"x\"},\n  {\"type\": \"relation\", \"subject\": \"周晨\", \"predicate\": \"is_about\", \"object\": \"周晨.pdf\", \"alias\": null, \"confidence\": 0.7, \"status\": \"confirmed\"}\n]}\n```";
        let output = parse_writer_output(raw).expect("parse");
        assert!(output.should_write);
        assert_eq!(output.memories.len(), 2, "非法项被丢弃");
        assert_eq!(output.memories[0].kind, MemoryKind::Alias);
        assert_eq!(output.memories[1].status, MemoryStatus::Confirmed);
    }

    #[test]
    fn parse_writer_output_failure_yields_none() {
        assert!(parse_writer_output("not json at all").is_none());
    }

    fn registry() -> MemoryTargetRegistry<'static> {
        MemoryTargetRegistry {
            files: vec![
                (uuid::Uuid::now_v7(), "周晨.pdf"),
                (uuid::Uuid::now_v7(), "合同范本-2026.pdf"),
            ],
            entities: vec![entity("周晨")],
            collections: Vec::new(),
        }
    }

    #[test]
    fn resolve_alias_to_file_and_relation_between_entity_and_file() {
        let proposals = vec![
            proposal(
                MemoryKind::Alias,
                "周晨.pdf",
                None,
                None,
                Some("我的简历"),
                0.95,
                MemoryStatus::Candidate,
            ),
            proposal(
                MemoryKind::Relation,
                "周晨",
                Some("is_about"),
                Some("周晨.pdf"),
                None,
                0.8,
                MemoryStatus::Candidate,
            ),
        ];
        let writes = resolve_proposal_targets(proposals, &registry());
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].kind, MemoryKind::Alias);
        assert_eq!(writes[0].subject_type, MemoryTargetType::File);
        assert_eq!(writes[0].alias.as_deref(), Some("我的简历"));
        assert_eq!(writes[1].kind, MemoryKind::Relation);
        assert_eq!(writes[1].subject_type, MemoryTargetType::Entity);
        assert_eq!(writes[1].object_type, MemoryTargetType::File);
        // 统一来源与状态
        for write in &writes {
            assert_eq!(write.source_type, MemorySource::ModelInference);
            assert_eq!(write.status, MemoryStatus::Candidate);
        }
    }

    #[test]
    fn unresolvable_targets_are_dropped_not_written() {
        let proposals = vec![
            proposal(
                MemoryKind::Alias,
                "不存在的文件.pdf",
                None,
                None,
                Some("我的简历"),
                0.95,
                MemoryStatus::Candidate,
            ),
            proposal(
                MemoryKind::Relation,
                "张三",
                Some("is_about"),
                Some("周晨.pdf"),
                None,
                0.7,
                MemoryStatus::Candidate,
            ),
            proposal(
                MemoryKind::Relation,
                "周晨",
                Some("is_about"),
                Some("不存在.pdf"),
                None,
                0.7,
                MemoryStatus::Candidate,
            ),
        ];
        let writes = resolve_proposal_targets(proposals, &registry());
        assert!(writes.is_empty(), "无法解析到合法目标的一律丢弃，绝不写悬空记忆");
    }

    #[test]
    fn entity_to_entity_relations_resolve_when_both_entities_known() {
        let mut entity_registry = registry();
        entity_registry.entities.push(entity("北京大学"));
        let proposals = vec![proposal(
            MemoryKind::Relation,
            "周晨",
            Some("graduated_from"),
            Some("北京大学"),
            None,
            0.85,
            MemoryStatus::Candidate,
        )];
        let writes = resolve_proposal_targets(proposals, &entity_registry);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].object_type, MemoryTargetType::Entity);
        assert_eq!(writes[0].predicate, "graduated_from");
    }

    #[test]
    fn alias_to_collection_resolves_when_collection_in_registry() {
        // Phase 2 欠项：collection registry 可用性——别名目标可指向收藏集
        let mut collection_registry = registry();
        let collection_id = uuid::Uuid::now_v7();
        collection_registry
            .collections
            .push((collection_id, "法律项目"));
        let proposals = vec![proposal(
            MemoryKind::Alias,
            "法律项目",
            None,
            None,
            Some("法务库"),
            0.9,
            MemoryStatus::Candidate,
        )];
        let writes = resolve_proposal_targets(proposals, &collection_registry);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].subject_type, MemoryTargetType::Collection);
        assert_eq!(writes[0].subject_id, collection_id);
        assert_eq!(writes[0].alias.as_deref(), Some("法务库"));
    }

    #[test]
    fn alias_to_unknown_collection_is_dropped() {
        let mut collection_registry = registry();
        collection_registry.collections.push((uuid::Uuid::now_v7(), "法律项目"));
        let proposals = vec![proposal(
            MemoryKind::Alias,
            "不存在的收藏集",
            None,
            None,
            Some("法务库"),
            0.9,
            MemoryStatus::Candidate,
        )];
        let writes = resolve_proposal_targets(proposals, &collection_registry);
        assert!(writes.is_empty(), "不在 registry 里的收藏集名绝不写入");
    }

    #[test]
    fn name_matching_is_whitespace_and_case_insensitive() {
        let proposals = vec![proposal(
            MemoryKind::Alias,
            " 合同范本-2026.pdf ",
            None,
            None,
            Some("LEGAL  PROJECT"),
            0.9,
            MemoryStatus::Candidate,
        )];
        let writes = resolve_proposal_targets(proposals, &registry());
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].subject_type, MemoryTargetType::File);
        assert_eq!(writes[0].alias.as_deref(), Some("legalproject"));
    }
}
