import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

const outputDir = path.resolve(process.argv[2]);
const qaDir = path.resolve(process.argv[3]);
const artifactWorkspace = path.resolve(process.argv[4]);
const artifactEntrypoint = path.join(
  artifactWorkspace,
  "node_modules",
  "@oai",
  "artifact-tool",
  "dist",
  "artifact_tool.mjs",
);
const { Presentation, PresentationFile, SpreadsheetFile, Workbook } = await import(
  pathToFileURL(artifactEntrypoint).href
);
await fs.mkdir(outputDir, { recursive: true });
await fs.mkdir(qaDir, { recursive: true });

async function saveBlob(targetPath, blob) {
  await fs.writeFile(targetPath, new Uint8Array(await blob.arrayBuffer()));
}

async function buildWorkbook() {
  const workbook = Workbook.create();
  const results = workbook.worksheets.add("评估结果");
  const ledger = workbook.worksheets.add("项目台账");

  results.showGridLines = false;
  results.getRange("A1:E1").merge();
  results.getRange("A1").values = [["归航计划检索效果评估"]];
  results.getRange("A1:E1").format = {
    fill: "#E8ECF8",
    font: { bold: true, color: "#25324D", size: 16 },
    horizontalAlignment: "left",
  };
  results.getRange("A3:E7").values = [
    ["方案", "Recall@10", "P95延迟(ms)", "查询数", "是否达标"],
    ["文件名", 0.72, 86, 50, null],
    ["全文FTS5", 0.84, 218, 50, null],
    ["语义向量", 0.88, 1860, 50, null],
    ["混合RRF(k=60)", 0.94, 2360, 50, null],
  ];
  results.getRange("E4").formulas = [["=IF(B4>=0.9,\"是\",\"否\")"]];
  results.getRange("E4:E7").fillDown();
  results.getRange("A3:E3").format = { fill: "#536FAE", font: { bold: true, color: "#FFFFFF" } };
  results.getRange("A3:E7").format.borders = { preset: "inside", style: "thin", color: "#D9DEEA" };
  results.getRange("B4:B7").format.numberFormat = "0.0%";
  results.getRange("C4:D7").format.numberFormat = "#,##0";
  results.getRange("A:A").format.columnWidth = 24;
  results.getRange("B:B").format.columnWidth = 14;
  results.getRange("C:C").format.columnWidth = 18;
  results.getRange("D:E").format.columnWidth = 14;
  results.freezePanes.freezeRows(3);

  ledger.showGridLines = false;
  ledger.getRange("A1:D1").merge();
  ledger.getRange("A1").values = [["项目台账"]];
  ledger.getRange("A1:D1").format = { fill: "#F4E8F1", font: { bold: true, color: "#4A3154", size: 16 } };
  ledger.getRange("A3:D7").values = [
    ["项目编号", "负责人", "预算(元)", "评审日期"],
    ["GH-2025-017", "林晓岚", 286500, new Date("2025-11-18T00:00:00Z")],
    ["GH-2025-018", "周予安", 92000, new Date("2025-12-02T00:00:00Z")],
    ["GH-2025-019", "陈默", 86000, new Date("2025-09-26T00:00:00Z")],
    ["合计", "", null, null],
  ];
  ledger.getRange("C7").formulas = [["=SUM(C4:C6)"]];
  ledger.getRange("A3:D3").format = { fill: "#765785", font: { bold: true, color: "#FFFFFF" } };
  ledger.getRange("C4:C7").format.numberFormat = "#,##0";
  ledger.getRange("D4:D6").format.numberFormat = "yyyy-mm-dd";
  ledger.getRange("A:D").format.columnWidth = 18;

  const inspect = await workbook.inspect({ kind: "table,formula", maxChars: 5000, tableMaxRows: 10, tableMaxCols: 8 });
  await fs.writeFile(path.join(qaDir, "workbook-inspect.ndjson"), inspect.ndjson, "utf8");
  const errors = await workbook.inspect({
    kind: "match",
    searchTerm: "#REF!|#DIV/0!|#VALUE!|#NAME\\?|#N/A",
    options: { useRegex: true, maxResults: 100 },
    summary: "fixture formula error scan",
  });
  await fs.writeFile(path.join(qaDir, "workbook-errors.ndjson"), errors.ndjson, "utf8");
  await saveBlob(
    path.join(qaDir, "workbook-评估结果.png"),
    await workbook.render({ sheetName: "评估结果", range: "A1:E7", scale: 2 }),
  );
  await saveBlob(
    path.join(qaDir, "workbook-项目台账.png"),
    await workbook.render({ sheetName: "项目台账", range: "A1:D7", scale: 2 }),
  );
  const file = await SpreadsheetFile.exportXlsx(workbook);
  const outputPath = path.join(outputDir, "06-检索评估与项目台账.xlsx");
  await file.save(outputPath);
  await fs.rm(`${outputPath}.inspect.ndjson`, { force: true });
}

function addText(slide, name, text, position, style) {
  const shape = slide.shapes.add({
    geometry: "textbox",
    name,
    position,
    fill: "none",
    line: { style: "solid", fill: "none", width: 0 },
  });
  shape.text = text;
  shape.text.style = style;
  return shape;
}

async function buildPresentation() {
  const deck = Presentation.create({ slideSize: { width: 1280, height: 720 } });
  const first = deck.slides.add();
  first.background.fill = "#F7F1ED";
  addText(first, "brand", "拾忆 · 阶段0测试资料", { left: 72, top: 58, width: 420, height: 32 }, { fontSize: 18, bold: true, color: "#6F7290" });
  addText(first, "title", "归航计划阶段汇报", { left: 72, top: 205, width: 900, height: 88 }, { fontSize: 54, bold: true, color: "#26324D" });
  addText(first, "subtitle", "完全离线的信息找回与证据定位", { left: 76, top: 310, width: 760, height: 54 }, { fontSize: 28, color: "#725E78" });
  addText(first, "meta", "项目编号 GH-2025-017  |  负责人 林晓岚  |  2025-11-18", { left: 76, top: 575, width: 940, height: 36 }, { fontSize: 18, color: "#767784" });

  const second = deck.slides.add();
  second.background.fill = "#F6F8FC";
  addText(second, "title", "混合RRF率先达到搜索目标", { left: 72, top: 54, width: 920, height: 58 }, { fontSize: 38, bold: true, color: "#26324D" });
  addText(second, "takeaway", "Recall@10 = 94%  ·  评测基线 k = 60", { left: 74, top: 125, width: 730, height: 44 }, { fontSize: 24, color: "#6E4E80", bold: true });
  second.charts.add("bar", {
    position: { left: 110, top: 215, width: 780, height: 360 },
    categories: ["文件名", "FTS5", "语义", "混合RRF"],
    series: [{ name: "Recall@10", values: [72, 84, 88, 94], fill: "#7085C4" }],
    hasLegend: false,
    dataLabels: { showValue: true, position: "outEnd" },
    xAxis: { min: 0, max: 100 },
  });
  addText(second, "note", "语义不可用时仍保留文件名与全文结果\n基础模式不被阻塞", { left: 925, top: 265, width: 300, height: 130 }, { fontSize: 18, color: "#4E566B" });

  const third = deck.slides.add();
  third.background.fill = "#FAF4F7";
  addText(third, "title", "下一检查点聚焦源文件保护与严格引用", { left: 72, top: 54, width: 1080, height: 62 }, { fontSize: 37, bold: true, color: "#342B46" });
  addText(third, "item1", "01  源文件E2E前后SHA-256保持不变", { left: 105, top: 205, width: 880, height: 54 }, { fontSize: 27, bold: true, color: "#536FAE" });
  addText(third, "item2", "02  每个事实性结论至少一个原文引用", { left: 105, top: 310, width: 880, height: 54 }, { fontSize: 27, bold: true, color: "#75577E" });
  addText(third, "item3", "03  无足够证据时明确拒答，不生成unsupported结论", { left: 105, top: 415, width: 1000, height: 72 }, { fontSize: 27, bold: true, color: "#A5667B" });
  addText(third, "footer", "下一次复核：2025年12月2日 10:00", { left: 105, top: 585, width: 620, height: 34 }, { fontSize: 18, color: "#767784" });

  const snapshot = await deck.inspect({ kind: "slide,textbox,chart", maxChars: 8000 });
  await fs.writeFile(path.join(qaDir, "presentation-inspect.ndjson"), snapshot.ndjson, "utf8");
  for (const [index, slide] of deck.slides.items.entries()) {
    await saveBlob(
      path.join(qaDir, `presentation-slide-${index + 1}.png`),
      await deck.export({ slide, format: "png", scale: 1 }),
    );
    const layout = await slide.export({ format: "layout" });
    await fs.writeFile(path.join(qaDir, `presentation-slide-${index + 1}.layout.json`), await layout.text(), "utf8");
  }
  const file = await PresentationFile.exportPptx(deck);
  const outputPath = path.join(outputDir, "07-归航计划阶段汇报.pptx");
  await file.save(outputPath);
  await fs.rm(`${outputPath}.inspect.ndjson`, { force: true });
}

await buildWorkbook();
await buildPresentation();
