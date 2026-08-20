//! Context Resolver：把 AMBIGUOUS 请求结合会话工作上下文恢复为 LOCAL。
//!
//! Source Router 判定「指代不清 → AMBIGUOUS」后，本模块用
//! [`AskSessionContext`] 恢复目标：上一轮定位/引用的文件、文档类型。
//! 恢复成功即锁定 `file_ids` 白名单，禁止重新全库搜索；恢复失败返回
//! `Unresolved`（P0 由编排层安全回退 GENERAL_CHAT，不猜文件）。
//!
//! 纯函数，无 IO；会话上下文由编排层从存储读取后传入。

use crate::AskSessionContext;
use crate::ask::query_plan::{QueryIntent, ResolutionStatus, SourceIntent};
use crate::contracts::DocumentType;

/// 恢复依据的会话上下文信号（用于 tracing）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextSignal {
    /// `active_file_id` 单独命中（最强：上一轮明确锁定的文件）
    ActiveFile,
    /// `active_file_ids` 命中（上一轮多文件工作集）
    ActiveFileSet,
    /// `last_referenced_file_ids` 命中（引用过但未锁定，较弱）
    LastReferenced,
    /// 只有 `active_document_type`（需要再按类型找文件）
    ActiveDocumentType,
}

/// Context Resolver 的输出。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ContextResolution {
    /// Resolved = 已从会话上下文恢复目标；Unresolved = 无可恢复依据。
    pub status: ResolutionStatus,
    /// 恢复后的来源（成功即 Local；失败为 General 兜底）。
    pub source: SourceIntent,
    /// 恢复后的意图（按文档语义固定为 DocumentQa，编排层可再被 Query Parser 细化）。
    pub intent: QueryIntent,
    /// 恢复出的文件白名单（scope.file_ids 直接来源）。
    pub resolved_file_ids: Vec<uuid::Uuid>,
    /// 恢复出的文档类型（仅当有 active_document_type 时）。
    pub resolved_document_type: Option<DocumentType>,
    pub confidence: f32,
    /// 命中的信号；Unresolved 时为 None。
    pub signal: Option<ContextSignal>,
    /// Unresolved 时的原因（P0 只读：不猜文件）。
    pub fallback_reason: Option<String>,
}

impl ContextResolution {
    pub fn is_resolved(&self) -> bool {
        self.status == ResolutionStatus::Resolved
    }
}

/// 结合会话工作上下文解析 AMBIGUOUS 请求。
///
/// 优先级：active_file_id > active_file_ids > last_referenced_file_ids >
/// active_document_type > 无法恢复。
pub fn resolve_ambiguous(context: &AskSessionContext) -> ContextResolution {
    if let Some(active_file_id) = context.active_file_id {
        let mut file_ids = context.active_file_ids.clone();
        if !file_ids.contains(&active_file_id) {
            file_ids.push(active_file_id);
        }
        ContextResolution {
            status: ResolutionStatus::Resolved,
            source: SourceIntent::Local,
            intent: QueryIntent::DocumentQa,
            resolved_file_ids: file_ids,
            resolved_document_type: context.active_document_type,
            confidence: 0.95,
            signal: Some(ContextSignal::ActiveFile),
            fallback_reason: None,
        }
    } else if !context.active_file_ids.is_empty() {
        ContextResolution {
            status: ResolutionStatus::Resolved,
            source: SourceIntent::Local,
            intent: QueryIntent::DocumentQa,
            resolved_file_ids: context.active_file_ids.clone(),
            resolved_document_type: context.active_document_type,
            confidence: 0.9,
            signal: Some(ContextSignal::ActiveFileSet),
            fallback_reason: None,
        }
    } else if !context.last_referenced_file_ids.is_empty() {
        ContextResolution {
            status: ResolutionStatus::Resolved,
            source: SourceIntent::Local,
            intent: QueryIntent::DocumentQa,
            resolved_file_ids: context.last_referenced_file_ids.clone(),
            resolved_document_type: context.active_document_type,
            confidence: 0.75,
            signal: Some(ContextSignal::LastReferenced),
            fallback_reason: None,
        }
    } else if let Some(document_type) = context.active_document_type {
        ContextResolution {
            status: ResolutionStatus::Resolved,
            source: SourceIntent::Local,
            intent: QueryIntent::DocumentQa,
            resolved_file_ids: Vec::new(),
            resolved_document_type: Some(document_type),
            confidence: 0.6,
            signal: Some(ContextSignal::ActiveDocumentType),
            fallback_reason: None,
        }
    } else {
        ContextResolution {
            status: ResolutionStatus::Unresolved,
            source: SourceIntent::General,
            intent: QueryIntent::GeneralChat,
            resolved_file_ids: Vec::new(),
            resolved_document_type: None,
            confidence: 0.0,
            signal: None,
            fallback_reason: Some("会话上下文为空，指代无法恢复为具体文件".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> AskSessionContext {
        AskSessionContext::default()
    }

    fn with_active_file() -> AskSessionContext {
        AskSessionContext {
            active_file_id: Some(uuid::Uuid::now_v7()),
            ..AskSessionContext::default()
        }
    }

    #[test]
    fn active_file_recovers_local_qa_with_file_scope() {
        let context = with_active_file();
        let resolution = resolve_ambiguous(&context);
        assert!(resolution.is_resolved());
        assert_eq!(resolution.source, SourceIntent::Local);
        assert_eq!(resolution.intent, QueryIntent::DocumentQa);
        assert_eq!(
            resolution.resolved_file_ids,
            vec![context.active_file_id.unwrap()]
        );
        assert_eq!(resolution.signal, Some(ContextSignal::ActiveFile));
        assert!((resolution.confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn active_file_merges_with_active_file_set() {
        let file_a = uuid::Uuid::now_v7();
        let file_b = uuid::Uuid::now_v7();
        let context = AskSessionContext {
            active_file_id: Some(file_a),
            active_file_ids: vec![file_b],
            ..AskSessionContext::default()
        };
        let resolution = resolve_ambiguous(&context);
        // 白名单 = active_file_ids ∪ {active_file_id}
        assert!(resolution.resolved_file_ids.contains(&file_a));
        assert!(resolution.resolved_file_ids.contains(&file_b));
    }

    #[test]
    fn active_file_set_recovers_without_single_active() {
        let file_b = uuid::Uuid::now_v7();
        let context = AskSessionContext {
            active_file_ids: vec![file_b],
            ..AskSessionContext::default()
        };
        let resolution = resolve_ambiguous(&context);
        assert!(resolution.is_resolved());
        assert_eq!(resolution.resolved_file_ids, vec![file_b]);
        assert_eq!(resolution.signal, Some(ContextSignal::ActiveFileSet));
    }

    #[test]
    fn last_referenced_recovers_with_lower_confidence() {
        let file_c = uuid::Uuid::now_v7();
        let context = AskSessionContext {
            last_referenced_file_ids: vec![file_c],
            ..AskSessionContext::default()
        };
        let resolution = resolve_ambiguous(&context);
        assert!(resolution.is_resolved());
        assert_eq!(resolution.resolved_file_ids, vec![file_c]);
        assert_eq!(resolution.signal, Some(ContextSignal::LastReferenced));
        assert!((resolution.confidence - 0.75).abs() < 1e-6);
    }

    #[test]
    fn document_type_alone_recovers_for_type_based_search() {
        let context = AskSessionContext {
            active_document_type: Some(DocumentType::Resume),
            ..AskSessionContext::default()
        };
        let resolution = resolve_ambiguous(&context);
        assert!(resolution.is_resolved());
        assert_eq!(resolution.resolved_file_ids, Vec::<uuid::Uuid>::new());
        assert_eq!(
            resolution.resolved_document_type,
            Some(DocumentType::Resume)
        );
        assert_eq!(resolution.signal, Some(ContextSignal::ActiveDocumentType));
    }

    #[test]
    fn empty_context_returns_unresolved_without_guessing_files() {
        let resolution = resolve_ambiguous(&context());
        assert!(!resolution.is_resolved());
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        // P0 安全兜底：不猜文件、不假装 LOCAL
        assert_eq!(resolution.source, SourceIntent::General);
        assert_eq!(resolution.intent, QueryIntent::GeneralChat);
        assert_eq!(resolution.resolved_file_ids, Vec::<uuid::Uuid>::new());
        assert!(resolution.fallback_reason.is_some());
        assert_eq!(resolution.signal, None);
    }
}
