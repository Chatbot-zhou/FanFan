//! DocumentProfile Builder 纯逻辑层（无 IO、无模型调用）。
//!
//! 生产链（SQL 组装在 storage.rs，本模块负责确定性文本逻辑）：
//!
//! ```text
//! parsed + 全量嵌入完成
//!   → 代表性内容 = title + section_titles + head/mid/tail chunk
//!   → representative_text_hash（sha256，内容变更即失效）
//!   → DocumentProfile 写入（revision_id 绑定当前 revision）
//! ```
//!
//! 约束（需求 Step 1 / 2）：
//! - section_titles **确定性**提取自 `document_nodes.heading_path_json`
//!   （JSON 字符串数组），绝不用 LLM；
//! - 画像生命周期绑定文件 revision：stale 画像（revision 不匹配）在
//!   检索侧被过滤，新 revision 由生产链重建；
//! - 构建失败只影响 Document Resolver 的定位能力，不影响浏览 / FTS /
//!   语义检索 / 基础 RAG。

use sha2::{Digest, Sha256};

use crate::contracts::DocumentType;

// ===========================================================================
// Step 2：文档类型分类器（规则特征 + Embedding 原型相似度）
// ===========================================================================

/// 每类型的规则关键词（确定性匹配）。文件名只作弱信号（权重最低），
/// 正文语义（title / section_titles / head text）优先。
pub const TYPE_KEYWORDS: &[(DocumentType, &[&str])] = &[
    (
        DocumentType::Resume,
        &[
            "简历", "求职意向", "教育背景", "工作经历", "项目经历", "技能特长", "自我评价",
            "resume", "curriculum vitae", "objective",
        ],
    ),
    (
        DocumentType::Contract,
        &[
            "合同", "甲方", "乙方", "条款", "违约责任", "合同期限", "签署日期", "合同编号",
            "contract", "agreement", "breach", "party",
        ],
    ),
    (
        DocumentType::Invoice,
        &[
            "发票", "发票号码", "税额", "纳税人识别号", "价税合计", "开票日期", "invoice", "tax",
            "vat",
        ],
    ),
    (
        DocumentType::Paper,
        &[
            "论文", "摘要", "引言", "参考文献", "实验方法", "doi", "paper", "abstract",
            "references", "citation",
        ],
    ),
    (
        DocumentType::ProjectDocument,
        &[
            "项目", "需求", "实施方案", "里程碑", "交付物", "项目范围", "项目计划", "project",
            "requirement", "milestone", "deliverable",
        ],
    ),
    (
        DocumentType::Meeting,
        &[
            "会议纪要", "会议议程", "参会人员", "议题", "决议", "行动项", "会议时间", "meeting",
            "minutes", "agenda",
        ],
    ),
    (
        DocumentType::LearningMaterial,
        &[
            "课程", "讲义", "教材", "知识点", "练习题", "课件", "学习目标", "course", "lecture",
            "textbook", "syllabus",
        ],
    ),
    (
        DocumentType::Certificate,
        &[
            "证书", "认证", "颁发", "考核", "资格", "证书编号", "certificate", "certification",
            "credential", "award",
        ],
    ),
    (
        DocumentType::Report,
        &[
            "报告", "总结", "分析", "结论", "建议", "统计数据", "report", "analysis", "conclusion",
            "summary",
        ],
    ),
    (
        DocumentType::Spreadsheet,
        &[
            "表格", "数据", "统计表", "汇总", "明细", "工作表", "excel", "spreadsheet", "sheet",
            "cell",
        ],
    ),
    (DocumentType::Other, &[]),
];

/// 每类型的 Embedding 原型文本（中英双语代表性句子）。
/// 桌面层用活动 Embedding 模型嵌入后取均值（L2 归一化）作为原型向量，
/// 与文档向量（文件前 3 chunk 均值）做余弦相似度。
pub const TYPE_PROTOTYPE_TEXTS: &[(DocumentType, &[&str])] = &[
    (
        DocumentType::Resume,
        &[
            "个人简历：教育背景、工作经历、项目经历、技能特长与求职意向",
            "Resume: education, work experience, projects, skills and objective",
        ],
    ),
    (
        DocumentType::Contract,
        &[
            "合同文本：甲方乙方权利义务、主要条款、违约责任与合同期限",
            "Contract agreement between parties with terms, obligations and breach liability",
        ],
    ),
    (
        DocumentType::Invoice,
        &[
            "发票：商品明细、数量金额、税率税额、纳税人信息",
            "Invoice with item details, amounts, tax rates and payer information",
        ],
    ),
    (
        DocumentType::Paper,
        &[
            "学术论文：摘要、引言、相关工作、实验方法与结果、参考文献",
            "Academic paper with abstract, introduction, experiments, results and references",
        ],
    ),
    (
        DocumentType::ProjectDocument,
        &[
            "项目文档：需求说明、实施方案、进度计划、里程碑与交付物",
            "Project document: requirements, implementation plan, schedule, milestones and deliverables",
        ],
    ),
    (
        DocumentType::Meeting,
        &[
            "会议纪要：议程议题、参会人员、讨论结论与行动项",
            "Meeting minutes: agenda, attendees, conclusions and action items",
        ],
    ),
    (
        DocumentType::LearningMaterial,
        &[
            "课程讲义：知识点讲解、例题练习与学习目标",
            "Course lecture notes with key concepts, examples and learning objectives",
        ],
    ),
    (
        DocumentType::Certificate,
        &[
            "证书：颁发机构、认证内容、考核结果与证书编号",
            "Certificate issued after examination with credential details",
        ],
    ),
    (
        DocumentType::Report,
        &[
            "报告：背景概述、数据分析、结论与建议",
            "Report with overview, data analysis, conclusion and recommendations",
        ],
    ),
    (
        DocumentType::Spreadsheet,
        &[
            "数据表格：行列数据、统计汇总、公式计算",
            "Spreadsheet with rows and columns of data, totals and formulas",
        ],
    ),
    (DocumentType::Other, &[]),
];

/// 原型相似度低于该值视为噪声，不参与判定。
pub const PROTOTYPE_MIN_SIMILARITY: f32 = 0.55;
/// 仅靠 Embedding（无规则命中）锁定类型的最低相似度。
pub const PROTOTYPE_LOCK_SIMILARITY: f32 = 0.65;
/// Embedding 相似度超过该值时压过规则（语义强信号锁定）。
pub const EMBED_OVER_RULE_SIMILARITY: f32 = 0.72;
/// 规则信号归一化分数达到该值即锁定（title/section 命中即可达到）。
pub const RULE_LOCK_CONFIDENCE: f32 = 0.5;
/// 规则分数归一化分母（title×3 + section×2 + head×1 + filename×1 的上界）。
pub const RULE_SCORE_CAP: f32 = 6.0;

/// 类型原型向量（桌面层嵌入 TYPE_PROTOTYPE_TEXTS 后按类型取均值传入）。
#[derive(Debug, Clone, Copy)]
pub struct TypePrototype<'a> {
    pub document_type: DocumentType,
    pub vector: &'a [f32],
}

/// 对单个文档画像做类型判定（纯函数，无 IO；失败返回 None → 调用方写 NULL，
/// 绝不阻塞索引）。
///
/// 组合规则（确定性）：
/// - 规则特征：TYPE_KEYWORDS 在 title（×3）/ section_titles（×2）/
///   head text（×1）/ filename（×1）中的命中数归一化；
/// - Embedding 原型：文档向量与每类型原型向量的余弦相似度；
/// - 规则与原型一致 → 置信度取二者较高者并上调；
/// - 原型相似度足够高可单独锁定（语义强信号）；
/// - 规则命中足够强可单独锁定（title/section 多关键词）；
/// - 其余情况返回 None（不猜、不返回 Other）。
pub fn classify_document_type(
    title: &str,
    filename: &str,
    section_titles: &[String],
    head_text: &str,
    vector: &[f32],
    prototypes: &[TypePrototype<'_>],
) -> Option<(DocumentType, f32)> {
    let rule = best_rule_type(title, filename, section_titles, head_text);
    let embed = best_prototype_type(vector, prototypes);

    match (rule, embed) {
        // 规则与原型一致：置信度取两者较高者（0.6 起跳，双方一致本身就是强证据）
        (Some((rule_type, rule_conf)), Some((embed_type, embed_sim)))
            if rule_type == embed_type =>
        {
            Some((rule_type, 0.6 + 0.4 * rule_conf.max(embed_sim)))
        }
        // 原型相似度非常高：语义强信号压过规则
        (Some(_), Some((embed_type, embed_sim))) if embed_sim >= EMBED_OVER_RULE_SIMILARITY => {
            Some((embed_type, embed_sim))
        }
        // 规则命中足够强：独立锁定
        (Some((rule_type, rule_conf)), _) if rule_conf >= RULE_LOCK_CONFIDENCE => {
            Some((rule_type, rule_conf))
        }
        // 无规则命中：原型达到锁定阈值才敢下结论
        (None, Some((embed_type, embed_sim))) if embed_sim >= PROTOTYPE_LOCK_SIMILARITY => {
            Some((embed_type, embed_sim))
        }
        _ => None,
    }
}

/// 规则特征打分：返回 (类型, 归一化置信度)。归一化 = hits / RULE_SCORE_CAP。
fn best_rule_type(
    title: &str,
    filename: &str,
    section_titles: &[String],
    head_text: &str,
) -> Option<(DocumentType, f32)> {
    let mut best: Option<(DocumentType, f32)> = None;
    for (document_type, keywords) in TYPE_KEYWORDS {
        if keywords.is_empty() {
            continue;
        }
        let mut hits = 0.0;
        for keyword in *keywords {
            if title.contains(keyword) {
                hits += 3.0;
            }
            if section_titles
                .iter()
                .any(|section| section.contains(keyword))
            {
                hits += 2.0;
            }
            if head_text.contains(keyword) {
                hits += 1.0;
            }
            // 文件名是最弱信号：命中一次只计 1（且单独命中远不足以锁定）
            if filename.contains(keyword) {
                hits += 1.0;
            }
        }
        if hits <= 0.0 {
            continue;
        }
        let confidence = (hits / RULE_SCORE_CAP).min(1.0);
        if best.as_ref().is_none_or(|(_, current)| confidence > *current) {
            best = Some((*document_type, confidence));
        }
    }
    best
}

/// Embedding 原型相似度：返回相似度最高的 (类型, 余弦相似度)。
fn best_prototype_type<'a>(
    vector: &[f32],
    prototypes: &'a [TypePrototype<'_>],
) -> Option<(DocumentType, f32)> {
    let mut best: Option<(DocumentType, f32)> = None;
    for prototype in prototypes {
        let similarity = cosine_similarity(vector, prototype.vector);
        if similarity < PROTOTYPE_MIN_SIMILARITY {
            continue;
        }
        if best.as_ref().is_none_or(|(_, current)| similarity > *current) {
            best = Some((prototype.document_type, similarity));
        }
    }
    best
}

/// 余弦相似度（两个向量都应为 L2 归一化向量，容错未归一化输入）。
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm_a.is_finite() || !norm_b.is_finite() || norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

/// section_titles 的最大条数（超过后截断，避免画像无限膨胀）。
pub const MAX_SECTION_TITLES: usize = 64;
/// 单个 chunk 文本进代表性内容时的字符上限（与 compact_profile_text 一致）。
pub const REPRESENTATIVE_CHUNK_LIMIT: usize = 260;
/// 代表性文本总字符上限（title + sections + 3 chunk 均被压缩到该界内）。
pub const REPRESENTATIVE_TEXT_LIMIT: usize = 4_000;

/// 从 (ordinal, heading_path_json) 行提取 section_titles：
/// - 解析每个节点的 heading_path（JSON 字符串数组，解析失败跳过）；
/// - 取每条 path 的**叶子标题**（最后一级），空 path 跳过；
/// - 按文档顺序去重（保留首次出现），超过 [`MAX_SECTION_TITLES`] 截断。
///
/// 确定性：相同输入必然相同输出，无模型参与。
pub fn extract_section_titles<'a>(rows: impl Iterator<Item = (u64, &'a str)>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut titles = Vec::new();
    for (_ordinal, heading_path_json) in rows {
        if titles.len() >= MAX_SECTION_TITLES {
            break;
        }
        let Ok(path) = serde_json::from_str::<Vec<String>>(heading_path_json) else {
            continue;
        };
        let Some(leaf) = path.last().map(String::as_str).map(str::trim) else {
            continue;
        };
        if leaf.is_empty() || !seen.insert(leaf.to_owned()) {
            continue;
        }
        titles.push(leaf.to_owned());
    }
    titles
}

/// 选出代表性 chunk：头 / 中 / 尾（按文档顺序传入）。少于 3 个时
/// 缺位用已有 chunk 补齐（绝不越界）。返回 (head, mid, tail)。
pub fn pick_head_mid_tail(chunks: &[String]) -> (String, String, String) {
    let head = chunks.first().cloned().unwrap_or_default();
    let tail = chunks.last().cloned().unwrap_or_default();
    let mid = if chunks.len() >= 3 {
        chunks[chunks.len() / 2].clone()
    } else if chunks.len() == 2 {
        chunks[0].clone()
    } else {
        head.clone()
    };
    (head, mid, tail)
}

/// 组装代表性文本：title + section titles + head/mid/tail chunk，
/// 全部压缩到长度上限后拼接。只用于生成 representative_text_hash，
/// 不作为可引用证据（画像定位用，S# 只能来自原始 chunk）。
pub fn build_representative_text(
    title: &str,
    section_titles: &[String],
    head: &str,
    mid: &str,
    tail: &str,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(2 + section_titles.len());
    let title = title.trim();
    if !title.is_empty() {
        parts.push(compact(title, REPRESENTATIVE_CHUNK_LIMIT));
    }
    let sections = section_titles
        .iter()
        .take(MAX_SECTION_TITLES)
        .map(|value| compact(value, REPRESENTATIVE_CHUNK_LIMIT))
        .collect::<Vec<_>>();
    if !sections.is_empty() {
        parts.push(sections.join("；"));
    }
    for chunk in [head, mid, tail] {
        let chunk = compact(chunk, REPRESENTATIVE_CHUNK_LIMIT);
        if !chunk.is_empty() {
            parts.push(chunk);
        }
    }
    let text = parts.join(" || ");
    compact(&text, REPRESENTATIVE_TEXT_LIMIT)
}

/// 代表性文本的 sha256 十六进制摘要（revision 内内容变化 → hash 变化 →
/// 画像与内容一致性的确定性校验依据）。
pub fn representative_text_hash(text: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    format!("{:x}", digest.finalize())
}

/// 空白折叠 + 字符截断（与 storage.rs 的 compact_profile_text 同口径）。
fn compact(value: &str, limit: usize) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        collapsed
    } else {
        collapsed.chars().take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prototypes() -> Vec<TypePrototype<'static>> {
        vec![
            TypePrototype {
                document_type: DocumentType::Resume,
                vector: &[1.0, 0.0, 0.0, 0.0],
            },
            TypePrototype {
                document_type: DocumentType::Contract,
                vector: &[0.0, 1.0, 0.0, 0.0],
            },
            TypePrototype {
                document_type: DocumentType::Paper,
                vector: &[0.0, 0.0, 1.0, 0.0],
            },
        ]
    }

    #[test]
    fn classify_resume_by_rule_keywords_ignoring_filename() {
        // 正文（title + sections）命中简历关键词 → Resume；文件名即使不含简历也不影响
        let sections = vec!["项目经历".to_owned(), "教育背景".to_owned()];
        let result = classify_document_type(
            "大模型开发工程师",
            "大模型开发工程师-周晨.pdf", // 文件名无「简历」
            &sections,
            "软件工程师简历",
            &[0.2, 0.1, 0.1, 0.0],
            &prototypes(),
        );
        let (document_type, confidence) = result.expect("rule locks resume");
        assert_eq!(document_type, DocumentType::Resume);
        assert!(confidence >= RULE_LOCK_CONFIDENCE);
    }

    #[test]
    fn classify_contract_by_rule_keywords() {
        let sections = vec!["甲方乙方".to_owned(), "违约责任".to_owned()];
        let result = classify_document_type(
            "房屋租赁合同",
            "房屋租赁合同.pdf",
            &sections,
            "合同条款",
            &[0.1, 0.2, 0.1, 0.0],
            &prototypes(),
        );
        let (document_type, _) = result.expect("contract locked by rules");
        assert_eq!(document_type, DocumentType::Contract);
    }

    #[test]
    fn filename_alone_is_weak_and_never_locks() {
        // 只有文件名含「简历」→ 弱信号不足（0.17 < 0.5），原型不匹配 → None
        let result = classify_document_type(
            "学习笔记",
            "如何写好简历.pdf",
            &[],
            "这篇文章讲解写作技巧",
            &[0.0, 0.1, 0.1, 0.0],
            &prototypes(),
        );
        assert_eq!(result, None, "仅文件名弱信号不得锁定类型");
    }

    #[test]
    fn classify_contract_against_resume_filename_by_semantics() {
        // 文件名像简历（weak），正文像合同（title/sections 强规则）→ 合同
        let sections = vec!["违约责任".to_owned(), "合同期限".to_owned()];
        let result = classify_document_type(
            "采购合同",
            "采购合同-周晨.pdf",
            &sections,
            "乙方义务",
            &[0.1, 0.2, 0.1, 0.0],
            &prototypes(),
        );
        let (document_type, _) = result.expect("contract wins by body semantics");
        assert_eq!(document_type, DocumentType::Contract);
    }

    #[test]
    fn classify_by_prototype_similarity_when_rules_silent() {
        // 无规则命中，向量贴近 Resume 原型（sim≈0.97 ≥ 0.65）→ 语义锁定
        let result = classify_document_type(
            "未命名文档",
            "file-2024.pdf",
            &[],
            "无法识别的正文",
            &[0.8, 0.2, 0.0, 0.0],
            &prototypes(),
        );
        let (document_type, confidence) = result.expect("prototype locks resume");
        assert_eq!(document_type, DocumentType::Resume);
        assert!(confidence >= PROTOTYPE_LOCK_SIMILARITY);
    }

    #[test]
    fn high_prototype_similarity_overrides_weaker_rule_signal() {
        // 规则命中合同（title 命中 1 次 = 3/6 = 0.5，刚好够锁），但原型更贴 Resume
        // （sim≈0.97 ≥ 0.72）→ 语义强信号压过规则
        let result = classify_document_type(
            "合同",
            "合同.pdf",
            &[],
            "正文内容与合同无关",
            &[0.8, 0.2, 0.0, 0.0],
            &prototypes(),
        );
        let (document_type, _) = result.expect("embed over rule");
        assert_eq!(document_type, DocumentType::Resume);
    }

    #[test]
    fn rule_and_prototype_agreement_raises_confidence() {
        let sections = vec!["项目经历".to_owned()];
        let result = classify_document_type(
            "我的简历",
            "我的简历.pdf",
            &sections,
            "个人简历",
            &[0.9, 0.1, 0.0, 0.0],
            &prototypes(),
        );
        let (document_type, confidence) = result.expect("agreement locks");
        assert_eq!(document_type, DocumentType::Resume);
        assert!(confidence >= 0.6, "一致时置信度从 0.6 起跳，实际 {confidence}");
    }

    #[test]
    fn nothing_matches_returns_none() {
        // 规则全空 + 原型全部低相似 → None（不猜、不返回 other，调用方写 NULL）
        let result = classify_document_type(
            "未命名文档",
            "abc.pdf",
            &[],
            "正文内容与任何类型原型都不相关",
            &[0.0, 0.0, 0.0, 1.0],
            &prototypes(),
        );
        assert_eq!(result, None);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_or_empty_vectors() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[]), 0.0);
        let (a, b) = (vec![3.0, 4.0], vec![6.0, 8.0]);
        let similarity = cosine_similarity(&a, &b);
        assert!((similarity - 1.0).abs() < 1e-5, "同向向量余弦应≈1，实际 {similarity}");
    }

    fn rows(items: &[(&str, &str)]) -> Vec<(u64, String)> {
        items
            .iter()
            .enumerate()
            .map(|(index, (ordinal, json))| (ordinal.parse().unwrap_or(index as u64), (*json).to_owned()))
            .collect()
    }

    #[test]
    fn extracts_leaf_headings_in_document_order_deduped() {
        let source = rows(&[
            ("0", "[]"),
            ("1", r#"["教育背景"]"#),
            ("2", r#"["教育背景","北京大学"]"#),
            ("3", r#"["项目经历"]"#),
            ("4", r#"["项目经历","LangGraph 多智能体"]"#),
            ("5", r#"["教育背景","北京大学"]"#), // 重复 → 去重
        ]);
        let titles = extract_section_titles(source.iter().map(|(ordinal, json)| (*ordinal, json.as_str())));
        assert_eq!(
            titles,
            vec!["教育背景", "北京大学", "项目经历", "LangGraph 多智能体"]
        );
    }

    #[test]
    fn tolerates_bad_json_and_empty_paths() {
        let source = rows(&[
            ("0", "not json"),
            ("1", "null"),
            ("2", r#"["项目经历"]"#),
            ("3", r#"{broken"#),
        ]);
        let titles =
            extract_section_titles(source.iter().map(|(ordinal, json)| (*ordinal, json.as_str())));
        assert_eq!(titles, vec!["项目经历"]);
    }

    #[test]
    fn caps_section_titles() {
        let source = (0..200)
            .map(|index| (index as u64, format!(r#"["第{index}节"]"#)))
            .collect::<Vec<_>>();
        let titles =
            extract_section_titles(source.iter().map(|(ordinal, json)| (*ordinal, json.as_str())));
        assert_eq!(titles.len(), MAX_SECTION_TITLES);
    }

    #[test]
    fn picks_head_mid_tail_without_underflow() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(pick_head_mid_tail(&empty), (String::new(), String::new(), String::new()));

        let one = vec!["仅一段".to_owned()];
        let (head, mid, tail) = pick_head_mid_tail(&one);
        assert_eq!((head.as_str(), mid.as_str(), tail.as_str()), ("仅一段", "仅一段", "仅一段"));

        let three = vec!["头".to_owned(), "中".to_owned(), "尾".to_owned()];
        let (head, mid, tail) = pick_head_mid_tail(&three);
        assert_eq!((head.as_str(), mid.as_str(), tail.as_str()), ("头", "中", "尾"));
    }

    #[test]
    fn representative_text_is_bounded_and_deterministic() {
        let sections = vec!["项目经历".to_owned(), "教育背景".to_owned()];
        let text = build_representative_text("大模型开发工程师-周晨", &sections, "头段", "中段", "尾段");
        assert!(text.contains("大模型开发工程师-周晨"));
        assert!(text.contains("项目经历"));
        assert!(text.contains("头段"));
        assert!(text.chars().count() <= REPRESENTATIVE_TEXT_LIMIT);
        let again = build_representative_text("大模型开发工程师-周晨", &sections, "头段", "中段", "尾段");
        assert_eq!(text, again);
        // hash 稳定且随内容变化
        assert_eq!(representative_text_hash(&text), representative_text_hash(&text));
        let changed = build_representative_text("其他标题", &sections, "头段", "中段", "尾段");
        assert_ne!(representative_text_hash(&text), representative_text_hash(&changed));
    }

    #[test]
    fn long_chunks_are_truncated() {
        let long = "长".repeat(10_000);
        let text = build_representative_text("标题", &[], &long, "", "");
        assert!(text.chars().count() <= REPRESENTATIVE_TEXT_LIMIT);
    }
}
