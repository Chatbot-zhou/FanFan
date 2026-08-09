import appIconUrl from "../../src-tauri/icons/icon.svg";

interface BrandMarkProps {
  compact?: boolean;
  inverse?: boolean;
}

export function BrandMark({ compact = false, inverse = false }: BrandMarkProps) {
  return (
    <div className={`brand-mark${compact ? " brand-mark--compact" : ""}${inverse ? " brand-mark--inverse" : ""}`} aria-label="拾忆">
      <img className="brand-mark__symbol" src={appIconUrl} alt="" aria-hidden="true" draggable={false} />
      <span className="brand-mark__word">拾忆</span>
    </div>
  );
}
