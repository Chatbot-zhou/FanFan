//! Memory Resolver（Step 4）：把「我的简历」「法律项目」这类别名与已确认的
//! 关系，解析成检索范围提示（file / collection / entity 目标）。
//!
//! 设计约束（需求文档「六、Memory 数据层」+「三、Context/Memory Resolver」）：
//! - Memory 只帮助理解和定位；**绝不能作为最终事实证据**（证据仍来自
//!   Chunk + Citation Validation）；
//! - 别名解析出的 file_id 仍必须通过合法性检查（文件存在 + present +
//!   授权根），合法性检查在编排层用存储层完成，**绝不绕过 Document Resolver
//!   同口径检查**注入目标；
//! - 只有 confirmed 关系参与定位；candidate 只等用户确认，绝不参与；
//! - 长别名优先，子串别名不抢命中（「我的简历」命中时不因「简历」重复触发）；
//! - 本模块是纯函数、无 IO：别名/关系/实体由编排层读好后传入。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::{
    MemoryAlias, MemoryEntity, MemoryRelation, MemorySource, MemoryStatus, MemoryTargetType,
    normalize_alias,
};

/// 单个记忆命中（供 trace 与 scope 构建）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHint {
    /// 命中的别名 / 实体名（规范化后的键）
    pub matched_text: String,
    /// 命中来源：`alias`（用户起的名字）或 `relation`（已确认的关系）
    pub kind: &'static str,
    pub target_type: MemoryTargetType,
    pub target_id: Uuid,
    pub confidence: f32,
    pub source_type: MemorySource,
}

/// 从问题文本中找出别名命中（纯函数）。
///
/// 规则：
/// - 问题与别名都先规范化（去所有空白、ASCII 小写），「我的 简历」=「我的简历」；
/// - 别名按长度降序匹配，已被更长别名命中的子串别名不再触发
///   （「我的简历」命中后，单独的「简历」不会产生第二个提示）；
/// - 同一别名指向多个目标时全部返回，由调用方（编排层）决定是否明确。
pub fn match_alias_hints(question: &str, aliases: &[MemoryAlias]) -> Vec<MemoryHint> {
    let Some(question_key) = normalize_alias(question) else {
        return Vec::new();
    };
    let mut sorted = aliases.to_vec();
    sorted.sort_by(|a, b| {
        b.alias
            .chars()
            .count()
            .cmp(&a.alias.chars().count())
            .then_with(|| a.alias_id.to_string().cmp(&b.alias_id.to_string()))
    });
    let mut hints = Vec::new();
    let mut matched_keys: Vec<String> = Vec::new();
    for alias in sorted {
        if matched_keys
            .iter()
            .any(|key| key.as_str() != alias.alias && key.contains(alias.alias.as_str()))
        {
            continue; // 已被更长别名覆盖的子串别名：不抢命中
        }
        if question_key.contains(&alias.alias) {
            hints.push(MemoryHint {
                matched_text: alias.alias.clone(),
                kind: "alias",
                target_type: alias.target_type,
                target_id: alias.target_id,
                confidence: alias.confidence,
                source_type: alias.source_type,
            });
            matched_keys.push(alias.alias.clone());
        }
    }
    hints
}

/// 从问题文本中找出已确认关系命中（纯函数）。
///
/// 规则：关系两端之一（实体）的名字出现在问题里 → 把关系另一端（若指向
/// file / collection）作为定位提示。只使用 `confirmed` 关系；candidate /
/// rejected / stale 一律不参与定位（STRICT：推断类只写候选，绝不自动确认）。
pub fn match_relation_hints(
    question: &str,
    entities: &[MemoryEntity],
    relations: &[MemoryRelation],
) -> Vec<MemoryHint> {
    let Some(question_key) = normalize_alias(question) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    for relation in relations
        .iter()
        .filter(|relation| relation.status == MemoryStatus::Confirmed)
    {
        // 找出关系两端中名字出现在问题里的实体
        let subject_name = entities
            .iter()
            .find(|entity| entity.entity_id == relation.subject_id)
            .map(|entity| normalize_alias(&entity.canonical_name).unwrap_or_default());
        let object_name = entities
            .iter()
            .find(|entity| entity.entity_id == relation.object_id)
            .map(|entity| normalize_alias(&entity.canonical_name).unwrap_or_default());
        let subject_mentioned = subject_name
            .as_deref()
            .is_some_and(|name| !name.is_empty() && question_key.contains(name));
        let object_mentioned = object_name
            .as_deref()
            .is_some_and(|name| !name.is_empty() && question_key.contains(name));
        // 问题提到的必须是实体端；另一端是检索目标。
        let (mentioned_entity_name, target_type, target_id) =
            if subject_mentioned && relation.object_type != MemoryTargetType::Entity {
                (
                    subject_name.unwrap_or_default(),
                    relation.object_type,
                    relation.object_id,
                )
            } else if object_mentioned && relation.subject_type != MemoryTargetType::Entity {
                (
                    object_name.unwrap_or_default(),
                    relation.subject_type,
                    relation.subject_id,
                )
            } else {
                continue;
            };
        hints.push(MemoryHint {
            matched_text: mentioned_entity_name,
            kind: "relation",
            target_type,
            target_id,
            confidence: relation.confidence,
            source_type: relation.source_type,
        });
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn alias(text: &str, target_type: MemoryTargetType, target_id: Uuid) -> MemoryAlias {
        MemoryAlias {
            alias_id: Uuid::now_v7(),
            alias: normalize_alias(text).expect("alias text"),
            target_type,
            target_id,
            confidence: 0.95,
            source_type: MemorySource::UserExplicit,
            source_id: None,
            status: MemoryStatus::Confirmed,
            hit_count: 3,
            last_used_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn entity(id: Uuid, name: &str) -> MemoryEntity {
        MemoryEntity {
            entity_id: id,
            entity_type: "person".to_owned(),
            canonical_name: name.to_owned(),
            metadata_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn relation(
        subject_type: MemoryTargetType,
        subject_id: Uuid,
        object_type: MemoryTargetType,
        object_id: Uuid,
        status: MemoryStatus,
    ) -> MemoryRelation {
        MemoryRelation {
            relation_id: Uuid::now_v7(),
            subject_type,
            subject_id,
            predicate: "is_about".to_owned(),
            object_type,
            object_id,
            confidence: 0.9,
            status,
            source_type: MemorySource::UserConfirmed,
            source_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn alias_hits_embedded_mention_with_whitespace_insensitive_key() {
        let file = Uuid::now_v7();
        let aliases = vec![alias("我的简历", MemoryTargetType::File, file)];
        let hints = match_alias_hints("我的 简历 里有没有 LangGraph？", &aliases);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].matched_text, "我的简历");
        assert_eq!(hints[0].target_type, MemoryTargetType::File);
        assert_eq!(hints[0].target_id, file);
    }

    #[test]
    fn longest_alias_wins_over_substring_alias() {
        let file = Uuid::now_v7();
        let other = Uuid::now_v7();
        let aliases = vec![
            alias("简历", MemoryTargetType::File, other),
            alias("我的简历", MemoryTargetType::File, file),
        ];
        let hints = match_alias_hints("我的简历里有什么项目？", &aliases);
        assert_eq!(hints.len(), 1, "子串别名不得抢命中");
        assert_eq!(hints[0].target_id, file);
    }

    #[test]
    fn same_alias_pointing_to_two_targets_returns_both() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let aliases = vec![
            alias("法律项目", MemoryTargetType::File, a),
            alias("法律项目", MemoryTargetType::File, b),
        ];
        let hints = match_alias_hints("法律项目的结论是什么？", &aliases);
        assert_eq!(hints.len(), 2, "同一别名多目标全部返回，由编排层裁决");
    }

    #[test]
    fn no_mention_produces_no_hints() {
        let file = Uuid::now_v7();
        let aliases = vec![alias("我的简历", MemoryTargetType::File, file)];
        assert!(match_alias_hints("Transformer 是什么？", &aliases).is_empty());
        assert!(match_alias_hints("   ", &aliases).is_empty());
    }

    #[test]
    fn confirmed_relation_points_question_entity_to_file() {
        let person = Uuid::now_v7();
        let file = Uuid::now_v7();
        let entities = vec![entity(person, "周晨")];
        let relations = vec![relation(
            MemoryTargetType::Entity,
            person,
            MemoryTargetType::File,
            file,
            MemoryStatus::Confirmed,
        )];
        let hints = match_relation_hints("周晨的项目经历有哪些？", &entities, &relations);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "relation");
        assert_eq!(hints[0].target_type, MemoryTargetType::File);
        assert_eq!(hints[0].target_id, file);
    }

    #[test]
    fn candidate_relations_never_participate_in_resolution() {
        let person = Uuid::now_v7();
        let file = Uuid::now_v7();
        let entities = vec![entity(person, "周晨")];
        let relations = vec![relation(
            MemoryTargetType::Entity,
            person,
            MemoryTargetType::File,
            file,
            MemoryStatus::Candidate,
        )];
        assert!(
            match_relation_hints("周晨是谁？", &entities, &relations).is_empty(),
            "候选关系绝不参与定位（STRICT）"
        );
    }

    #[test]
    fn entity_entity_relations_do_not_produce_scope_targets() {
        let person = Uuid::now_v7();
        let project = Uuid::now_v7();
        let entities = vec![entity(person, "周晨"), entity(project, "法律项目")];
        let relations = vec![relation(
            MemoryTargetType::Entity,
            person,
            MemoryTargetType::Entity,
            project,
            MemoryStatus::Confirmed,
        )];
        // 两端都是实体：不产生检索范围提示（没有文件/收藏集目标）
        assert!(match_relation_hints("周晨的法律项目", &entities, &relations).is_empty());
    }
}
