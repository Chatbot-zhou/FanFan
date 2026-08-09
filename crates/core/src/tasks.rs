use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{AppError, ExplorationCandidate, ExtractionRunResult, JobRecord, ValidationCheckpoint};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDefinition {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub risk_level: String,
    pub source_files_readonly: bool,
    pub export_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanSkillRequest {
    #[serde(default)]
    pub task_id: Option<Uuid>,
    pub skill_id: String,
    pub file_ids: Vec<Uuid>,
    pub parameters: Value,
    pub user_instruction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStep {
    pub step_id: Uuid,
    pub ordinal: u32,
    pub step_type: String,
    pub label: String,
    pub inputs: Value,
    pub expected_outputs: Value,
    pub status: String,
    pub attempt_count: u32,
    pub checkpoint: String,
    pub error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskPlan {
    pub task_id: Uuid,
    pub skill_id: String,
    pub skill_version: String,
    pub summary: String,
    pub steps: Vec<TaskStep>,
    pub estimated_file_count: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskExecutionResult {
    pub plan: TaskPlan,
    pub job: JobRecord,
    pub result: ExtractionRunResult,
    pub checkpoints: Vec<ValidationCheckpoint>,
    pub candidates: Vec<ExplorationCandidate>,
}

pub fn registered_skills() -> Vec<SkillDefinition> {
    vec![
        ready(
            "batch_field_extraction",
            "批量字段抽取",
            "使用固定规则模板逐字段抽取，并保留来源。",
            true,
        ),
        ready(
            "generate_catalog",
            "生成资料目录",
            "从文件元数据生成可复核目录，并显式导出新文件。",
            true,
        ),
        ready(
            "duplicate_review",
            "重复文件审查",
            "仅对选中且同大小的资料计算SHA-256，生成待人工确认的重复候选。",
            false,
        ),
        ready(
            "multi_document_summary",
            "多文档摘要",
            "从每份资料提取带逐段来源的保守摘要，不补充外部知识。",
            true,
        ),
        ready(
            "version_compare",
            "多版本内容对比",
            "以最早修改版本为基准比较正文块增删，并保留双侧来源。",
            true,
        ),
        ready(
            "recommend_filename",
            "推荐文件名",
            "比较当前名称、正文标题和保守回退路径，只输出建议。",
            true,
        ),
        ready(
            "recommend_folders",
            "推荐目录结构",
            "比较元数据、正文关键词和保守回退路径，只输出虚拟集合建议。",
            true,
        ),
        ready(
            "merge_tables",
            "合并表格并导出",
            "按每张表的首行表头对齐Word与Excel表格，保留原始值与来源。",
            true,
        ),
        ready(
            "rerun_ocr",
            "重新 OCR",
            "使用Windows本地OCR强制重新识别选中的图片或PDF，并原子替换派生索引。",
            false,
        ),
        ready(
            "export_index",
            "导出知识库索引",
            "导出经过复核的文件元数据索引，不包含正文和内部模型数据。",
            true,
        ),
    ]
}

pub fn plan_skill(request: &PlanSkillRequest) -> Result<TaskPlan, AppError> {
    if request.file_ids.is_empty() || request.file_ids.len() > 500 {
        return Err(AppError::new(
            "TASK_FILES_INVALID",
            "任务需要选择1到500份资料",
            false,
        ));
    }
    if request.file_ids.iter().collect::<HashSet<_>>().len() != request.file_ids.len() {
        return Err(AppError::new(
            "TASK_FILES_DUPLICATED",
            "任务范围包含重复文件",
            false,
        ));
    }
    if matches!(
        request.skill_id.as_str(),
        "duplicate_review" | "version_compare"
    ) && request.file_ids.len() < 2
    {
        return Err(AppError::new(
            "TASK_COMPARISON_FILES_INVALID",
            "重复审查或版本对比至少需要选择2份资料",
            false,
        ));
    }
    let skill = registered_skills()
        .into_iter()
        .find(|skill| skill.skill_id == request.skill_id)
        .ok_or_else(|| AppError::new("TASK_SKILL_NOT_FOUND", "处理能力未注册", false))?;
    if !skill.available {
        return Err(AppError::new(
            "TASK_SKILL_UNAVAILABLE",
            skill
                .unavailable_reason
                .unwrap_or_else(|| "处理能力当前不可用".into()),
            false,
        ));
    }
    let mut steps = vec![
        step(
            1,
            "scope.validate",
            "验证资料权限与当前修订",
            json!({"file_ids": request.file_ids}),
            json!({"authorized": true}),
            "permission.source_readonly",
        ),
        step(
            2,
            "input.snapshot",
            "固定本次处理输入快照",
            json!({"count": request.file_ids.len()}),
            json!({"revision_ids": "non_empty"}),
            "invariant.revision_current",
        ),
    ];
    match request.skill_id.as_str() {
        "batch_field_extraction"
        | "generate_catalog"
        | "recommend_filename"
        | "recommend_folders"
        | "export_index" => {
            steps.push(step(
                3,
                "extraction.rules_first",
                "逐文件执行规则抽取",
                request.parameters.clone(),
                json!({"every_non_empty_value_has_evidence": true}),
                "evidence.field_level",
            ));
            steps.push(step(
                4,
                "result.review",
                "生成应用内复核表",
                json!({}),
                json!({"exported": false}),
                "schema.extraction_result",
            ));
        }
        "duplicate_review" => {
            steps.push(step(
                3,
                "relation.hash",
                "按文件大小分组并计算SHA-256",
                json!({}),
                json!({"source_files_modified": false}),
                "invariant.source_hash_unchanged",
            ));
            steps.push(step(
                4,
                "result.review",
                "生成重复与版本候选清单",
                json!({}),
                json!({"automatic_delete": false}),
                "schema.relation_result",
            ));
        }
        "version_compare" => {
            steps.push(step(
                3,
                "comparison.chunk_diff",
                "以最早修改版本为基准比较正文块增删",
                json!({}),
                json!({"two_sided_evidence": true}),
                "evidence.version_diff",
            ));
            steps.push(step(
                4,
                "result.review",
                "生成多版本差异复核表",
                json!({}),
                json!({"source_files_modified": false}),
                "schema.version_comparison",
            ));
        }
        "merge_tables" => {
            steps.push(step(
                3,
                "table.align",
                "按表头对齐表格行并保留原始值",
                json!({}),
                json!({"missing_columns": "empty", "type_coercion": false}),
                "evidence.table_row",
            ));
            steps.push(step(
                4,
                "result.review",
                "生成合并表格复核结果",
                json!({}),
                json!({"exported": false}),
                "schema.merged_table",
            ));
        }
        "rerun_ocr" => {
            steps.push(step(
                3,
                "ocr.force",
                "使用本地OCR重新识别并校验当前文件修订",
                json!({"ocr_policy": "force"}),
                json!({"source_files_modified": false}),
                "evidence.ocr_output",
            ));
            steps.push(step(
                4,
                "result.review",
                "生成OCR页数与字符数报告",
                json!({}),
                json!({"index_committed": true}),
                "schema.ocr_report",
            ));
        }
        "multi_document_summary" => {
            steps.push(step(
                3,
                "retrieval.evidence",
                "检索每份资料的可引用内容",
                json!({}),
                json!({"citations": "required"}),
                "evidence.claim_coverage",
            ));
            steps.push(step(
                4,
                "summary.compose",
                "在当前能力下生成摘要或严格摘录",
                json!({}),
                json!({"fallback": "extractive"}),
                "quality.grounded_only",
            ));
        }
        _ => {
            return Err(AppError::new(
                "TASK_SKILL_UNAVAILABLE",
                "处理能力当前不可执行",
                false,
            ));
        }
    }
    Ok(TaskPlan {
        task_id: request.task_id.unwrap_or_else(Uuid::now_v7),
        skill_id: request.skill_id.clone(),
        skill_version: "1.0.0".into(),
        summary: format!("对{}份资料执行“{}”", request.file_ids.len(), skill.name),
        steps,
        estimated_file_count: request.file_ids.len() as u64,
        warnings: vec![
            "任务只读取源文件；产生的结果先在应用内复核，导出需要再次由你选择保存位置。".into(),
        ],
    })
}

fn ready(id: &str, name: &str, description: &str, export_required: bool) -> SkillDefinition {
    SkillDefinition {
        skill_id: id.into(),
        name: name.into(),
        description: description.into(),
        available: true,
        unavailable_reason: None,
        risk_level: "low".into(),
        source_files_readonly: true,
        export_required,
    }
}

fn step(
    ordinal: u32,
    step_type: &str,
    label: &str,
    inputs: Value,
    expected_outputs: Value,
    checkpoint: &str,
) -> TaskStep {
    TaskStep {
        step_id: Uuid::now_v7(),
        ordinal,
        step_type: step_type.into(),
        label: label.into(),
        inputs,
        expected_outputs,
        status: "pending".into(),
        attempt_count: 0,
        checkpoint: checkpoint.into(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn available_skill_has_atomic_plan_and_checkpoints() {
        let result = plan_skill(&PlanSkillRequest {
            task_id: None,
            skill_id: "batch_field_extraction".into(),
            file_ids: vec![Uuid::now_v7()],
            parameters: json!({"preset_id":"contact_clues"}),
            user_instruction: None,
        })
        .unwrap();
        assert_eq!(result.steps.len(), 4);
        assert!(result.steps.iter().all(|step| !step.checkpoint.is_empty()));
    }
    #[test]
    fn all_v1_skills_are_executable_with_atomic_plans() {
        let skills = registered_skills();
        assert_eq!(skills.len(), 10);
        assert!(skills.iter().all(|skill| skill.available));
        for skill in skills {
            let count = if matches!(
                skill.skill_id.as_str(),
                "duplicate_review" | "version_compare"
            ) {
                2
            } else {
                1
            };
            let plan = plan_skill(&PlanSkillRequest {
                task_id: None,
                skill_id: skill.skill_id,
                file_ids: (0..count).map(|_| Uuid::now_v7()).collect(),
                parameters: json!({"preset_id":"contact_clues"}),
                user_instruction: None,
            })
            .expect("V1 skill should produce a plan");
            assert_eq!(plan.steps.len(), 4);
            assert!(plan.steps.iter().all(|step| !step.checkpoint.is_empty()));
        }
    }
}
