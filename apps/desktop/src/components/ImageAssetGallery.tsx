import { useState } from "react";
import { bridge, type ImageAsset } from "../bridge";
import { errorMessage } from "../utils/app-error";

export const imageAssetUrl = (assetId: string) => `http://fanfan-image.localhost/${encodeURIComponent(assetId)}`;

const locationLabel = (asset: ImageAsset) => {
  const { locator } = asset;
  if (locator.page_no) return `第 ${locator.page_no} 页${locator.shape_no ? ` · 区域 ${locator.shape_no}` : ""}`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片${locator.shape_no ? ` · 图形 ${locator.shape_no}` : ""}`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  return asset.asset_kind === "standalone_image" ? "独立图片" : "文档内嵌图片";
};

const statusLabel = (asset: ImageAsset, queued: boolean) => {
  if (queued) return "已重新加入本地图片理解队列";
  if (asset.status === "pending_ocr") return "等待本地 OCR；识别不足时会自动进入图片理解";
  if (asset.status === "ocr_processing") return "本地 OCR 正在识别图片";
  if (asset.status === "processing") return "本地图片模型正在理解";
  if (asset.status === "pending_understanding") return "OCR 信息不足，等待本地图片模型理解";
  return asset.error?.message ?? "暂无图片说明";
};

export function ImageAssetGallery({ assets }: { assets: ImageAsset[] }) {
  const [retrying, setRetrying] = useState<string | null>(null);
  const [queued, setQueued] = useState<string[]>([]);
  const [retryError, setRetryError] = useState<string | null>(null);
  if (!assets.length) return null;
  return <section className="image-asset-gallery" aria-label="资料中的图片证据">
    <header><strong>图片与图表</strong><small>{assets.length} 项 · 只读缓存预览</small></header>
    <div>
      {assets.map((asset) => <figure key={asset.asset_id}>
        <img src={imageAssetUrl(asset.asset_id)} alt={asset.description ?? asset.ocr_text ?? locationLabel(asset)} loading="lazy" />
        <figcaption>
          <strong>{locationLabel(asset)}</strong>
          <span>{asset.description ?? asset.ocr_text ?? statusLabel(asset, queued.includes(asset.asset_id))}</span>
          {asset.ocr_engine && <small>OCR：{asset.ocr_engine}{asset.ocr_confidence !== null ? ` · 置信度 ${Math.round(asset.ocr_confidence * 100)}%` : ""}</small>}
          {asset.vision_route_reason && asset.status !== "ready" && <small>图片理解原因：{asset.vision_route_reason}</small>}
          {asset.status === "failed" && !queued.includes(asset.asset_id) && <button type="button" disabled={retrying === asset.asset_id} onClick={() => {
            setRetrying(asset.asset_id);
            setRetryError(null);
            void bridge.image_understanding_retry(asset.asset_id)
              .then(() => setQueued((current) => [...current, asset.asset_id]))
        .catch((error) => setRetryError(errorMessage(error)))
              .finally(() => setRetrying(null));
          }}>{retrying === asset.asset_id ? "正在重试" : "重试图片理解"}</button>}
        </figcaption>
      </figure>)}
    </div>
    {retryError && <small role="alert" className="inline-error">{retryError}</small>}
  </section>;
}
