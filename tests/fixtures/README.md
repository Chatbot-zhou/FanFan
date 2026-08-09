# 拾忆中文多格式测试集

本目录是阶段 0 的确定性测试资料，不包含真实用户数据。所有样本围绕虚构的“归航计划”构造，用于验证中文解析、搜索、问答、抽取、关系识别、异常隔离和源文件只读。

## 目录

- `manifest.json`：格式、预期结果和暂缓项；
- `corpus/`：16 个真实文件样本；
- `../baselines/`：搜索、问答、抽取和关系识别的 JSONL 基准答案；
- `../../scripts/fixtures/`：DOCX、PDF、XLSX、PPTX 的可重复生成脚本。

确定性事实：项目编号 `GH-2025-017`、负责人 `林晓岚`、预算 `286500` 元、RRF 参数 `k=60`。新增样本不得复用真实人员、客户或文件内容。

## 当前覆盖

- 现代 Office：DOCX、XLSX、PPTX；
- PDF：文本型、纯扫描型、加密型和损坏型；
- 文本与网页：Markdown、TXT、CSV、TSV、HTML；
- 图像：PNG OCR 样本；
- 关系：完全重复和版本候选。

DOC、XLS、PPT 及宏格式不会通过改扩展名伪造。旧 Office 样本必须与可选 LibreOffice 离线兼容包一起在阶段 2 补齐；宏格式必须验证只读正文且从不执行宏。

## 自动检查

从仓库根目录运行：

```powershell
python scripts\validate_test_corpus.py
```

检查包括文件完整性、Open XML 结构、确定性事实、加密/损坏 PDF 特征、PNG 签名、完全重复 SHA-256 和四类基准 JSONL 的唯一编号。

