import appIconUrl from "../assets/fanfan-logo.png";
import wordmarkUrl from "../assets/fanfan-wordmark.png";

interface BrandMarkProps {
  compact?: boolean;
  inverse?: boolean;
}

export function BrandMark({ compact = false, inverse = false }: BrandMarkProps) {
  return (
    <div className={`brand-mark${compact ? " brand-mark--compact" : ""}${inverse ? " brand-mark--inverse" : ""}`} aria-label="翻翻">
      <img className="brand-mark__symbol" src={appIconUrl} alt="" aria-hidden="true" draggable={false} />
      <img className="brand-mark__wordmark" src={wordmarkUrl} alt="翻翻" draggable={false} />
    </div>
  );
}
