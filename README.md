# 翻翻 FanFan

翻翻是一款完全本地化、源文件只读的 AI 信息管理助手。它帮助用户理解、整理和找回电脑中散落的资料，让每一份被遗忘的信息重新产生价值。

## 当前实现

- React + TypeScript + Vite 主界面；
- 首次欢迎页状态机与本地完成状态；
- 自定义标题栏、模型未配置提示和两栏主窗口；
- 首页、找资料、问资料、全部资料、智能集合、收件箱、设置；
- 轻量版/标准版本地模型配置流程；
- Tauri 2 / Rust 公共契约、欢迎状态持久化和 Windows Known Folder 发现；
- SQLite 本地目录库、独立扫描任务、重复任务复用和失败状态回写；
- 桌面、文档、下载、图片四个默认资料根的后台只读元数据扫描；
- 系统目录、开发依赖、临时文件、重解析点和翻翻内部数据目录排除；
- Python 3.12 Worker现代Office/PDF/文本解析、JSONL协议、只读检查与安装包内独立运行组件；
- SQLite FTS5中文全文搜索、RRF融合、真实摘要与原文定位；
- SQLite持久向量真值库与精确语义检索，20,000文件/384维基准p95低于250ms；
- 搜索快照分页、停止本次搜索、长文档分批预览和引用节点锚定；
- 可追溯本地问答，内置官方llama.cpp CPU运行时，模型仍由用户确认后导入或下载；
- 可恢复的原子任务、检查站、三路径探索，以及摘要、字段抽取、资料目录、文件名/目录建议和索引导出；
- JSON/CSV/XLSX/DOCX显式导出，禁止覆盖现有文件；
- Windows x64简体中文NSIS安装程序，内含WebView2离线安装器、独立Worker与llama.cpp运行时。

## 本地开发

```powershell
corepack pnpm install
corepack pnpm dev
```

浏览器访问：

- `http://127.0.0.1:1420/?welcome=1`：强制预览首次欢迎页；
- `http://127.0.0.1:1420/?welcome=0`：跳过欢迎页预览主程序。

完整检查：

```powershell
corepack pnpm verify
corepack pnpm verify:release
```

安装 Rust stable、MSVC Build Tools 与 WebView2 后可以运行或构建：

```powershell
corepack pnpm tauri:dev
corepack pnpm tauri:build
```

Windows 环境的可复现安装和验证步骤参见 [Windows 开发环境](docs/开发/Windows开发环境.md)。

## Windows安装程序

构建独立Worker并生成NSIS `.exe`：

```powershell
corepack pnpm installer:build
```

输出位置：

```text
.artifacts/cargo-target/release/bundle/nsis/翻翻_<version>_x64-setup.exe
```

安装、安装后Worker解析、主程序启动和卸载冒烟：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File scripts/validate_installer.ps1 `
  -InstallerPath ".artifacts\cargo-target\release\bundle\nsis\翻翻_0.1.0_x64-setup.exe"
```

完整打包边界和正式签名门禁参见 [Windows安装程序与发布准备](docs/开发/Windows安装程序与发布准备.md)。

## 工程结构

```text
apps/desktop       React + Tauri 桌面应用
crates/core        Rust 领域契约与核心服务
services/worker    Python 解析、OCR、Embedding 与 AI Worker
docs               产品规划、逐条审查、视觉和开发记录
```

Git 远端始终使用 SSH：

```powershell
git clone git@github.com:Chatbot-zhou/FanFan.git
```

应用运行数据默认写入当前 Windows 用户的应用数据目录；资料源只进行读取和元数据枚举。开发构建产物、模型、数据库和本地验证产物均被 Git 忽略。

当前边界：目录扫描、增量监听、现代格式与PDF解析、Windows OCR、全文/语义检索、原文定位、可追溯问答、规则优先抽取、智能集合、任务恢复、显式导出和NSIS开发安装包均已接入真实链路。V1采用SQLite持久向量真值库与精确检索；20,000文件基准远低于2秒门禁，因此暂不引入USearch原生依赖。基础安装包不内置模型GGUF，首次联网下载仍需要用户确认。

最新开发交付包：`.artifacts/cargo-target/release/bundle/nsis/翻翻_0.1.0_x64-setup.exe`，SHA-256为`a7cb4de76335241b590dd5717392120be1038b91c8349c35df1d4a9a370fadef`。该包已统一应用界面、Release `.exe`、安装器和卸载器图标，并完成隔离安装/启动/解析/卸载验收；Authenticode状态仍为`NotSigned`，不能视为正式可信公开发行版。
