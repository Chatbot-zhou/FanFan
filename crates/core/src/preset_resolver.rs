//! ModelPreset → Runtime 接通的核心：把一个已选定的 `selected_preset_id`
//! 解析成一份各角色所需的 `RuntimeModelPlan`。
//!
//! 职责边界：
//! - 仅把 `ModelPreset` 的角色 catalog_id 组装成运行计划，不内嵌下载 URL。
//! - 视觉模型按「预置、仅在需要时开启」处理，从 catalog 中挑选最小的可下载
//!   Vision 条目作为预置项，避免把 `ModelPreset` 结构带上版本变更。
//! - 运行时加载必须经此计划取模型，禁止直接读旧 `model_role_config`。

use serde::{Deserialize, Serialize};

use crate::model_catalog::{ModelPreset, built_in_model_catalog, preset_by_id};
use crate::models::ModelRole;

/// 一份选定 Preset 落地到 RAG 各角色所需模型的运行计划。
/// 只引用 catalog_id；具体下载 / 加载经「ModelCatalog → Artifact」链路解析。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeModelPlan {
    /// 来源 preset_id，例如 `smooth`。
    pub preset_id: String,
    /// 文本生成模型 catalog_id。
    pub generation: String,
    /// 向量生成模型 catalog_id（换代会影响向量索引）。
    pub embedding: String,
    /// 重排序模型 catalog_id；`None` 表示该档不启用 reranker。
    pub reranker: Option<String>,
    /// OCR 模型 catalog_id。
    pub ocr: String,
    /// ASR（语音转写）模型 catalog_id；`None` 表示不启用。
    pub asr: Option<String>,
    /// 预置的视觉模型 catalog_id；仅在需要多模态问答时按需加载。
    pub vision: Option<String>,
}

impl RuntimeModelPlan {
    /// 返回该计划需要下载/就绪的全部 catalog_id（按角色顺序、去重）。
    /// 下载编排据此计算「已装/缺失」，同一 catalog_id 在跨档切换时复用。
    pub fn required_catalog_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = Vec::with_capacity(7);
        ids.push(self.generation.as_str());
        ids.push(self.embedding.as_str());
        if let Some(value) = &self.reranker {
            ids.push(value.as_str());
        }
        ids.push(self.ocr.as_str());
        if let Some(value) = &self.asr {
            ids.push(value.as_str());
        }
        if let Some(value) = &self.vision {
            ids.push(value.as_str());
        }
        ids
    }
}

/// 依据 `selected_preset_id` 解析出一份 `RuntimeModelPlan`。未知 id 返回 `None`。
///
/// 这是 `selected_preset_id` 成为真实运行入口的唯一解析入口；运行时若读不到
/// 有效计划，应视为配置异常并走迁移/默认档，而非回退到旧 `model_role_config`。
pub fn resolve_runtime_model_plan(preset_id: &str) -> Option<RuntimeModelPlan> {
    let preset = preset_by_id(preset_id)?;
    Some(RuntimeModelPlan {
        preset_id: preset.preset_id.clone(),
        generation: preset.generation.clone(),
        embedding: preset.embedding.clone(),
        reranker: preset.reranker.clone(),
        ocr: preset.ocr.clone(),
        asr: preset.asr.clone(),
        vision: resolve_preset_vision(&preset),
    })
}

/// 预置一个可下载的视觉模型（不修改 `ModelPreset` 结构）。
/// 策略：从 catalog 中选「体积最小且已锁可下载 edition」的 Vision 条目，
/// 满足『都预置、只在合适的时候开启』；需要图片问答时再按需加载，不与文本
/// 生成模型常驻抢占显存。
fn resolve_preset_vision(preset: &ModelPreset) -> Option<String> {
    let _ = preset; // 预留：后续可按 preset 档位选择同代际 VL。
    built_in_model_catalog()
        .iter()
        .filter(|entry| entry.role == ModelRole::Vision && entry.install_edition_id.is_some())
        .min_by(|a, b| a.estimated_memory_gb.total_cmp(&b.estimated_memory_gb))
        .map(|entry| entry.catalog_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_plan_maps_every_role_from_preset() {
        for preset_id in ["basic", "smooth", "balanced", "high"] {
            let plan = resolve_runtime_model_plan(preset_id)
                .unwrap_or_else(|| panic!("preset {preset_id} must resolve"));
            assert_eq!(plan.preset_id, preset_id);
            assert!(!plan.generation.is_empty());
            assert!(!plan.embedding.is_empty());
            assert!(!plan.ocr.is_empty());
        }
    }

    #[test]
    fn smooth_anchor_embeds_small_zh() {
        let plan = resolve_runtime_model_plan("smooth").expect("smooth resolves");
        assert_eq!(plan.embedding, "bge-small-zh-int8");
        // 标准模式（平滑档 smooth）复用了 base 同款 reranker，与定稿一致。
        assert_eq!(plan.reranker.as_deref(), Some("bge-reranker-base-int8"));
    }

    #[test]
    fn high_and_balanced_reranker_unified_to_base() {
        for preset_id in ["balanced", "high"] {
            let plan = resolve_runtime_model_plan(preset_id).expect("preset resolves");
            assert_eq!(plan.embedding, "bge-m3");
            assert_eq!(plan.reranker.as_deref(), Some("bge-reranker-base-int8"));
            assert_eq!(plan.ocr, "ppocr-v6-medium");
        }
    }

    #[test]
    fn plan_catalog_ids_are_deduplicated() {
        for preset_id in ["basic", "smooth", "balanced", "high"] {
            let plan = resolve_runtime_model_plan(preset_id).expect("preset resolves");
            let ids = plan.required_catalog_ids();
            let unique: std::collections::HashSet<&&str> = ids.iter().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "duplicate catalog_id in {preset_id}"
            );
        }
    }

    #[test]
    fn unknown_preset_id_is_none() {
        assert!(resolve_runtime_model_plan("does-not-exist").is_none());
    }
}
