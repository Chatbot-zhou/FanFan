//! Memory 设置 + 记忆摘要视图命令（Phase 4.2 spec 二十二~三十八）。
//!
//! 前端「设置 → 记忆」页只消费这里的 ViewModel：
//! - 「使用记忆」总开关（memory.json 持久化，关闭不删数据、不阻断会话上下文）；
//! - MemorySummary 列表（confirmed 正式记忆 / candidate 待确认两组）；
//! - confirm / reject / delete 单条操作（按 "alias:<id>" / "relation:<id>" 分发）。
//!
//! 纪律：不暴露内部表结构（predicate / source_id / 裸 UUID）给普通 UI；
//! 清空全部走既有 `memory_clear`（二次确认短语 CLEAR_MEMORY）。

use std::collections::HashMap;
use std::sync::Mutex;

use fanfan_core::{
    AppError, MemorySettings, MemorySettingsService, MemoryStatus, MemorySummary, MemoryTargetType,
    alias_summary_view, relation_summary_view,
};
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use crate::commands::app_data::CatalogServiceState;

/// 记忆设置服务状态（lib.rs manage；与 ThemeServiceState 同构）。
pub struct MemorySettingsServiceState(pub Mutex<MemorySettingsService>);

#[derive(Debug, Deserialize)]
pub struct MemorySettingsUpdateRequest {
    pub enabled: bool,
}

/// 记忆摘要列表（spec 二十八：confirmed 正常显示；candidate 单独区域）。
#[derive(Debug, Clone, Serialize)]
pub struct MemorySummaryList {
    pub confirmed: Vec<MemorySummary>,
    pub candidates: Vec<MemorySummary>,
}

/// 目标名称与可用性解析上下文（一次性装载，组装全部摘要）。
struct TargetNameIndex {
    file_names: HashMap<Uuid, String>,
    collection_names: HashMap<Uuid, String>,
    entity_names: HashMap<Uuid, String>,
}

impl TargetNameIndex {
    /// 装载文件 / 收藏集 / 实体名称映射（读取失败按空处理，不阻断列表）。
    fn load(catalog: &fanfan_core::CatalogService) -> Self {
        let file_names = catalog
            .list_files()
            .map(|files| {
                files
                    .into_iter()
                    .map(|file| (file.file_id, file.display_name))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let collection_names = catalog
            .list_collections()
            .map(|collections| {
                collections
                    .into_iter()
                    .map(|collection| (collection.collection_id, collection.name))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let entity_names = catalog
            .list_memory_entities(2000)
            .map(|entities| {
                entities
                    .into_iter()
                    .map(|entity| (entity.entity_id, entity.canonical_name))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        Self {
            file_names,
            collection_names,
            entity_names,
        }
    }

    /// 按目标类型取显示名。
    fn name(&self, target_type: MemoryTargetType, target_id: Uuid) -> Option<&str> {
        match target_type {
            MemoryTargetType::File => self.file_names.get(&target_id).map(String::as_str),
            MemoryTargetType::Collection => {
                self.collection_names.get(&target_id).map(String::as_str)
            }
            MemoryTargetType::Entity => self.entity_names.get(&target_id).map(String::as_str),
        }
    }

    /// 文件 / 收藏集目标是否仍可用（存在 + present + 授权根；实体恒有效）。
    fn available(
        &self,
        catalog: &fanfan_core::CatalogService,
        target_type: MemoryTargetType,
        target_id: Uuid,
    ) -> bool {
        match target_type {
            MemoryTargetType::File => catalog.memory_file_target_valid(target_id).unwrap_or(false),
            MemoryTargetType::Collection => catalog
                .memory_collection_target_valid(target_id)
                .unwrap_or(false),
            MemoryTargetType::Entity => true,
        }
    }
}

/// 组装全部记忆摘要（confirmed / candidate 分组；rejected 与 stale 不展示）。
fn build_summary_list(
    catalog: &fanfan_core::CatalogService,
) -> Result<MemorySummaryList, AppError> {
    let index = TargetNameIndex::load(catalog);
    let mut confirmed = Vec::new();
    let mut candidates = Vec::new();
    for alias in catalog.list_memory_aliases(500)? {
        let summary = alias_summary_view(
            &alias,
            index.name(alias.target_type, alias.target_id),
            index.available(catalog, alias.target_type, alias.target_id),
        );
        match alias.status {
            MemoryStatus::Confirmed => confirmed.push(summary),
            MemoryStatus::Candidate => candidates.push(summary),
            _ => {}
        }
    }
    for relation in catalog.list_memory_relations(None, 2000)? {
        let subject_available =
            index.available(catalog, relation.subject_type, relation.subject_id);
        let object_available = index.available(catalog, relation.object_type, relation.object_id);
        let summary = relation_summary_view(
            &relation,
            index.name(relation.subject_type, relation.subject_id),
            index.name(relation.object_type, relation.object_id),
            subject_available && object_available,
        );
        match relation.status {
            MemoryStatus::Confirmed => confirmed.push(summary),
            MemoryStatus::Candidate => candidates.push(summary),
            _ => {}
        }
    }
    confirmed.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    candidates.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    Ok(MemorySummaryList {
        confirmed,
        candidates,
    })
}

/// 解析 "alias:<uuid>" / "relation:<uuid>" 视图标识。
fn parse_summary_id(summary_id: &str) -> Result<(&str, Uuid), AppError> {
    let (kind, id) = summary_id
        .split_once(':')
        .ok_or_else(|| AppError::new("MEMORY_SUMMARY_ID_INVALID", "记忆标识无效", false))?;
    let id = Uuid::parse_str(id)
        .map_err(|_| AppError::new("MEMORY_SUMMARY_ID_INVALID", "记忆标识无效", false))?;
    match kind {
        "alias" | "relation" => Ok((kind, id)),
        _ => Err(AppError::new(
            "MEMORY_SUMMARY_ID_INVALID",
            "记忆标识无效",
            false,
        )),
    }
}

#[tauri::command(async)]
pub fn memory_settings_get(
    service: State<'_, MemorySettingsServiceState>,
) -> Result<MemorySettings, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("记忆设置状态锁不可用", true))?
        .get()
}

#[tauri::command(async)]
pub fn memory_settings_update(
    request: MemorySettingsUpdateRequest,
    service: State<'_, MemorySettingsServiceState>,
) -> Result<MemorySettings, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("记忆设置状态锁不可用", true))?
        .set_enabled(request.enabled)
}

#[tauri::command(async)]
pub fn memory_summary_list(
    catalog: State<'_, CatalogServiceState>,
) -> Result<MemorySummaryList, AppError> {
    let catalog = catalog.get()?;
    build_summary_list(&catalog)
}

#[tauri::command(async)]
pub fn memory_summary_get(
    summary_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<MemorySummary, AppError> {
    let catalog = catalog.get()?;
    let (kind, id) = parse_summary_id(&summary_id)?;
    let index = TargetNameIndex::load(&catalog);
    match kind {
        "alias" => {
            let alias = catalog.memory_alias_by_id(id)?.ok_or_else(|| {
                AppError::new("MEMORY_SUMMARY_NOT_FOUND", "这条记忆已不存在", false)
            })?;
            Ok(alias_summary_view(
                &alias,
                index.name(alias.target_type, alias.target_id),
                index.available(&catalog, alias.target_type, alias.target_id),
            ))
        }
        _ => {
            let relation = catalog.memory_relation_by_id(id)?.ok_or_else(|| {
                AppError::new("MEMORY_SUMMARY_NOT_FOUND", "这条记忆已不存在", false)
            })?;
            let subject_available =
                index.available(&catalog, relation.subject_type, relation.subject_id);
            let object_available =
                index.available(&catalog, relation.object_type, relation.object_id);
            Ok(relation_summary_view(
                &relation,
                index.name(relation.subject_type, relation.subject_id),
                index.name(relation.object_type, relation.object_id),
                subject_available && object_available,
            ))
        }
    }
}

/// 确认单条候选记忆（spec 二十九）：alias / relation 均升级 confirmed；
/// 别名同时升级来源为 user_confirmed。
#[tauri::command(async)]
pub fn memory_confirm(
    summary_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let (kind, id) = parse_summary_id(&summary_id)?;
    let catalog = catalog.get()?;
    match kind {
        "alias" => catalog.update_memory_alias_status(id, MemoryStatus::Confirmed),
        _ => catalog.update_memory_relation_status(id, MemoryStatus::Confirmed),
    }
}

/// 拒绝单条候选记忆：rejected 后不再参与 Memory Resolution。
#[tauri::command(async)]
pub fn memory_reject(
    summary_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let (kind, id) = parse_summary_id(&summary_id)?;
    let catalog = catalog.get()?;
    match kind {
        "alias" => catalog.update_memory_alias_status(id, MemoryStatus::Rejected),
        _ => catalog.update_memory_relation_status(id, MemoryStatus::Rejected),
    }
}

/// 删除单条记忆（spec 三十一）：只删对应 Memory，不动文件 / 索引 / Ask 历史。
#[tauri::command(async)]
pub fn memory_delete(
    summary_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let (kind, id) = parse_summary_id(&summary_id)?;
    let catalog = catalog.get()?;
    match kind {
        "alias" => catalog.delete_memory_alias(id),
        _ => catalog.delete_memory_relation(id),
    }
}
