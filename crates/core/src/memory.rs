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

use crate::AppError;

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
    pub status: MemoryStatus,
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

// ============================================================
// Memory Summary ViewModel（Phase 4.2 spec 二十六 / 二十七）
// ============================================================

/// 记忆摘要（用户可读形态）：Memory UI 的核心单位是「翻翻记得什么」
/// 的一句话自然语言摘要，而不是数据库记录。前端不组合底层三张表。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemorySummary {
    /// 稳定视图标识："alias:<uuid>" / "relation:<uuid>"（前端操作回传用，
    /// 不向普通 UI 暴露内部 UUID 本体以外的表结构细节）
    pub id: String,
    /// 卡片标题（如「我的简历」「法律项目」）
    pub title: String,
    /// 一句话自然语言摘要（如「“我的简历”通常指向《周晨-大模型开发工程师.pdf》」）
    pub summary: String,
    /// 内部分类：file_alias / file_relation / entity_alias / entity_relation / other
    pub kind: String,
    /// confirmed → 正式记忆；candidate → 待确认（「翻翻猜测」区）
    pub status: MemoryStatus,
    /// 来源的人话标签（「你在对话中确认过」「你主动设置」「翻翻的推测」等）
    pub source_label: String,
    /// 目标的显示名（文件名 / 收藏集名 / 实体名；实体别名时可为空）
    pub target_display_name: Option<String>,
    /// 目标当前是否可用（文件离线/删除/越权 → false，UI 显示不可用状态）
    pub target_available: bool,
    pub updated_at: DateTime<Utc>,
}

/// 来源 → 用户可读标签（spec 二十六：不展示 predicate/source_id 等术语）。
pub fn memory_source_label(source: MemorySource) -> &'static str {
    match source {
        MemorySource::UserExplicit => "你主动设置",
        MemorySource::UserConfirmed => "你在对话中确认过",
        MemorySource::UserSelection => "你选择过该文件",
        MemorySource::RepeatedUsage => "多次使用后自动记住",
        MemorySource::DocumentInference => "翻翻从资料中推测",
        MemorySource::ModelInference => "翻翻的推测",
    }
}

/// 目标显示名兜底。
fn display_name_or_placeholder(name: Option<&str>) -> String {
    name.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "一份已不可用的资料".to_owned())
}

/// 别名 → 记忆摘要视图。`target_name` 为目标文件/收藏集显示名。
pub fn alias_summary_view(
    alias: &MemoryAlias,
    target_name: Option<&str>,
    target_available: bool,
) -> MemorySummary {
    let target = display_name_or_placeholder(target_name);
    MemorySummary {
        id: format!("alias:{}", alias.alias_id),
        title: alias.alias.clone(),
        summary: format!("“{}”通常指向《{}》", alias.alias, target),
        kind: match alias.target_type {
            MemoryTargetType::File => "file_alias",
            MemoryTargetType::Collection => "collection_alias",
            MemoryTargetType::Entity => "entity_alias",
        }
        .to_owned(),
        status: alias.status,
        source_label: memory_source_label(alias.source_type).to_owned(),
        target_display_name: Some(target),
        target_available,
        updated_at: alias.updated_at,
    }
}

/// 关系 → 记忆摘要视图。`subject_name` / `object_name` 为两端显示名
///（文件名 / 收藏集名 / 实体规范名；缺省回退占位）。
pub fn relation_summary_view(
    relation: &MemoryRelation,
    subject_name: Option<&str>,
    object_name: Option<&str>,
    target_available: bool,
) -> MemorySummary {
    let subject = display_name_or_placeholder(subject_name);
    let object = display_name_or_placeholder(object_name);
    MemorySummary {
        id: format!("relation:{}", relation.relation_id),
        title: subject.clone(),
        summary: format!(
            "《{subject}》与《{object}》存在关联（{}）",
            relation.predicate
        ),
        kind: match (relation.subject_type, relation.object_type) {
            (MemoryTargetType::File, MemoryTargetType::File)
            | (MemoryTargetType::File, MemoryTargetType::Collection)
            | (MemoryTargetType::Collection, MemoryTargetType::File) => "file_relation",
            _ => "entity_relation",
        }
        .to_owned(),
        status: relation.status,
        source_label: memory_source_label(relation.source_type).to_owned(),
        target_display_name: Some(object),
        target_available,
        updated_at: relation.updated_at,
    }
}

// ============================================================
// Memory Settings（Phase 4.2 spec 二十五：「使用记忆」总开关）
// ============================================================

/// 记忆设置（持久化于应用配置目录 memory.json，与主题设置同一存储模式；
/// 不另造独立配置文件）。关闭时：不删除 Memory、Resolver/Writer 不参与
/// 新 Ask；重新开启后原 Memory 继续可用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySettings {
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

/// 记忆默认开启（已有行为不因升级改变）。
fn default_memory_enabled() -> bool {
    true
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: default_memory_enabled(),
            updated_at: None,
        }
    }
}

/// 记忆设置服务：读/原子写 memory.json（与 ThemeService 同构）。
#[derive(Debug, Clone)]
pub struct MemorySettingsService {
    state_file: std::path::PathBuf,
}

impl MemorySettingsService {
    /// `config_dir` 为应用配置目录（与 ThemeService 共用的同一目录）。
    pub fn new(config_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            state_file: config_dir.into().join("memory.json"),
        }
    }

    pub fn get(&self) -> Result<MemorySettings, AppError> {
        if !self.state_file.exists() {
            return Ok(MemorySettings::default());
        }
        let bytes = std::fs::read(&self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| AppError::local_config(error.to_string(), true))
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<MemorySettings, AppError> {
        let settings = MemorySettings {
            enabled,
            updated_at: Some(Utc::now()),
        };
        let parent = self
            .state_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let temporary = self.state_file.with_extension("json.new");
        let bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|error| AppError::local_config(error.to_string(), false))?;
        std::fs::write(&temporary, bytes)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if self.state_file.exists() {
            std::fs::remove_file(&self.state_file)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        std::fs::rename(&temporary, &self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        Ok(settings)
    }
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
            assert_eq!(MemoryStatus::parse_storage(status.as_storage()), status);
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
            assert_eq!(MemoryTargetType::parse_storage(target.as_storage()), target);
        }
    }

    /// spec 三十九 1/4/5：默认开启；关闭 → 持久化；重新打开 → 设置恢复。
    /// 设置只存自身状态文件，不触碰 Memory 数据（关闭不删除）。
    #[test]
    fn memory_settings_roundtrip_and_default_enabled() {
        let directory =
            std::env::temp_dir().join(format!("fanfan-memory-settings-{}", Uuid::now_v7()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create temp dir");

        let service = MemorySettingsService::new(&directory);
        // 文件缺失 → 默认开启（既有行为不因升级改变）
        assert!(service.get().expect("default settings").enabled);

        // 关闭 → 持久化到 memory.json
        let updated = service.set_enabled(false).expect("disable memory");
        assert!(!updated.enabled);
        assert!(updated.updated_at.is_some());
        assert!(directory.join("memory.json").exists());
        assert!(!service.get().expect("read after disable").enabled);

        // 重新打开 → 原设置继续可用
        let restored = service.set_enabled(true).expect("enable memory");
        assert!(restored.enabled);
        assert!(service.get().expect("read after enable").enabled);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// spec 三十九 6/12/13：confirmed 摘要正常展示；目标不可用反映在
    /// target_available；摘要不泄露内部 UUID（id 前缀只是视图标识，
    /// 普通 UI 不展示）。
    #[test]
    fn alias_summary_view_is_user_friendly() {
        let now = Utc::now();
        let alias = MemoryAlias {
            alias_id: Uuid::now_v7(),
            alias: "我的简历".to_owned(),
            target_type: MemoryTargetType::File,
            target_id: Uuid::now_v7(),
            confidence: 0.95,
            status: MemoryStatus::Confirmed,
            source_type: MemorySource::UserConfirmed,
            source_id: None,
            hit_count: 2,
            last_used_at: None,
            created_at: now,
            updated_at: now,
        };
        let summary = alias_summary_view(&alias, Some("周晨-大模型开发工程师.pdf"), true);
        assert_eq!(summary.title, "我的简历");
        assert!(summary.summary.contains("周晨-大模型开发工程师.pdf"));
        assert_eq!(summary.source_label, "你在对话中确认过");
        assert!(summary.target_available);
        assert_eq!(summary.status, MemoryStatus::Confirmed);
        // 自然语言摘要里不得出现裸 UUID
        assert!(!summary.summary.contains(&alias.target_id.to_string()));

        // 目标不可用：available=false + 显示名兜底
        let stale = alias_summary_view(&alias, None, false);
        assert!(!stale.target_available);
        assert!(stale.summary.contains("已不可用"));
    }
}
