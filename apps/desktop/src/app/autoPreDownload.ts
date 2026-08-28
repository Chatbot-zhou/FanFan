import { bridge } from "../bridge";

/**
 * 「首启自动预下载」的一次性标记键。
 *
 * 仅当某次安装首次进入主界面（且已通过欢迎引导、后端就绪）时触发一次自动下载，之后
 * 不再重复触发，避免每次启动都做网络探测/重复入队。存在该键仅代表“已尝试过”，不代表
 * 模型已全部就绪——下载本身幂等（只对缺失的 ModelScope 文件模型入队），未就绪仍可走
 * 「模型配置」页手动补齐。
 */
const AUTO_MODEL_DOWNLOAD_KEY = "fanfan.auto_model_download.v1";

/**
 * 首启静默预下载缺失的 ModelScope 文件模型（rerank/OCR/ASR）。
 *
 * 设计意图：安装包只内嵌 worker 引擎与主程序，文件模型均在线（ModelScope）。旧逻辑里
 * 模型要等用户进「模型配置」页点下载、或首次用到该功能才拉取；这里改为用户安装后首次
 * 启动即静默后台入队，保证后续提问/检索/OCR/语音功能无需等待。
 *
 * 规则约束：
 *   1. 只下载缺失组件（`model_preset_plan` 的 missing），已就绪的不重建任务；
 *   2. 只处理 ModelScope 文件模型角色（reranker/ocr/asr），generation/embedding/vision
 *      由本机 Ollama 管理，不在此自动拉取，避免与既有 Ollama 引导/驻留逻辑冲突；
 *   3. 跳过已在 queued/running 的相同版本任务，避免制造重复下载；
 *   4. 全程不弹确认框（静默后台），进度由标题栏状态中心展示；
 *   5. 幂等：任一步骤异常时不落「已完成」标记，下次启动可重试；单模型入队失败不阻断
 *      其余模型。
 */
export async function maybeAutoPreDownload(): Promise<void> {
  if (localStorage.getItem(AUTO_MODEL_DOWNLOAD_KEY) === "done") return;
  const run = async (): Promise<boolean> => {
    // 解析目标预设：优先用已持久化档位，未选择时用硬件推荐档位（推荐 ≠ 强制选择）。
    let presetId: string | null = null;
    try {
      presetId = await bridge.model_preset_selected_get();
    } catch {
      presetId = null;
    }
    if (!presetId) {
      try {
        presetId = await bridge.model_preset_recommendation();
      } catch {
        presetId = null;
      }
    }
    if (!presetId) return true; // 无档位可参考，本次静默跳过且不再重试
    const report = await bridge.model_preset_plan(presetId);
    if (report.missing.length === 0) return true;
    const entries = await bridge.model_role_catalog_list();
    const jobs = await bridge.model_download_list();
    const activeEditions = new Set(
      jobs
        .filter((job) => job.status === "queued" || job.status === "running")
        .map((job) => job.edition_id),
    );
    for (const item of report.missing) {
      // 仅供 Ollama 托管/可自动拉取的角色不在此处理，留给既有引导流程。
      if (item.role === "generation" || item.role === "embedding" || item.role === "vision") continue;
      const entry = entries.find(
        (candidate) => candidate.catalog_id === item.catalog_id && candidate.role === item.role,
      );
      const editionId = entry?.install_edition_id;
      if (!editionId) continue;
      if (!entry!.supported_sources.includes("modelscope")) continue;
      if (activeEditions.has(editionId)) continue;
      try {
        await bridge.model_download_start(editionId, "modelscope", true);
        activeEditions.add(editionId);
      } catch {
        // 单模型入队失败不阻断其余模型；错误统一显示在标题栏状态中心/下载任务区。
      }
    }
    return true;
  };
  await run()
    .then(() => {
      localStorage.setItem(AUTO_MODEL_DOWNLOAD_KEY, "done");
    })
    .catch(() => {
      // 关键流程失败（如后端未就绪）不落已完成标记，下次启动重试。
      localStorage.removeItem(AUTO_MODEL_DOWNLOAD_KEY);
    });
}