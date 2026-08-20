import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { Modal } from "antd";
import { bridge, type EnvironmentCheck, type ModelCatalogEntry, type ModelPreset, type PresetPlanReport } from "../../bridge";
import { errorMessage } from "../../utils/app-error";

/**
 * 将 role catalog 的 catalog_id 映射为用户可读的模型主名；未知 id 回退到 id 本身。
 * 只保留“ · ”之前的模型名（例如 “BGE-small-zh-v1.5 · 默认” → “BGE-small-zh-v1.5”），
 * 点号后属于用途/档位的介绍性后缀，不在“查看模型详情”里展示。
 */
function catalogNameMap(entries: ModelCatalogEntry[]): Map<string, string> {
  return new Map(entries.map((entry) => {
    const separator = entry.name.indexOf(" · ");
    const name = separator >= 0 ? entry.name.slice(0, separator) : entry.name;
    return [entry.catalog_id, name];
  }));
}

/**
 * 由 capability_profile 生成面向普通用户的能力标签。
 * 不暴露 BGE / GGUF / 量化等内部技术参数，只描述“能做什么”。
 */
function capabilityLabels(preset: ModelPreset): string[] {
  const labels = ["完整问答", "智能检索", "增强 OCR"];
  if (preset.capability_profile.reranker) labels.push("智能精排");
  if (preset.capability_profile.asr) labels.push("语音输入");
  return labels;
}

/**
 * 依据环境探测的硬件档案判断档位是否在设备能力之内。
 * 只用于提示（推荐 ≠ 强制），不会禁止用户选择。
 */
function presetFitsHardware(preset: ModelPreset, environment: EnvironmentCheck | null | undefined): boolean {
  const ram = environment?.memory_total_gb ?? 0;
  const vram = environment?.gpu_memory_gb ?? 0;
  if ((preset.hardware_profile.min_ram_gb ?? 0) > ram) return false;
  const minVram = preset.hardware_profile.min_vram_gb;
  if (minVram != null && minVram > vram) return false;
  return true;
}

/**
 * 官方四档模型配置预设选择面板（普通用户的唯一配置入口）。
 * 展示四张预设卡片：基础 / 流畅 / 均衡 / 高配；选中后仅持久化 preset_id，
 * 不删除旧模型，具体模型名称折叠在“查看模型详情”里。
 */
export function ModelPresetPanel() {
  const queryClient = useQueryClient();
  const presets = useQuery({ queryKey: ["model-presets"], queryFn: () => bridge.model_preset_list() });
  const selected = useQuery({ queryKey: ["model-preset-selected"], queryFn: () => bridge.model_preset_selected_get() });
  const recommendation = useQuery({ queryKey: ["model-preset-recommendation"], queryFn: () => bridge.model_preset_recommendation() });
  const roleCatalog = useQuery({ queryKey: ["model-role-catalog"], queryFn: () => bridge.model_role_catalog_list() });
  const environment = useQuery({ queryKey: ["environment"], queryFn: () => bridge.environment_get_latest() });
  const indexStale = useQuery({ queryKey: ["index-stale-check"], queryFn: () => bridge.index_stale_check() });

  const names = useMemo(() => catalogNameMap(roleCatalog.data ?? []), [roleCatalog.data]);
  const recommendedId = recommendation.data ?? "";
  const deviceLabel = environment.data?.gpu_name ? `GPU · ${environment.data.gpu_name}` : "CPU";

  // 缺失模型统一从 ModelScope（魔搭社区）下载；确认后再入队，避免静默联网下载。
  const [sourceChoice, setSourceChoice] = useState<PresetPlanReport | null>(null);
  // 临时激活的卡片 id：点击某卡片时高亮并显示资源/速度提示，点击空白处取消，不代表持久生效配置。
  const [activeCard, setActiveCard] = useState<string | null>(null);
  /**
   * 最近一次选中档位的运行时计划报告。
   * 用于「当前配置」卡片的按钮区分文案：都就绪时提示已完成，
   * 仍有缺失模型时提示下载缺失，取消下载后再次点击当前卡片也可直接重新触发。
   *
   * 注意：useState 必须放在所有引用 lastPlan 的派生逻辑之前，否则会命中
   * 暂时性死区 (TDZ)，页面首次渲染直接崩溃。
   */
  const [lastPlan, setLastPlan] = useState<PresetPlanReport | null>(null);

  /**
   * 已持久化档位与就绪状态共同决定「当前配置」的呈现：
   * - selectedPresetId 存在即代表用户已选择该档位（顶部显示「当前配置：xx」）；
   * - 只有全部模型就绪（lastPlan.missing 为 0）才在卡片上打「当前」徽章并显示
   *   「当前配置已就绪」，否则显示「（模型未全部就绪，等待下载完成）」。
   */
  const selectedPresetId = selected.data ?? null;
  const currentMatchesLastPlan = selectedPresetId != null && lastPlan?.preset_id === selectedPresetId;
  const allModelsReady = currentMatchesLastPlan && (lastPlan?.missing.length ?? 0) === 0;
  const hasEffectiveCurrent = selectedPresetId != null && allModelsReady;
  // 已持久化选中档位（无论是否就绪），用于顶部状态与卡片按钮区分。
  const currentPreset = selectedPresetId != null
    ? presets.data?.find((preset) => preset.preset_id === selectedPresetId) ?? null
    : null;

  /**
   * 真正切换档位：持久化 preset_id 并把各角色 active 对齐到已就绪模型。
   * 只在「预览确认无缺失」或「用户在下载确认框点开始下载」之后才调用，
   * 避免「一点就切换、下载确认框还没弹」的误导时序。
   */
  const selectPreset = useMutation({
    mutationFn: (presetId: string) => bridge.model_preset_select(presetId),
    onSuccess: async (report) => {
      setLastPlan(report);
      await queryClient.invalidateQueries({ queryKey: ["model-preset-selected"] });
      // 预设切换只持久化 preset_id，不主动删除旧模型；同步刷新模型状态与角色配置。
      await queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
      await queryClient.invalidateQueries({ queryKey: ["index-stale-check"] });
      // 弹下载确认框改由 previewPreset 控制（先确认、再切换），这里不再自动弹，避免确认后重复弹。
    },
  });

  /**
   * 点击档位后的第一步：只读预览（不持久化、不切换、不写库）。
   * 缺模型就先弹「下载缺失模型」确认框，用户确认后才下载并切换；不缺模型才直接切换。
   */
  const previewPreset = useMutation({
    mutationFn: (presetId: string) => bridge.model_preset_plan(presetId),
    onSuccess: (report) => {
      if (report.missing.length > 0) {
        setSourceChoice(report);
      } else {
        selectPreset.mutate(report.preset_id);
      }
    },
  });

  // 下载任务查询：用于「下载完成后自动刷新为当前配置」。
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });

  /**
   * 让「已持久化档位」的就绪状态与界面自动同步：
   *   1. 首次进入页面 lastPlan 为空 → 用只读 plan_preset 填充，正确显示当前配置；
   *   2. 下载/激活完成后（无进行中任务但上次仍缺失）→ 重新评估，模型就绪后自动
   *      变为「当前配置」，无需用户再点一次。
   * 评估结果与上次一致时不更新引用，避免 effect 无限重查。
   */
  useEffect(() => {
    if (!selectedPresetId) return;
    const known = lastPlan?.preset_id === selectedPresetId;
    const anyPending = downloads.data?.some((job) => job.status === "queued" || job.status === "running") ?? false;
    if (known && (anyPending || (lastPlan?.missing.length ?? 0) === 0)) return;
    let cancelled = false;
    bridge.model_preset_plan(selectedPresetId).then((report) => {
      if (cancelled) return;
      setLastPlan((current) => {
        if (current
          && current.preset_id === report.preset_id
          && current.ready.length === report.ready.length
          && current.missing.length === report.missing.length
          && current.ready.every((item, index) => report.ready[index]?.role === item.role && report.ready[index]?.catalog_id === item.catalog_id)
          && current.missing.every((item, index) => report.missing[index]?.role === item.role && report.missing[index]?.catalog_id === item.catalog_id)
        ) {
          return current; // 内容一致，保持引用不变，避免死循环
        }
        return report;
      });
    }).catch(() => {
      // 只读预览失败不影响主流程，静默忽略。
    });
    return () => { cancelled = true; };
  }, [selectedPresetId, lastPlan, downloads.data]);

  /**
   * 为当前档位缺失的每个角色模型，统一从 ModelScope（魔搭社区）入队下载。
   * 单个模型入队失败不影响其余模型；失败统一显示在下载任务区。
   */
  const enqueueMissingDownloads = async (report: PresetPlanReport) => {
    const entries = roleCatalog.data ?? [];
    for (const item of report.missing) {
      const entry = entries.find((e) => e.catalog_id === item.catalog_id && e.role === item.role);
      if (!entry?.install_edition_id) continue;
      try {
        await bridge.model_download_start(entry.install_edition_id, "modelscope", true);
      } catch (cause) {
        // 单个缺失模型入队失败不影响其余模型；错误统一显示在下载任务区。
        console.error(`preset download failed: ${item.role} ${item.catalog_id}`, cause);
      }
    }
    await queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
  };

  const rebuildIndex = useMutation({
    mutationFn: () => bridge.index_rebuild("REBUILD_INDEX"),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["index-stale-check"] });
    },
  });

  return (
    <section className="model-preset">
      <h2>模型配置</h2>

      <div className="model-preset__status">
        <span>运行设备：<strong>{environment.data ? deviceLabel : "正在检测…"}</strong></span>
        {environment.data?.gpu_memory_gb != null && <span>显存 {environment.data.gpu_memory_gb} GB</span>}
        {environment.data?.memory_total_gb != null && <span>内存 {environment.data.memory_total_gb} GB</span>}
      </div>

      {currentPreset
        ? <p className="model-preset__current">当前配置：<strong>{currentPreset.display_name}</strong>{hasEffectiveCurrent ? "" : "（模型未全部就绪，等待下载完成）"}</p>
        : <p className="model-preset__current">尚未选择配置，可选择下方任一档位。</p>}

      {indexStale.data?.stale && (
        <div role="alert" className="model-preset__stale">
          <div>
            <strong>语义检索模型已更换，需要重建索引</strong>
            <p>更换/升级了语义检索模型，旧索引代仍然沿用旧模型。重建后新的问答检索会应用到最新模型，建议立即执行。</p>
          </div>
          <button type="button" className="primary-button" disabled={rebuildIndex.isPending} onClick={() => rebuildIndex.mutate()}>
            {rebuildIndex.isPending ? "正在重建" : "重建索引"}
          </button>
        </div>
      )}
      {rebuildIndex.isError && <p role="alert" className="inline-error">{errorMessage(rebuildIndex.error)}</p>}

      {selectPreset.isError && <p role="alert" className="inline-error">{errorMessage(selectPreset.error)}</p>}

      <div className="model-preset__columns" onClick={() => setActiveCard(null)}>
        {[0, 1].map((columnIndex) => (
          <div key={columnIndex} className="model-preset__column">
            {(presets.data ?? []).filter((_, index) => index % 2 === columnIndex).map((preset) => {
              // 仅当「持久化 preset_id 与卡片匹配，且所有模型都已就绪」时才显示「当前」
              // 徽章；否则即便数据库里存了 preset_id，只要模型没就绪就不算当前配置。
              const isSelected = hasEffectiveCurrent && preset.preset_id === selectedPresetId;
              const active = activeCard === preset.preset_id;
              const isRecommended = preset.preset_id === recommendedId;
              const fits = presetFitsHardware(preset, environment.data);
              const modelRows: Array<[string, string | null]> = [
                ["问答基础模型", preset.generation],
                ["语义检索", preset.embedding],
                ["智能精排", preset.reranker],
                ["语音识别", preset.asr],
                ["OCR 识别", preset.ocr],
              ];
              return (
                <article
                  key={preset.preset_id}
                  className={active ? "model-preset__card model-preset__card--selected" : "model-preset__card"}
                  onClick={(event) => {
                    // 点击按钮/详情/链接等可交互元素时不切换选中态；点击卡片空白处才能选中或取消。
                    event.stopPropagation();
                    const target = event.target as HTMLElement;
                    if (target.closest("button, details, summary, a")) return;
                    setActiveCard((prev) => (prev === preset.preset_id ? null : preset.preset_id));
                  }}
                >
                  <header>
                    <strong>{preset.display_name}</strong>
                    {isRecommended && <em className="model-preset__badge model-preset__badge--recommended">推荐</em>}
                    {isSelected && <em className="model-preset__badge model-preset__badge--current">当前</em>}
                  </header>
                  <p className="model-preset__desc">{preset.description}</p>
                  <ul className="model-preset__caps">{capabilityLabels(preset).map((label) => <li key={label}>{label}</li>)}</ul>
                  <p className="model-preset__hw">推荐配置：{preset.hardware_profile.description}</p>
                  {active && !fits && <p className="model-preset__warn">该配置可能需要较多内存或显存，运行速度可能较慢。</p>}

                  <details className="model-preset__details">
                    <summary>查看模型详情</summary>
                    <ul className="model-preset__models">
                      {modelRows.map(([label, catalogId]) => (
                        <li key={label}><span>{label}</span><strong>{catalogId ? (names.get(catalogId) ?? catalogId) : "不启用"}</strong></li>
                      ))}
                    </ul>
                  </details>

                  <button
                    type="button"
                    className="primary-button"
                    disabled={previewPreset.isPending || selectPreset.isPending}
                    onClick={() => previewPreset.mutate(preset.preset_id)}
                  >
                    {(() => {
                      // 先只读预览，缺模型才弹确认框；确认/切换期间才出现相应 loading 文案。
                      if (selectPreset.isPending) return "正在切换";
                      if (previewPreset.isPending) return "正在检查";
                      if (preset.preset_id !== selectedPresetId) return "使用此配置";
                      // 已持久化档位：只有「本次会话里被点击过」（即 lastPlan 命中此
                      // preset_id）才进入「已就绪 / 下载缺失模型」的细分状态，否则退回
                      // 「使用此配置」（点击后由预览填充 lastPlan，再显示细分状态）。
                      const matchesLast = lastPlan?.preset_id === preset.preset_id;
                      if (!matchesLast) return "使用此配置";
                      if ((lastPlan?.missing.length ?? 0) === 0) return "当前配置已就绪";
                      return "下载缺失模型";
                    })()}
                  </button>
                </article>
              );
            })}
          </div>
        ))}
      </div>

      {/* 缺失模型确认弹窗：统一从 ModelScope（魔搭社区）下载，不再提供海外源选项。 */}
      <Modal
        open={sourceChoice !== null}
        title="下载缺失模型"
        okText="开始下载"
        cancelText="取消"
        centered
        onCancel={() => setSourceChoice(null)}
        onOk={async () => {
          if (sourceChoice) {
            const report = sourceChoice;
            setSourceChoice(null);
            // 用户确认「开始下载」后，才真正切换档位（持久化 preset_id）+ 入队下载。
            await enqueueMissingDownloads(report);
            selectPreset.mutate(report.preset_id);
          }
        }}
      >
        <p className="model-preset__src-desc">
          当前配置需要从魔搭社区（ModelScope）下载 {sourceChoice?.missing.length ?? 0} 个模型：
        </p>
        {sourceChoice && (
          <ul className="model-preset__src-models">
            {(sourceChoice.missing).map((item) => (
              <li key={`${item.role}:${item.catalog_id}`}>
                <span>{names.get(item.catalog_id) ?? item.catalog_id}</span>
              </li>
            ))}
          </ul>
        )}
      </Modal>
    </section>
  );
}