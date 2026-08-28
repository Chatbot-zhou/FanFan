import { describe, expect, it } from "vitest";
import { pdfPreviewSourceUrls } from "./PdfVisualPreview";

describe("pdfPreviewSourceUrls", () => {
  it("uses the registered Tauri PDF protocol and Windows localhost fallback", () => {
    expect(pdfPreviewSourceUrls("018f0000-0000-7000-8000-000000000001")).toEqual([
      "fanfan-pdf://localhost/018f0000-0000-7000-8000-000000000001",
      "http://fanfan-pdf.localhost/018f0000-0000-7000-8000-000000000001",
    ]);
  });

  it("encodes path segments safely", () => {
    expect(pdfPreviewSourceUrls("id with spaces")).toEqual([
      "fanfan-pdf://localhost/id%20with%20spaces",
      "http://fanfan-pdf.localhost/id%20with%20spaces",
    ]);
  });
});