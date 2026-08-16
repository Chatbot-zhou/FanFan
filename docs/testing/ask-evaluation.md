# Ask Evaluation Runner 使用说明（真实用户测试准备）

> 面向第一轮真实用户测试的批量评估入口。设置页 **Developer / 问答调试 → Ask Evaluation Runner** 运行；
> 配合 **Ask Trace Viewer** 逐例复盘、**Export Debug Trace JSON** 导出脱敏快照。

## 1. 测试集格式

支持两种格式（以文件第一个非空字符判断）：

- **JSONL**：每行一个用例对象，`#` 开头行与空行忽略（行号出错时提示）。
- **JSON 数组**：以 `[` 开头，一整个数组。

```jsonl
# 示例测试集（UTF-8）
{"id": "r1", "question": "我的简历里写了哪些项目经历？", "expected_source": "local", "expected_intent": "document_qa", "expected_file_ids": ["<file_id>"], "expected_document_type": "resume", "expected_should_find_evidence": true}
{"id": "r2", "question": "什么是 Transformer？", "expected_source": "general"}
```

### 字段（全部可选，写了才校验）

| 字段 | 类型 | 含义 |
|---|---|---|
| `id` | string | 用例编号（必填，报告里对应用例） |
| `question` | string | 真实用户问题（必填） |
| `expected_source` | string | `local` / `general` / `ambiguous`，路由预期 |
| `expected_intent` | string | `document_qa` / `document_summary` / `library_qa` / `compare_documents` / `document_find` / `general_chat` 等 |
| `expected_file_ids` | string[] | 应命中的文件 id 列表（任一命中即通过） |
| `expected_document_type` | string | `resume` / `contract` / `invoice` 等 |
| `expected_should_find_evidence` | bool | `true`：必须找到证据且 grounded；`false`：必须 NO_EVIDENCE 拒绝（不应找到证据）；缺省不校验 |

## 2. 运行方式

1. 先在程序里添加文档、完成索引（问答前先自检生成 + Embedding 模型）。
2. 设置 → Developer / 问答调试 → 选择测试集（JSONL/JSON）→ 选择结果输出路径（必须是**不存在的绝对路径** `.json`，已存在会拒绝，防误覆盖）。
3. 点「运行评估」，二次确认后逐例执行。

### 每例运行隔离（防污染设计）

- 每例独立 `session_id`，跑完即删（不残留 Ask History）；
- 每例独立 `operation_id`，作为 `node_traces` 关联键——报告每行可点「查看 Trace」复盘 20 个阶段；
- **不启动 Memory Candidate Writer**（问答绝不自动写 Memory）；
- **不设置 clarification_selection**（不会产生 USER_SELECTION 记忆）；
- 不修改 Router Prompt / Resolver 权重 / 任何阈值。

## 3. 结果报告

报告写入所选路径，含 `schema_version / run_id / total / passed / failed / metrics / results[]`。

每例 `results[]` 的判定字段：

| 字段 | 含义 |
|---|---|
| `actual_source` / `actual_intent` / `actual_document_type` | 从 trace 提取的实际路由 / 意图 / 文档类型 |
| `actual_file_ids` | 实际使用的文件 id |
| `memory_used` / `clarification_used` | 是否命中 Memory / 是否返回澄清 |
| `retrieval_top_files` / `rerank_top_files` | 检索 top 文件 / 重排后 top 文件 |
| `grounding_status` | `grounded` / `partial` / `insufficient` |
| `evidence_found` | claims 或来源文件非空 |
| `answer_grounded` | Grounded 且无 Unsupported claim |
| `latency_ms` | 总耗时 |
| `error_category` / `error_message` | 13 类错误分类 / 运行错误信息 |
| `pass_fail` / `failed_fields` | 通过与否 / 失败字段名（`source_correct` 等） |

## 4. 14 项核心指标（只保证数据可采集，不做硬门槛）

`source_router_accuracy`、`intent_accuracy`、`document_resolution_top1_accuracy`、`document_resolution_top3_recall`、`memory_hit_accuracy`、`memory_wrong_hit_rate`、`clarification_rate`、`clarification_success_rate`、`retrieval_evidence_recall`、`no_evidence_false_negative_rate`、`grounded_answer_rate`、`citation_pass_rate`、`avg_total_ms`、`p50_total_ms`、`p95_total_ms`。

分母为该指标可判定的用例子集（如 `source_router_accuracy` 只统计写了 `expected_source` 的用例）。

## 5. 错误分类（13 类，snake_case，允许人工修改）

`router_error` / `query_parse_error` / `context_error` / `memory_error` / `document_resolution_error` / `document_recall_error` / `chunk_retrieval_error` / `rerank_error` / `no_evidence_error` / `generation_error` / `citation_error` / `clarification_error` / `unknown`。

分类优先级：**trace 节点失败 > 运行错误码家族 > NO_EVIDENCE > 引用核验**。报告中可直接人工改 `error_category` 重新归类（纯数据，不影响下次运行）。

## 6. 边界行为（10 项，代码与测试覆盖）

| # | 边界 | 覆盖 |
|---|---|---|
| 1 | Memory alias → 已不可用文件 → 失效/回退，不注入 scope | storage 测试 `memory_target_validation_rejects_missing_offline_and_unauthorized_files` |
| 2 | alias → 越权文件（不在授权根）→ 不采用 | `memory_target_rejects_file_outside_authorized_roots` |
| 3 | DocumentProfile revision ≠ 当前 → 不参与定位 | `document_profile_rebuild_switches_to_new_revision`（SQL 强制 `revision_id = current_revision_id`） |
| 4 | 澄清选择 → 文件已不存在 → 安全失败 | `run_clarified_answer` 返回 `CLARIFICATION_SELECTION_INVALID`（不崩溃） |
| 5 | SUMMARY 超 4000 节点 → 明确截断 trace | `file_document_node_count_tracks_current_revision_only`；trace 输出 `summary_truncated / nodes_total / nodes_used` |
| 6 | COMPARE 文件不存在 → 不崩溃 | `fallback_insufficient_targets` trace → 多文档检索回退；单侧无材料 → 固定拒绝文案 |
| 7 | EXTRACT 无匹配 → 空结果不 hallucination | trace `no_items` + 保留原 grounded 回答 |
| 8 | Memory Writer 非法 file_id → prewrite 验证拒绝 | `unresolvable_targets_are_dropped_not_written`、`alias_to_unknown_collection_is_dropped` |
| 9 | Router timeout → 不直接当 GENERAL | `timeout_or_garbage_never_maps_to_general`（解析失败返回 None，编排层显式 trace 回退或上抛） |
| 10 | Query Parser timeout → 确定性 fallback 不崩溃 | `timeout_or_garbage_output_falls_back_deterministically`（`QueryPlan::default()`） |

## 7. 铁律（本阶段起生效）

- **禁止自动学习真实测试结果**：Evaluation Runner 不会自动修改 Router Prompt / Resolver 权重 / Memory / 阈值 / Prototype。所有「调参」动作必须人工、显式、经代码评审后提交。
- **性能日志隔离**：所有详细 trace 只在 Developer / Diagnostics 模式启用；普通模式保留轻量 trace；禁止每次问答把所有 chunk 全文写日志。
- 测试集与结果文件属于工作区产物，提交前请脱敏（路径、个人文件内容）。

## 8. 测试命令

```bash
# 核心层（含 evaluation / memory_writer / storage 边界测试）
cargo test -p fanfan-core --lib

# 桌面层编译
cargo check -p fanfan-desktop

# 前端
npm run typecheck && npm run test && npm run build
```
