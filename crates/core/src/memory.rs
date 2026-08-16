//! Memory 数据层（Step 3）。
//!
//! Memory 不是第二份知识库：不复制 Chunk 正文，只保存
//! 「用户 ↔ 实体 ↔ 文件」之间的关系、别名和稳定信息，帮助**理解和定位**。
//! Memory 绝对不能作为最终事实证据——最终回答必须从原始文件 Chunk
//! 获取证据并通过 Citation Validation。
//!
//! 来源信任层级（写入/合并时低层级不能覆盖高层级）：
//! user_explicit > user_confirmed > user_selection > repeated_usage
//! > document_inference > model_inference
//!
//! 严禁：因为一个 PDF 出现「张三」→ 自动认定用户叫张三。
//! document_inference / model_inference 只能生成 candidate，
//! 绝不可直接 confirmed。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 记忆条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Relation,
    Alias,
}

/// 关系状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Candidate,
    Confirmed,
    Rejected,
    Stale,
}

impl MemoryStatus {
    pub fn as_storage(&self) -> &'static str {
        match self {
            MemoryStatus::Candidate => "candidate",
            MemoryStatus::Confirmed => "confirmed",
            MemoryStatus::Rejected => "rejected",
            MemoryStatus::Stale => "stale",
        }
    }

    pub fn parse_storage(value: &str) -> MemoryStatus {
        match value {
            "confirmed" => MemoryStatus::Confirmed,
            "rejected" => MemoryStatus::Rejected,
            "stale" => MemoryStatus::Stale,
            _ => MemoryStatus::Candidate,
        }
    }
}

/// 来源信任层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// 用户明确表达（“这是我的简历”）——最高
    UserExplicit,
    /// 用户明确确认候选（“对，就是第一份”）
    UserConfirmed,
    /// 用户在澄清选择中主动选中
    UserSelection,
    /// 同一关系被多次明确使用
    RepeatedUsage,
    /// 从文档内容推断（只允许 candidate）
    DocumentInference,
    /// 模型推断（只允许 candidate）——最低
    ModelInference,
}

impl MemorySource {
    pub fn as_storage(&self) -> &'static str {
        match self {
            MemorySource::UserExplicit => "user_explicit",
            MemorySource::UserConfirmed => "user_confirmed",
            MemorySource::UserSelection => "user_selection",
            MemorySource::RepeatedUsage => "repeated_usage",
            MemorySource::DocumentInference => "document_inference",
            MemorySource::ModelInference => "model_inference",
        }
    }

    pub fn parse_storage(value: &str) -> MemorySource {
        match value {
            "user_confirmed" => MemorySource::UserConfirmed,
            "user_selection" => MemorySource::UserSelection,
            "repeated_usage" => MemorySource::RepeatedUsage,
            "document_inference" => MemorySource::DocumentInference,
            "model_inference" => MemorySource::ModelInference,
            _ => MemorySource::UserExplicit,
        }
    }

    /// 信任等级（越大越可信）。写入合并时，低等级来源不能覆盖高等级事实。
    pub fn rank(&self) -> u8 {
        match self {
            MemorySource::UserExplicit => 6,
            MemorySource::UserConfirmed => 5,
            MemorySource::UserSelection => 4,
            MemorySource::RepeatedUsage => 3,
            MemorySource::DocumentInference => 2,
            MemorySource::ModelInference => 1,
        }
    }

    /// 该来源是否允许直接写入 confirmed（推断类只能写 candidate）。
    /// repeated_usage 也允许：它是「同一关系被多次明确使用」的用户派生信号，
    /// 不是来自文档内容的猜测。
    pub fn allows_confirmed(&self) -> bool {
        self.rank() >= MemorySource::RepeatedUsage.rank()
    }
}

/// Memory 引用的目标类型：实体 / 文件 / 收藏集。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTargetType {
    Entity,
    File,
    Collection,
}

impl MemoryTargetType {
    pub fn as_storage(&self) -> &'static str {
        match self {
            MemoryTargetType::Entity => "entity",
            MemoryTargetType::File => "file",
            MemoryTargetType::Collection => "collection",
        }
    }

    pub fn parse_storage(value: &str) -> MemoryTargetType {
        match value {
            "file" => MemoryTargetType::File,
            "collection" => MemoryTargetType::Collection,
            _ => MemoryTargetType::Entity,
        }
    }
}

/// 记忆实体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntity {
    pub entity_id: Uuid,
    pub entity_type: String,
    pub canonical_name: String,
    pub metadata_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 记忆关系（subject --predicate--> object）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRelation {
    pub relation_id: Uuid,
    pub subject_type: MemoryTargetType,
    pub subject_id: Uuid,
    pub predicate: String,
    pub object_type: MemoryTargetType,
    pub object_id: Uuid,
    pub confidence: f32,
    pub status: MemoryStatus,
    pub source_type: MemorySource,
    pub source_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 记忆别名（“我的简历” → file:A）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryAlias {
    pub alias_id: Uuid,
    pub alias: String,
    pub target_type: MemoryTargetType,
    pub target_id: Uuid,
    pub confidence: f32,
    pub source_type: MemorySource,
    pub source_id: Option<String>,
    pub hit_count: u32,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 记忆写入输入（Memory Writer 与用户确认共用）。
///
/// 调用方负责语义校验（Step 5/6）：推断类来源只能带 candidate，
/// alias 不得为空，`kind = Alias` 时 `alias` 必填。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryWriteInput {
    pub kind: MemoryKind,
    pub subject_type: MemoryTargetType,
    pub subject_id: Uuid,
    pub predicate: String,
    pub object_type: MemoryTargetType,
    pub object_id: Uuid,
    pub alias: Option<String>,
    pub confidence: f32,
    pub source_type: MemorySource,
    pub source_id: Option<String>,
    pub status: MemoryStatus,
}

/// 别名规范化：去掉**所有**空白并 ASCII 小写（中文原样）。
/// 别名是短用户短语，匹配键必须对「有没有空格/什么空格」不敏感，
/// 因此规范化后作为唯一键；空串/纯空白返回 None（调用方应拒绝写入）。
pub fn normalize_alias(value: &str) -> Option<String> {
    let normalized = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_alias_strips_all_whitespace_and_lowercases_ascii() {
        assert_eq!(normalize_alias("  我的 简历 ").as_deref(), Some("我的简历"));
        assert_eq!(normalize_alias("我的   简历").as_deref(), Some("我的简历"));
        assert_eq!(normalize_alias("MY RESUME").as_deref(), Some("myresume"));
        assert_eq!(normalize_alias("   "), None);
        assert_eq!(normalize_alias(""), None);
    }

    #[test]
    fn inference_sources_cannot_confirm() {
        assert!(!MemorySource::DocumentInference.allows_confirmed());
        assert!(!MemorySource::ModelInference.allows_confirmed());
        assert!(MemorySource::UserExplicit.allows_confirmed());
        assert!(MemorySource::UserConfirmed.allows_confirmed());
        assert!(MemorySource::UserSelection.allows_confirmed());
        assert!(MemorySource::RepeatedUsage.allows_confirmed());
    }

    #[test]
    fn source_ranking_matches_trust_chain() {
        assert!(MemorySource::UserExplicit.rank() > MemorySource::UserConfirmed.rank());
        assert!(MemorySource::UserConfirmed.rank() > MemorySource::UserSelection.rank());
        assert!(MemorySource::UserSelection.rank() > MemorySource::RepeatedUsage.rank());
        assert!(MemorySource::RepeatedUsage.rank() > MemorySource::DocumentInference.rank());
        assert!(MemorySource::DocumentInference.rank() > MemorySource::ModelInference.rank());
    }

    #[test]
    fn storage_round_trips() {
        for status in [
            MemoryStatus::Candidate,
            MemoryStatus::Confirmed,
            MemoryStatus::Rejected,
            MemoryStatus::Stale,
        ] {
            assert_eq!(
                MemoryStatus::parse_storage(status.as_storage()),
                status
            );
        }
        for source in [
            MemorySource::UserExplicit,
            MemorySource::UserConfirmed,
            MemorySource::UserSelection,
            MemorySource::RepeatedUsage,
            MemorySource::DocumentInference,
            MemorySource::ModelInference,
        ] {
            assert_eq!(MemorySource::parse_storage(source.as_storage()), source);
        }
        for target in [
            MemoryTargetType::Entity,
            MemoryTargetType::File,
            MemoryTargetType::Collection,
        ] {
            assert_eq!(
                MemoryTargetType::parse_storage(target.as_storage()),
                target
            );
        }
    }
}
