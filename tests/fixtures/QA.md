# 测试资料质量记录

更新日期：2026-08-01

| 资料 | 检查 | 结果 |
|---|---|---|
| DOCX | Open XML结构、必需正文、表格固定宽度 | 通过 |
| DOCX | LibreOffice转PDF逐页视觉检查 | 本机无LibreOffice，未执行；不得据此声明视觉验证通过 |
| XLSX | 两个工作表渲染、公式错误扫描、结构检查 | 通过 |
| PPTX | 三页渲染目检、布局溢出检查、结构检查 | 通过 |
| 文本PDF | Poppler 144 DPI渲染目检 | 通过 |
| 扫描PDF/PNG | Poppler渲染目检、PNG签名 | 通过 |
| 加密PDF | PDF加密字典存在 | 通过 |
| 损坏PDF | 无有效xref和EOF | 通过 |
| 完全重复样本 | SHA-256一致 | 通过 |
| 四类基准 | JSONL可解析且case_id全局唯一 | 通过，共14例 |

视觉检查产物只保存在 `.artifacts/fixture-build/`，不进入版本库。构建脚本、最终测试资料、清单和基准答案进入版本库。

