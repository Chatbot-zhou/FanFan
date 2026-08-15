# 翻翻 FanFan：完全本地化的 AI 信息管理助手

翻翻（FanFan）是一款完全本地化、源文件只读的 AI 信息管理助手。它帮助用户理解、整理和找回散落在电脑中的各类资料——文档、表格、PDF、图片——让每一份被遗忘的信息重新产生价值。所有扫描、解析、索引与问答推理均在本地完成，资料不会离开你的电脑。

- **面向用户**：重视数据隐私的个人知识工作者、研究者与日常办公用户，希望在不牺牲效率的前提下让 AI 管理自己的本地资料。
- **核心特点**：本地运行、源文件只读、中文优先、可追溯、可离线。内置 SQLite 向量检索与官方 llama.cpp CPU 运行时，无需联网、无需上传资料。
- **目标平台**：Windows 10/11 x64（当前版本为简体中文界面）。

## 安装指南

### 环境要求

| 依赖 | 版本要求 | 用途 |
| --- | --- | --- |
| Windows | 10/11 x64 | 目标运行平台 |
| Node.js | 24.x（含 corepack） | 前端构建 |
| pnpm | 11.9.0（corepack 自动管理） | 包管理 |
| Rust | stable + MSVC Build Tools | 核心服务与桌面壳 |
| Python | 3.12 | 文档解析 / OCR / AI Worker |
| WebView2 | 最新版 | Tauri 界面运行时 |

### 获取源码

Git 远端使用 SSH：

```powershell
git clone git@github.com:Chatbot-zhou/FanFan.git
cd FanFan
```

### 安装依赖并启动开发环境

```powershell
corepack pnpm install
corepack pnpm dev
```

浏览器访问：

- `http://127.0.0.1:1420/?welcome=1`：强制预览首次欢迎页
- `http://127.0.0.1:1420/?welcome=0`：跳过欢迎页预览主程序

安装 Rust stable、MSVC Build Tools 与 WebView2 后，可以运行桌面版或构建安装包：

```powershell
corepack pnpm tauri:dev      # 运行 Tauri 桌面应用
corepack pnpm tauri:build    # 构建 Release 桌面应用
corepack pnpm installer:build  # 构建独立 Worker 并生成 NSIS .exe
```

安装包输出位置：

```text
.artifacts/cargo-target/release/bundle/nsis/翻翻_<version>_x64-setup.exe
```

Windows 环境的可复现安装与验证步骤参见 [Windows 开发环境](docs/开发/Windows开发环境.md)，完整打包边界与签名门禁参见 [Windows安装程序与发布准备](docs/开发/Windows安装程序与发布准备.md)。

### 完整检查

```powershell
corepack pnpm verify           # 前端类型、测试与校验
corepack pnpm verify:release   # 发布级完整校验
```

## 使用示例

翻翻是桌面应用，首次启动后按以下流程即可完成核心功能闭环：

1. **欢迎页授权**：启动后按欢迎页提示，确认并授权翻翻访问本机的资料目录（桌面、文档、下载、图片为默认来源，可增删）。
2. **自动扫描**：后台以只读方式扫描授权目录，建立本地资料库（源文件只读，不会移动、重命名、删除或覆盖任何文件）。
3. **配置本地模型**：进入「设置 → 本地模型」，按角色（问答生成、多模态图片理解、Embedding、OCR、Rerank、语音合成/识别）从已验证模型池中导入或下载模型，模型文件保存在本地且需用户确认，全部模型经大小与 SHA-256 完整性校验后才激活。
4. **找资料（搜索）**：在搜索页输入关键词，同时获得全文（SQLite FTS5）与语义（本地向量）的融合检索结果，结果支持快照分页、停止搜索与原文定位。
5. **问资料（问答）**：在问答页提出自然语言问题，翻翻基于检索到的本地资料生成可追溯回答，并锚定到原文引用；回答由内置 llama.cpp 本地运行时完成，支持多模态图片理解。
6. **语音提问与朗读**：点击麦克风用语音提问（本地识别、识别结果确认后发送）；通过引用校验的答案可本地朗读，支持暂停、继续、语速与音色。
7. **整理与导出**：将常用检索条件保存为智能集合，或对检索结果显式导出为 JSON / CSV / XLSX / DOCX（禁止覆盖已有文件）。

也可在浏览器中通过 `http://127.0.0.1:1420/?welcome=1` 直接预览界面进行体验。

## 功能列表

- **本地资料库**：SQLite 本地目录库，独立扫描任务、重复任务复用与失败状态回写；自动排除系统目录、开发依赖、临时文件与重解析点
- **多格式解析**：Python Worker 支持现代 Office、PDF、文本解析与 Windows OCR，可恢复的原子任务与检查站
- **混合检索**：SQLite FTS5 中文全文搜索 + 持久化向量精确语义检索，RRF 融合与真实摘要；20,000 文件 / 384 维基准 p95 低于 250ms
- **可追溯问答**：本地检索增强生成，回答附带原文引用与锚定定位；内置官方 llama.cpp 本地运行时，模型经用户确认后导入或下载
- **多模态图片理解**：扫描页、内嵌图与图表由本地视觉语言模型（VLM）补齐文字提取与摘要，搜索与问答可命中图片内容
- **本地语音**：ASR（Paraformer + Silero VAD）语音提问、TTS（VITS）答案朗读，全部由本地 sherpa-onnx 独立进程完成，临时音频不落盘
- **多模态本地模型**：按角色管理生成、多模态（VLM）、Embedding、Rerank、OCR、TTS 语音合成、ASR 语音识别模型，带下载管理、完整性校验、自检与按需加载
- **智能集合**：手动、规则与 AI 集合推荐管理，AI 建议含成员、置信度与逐成员理由，可预览、编辑、确认或拒绝
- **资料关系**：完全重复、版本候选、同主题/同用途、包含/摘要/派生关系的识别与确认
- **收件箱与规则**：规则优先的字段抽取，资料目录、文件名 / 目录建议
- **显式导出**：JSON / CSV / XLSX / DOCX 导出，禁止覆盖现有文件
- **本地优先**：模型、索引、日志与解析过程全部留在本机；源文件只读是硬性安全边界；无遥测、无云端推理
- **一键安装**：Windows x64 简体中文 NSIS 安装程序，内含 WebView2 离线安装器、独立 Worker 与 llama.cpp 运行时

## 当前状态

翻翻处于体验版开发阶段（预发布版本 `0.2.0`），所有推理完全本地。截至 2026-08-15：

- **模型全部就绪**：7 个角色（问答生成 ×2、多模态图片理解、Embedding、Rerank、OCR、语音识别、语音合成）已安装并通过完整性校验与自检，大小与 SHA-256 逐文件锁定。
- **语音链路已接通**：语音识别与合成模型、本地 sherpa-onnx 运行环境均已就绪并通过自检；应用内语音按钮链路正在做最后一轮真实验证。
- **图片理解全量处理中**：资料解析产生的全部图片资产正由本地 VLM 逐一补齐文字与摘要，完成后进入检索与问答索引。
- **真实资料评测进行中**：本地评测器正在对真实授权资料跑搜索、问答、关系、集合、OCR 与语音基线，全部通过并达到门禁分数前不发布综合评分。

## 贡献指南

欢迎通过 Issue、PR 参与翻翻的开发。请先阅读 `docs/翻翻-V1-产品与开发规划书.md` 与 `docs/V1-方案逐条审查清单.md` 了解产品方向。

### 分支管理

- `main` 始终保持可构建、可验证，不直接承载日常功能开发
- 新工作从最新 `main` 创建 `codex/<类型>-<简短主题>` 分支，类型为 `feat` / `fix` / `docs` / `refactor` / `test` / `chore`
- 示例：`codex/feat-folder-authorization`、`codex/docs-onboarding-node`
- 一个分支只解决一个明确问题，禁止把无关改动混入同一提交或 PR

### 提交规范

- 使用 Conventional Commits：`feat(scope): ...`、`fix(scope): ...`、`docs(scope): ...` 等
- 提交前查看完整 diff，只暂存本次任务涉及的明确文件；优先使用 `git add <file>`，工作区混杂时不得使用 `git add -A`
- 每个提交必须能独立说明目的，并包含与风险相称的验证
- 模型文件、索引、用户资料、数据库、日志、凭据和本机配置不得提交

### 代码规范与验证

- 所有 Git fetch / push 操作必须使用 SSH（`git@github.com:<owner>/<repo>.git`），不得使用 HTTPS 地址或内嵌凭据
- 前端：TypeScript 严格检查，`pnpm check`（CI 中运行 `pnpm install --frozen-lockfile`）
- Rust：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`
- Python：`python -m unittest discover -s services/worker/tests -v`，以及契约、语料校验脚本
- CI 位于 `.github/workflows/check.yml`，对 `main` 与 `codex/**` 分支自动执行前端 / Worker / Rust 三组检查
- 未经用户明确要求，不提交、推送、创建标签或发布版本；发布时推送 `codex/*` 分支并创建 Draft PR，不得向远端 `main` 强制推送

### 产品安全边界

- 源文件始终只读；应用不得移动、重命名、删除或覆盖用户文件
- 业务导出必须由用户显式触发，新建文件且禁止覆盖现有目标
- 未授权目录不可读取；模型、索引、日志和解析过程保持本地
- 模型生成的结果必须经过 Schema、来源和权限校验

## 工程结构

```text
apps/desktop       React + TypeScript + Tauri 2 桌面应用
crates/core        Rust 领域契约与核心服务
services/worker    Python 解析、OCR、Embedding 与 AI Worker
docs               产品规划、逐条审查、视觉和开发记录
```

应用运行数据默认写入当前 Windows 用户的应用数据目录；资料源只进行读取和元数据枚举。开发构建产物、模型、数据库和本地验证产物均被 Git 忽略。

## 许可证

本项目基于 [MIT License](LICENSE) 发布，版权所有 © FanFan Contributors。

MIT 许可证授予你自由使用、复制、修改、合并、发布、分发、再许可及/或销售本软件的副本的权利，前提是保留上述版权声明与许可声明；软件按「现状」提供，不附带任何明示或暗示的担保，作者不对因使用本软件产生的任何损失承担责任。
