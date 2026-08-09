use std::collections::HashSet;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{AppError, EvidenceRef, FileRecord, SourceLocator};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionField {
    pub key: String,
    pub label: String,
    pub field_type: String,
    pub description: String,
    pub required: bool,
    pub multiple: bool,
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionPreset {
    pub preset_id: String,
    pub name: String,
    pub description: String,
    pub fields: Vec<ExtractionField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionRunRequest {
    pub file_ids: Vec<Uuid>,
    pub preset_id: String,
}

impl ExtractionRunRequest {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.file_ids.is_empty() || self.file_ids.len() > 500 {
            return Err(AppError::new(
                "EXTRACTION_FILES_INVALID",
                "每次需要选择1到500份资料",
                false,
            ));
        }
        if self.file_ids.iter().collect::<HashSet<_>>().len() != self.file_ids.len() {
            return Err(AppError::new(
                "EXTRACTION_FILES_DUPLICATED",
                "抽取范围包含重复的文件标识",
                false,
            ));
        }
        if preset_by_id(&self.preset_id).is_none() {
            return Err(AppError::new(
                "EXTRACTION_PRESET_NOT_FOUND",
                "抽取模板不存在",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ExtractionChunk {
    pub node_id: Uuid,
    pub chunk_id: Uuid,
    pub node_type: String,
    pub text: String,
    pub locator: SourceLocator,
}

#[derive(Debug, Clone)]
pub struct ExtractionTable {
    pub node_id: Uuid,
    pub ordinal: u32,
    pub table_data: Value,
    pub locator: SourceLocator,
}

#[derive(Debug, Clone)]
pub struct ExtractionDocument {
    pub file: FileRecord,
    pub revision_id: Uuid,
    pub chunks: Vec<ExtractionChunk>,
    pub tables: Vec<ExtractionTable>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractedValue {
    pub field_key: String,
    pub raw_value: Value,
    pub normalized_value: Value,
    pub confidence: f32,
    pub method: String,
    pub review_state: String,
    pub evidence: Vec<EvidenceRef>,
    pub validation_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionRow {
    pub file: FileRecord,
    pub values: Vec<ExtractedValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionRunResult {
    pub run_id: Uuid,
    pub preset: ExtractionPreset,
    pub status: String,
    pub rows: Vec<ExtractionRow>,
    pub completed_at: DateTime<Utc>,
    pub warnings: Vec<String>,
}

pub fn extraction_presets() -> Vec<ExtractionPreset> {
    vec![
        ExtractionPreset {
            preset_id: "file_catalog".into(),
            name: "资料目录".into(),
            description: "抽取文件名、类型、修改时间、大小和路径，用于生成资料清单。".into(),
            fields: vec![
                field("file_name", "文件名", "string", true, false),
                field("extension", "类型", "string", true, false),
                field("modified_at", "修改时间", "datetime", true, false),
                field("size_bytes", "字节数", "integer", true, false),
                field("path", "原始路径", "string", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "duplicate_review".into(),
            name: "重复文件审查".into(),
            description: "按字节数与SHA-256列出完全重复候选，不自动删除任何文件。".into(),
            fields: vec![
                field("file_name", "文件名", "string", true, false),
                field("size_bytes", "字节数", "integer", true, false),
                field("content_sha256", "SHA-256", "string", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "version_compare".into(),
            name: "多版本内容对比".into(),
            description: "以最早修改版本为基准，比较带来源的正文块增删。".into(),
            fields: vec![
                field("file_name", "文件名", "string", true, false),
                field("modified_at", "修改时间", "datetime", true, false),
                field("version_diff", "相对基准的内容变化", "object", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "merge_tables".into(),
            name: "合并表格".into(),
            description: "按表头对齐Word与Excel表格行，保留来源文件、工作表和原始值。".into(),
            fields: vec![
                field("source_file", "来源文件", "string", true, false),
                field("sheet_name", "工作表/表格", "string", false, false),
                field("row_number", "原始行号", "integer", true, false),
                field("row_data", "按表头对齐的数据", "object", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "ocr_report".into(),
            name: "重新 OCR".into(),
            description: "强制重新识别图片或PDF，并报告写入索引的页数与字符数。".into(),
            fields: vec![
                field("file_name", "文件名", "string", true, false),
                field("ocr_status", "OCR状态", "string", true, false),
                field("ocr_page_count", "识别页数", "integer", true, false),
                field("ocr_character_count", "识别字符数", "integer", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "contact_clues".into(),
            name: "联系方式".into(),
            description: "从正文中查找电子邮箱和中国大陆手机号码，每个值都保留原文位置。".into(),
            fields: vec![
                field("emails", "电子邮箱", "list", false, true),
                field("phones", "手机号码", "list", false, true),
            ],
        },
        ExtractionPreset {
            preset_id: "dates_amounts".into(),
            name: "日期与金额".into(),
            description: "从正文中查找常见中文日期和带币种/货币符号的金额，供人工复核。".into(),
            fields: vec![
                field("dates", "日期", "list", false, true),
                field("amounts", "金额", "list", false, true),
            ],
        },
        ExtractionPreset {
            preset_id: "extractive_summary".into(),
            name: "资料摘要".into(),
            description: "从每份资料的开头提取可追溯摘要，不补充原文之外的信息。".into(),
            fields: vec![field("summary", "摘录式摘要", "string", false, false)],
        },
        ExtractionPreset {
            preset_id: "filename_suggestions".into(),
            name: "文件名建议".into(),
            description: "根据文件名与正文标题给出安全的新名称建议，只展示建议，不修改源文件。"
                .into(),
            fields: vec![
                field("file_name", "当前文件名", "string", true, false),
                field("suggested_name", "建议文件名", "string", true, false),
            ],
        },
        ExtractionPreset {
            preset_id: "folder_suggestions".into(),
            name: "目录建议".into(),
            description: "根据文件类型、名称和正文关键词给出虚拟集合建议，不移动源文件。".into(),
            fields: vec![
                field("suggested_collection", "建议集合", "string", true, false),
                field("alternative_collections", "备选集合", "list", false, true),
            ],
        },
    ]
}

pub fn preset_by_id(preset_id: &str) -> Option<ExtractionPreset> {
    extraction_presets()
        .into_iter()
        .find(|preset| preset.preset_id == preset_id)
}

pub fn run_rules_first_extraction(
    request: &ExtractionRunRequest,
    documents: Vec<ExtractionDocument>,
) -> Result<ExtractionRunResult, AppError> {
    request.validate()?;
    let preset = preset_by_id(&request.preset_id)
        .ok_or_else(|| AppError::new("EXTRACTION_PRESET_NOT_FOUND", "抽取模板不存在", false))?;
    if request.preset_id == "merge_tables" {
        return merge_table_rows(preset, documents);
    }
    let baseline = (request.preset_id == "version_compare")
        .then(|| {
            documents
                .iter()
                .min_by_key(|document| document.file.fs_modified_at)
                .cloned()
        })
        .flatten();
    let mut rows = Vec::with_capacity(documents.len());
    for document in documents {
        let values = preset
            .fields
            .iter()
            .map(|field| {
                if field.key == "version_diff" {
                    version_diff(field, &document, baseline.as_ref().unwrap_or(&document))
                } else {
                    extract_field(field, &document)
                }
            })
            .collect::<Vec<_>>();
        rows.push(ExtractionRow {
            file: document.file,
            values,
        });
    }
    Ok(ExtractionRunResult {
        run_id: Uuid::now_v7(),
        preset,
        status: "completed".into(),
        rows,
        completed_at: Utc::now(),
        warnings: vec!["规则抽取结果不会修改源文件；低置信度或多值字段请在导出前复核。".into()],
    })
}

fn extract_field(field: &ExtractionField, document: &ExtractionDocument) -> ExtractedValue {
    match field.key.as_str() {
        "file_name" => metadata_value(field, json!(document.file.display_name), document),
        "extension" => metadata_value(field, json!(document.file.extension), document),
        "modified_at" => metadata_value(field, json!(document.file.fs_modified_at), document),
        "size_bytes" => metadata_value(field, json!(document.file.size_bytes), document),
        "path" => metadata_value(field, json!(document.file.canonical_path), document),
        "content_sha256" => document
            .file
            .content_sha256
            .as_ref()
            .map(|hash| metadata_value(field, json!(hash), document))
            .unwrap_or_else(|| missing_value(field)),
        "emails" => regex_values(field, document, email_regex()),
        "phones" => regex_values(field, document, phone_regex()),
        "dates" => regex_values(field, document, date_regex()),
        "amounts" => regex_values(field, document, amount_regex()),
        "summary" => extractive_summary(field, document),
        "suggested_name" => filename_suggestion(field, document),
        "suggested_collection" => folder_suggestion(field, document, false),
        "alternative_collections" => folder_suggestion(field, document, true),
        "ocr_status" => ocr_status(field, document),
        "ocr_page_count" => ocr_metric(field, document, true),
        "ocr_character_count" => ocr_metric(field, document, false),
        _ => missing_value(field),
    }
}

fn ocr_chunks(document: &ExtractionDocument) -> Vec<&ExtractionChunk> {
    document
        .chunks
        .iter()
        .filter(|chunk| chunk.node_type == "ocr_line")
        .collect()
}

fn ocr_status(field: &ExtractionField, document: &ExtractionDocument) -> ExtractedValue {
    let chunks = ocr_chunks(document);
    if let Some(chunk) = chunks.first() {
        content_value(
            field,
            document,
            chunk,
            json!("OCR 已写入索引"),
            "windows_ocr",
            1.0,
        )
    } else {
        metadata_value(field, json!("未识别到文字"), document)
    }
}

fn ocr_metric(
    field: &ExtractionField,
    document: &ExtractionDocument,
    pages: bool,
) -> ExtractedValue {
    let chunks = ocr_chunks(document);
    let value = if pages {
        chunks
            .iter()
            .filter_map(|chunk| chunk.locator.page_no)
            .collect::<HashSet<_>>()
            .len() as u64
    } else {
        chunks
            .iter()
            .map(|chunk| chunk.text.chars().count() as u64)
            .sum()
    };
    if let Some(chunk) = chunks.first() {
        content_value(field, document, chunk, json!(value), "windows_ocr", 1.0)
    } else {
        metadata_value(field, json!(value), document)
    }
}

fn version_diff(
    field: &ExtractionField,
    document: &ExtractionDocument,
    baseline: &ExtractionDocument,
) -> ExtractedValue {
    if document.file.file_id == baseline.file.file_id {
        return metadata_value(
            field,
            json!({"baseline_file": baseline.file.display_name, "added": [], "removed": []}),
            document,
        );
    }
    let baseline_text = baseline
        .chunks
        .iter()
        .map(|chunk| chunk.text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<HashSet<_>>();
    let current_text = document
        .chunks
        .iter()
        .map(|chunk| chunk.text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<HashSet<_>>();
    let added = document
        .chunks
        .iter()
        .filter(|chunk| !chunk.text.trim().is_empty() && !baseline_text.contains(chunk.text.trim()))
        .take(20)
        .collect::<Vec<_>>();
    let removed = baseline
        .chunks
        .iter()
        .filter(|chunk| !chunk.text.trim().is_empty() && !current_text.contains(chunk.text.trim()))
        .take(20)
        .collect::<Vec<_>>();
    let value = json!({
        "baseline_file": baseline.file.display_name,
        "added": added.iter().map(|chunk| truncate_chars(chunk.text.trim(), 180)).collect::<Vec<_>>(),
        "removed": removed.iter().map(|chunk| truncate_chars(chunk.text.trim(), 180)).collect::<Vec<_>>(),
    });
    let evidence = added
        .iter()
        .map(|chunk| chunk_evidence(document, chunk, 0.9))
        .chain(
            removed
                .iter()
                .map(|chunk| chunk_evidence(baseline, chunk, 0.9)),
        )
        .collect::<Vec<_>>();
    if evidence.is_empty() {
        return metadata_value(field, value, document);
    }
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: value.clone(),
        normalized_value: value,
        confidence: 0.9,
        method: "chunk_diff".into(),
        review_state: "needs_review".into(),
        evidence,
        validation_errors: Vec::new(),
    }
}

fn chunk_evidence(
    document: &ExtractionDocument,
    chunk: &ExtractionChunk,
    score: f32,
) -> EvidenceRef {
    EvidenceRef {
        evidence_id: Uuid::now_v7(),
        file_id: document.file.file_id,
        revision_id: document.revision_id,
        node_id: chunk.node_id,
        chunk_id: chunk.chunk_id,
        quote: truncate_chars(chunk.text.trim(), 220),
        locator: chunk.locator.clone(),
        retrieval_score: score,
    }
}

fn merge_table_rows(
    preset: ExtractionPreset,
    documents: Vec<ExtractionDocument>,
) -> Result<ExtractionRunResult, AppError> {
    let mut rows = Vec::new();
    for document in documents {
        let mut sheet_headers = std::collections::HashMap::<String, Vec<String>>::new();
        for table in &document.tables {
            let groups = if let Some(table_rows) =
                table.table_data.get("rows").and_then(Value::as_array)
            {
                table_rows.clone()
            } else if let Some(cells) = table.table_data.get("cells").and_then(Value::as_array) {
                vec![Value::Array(cells.clone())]
            } else {
                continue;
            };
            let group_name = table
                .locator
                .sheet_name
                .clone()
                .unwrap_or_else(|| format!("表格{}", table.ordinal));
            for (row_index, row) in groups.iter().enumerate() {
                let Some(cells) = row.as_array() else {
                    continue;
                };
                let cell_values = cells.iter().map(display_json).collect::<Vec<_>>();
                if !sheet_headers.contains_key(&group_name) {
                    let headers = cell_values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            if value.trim().is_empty() {
                                format!("列{}", index + 1)
                            } else {
                                value.clone()
                            }
                        })
                        .collect();
                    sheet_headers.insert(group_name.clone(), headers);
                    continue;
                }
                let headers = &sheet_headers[&group_name];
                let aligned = headers
                    .iter()
                    .enumerate()
                    .map(|(index, header)| {
                        (
                            header.clone(),
                            json!(cell_values.get(index).cloned().unwrap_or_default()),
                        )
                    })
                    .collect::<serde_json::Map<String, Value>>();
                let evidence = EvidenceRef {
                    evidence_id: Uuid::now_v7(),
                    file_id: document.file.file_id,
                    revision_id: document.revision_id,
                    node_id: table.node_id,
                    chunk_id: Uuid::nil(),
                    quote: truncate_chars(&Value::Object(aligned.clone()).to_string(), 220),
                    locator: table.locator.clone(),
                    retrieval_score: 1.0,
                };
                let make_value =
                    |field_key: &str, value: Value, with_evidence: bool| ExtractedValue {
                        field_key: field_key.into(),
                        raw_value: value.clone(),
                        normalized_value: value,
                        confidence: 1.0,
                        method: "table_header_alignment".into(),
                        review_state: "needs_review".into(),
                        evidence: with_evidence
                            .then(|| evidence.clone())
                            .into_iter()
                            .collect(),
                        validation_errors: Vec::new(),
                    };
                rows.push(ExtractionRow {
                    file: document.file.clone(),
                    values: vec![
                        make_value("source_file", json!(document.file.display_name), true),
                        make_value("sheet_name", json!(group_name), true),
                        make_value("row_number", json!(row_index + 1), true),
                        make_value("row_data", Value::Object(aligned), true),
                    ],
                });
                if rows.len() >= 50_000 {
                    break;
                }
            }
            if rows.len() >= 50_000 {
                break;
            }
        }
        if rows.len() >= 50_000 {
            break;
        }
    }
    Ok(ExtractionRunResult {
        run_id: Uuid::now_v7(),
        preset,
        status: "completed".into(),
        rows,
        completed_at: Utc::now(),
        warnings: vec![
            "表格按首行表头对齐；缺失列保留为空，类型与原始值保持不变，导出前请复核。".into(),
        ],
    })
}

fn extractive_summary(field: &ExtractionField, document: &ExtractionDocument) -> ExtractedValue {
    let chunks = document
        .chunks
        .iter()
        .filter(|chunk| !chunk.text.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        return missing_value(field);
    }
    let summary = chunks
        .iter()
        .map(|chunk| truncate_chars(chunk.text.trim(), 180))
        .collect::<Vec<_>>()
        .join(" ");
    let evidence = chunks
        .iter()
        .map(|chunk| EvidenceRef {
            evidence_id: Uuid::now_v7(),
            file_id: document.file.file_id,
            revision_id: document.revision_id,
            node_id: chunk.node_id,
            chunk_id: chunk.chunk_id,
            quote: truncate_chars(chunk.text.trim(), 220),
            locator: chunk.locator.clone(),
            retrieval_score: 1.0,
        })
        .collect();
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: json!(summary),
        normalized_value: json!(summary),
        confidence: 0.82,
        method: "extractive".into(),
        review_state: "needs_review".into(),
        evidence,
        validation_errors: Vec::new(),
    }
}

fn filename_suggestion(field: &ExtractionField, document: &ExtractionDocument) -> ExtractedValue {
    let Some(chunk) = document
        .chunks
        .iter()
        .find(|chunk| !chunk.text.trim().is_empty())
    else {
        return metadata_value(field, json!(document.file.display_name), document);
    };
    let title = chunk
        .text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(&document.file.display_name);
    let sanitized = title
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let base = truncate_chars(
        sanitized
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim(),
        60,
    );
    let suggested = if base.is_empty() {
        document.file.display_name.clone()
    } else if document.file.extension.is_empty() {
        base
    } else {
        format!("{base}.{}", document.file.extension)
    };
    content_value(
        field,
        document,
        chunk,
        json!(suggested),
        "content_heading",
        0.78,
    )
}

fn folder_suggestion(
    field: &ExtractionField,
    document: &ExtractionDocument,
    alternatives: bool,
) -> ExtractedValue {
    let searchable = format!(
        "{} {}",
        document.file.display_name,
        document
            .chunks
            .iter()
            .take(5)
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    let primary = if searchable.contains("发票") || searchable.contains("invoice") {
        "财务凭证"
    } else if searchable.contains("合同") || searchable.contains("协议") {
        "合同材料"
    } else if searchable.contains("简历") || searchable.contains("面试") {
        "求职资料"
    } else if searchable.contains("会议") || searchable.contains("纪要") {
        "会议记录"
    } else if matches!(
        document.file.extension.as_str(),
        "xlsx" | "xlsm" | "csv" | "tsv"
    ) {
        "表格数据"
    } else if matches!(document.file.extension.as_str(), "pptx" | "pptm") {
        "演示资料"
    } else {
        "项目资料"
    };
    let value = if alternatives {
        json!([primary, "待处理", "最近修改"])
    } else {
        json!(primary)
    };
    if let Some(chunk) = document.chunks.first() {
        content_value(field, document, chunk, value, "metadata_keyword", 0.74)
    } else {
        metadata_value(field, value, document)
    }
}

fn content_value(
    field: &ExtractionField,
    document: &ExtractionDocument,
    chunk: &ExtractionChunk,
    value: Value,
    method: &str,
    confidence: f32,
) -> ExtractedValue {
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: value.clone(),
        normalized_value: value,
        confidence,
        method: method.into(),
        review_state: "needs_review".into(),
        evidence: vec![EvidenceRef {
            evidence_id: Uuid::now_v7(),
            file_id: document.file.file_id,
            revision_id: document.revision_id,
            node_id: chunk.node_id,
            chunk_id: chunk.chunk_id,
            quote: truncate_chars(chunk.text.trim(), 220),
            locator: chunk.locator.clone(),
            retrieval_score: confidence,
        }],
        validation_errors: Vec::new(),
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut result = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        result.push('…');
    }
    result
}

fn metadata_value(
    field: &ExtractionField,
    value: Value,
    document: &ExtractionDocument,
) -> ExtractedValue {
    let locator = SourceLocator::default();
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: value.clone(),
        normalized_value: value.clone(),
        confidence: 1.0,
        method: "metadata".into(),
        review_state: "auto".into(),
        evidence: vec![EvidenceRef {
            evidence_id: Uuid::now_v7(),
            file_id: document.file.file_id,
            revision_id: document.revision_id,
            node_id: Uuid::nil(),
            chunk_id: Uuid::nil(),
            quote: format!("文件元数据：{} = {}", field.label, display_json(&value)),
            locator,
            retrieval_score: 1.0,
        }],
        validation_errors: Vec::new(),
    }
}

fn regex_values(
    field: &ExtractionField,
    document: &ExtractionDocument,
    regex: &Regex,
) -> ExtractedValue {
    let mut values = Vec::<String>::new();
    let mut evidence = Vec::new();
    for chunk in &document.chunks {
        for matched in regex.find_iter(&chunk.text) {
            let value = matched.as_str().trim().to_owned();
            if values.iter().any(|current| current == &value) {
                continue;
            }
            values.push(value.clone());
            evidence.push(EvidenceRef {
                evidence_id: Uuid::now_v7(),
                file_id: document.file.file_id,
                revision_id: document.revision_id,
                node_id: chunk.node_id,
                chunk_id: chunk.chunk_id,
                quote: context_around(&chunk.text, matched.start(), matched.end(), 70),
                locator: chunk.locator.clone(),
                retrieval_score: 1.0,
            });
            if values.len() >= 20 {
                break;
            }
        }
        if values.len() >= 20 {
            break;
        }
    }
    if values.is_empty() {
        return missing_value(field);
    }
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: json!(values),
        normalized_value: json!(values),
        confidence: 0.88,
        method: "regex".into(),
        review_state: "needs_review".into(),
        evidence,
        validation_errors: Vec::new(),
    }
}

fn missing_value(field: &ExtractionField) -> ExtractedValue {
    ExtractedValue {
        field_key: field.key.clone(),
        raw_value: Value::Null,
        normalized_value: Value::Null,
        confidence: 0.0,
        method: "rules".into(),
        review_state: "missing".into(),
        evidence: Vec::new(),
        validation_errors: if field.required {
            vec!["必填字段未找到".into()]
        } else {
            Vec::new()
        },
    }
}

fn field(
    key: &str,
    label: &str,
    field_type: &str,
    required: bool,
    multiple: bool,
) -> ExtractionField {
    ExtractionField {
        key: key.into(),
        label: label.into(),
        field_type: field_type.into(),
        description: label.into(),
        required,
        multiple,
        hints: Vec::new(),
    }
}

fn display_json(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn context_around(text: &str, start: usize, end: usize, radius: usize) -> String {
    let mut left = start.saturating_sub(radius);
    while left > 0 && !text.is_char_boundary(left) {
        left -= 1;
    }
    let mut right = (end + radius).min(text.len());
    while right < text.len() && !text.is_char_boundary(right) {
        right += 1;
    }
    format!(
        "{}{}{}",
        if left > 0 { "…" } else { "" },
        &text[left..right],
        if right < text.len() { "…" } else { "" }
    )
}

fn email_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}").unwrap())
}

fn phone_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?:\+?86[- ]?)?1[3-9]\d{9}").unwrap())
}

fn date_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(
            r"(?:20\d{2}|19\d{2})[年./-]\s?(?:0?[1-9]|1[0-2])[月./-]\s?(?:0?[1-9]|[12]\d|3[01])日?",
        )
        .unwrap()
    })
}

fn amount_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?:人民币|RMB|CNY|¥|￥)\s?[0-9]+(?:,[0-9]{3})*(?:\.[0-9]{1,2})?|[0-9]+(?:,[0-9]{3})*(?:\.[0-9]{1,2})?\s?元").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Availability, ParseStatus, SourceKind};

    #[test]
    fn rules_extract_values_with_field_level_evidence() {
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let document = ExtractionDocument {
            file: FileRecord {
                file_id,
                volume_id: "v".into(),
                canonical_path: "D:\\资料\\合同.txt".into(),
                display_name: "合同.txt".into(),
                extension: "txt".into(),
                mime_type: "text/plain".into(),
                size_bytes: 12,
                fs_created_at: None,
                fs_modified_at: Utc::now(),
                windows_file_id: None,
                content_sha256: None,
                availability: Availability::Present,
                current_revision_id: Some(revision_id),
                parse_status: ParseStatus::Parsed,
                first_seen_at: Utc::now(),
                last_seen_at: Utc::now(),
            },
            revision_id,
            chunks: vec![ExtractionChunk {
                node_id: Uuid::now_v7(),
                chunk_id: Uuid::now_v7(),
                node_type: "paragraph".into(),
                text: "联系人 test@example.com，电话 13800138000，金额人民币 1,200.50 元。".into(),
                locator: SourceLocator {
                    kind: SourceKind::Text,
                    line_start: Some(1),
                    ..Default::default()
                },
            }],
            tables: vec![],
        };
        let result = run_rules_first_extraction(
            &ExtractionRunRequest {
                file_ids: vec![file_id],
                preset_id: "contact_clues".into(),
            },
            vec![document],
        )
        .unwrap();
        assert_eq!(result.rows[0].values[0].evidence.len(), 1);
        assert!(
            result.rows[0].values[0]
                .normalized_value
                .to_string()
                .contains("test@example.com")
        );
        assert!(
            result.rows[0].values[1]
                .normalized_value
                .to_string()
                .contains("13800138000")
        );
    }

    #[test]
    fn advisory_presets_are_source_readonly_and_evidence_backed() {
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let document = ExtractionDocument {
            file: FileRecord {
                file_id,
                volume_id: "v".into(),
                canonical_path: "D:\\资料\\旧合同.txt".into(),
                display_name: "旧合同.txt".into(),
                extension: "txt".into(),
                mime_type: "text/plain".into(),
                size_bytes: 32,
                fs_created_at: None,
                fs_modified_at: Utc::now(),
                windows_file_id: None,
                content_sha256: None,
                availability: Availability::Present,
                current_revision_id: Some(revision_id),
                parse_status: ParseStatus::Parsed,
                first_seen_at: Utc::now(),
                last_seen_at: Utc::now(),
            },
            revision_id,
            chunks: vec![ExtractionChunk {
                node_id: Uuid::now_v7(),
                chunk_id: Uuid::now_v7(),
                node_type: "paragraph".into(),
                text: "项目:计划/2026\n这是一份合同材料，记录双方的交付安排。".into(),
                locator: SourceLocator {
                    kind: SourceKind::Text,
                    line_start: Some(1),
                    ..Default::default()
                },
            }],
            tables: vec![],
        };
        for preset_id in [
            "extractive_summary",
            "filename_suggestions",
            "folder_suggestions",
        ] {
            let result = run_rules_first_extraction(
                &ExtractionRunRequest {
                    file_ids: vec![file_id],
                    preset_id: preset_id.into(),
                },
                vec![document.clone()],
            )
            .expect("run advisory preset");
            assert!(
                result.rows[0].values.iter().all(|value| {
                    value.normalized_value.is_null() || !value.evidence.is_empty()
                })
            );
            assert!(
                result.rows[0]
                    .values
                    .iter()
                    .all(|value| value.review_state == "auto"
                        || value.review_state == "needs_review")
            );
        }

        let filename = run_rules_first_extraction(
            &ExtractionRunRequest {
                file_ids: vec![file_id],
                preset_id: "filename_suggestions".into(),
            },
            vec![document.clone()],
        )
        .expect("suggest filename");
        let suggested = filename.rows[0].values[1]
            .normalized_value
            .as_str()
            .expect("suggested filename");
        assert_eq!(suggested, "项目 计划 2026.txt");
        assert!(!suggested.chars().any(|character| matches!(
            character,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        )));

        let folder = run_rules_first_extraction(
            &ExtractionRunRequest {
                file_ids: vec![file_id],
                preset_id: "folder_suggestions".into(),
            },
            vec![document],
        )
        .expect("suggest collection");
        assert_eq!(folder.rows[0].values[0].normalized_value, json!("合同材料"));
    }

    fn comparison_document(
        name: &str,
        modified_at: DateTime<Utc>,
        text: &str,
    ) -> ExtractionDocument {
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        ExtractionDocument {
            file: FileRecord {
                file_id,
                volume_id: "v".into(),
                canonical_path: format!("D:\\资料\\{name}"),
                display_name: name.into(),
                extension: "docx".into(),
                mime_type:
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
                size_bytes: 128,
                fs_created_at: None,
                fs_modified_at: modified_at,
                windows_file_id: None,
                content_sha256: None,
                availability: Availability::Present,
                current_revision_id: Some(revision_id),
                parse_status: ParseStatus::Parsed,
                first_seen_at: modified_at,
                last_seen_at: modified_at,
            },
            revision_id,
            chunks: vec![ExtractionChunk {
                node_id: Uuid::now_v7(),
                chunk_id: Uuid::now_v7(),
                node_type: "paragraph".into(),
                text: text.into(),
                locator: SourceLocator {
                    kind: SourceKind::Docx,
                    paragraph_no: Some(1),
                    ..Default::default()
                },
            }],
            tables: vec![],
        }
    }

    #[test]
    fn version_compare_keeps_added_and_removed_evidence() {
        let baseline = comparison_document(
            "方案-v1.docx",
            Utc::now() - chrono::Duration::days(1),
            "旧版交付范围",
        );
        let current = comparison_document("方案-v2.docx", Utc::now(), "新版交付范围");
        let result = run_rules_first_extraction(
            &ExtractionRunRequest {
                file_ids: vec![baseline.file.file_id, current.file.file_id],
                preset_id: "version_compare".into(),
            },
            vec![current.clone(), baseline.clone()],
        )
        .expect("compare versions");
        let current_row = result
            .rows
            .iter()
            .find(|row| row.file.file_id == current.file.file_id)
            .unwrap();
        let diff = current_row
            .values
            .iter()
            .find(|value| value.field_key == "version_diff")
            .unwrap();
        assert!(
            diff.normalized_value["added"]
                .to_string()
                .contains("新版交付范围")
        );
        assert!(
            diff.normalized_value["removed"]
                .to_string()
                .contains("旧版交付范围")
        );
        assert_eq!(diff.evidence.len(), 2);
    }

    #[test]
    fn merge_tables_aligns_headers_and_preserves_row_evidence() {
        let mut document = comparison_document("台账.docx", Utc::now(), "表格台账");
        document.tables.push(ExtractionTable {
            node_id: Uuid::now_v7(),
            ordinal: 1,
            table_data: json!({"rows": [["姓名", "金额"], ["甲", "10"], ["乙", "20"]]}),
            locator: SourceLocator {
                kind: SourceKind::Docx,
                paragraph_no: Some(2),
                ..Default::default()
            },
        });
        let result = run_rules_first_extraction(
            &ExtractionRunRequest {
                file_ids: vec![document.file.file_id],
                preset_id: "merge_tables".into(),
            },
            vec![document],
        )
        .expect("merge table rows");
        assert_eq!(result.rows.len(), 2);
        let row_data = result.rows[0]
            .values
            .iter()
            .find(|value| value.field_key == "row_data")
            .unwrap();
        assert_eq!(row_data.normalized_value["姓名"], json!("甲"));
        assert_eq!(row_data.normalized_value["金额"], json!("10"));
        assert_eq!(row_data.evidence.len(), 1);
    }
}
