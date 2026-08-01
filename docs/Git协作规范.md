# 拾忆 Git 协作规范

## 1. 仓库信息

- 本地目录：`E:\Desktop\Remin`
- GitHub：`git@github.com:Chatbot-zhou/Remin.git`
- 稳定分支：`main`
- 开发分支前缀：`codex/`
- 远端协议：仅 SSH

### SSH 强制规则

- 所有 `fetch`、`pull`、`push` 和 submodule 操作始终使用 SSH。
- GitHub remote 统一格式：`git@github.com:<owner>/<repo>.git`。
- 禁止配置 `https://github.com/...` 形式的 Git remote。
- SSH key 由用户系统和 ssh-agent 管理，仓库中不得保存私钥、访问令牌或密码。
- 每次首次连接新远端时核对主机指纹；不得通过关闭主机校验绕过错误。
- `.gitmodules` 中的子模块地址也必须使用 SSH。

这里的 SSH-only 专指 Git 数据传输。以下 GitHub 协作操作按平台标准使用 GitHub App 或已认证的 `gh`：

- Issue 创建、查询、标签和讨论；
- Pull Request 创建、Review、评论和合并状态；
- GitHub Actions 检查与日志；
- Release、里程碑和仓库协作元数据。

GitHub App 或 `gh` 使用 GitHub API 不构成 HTTPS Git remote；不得因此把 `origin` 改成 HTTPS。

检查与修正命令：

```powershell
git remote -v
git remote set-url origin git@github.com:Chatbot-zhou/Remin.git
ssh -T git@github.com
```

## 2. 标准开发流程

```text
检查工作区
→ 获取远端状态
→ 从最新 main 创建 codex/* 分支
→ 实现并验证一个明确任务
→ 检查 diff
→ 显式暂存相关文件
→ Conventional Commit
→ 推送功能分支
→ 创建 Draft PR
→ CI和评审通过
→ 合并并验证 origin/main
```

参考命令：

```powershell
git status --short --branch
git fetch origin
git switch main
git pull --ff-only origin main
git switch -c codex/feat-short-description

git diff
git add path/to/file1 path/to/file2
git diff --cached
git commit -m "feat(scope): concise description"
git push -u origin codex/feat-short-description
```

## 3. 分支命名

| 类型 | 用途 | 示例 |
|---|---|---|
| `codex/feat-*` | 新能力 | `codex/feat-hybrid-search` |
| `codex/fix-*` | 缺陷修复 | `codex/fix-index-resume` |
| `codex/docs-*` | 文档或用户旅程 | `codex/docs-onboarding-node` |
| `codex/refactor-*` | 不改变行为的重构 | `codex/refactor-parser-contract` |
| `codex/test-*` | 测试与评测集 | `codex/test-search-benchmark` |
| `codex/chore-*` | 构建、依赖和工具 | `codex/chore-tauri-bootstrap` |

分支名使用小写英文和短横线。一个分支只对应一个可描述、可验证的目标。

## 4. 提交信息

格式：

```text
<type>(<scope>): <description>
```

允许类型：`feat`、`fix`、`docs`、`refactor`、`test`、`chore`、`build`、`ci`。

示例：

```text
docs(plan): add V1 product and journey specifications
feat(catalog): add read-only folder authorization
fix(index): resume interrupted embedding jobs
test(search): add Chinese known-item retrieval benchmark
```

要求：

- 使用祈使语气描述变化，不写“update files”一类无信息内容。
- 一个提交只包含一个逻辑变化。
- 功能和修复提交应同时包含对应测试或说明未能测试的原因。
- 不使用提交修复来掩盖未审查的混杂改动。

## 5. 暂存和工作区安全

- 每次工作前后都运行 `git status --short --branch`。
- 使用 `git diff` 检查未暂存内容，使用 `git diff --cached` 检查待提交内容。
- 默认显式暂存文件；只有确认整个工作区都属于当前任务时才允许 `git add -A`。
- 不覆盖、不回滚、不格式化用户的无关改动。
- 禁止对不明确目标使用 `git reset --hard`、`git clean -fd` 或强制签出。
- 发现意外改动时停止提交，并先确认归属。

## 6. Pull Request

Draft PR 至少包含：

- 做了什么；
- 为什么要做；
- 对用户和开发者的影响；
- 关键设计和安全边界；
- 已运行的验证及结果；
- 已知限制和后续工作。

PR 应保持可审查：避免大规模无关格式化，避免同时混合架构改造、功能开发和依赖升级。

PR 元数据、评论和评审优先通过 GitHub App 管理；当前分支 PR 发现、Actions 日志或连接器覆盖不足时使用 `gh`。Git 分支的获取与推送仍必须通过 SSH remote。

## 7. 合并与发布

- `main` 必须通过相关检查后才能合并。
- 禁止强制推送 `main`。
- 版本发布使用明确标签，如 `v0.1.0`，标签与发布说明必须经过用户确认。
- 推送功能分支不等于发布完成；必须确认目标分支已经合并并验证 `main...origin/main`。

## 8. 禁止提交的内容

- GGUF、ONNX、Safetensors 等模型文件；
- 用户授权目录中的任何原始资料；
- SQLite 数据库、FTS 或向量索引；
- `.env`、密钥、令牌、证书和个人路径配置；
- OCR、解析缓存、日志和生成中的临时文件；
- `node_modules`、Rust `target`、Python 虚拟环境和覆盖率产物。

需要共享的配置应提供去敏后的示例文件，例如 `.env.example`。

## 9. 文档同步

- 用户旅程节点确定后，先保存节点文档，再回写主规划书。
- API、数据库字段或安全边界变化必须同步更新相关规划和测试。
- 被替代的决策保留在节点文档的决策记录中，主规划只保留当前有效方案。
