import { FolderOpenOutlined, HddOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { bridge } from "../../bridge";

export function RootAuthorizationPage({ onCompleted }: { onCompleted: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const choose = async (volumeOnly: boolean) => {
    setError(null);
    if (!isTauri()) {
      setError("浏览器预览不会读取电脑目录，请在桌面程序中选择资料位置。");
      return;
    }
    const selected = await open({
      directory: true,
      multiple: false,
      title: volumeOnly ? "选择要扫描的本地磁盘" : "选择要加入拾忆的资料文件夹",
    });
    if (typeof selected !== "string") return;
    const fullVolume = /^[a-zA-Z]:\\?$/.test(selected);
    if (volumeOnly && !fullVolume) {
      setError("添加整个磁盘时请选择盘符根目录，例如 D:\\。");
      return;
    }
    if (fullVolume && !window.confirm("扫描整个磁盘可能持续较长时间。系统、程序、凭据、应用数据和拾忆自身目录会被强制排除。确认授权吗？")) return;
    setBusy(true);
    try {
      await bridge.root_add({
        path: selected,
        label: null,
        watch_mode: "realtime",
        authorization_source: "user_selected",
        full_volume_confirmed: fullVolume,
      });
      await onCompleted();
    } catch (actionError) {
      setError(actionError instanceof Error ? actionError.message : String(actionError));
    } finally {
      setBusy(false);
    }
  };

  const skip = async () => {
    setBusy(true);
    setError(null);
    try {
      await onCompleted();
    } catch (actionError) {
      setError(actionError instanceof Error ? actionError.message : String(actionError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="authorization-page">
      <div className="authorization-card">
        <span className="authorization-card__mark"><SafetyCertificateOutlined /></span>
        <h1>选择要交给拾忆理解的资料</h1>
        <p>拾忆不会自动扫描桌面、文档或图片。只有你明确选择的位置才会被读取，源文件始终保持只读。</p>
        <div className="authorization-actions">
          <button className="primary-button" type="button" disabled={busy} onClick={() => void choose(false)}><FolderOpenOutlined /> 选择资料文件夹</button>
          <button type="button" disabled={busy} onClick={() => void choose(true)}><HddOutlined /> 添加整个磁盘</button>
        </div>
        <small>整盘授权仍会强制排除系统、程序、凭据、应用数据和重解析点。</small>
        {error && <p className="inline-error" role="alert">{error}</p>}
        <button className="text-button authorization-skip" type="button" disabled={busy} onClick={() => void skip()}>暂不添加，进入空资料库</button>
      </div>
    </main>
  );
}
