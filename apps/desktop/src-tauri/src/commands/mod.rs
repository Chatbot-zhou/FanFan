pub mod app_data;
// Phase 0+1：新工具执行器尚未被默认链路调用，标 `dead_code` 由 Phase 2 接入。
#[allow(dead_code)]
pub mod ask_tools;
pub mod memory_view;
pub mod ollama;
pub mod startup;
pub mod theme;
pub mod welcome;
