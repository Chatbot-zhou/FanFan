import { useState } from "react";
import { bridge, type ImageAsset } from "../bridge";

export const imageAssetUrl = (assetId: string) => `http://remin-image.localhost/${encodeURIComponent(assetId)}`;

const locationLabel = (asset: ImageAsset) => {
  const { locator } = asset;
  if (locator.page_no) return `第 ${locator.page_no} 页${locator.shape_no ? ` · 区域 ${locator.shape_no}` : ""}`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片${locator.shape_no ? ` · 图形 ${locator.shape_no}` : ""}`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  return asset.asset_kind === "standalone_image" ? "独立图片" : "文档内嵌图片";
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
          <span>{asset.description ?? asset.ocr_text ?? (queued.includes(asset.asset_id) ? "已重新加入本地图片理解队列" : asset.status === "processing" ? "本地图片模型正在理解" : asset.status === "pending_understanding" ? "等待本地图片模型理解" : asset.error?.message ?? "暂无图片说明")}</span>
          {asset.status === "failed" && !queued.includes(asset.asset_id) && <button type="button" disabled={retrying === asset.asset_id} onClick={() => {
            setRetrying(asset.asset_id);
            setRetryError(null);
            void bridge.image_understanding_retry(asset.asset_id)
              .then(() => setQueued((current) => [...current, asset.asset_id]))
              .catch((error) => setRetryError(error instanceof Error ? error.message : String(error)))
              .finally(() => setRetrying(null));
          }}>{retrying === asset.asset_id ? "正在重试" : "重试图片理解"}</button>}
        </figcaption>
      </figure>)}
    </div>
    {retryError && <small role="alert" className="inline-error">{retryError}</small>}
  </section>;
}
