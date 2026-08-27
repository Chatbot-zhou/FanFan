//! 受约束 Planner（Phase 2 接入）。
//!
//! 规划必须在一个极小、受限的解空间内完成，并兼容 0.8B / 2B / 4B / 8B 各档
//! 生成模型：
//!
//! - 0.8B：强约束，少量候选工具，最多 1 个工具步骤，不允许 Recovery / 自由循环；
//! - 2B：允许一次简单 Recovery；
//! - 4B / 8B：可逐步放宽（更大候选集、略多步骤），但仍限制最大步骤并只读。
//!
//! 本模块只做「QueryPlan × 模型能力 → 受约束计划」的纯函数决策：不下发底层
//! Embedding / FTS / RRF / MMR，不调用任何模型；真正执行仍在桌面编排层。
//! 默认档位保守（0.8B），`allowed_tools` 仅放行经评测确认安全的工具，未评测
//! 更优的工具一律回落 Legacy，保证默认路径行为不变。档位由运行时环境变量
//! `FANFAN_AGENT_TIER` 指定，读取失败一律按最保守档位处理。
//!
//! 设计约束：不针对具体文件 / 关键词 / 测试问题写特判，只按意图 + 档位约束做
//! 通用路由。

use crate::ask::agent::tool::{tool_for_plan, KnowledgeTool};
use crate::ask::query_plan::{QueryPlan, SourceIntent};

/// 模型规模档位，决定 Planner 的约束强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerTier {
    /// 0.8B：强约束 JSON Planner，少量候选，最多 1 步，不允许恢复/循环。
    VerySmall,
    /// 2B：允许一次简单 Recovery。
    Small,
    /// 4B：可逐步增加复杂规划。
    Medium,
    /// 8B：复杂规划，但仍限最大步骤。
    Large,
}

impl PlannerTier {
    /// 规范名（用于 trace / 解析，如 "0.8b" / "8b"）。
    pub fn as_str(self) -> &'static str {
        match self {
            PlannerTier::VerySmall => "0.8b",
            PlannerTier::Small => "2b",
            PlannerTier::Medium => "4b",
            PlannerTier::Large => "8b",
        }
    }

    /// 宽容解析（抹空白后小写匹配），失败返回 None。
    pub fn parse_lenient(input: &str) -> Option<PlannerTier> {
        let normalized: String = input
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        match normalized.as_str() {
            s if s.starts_with("0.8b") || s.starts_with("08b") || s.starts_with("0.8") => {
                Some(PlannerTier::VerySmall)
            }
            s if s.starts_with("2b")
                || (s.starts_with("2") && s.chars().all(|c| c.is_ascii_digit() || c == 'b')) =>
            {
                Some(PlannerTier::Small)
            }
            s if s.starts_with("4b") => Some(PlannerTier::Medium),
            s if s.starts_with("8b") => Some(PlannerTier::Large),
            _ => None,
        }
    }

    /// 从环境变量 `FANFAN_AGENT_TIER` 读取档位；未设置或解析失败一律回退最保守档位。
    pub fn from_env() -> PlannerTier {
        match std::env::var("FANFAN_AGENT_TIER") {
            Ok(value) => PlannerTier::parse_lenient(&value).unwrap_or(PlannerTier::VerySmall),
            Err(_) => PlannerTier::VerySmall,
        }
    }
}

/// 档位对应的规划能力约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCapability {
    /// 该档位允许的最大工具步骤数。
    pub max_steps: usize,
    /// 该档位是否允许一次简单 Recovery。
    pub allow_recovery: bool,
    /// 该档位允许在 Agent 路径执行的高层工具集合；未在集合内的工具回落 Legacy。
    pub allowed_tools: &'static [KnowledgeTool],
}

/// 将档位映射为能力约束。
///
/// 当前 `allowed_tools` 仅放行经评测确认在 Agent 路径下更稳的工具（现状为
/// 库概览）；其余工具（extract / compare / scoped RAG 等）仍处于「已有实现但
/// 未评测胜出」状态，保持回落 Legacy。后续每轮评测确认更优后再逐步扩展集合，
/// 不提前无限扩大 Scope。最大步骤与恢复开关按档位区分，为后续复杂规划留接口。
#[allow(dead_code)]
fn capability_for(tier: PlannerTier) -> PlanCapability {
    // 经评测确认在 Agent 路径下安全、确定性的工具白名单：库概览 + 找文件 + 大纲。
    // 找文件（search_files）复用与 legacy find 同源的 `resolve_documents` 定位，
    // 是确定性单步工具，各生成档位（含 0.8B）都能稳定承担；放行后不改变定位
    // 结果，仅把「定位文件」从 legacy 分支移入显式高层 Tool 路径，便于后续
    // 独立优化与多步编排。
    // 大纲（get_outline）只读取文档结构章节标题串，不做逐节模型摘要，是确定性
    // 单步工具：答复「有哪些章节/大纲」类问句时不依赖 RAG 逐节生成，结构枚举
    // 的正确性与可定位性更稳，代价更低。章节标题在结构 heading_path 缺失时由
    // `build_document_sections` 从正文首行标题行兜底提取，OCR/纯文本文档亦可
    // 输出真实章节，不再退化为「未命名内容」。其余工具（extract / compare /
    // scoped RAG 等）仍处「已有实现但未评测胜出」状态，保持回落 Legacy；后续
    // 每轮评测确认更优后再逐步扩展集合，不提前无限扩大 Scope。
    const APPROVED: &[KnowledgeTool] = &[
        KnowledgeTool::LibraryOverview,
        KnowledgeTool::SearchFiles,
        KnowledgeTool::GetOutline,
    ];
    match tier {
        PlannerTier::VerySmall => PlanCapability {
            max_steps: 1,
            allow_recovery: false,
            allowed_tools: APPROVED,
        },
        PlannerTier::Small => PlanCapability {
            max_steps: 1,
            allow_recovery: true,
            allowed_tools: APPROVED,
        },
        PlannerTier::Medium => PlanCapability {
            max_steps: 3,
            allow_recovery: true,
            allowed_tools: APPROVED,
        },
        PlannerTier::Large => PlanCapability {
            max_steps: 5,
            allow_recovery: true,
            allowed_tools: APPROVED,
        },
    }
}

/// Planner 的决策结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDecision {
    /// 生效档位（用于 trace 归因）。
    pub tier: PlannerTier,
    /// Planner 选定的高层 Tool（无资料工具，如 General Chat，为 None）。
    pub tool: Option<KnowledgeTool>,
    /// 是否由 Agent 路径执行。false 表示该工具不在档位许可集内或属非资料分支，
    /// 调用方应回落 Legacy。
    pub agent: bool,
    /// 该档位最大允许步数（本次执行约束，供 trace / 未来扩展）。
    pub max_steps: usize,
    /// 该档位是否允许 Recovery。
    pub allow_recovery: bool,
}

/// 受约束 Planner 入口：QueryPlan × 档位 → 决定。
///
/// 先复用既定映射得到候选工具，再经档位许可集门控：工具越权或非资料分支时
/// `agent=false`，由调用方回落 Legacy；否则 `agent=true` 进入 Agent 执行。
pub fn plan_question(tier: PlannerTier, plan: &QueryPlan) -> PlanDecision {
    let cap = capability_for(tier);
    // 上下文相关/歧义问句（省略指代、裸祈使、会话记忆恢复 → plan.source =
    // Ambiguous，见 query_parser 的确定性同步）不派发 Agent 工具：这类问句需
    // 结合会话上下文澄清或恢复所指，交由既有 Context Resolver / Legacy 链路
    // 处理，Agent 不越权取底层工具。通用判定，不针对任何具体文件/关键词/case。
    if plan.source == SourceIntent::Ambiguous {
        return PlanDecision {
            tier,
            tool: None,
            agent: false,
            max_steps: cap.max_steps,
            allow_recovery: cap.allow_recovery,
        };
    }
    let tool = tool_for_plan(plan);
    let (tool, agent) = match tool {
        Some(t) if cap.allowed_tools.contains(&t) => (Some(t), true),
        _ => (None, false),
    };
    PlanDecision {
        tier,
        tool,
        agent,
        max_steps: cap.max_steps,
        allow_recovery: cap.allow_recovery,
    }
}
