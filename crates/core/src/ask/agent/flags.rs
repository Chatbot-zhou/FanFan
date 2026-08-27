//! 运行时 feature flag（Phase 0+1 起承载 Agent 路径开关）。
//!
//! 现状：Legacy RAG 是稳定 baseline，默认路径保持完全不变；Agent 工具层
//! 是否接管问答由环境变量显式决定，便于无头 A/B 评测与 CI 回归，不污染
//! 前端 UI。开关关闭时，`ask::agent` 模块只作为登记契约存在，不参与任何
//! 问答分发。

use std::env;

/// 是否启用 Agent 工具路由器（Planner 路径）。
///
/// 取值 `FANFAN_AGENT_ROUTER=1|true|yes|on` 视为开启，其余（含未设置）默认
/// 关闭。读取失败（非 UTF-8 等）一律按关闭处理，绝不因配置异常改变既有
/// 问答行为。
pub fn agent_router_enabled() -> bool {
    match env::var("FANFAN_AGENT_ROUTER") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}
