//! 高层只读 Knowledge Agent 的「工具契约层」（Phase 0+1）。
//!
//! 目标是把现有检索/摘要/抽取/比较能力封装成少量高层只读 Tool，供受约束
//! Planner（Phase 2）选择，**不把 Embedding / FTS / RRF / MMR / Chunk 等底层
//! 操作暴露给决策层**。
//!
//! 决策链：`planner::plan_question(tier, plan) → KnowledgeTool`，再由桌面编排
//! 层的工具执行器落实。档位（0.8B/2B/4B/8B）决定约束强度（最大步数、是否
//! Recovery、放行哪些工具）。
//!
//! 本阶段定位：纯契约 + 注册表 + 规划映射 + 输出校验 + 运行时开关。默认
//! （feature flag 关闭）完全不参与现有 Legacy RAG 链路，行为保持不变；
//! Agent 路径由运行时环境变量显式开启后，Phase 2 再接入 Planner。

pub mod flags;
pub mod planner;
pub mod tool;

pub use flags::agent_router_enabled;
pub use planner::{PlanCapability, PlanDecision, PlannerTier, plan_question};
pub use tool::{
    registry, tool_for_plan, KnowledgeTool, ToolInput, ToolOutput, ToolSpec, ToolStatus,
    ToolValidation, ToolValidator,
};
