import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { bridge } from "../bridge";
import { useAppStore } from "../state/app-store";
import { SearchPage } from "./SearchPage";


describe("SearchPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({ search_query: "" });
  });

  it("shows channel availability, match reasons and a real source locator", async () => {
    render(<SearchPage />);
    const input = screen.getByPlaceholderText("输入关键词");
    fireEvent.change(input, { target: { value: "RAG" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    expect(await screen.findByRole("heading", { name: "RAG项目总结.docx" })).toBeInTheDocument();
    expect(screen.getByText(/语义搜索未启用，已自动使用名称与全文/)).toBeInTheDocument();
    expect(screen.getByText("第 18 段")).toBeInTheDocument();
    expect(screen.getByText(/匹配：文件名 · 正文/)).toBeInTheDocument();
    fireEvent.click(screen.getAllByRole("button", { name: "查看内容" })[0]!);
    expect(await screen.findByLabelText("RAG项目总结.docx内容预览")).toHaveTextContent("混合召回和重排");
  });

  it("passes the selected mode, type, time and sort to the search contract", async () => {
    const originalSearch = bridge.search_start.bind(bridge);
    const searchStart = vi.spyOn(bridge, "search_start").mockImplementation((request) => originalSearch(request));
    render(<SearchPage />);
    const choose = async (selectLabel: string, optionLabel: string) => {
      const select = screen.getByLabelText(selectLabel);
      fireEvent.mouseDown(select);
      const option = await waitFor(() => {
        const match = [...document.querySelectorAll<HTMLElement>(".ant-select-item-option")]
          .find((item) => item.textContent?.trim() === optionLabel);
        expect(match).toBeDefined();
        return match!;
      });
      fireEvent.mouseDown(option);
      fireEvent.click(option);
      await waitFor(() => expect(select).toHaveAttribute("aria-expanded", "false"));
    };
    await choose("搜索方式", "文件名");
    await choose("资料类型", "PDF");
    await choose("修改时间", "最近30天");
    await choose("结果排序", "最近修改");
    fireEvent.change(screen.getByPlaceholderText("输入关键词"), { target: { value: "设计" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    expect(await screen.findByText(/找到/)).toBeInTheDocument();
    expect(searchStart).toHaveBeenCalledWith(expect.objectContaining({
      mode: "filename",
      sort: "modified_desc",
      scope: expect.objectContaining({ extensions: ["pdf"], modified_from: expect.any(String) }),
    }));
  });

  it("shows the exact cached image when image text is the search hit", async () => {
    vi.spyOn(bridge, "search_start").mockResolvedValue({
      search_id: "018f0000-0000-7000-8000-000000000710",
      status: "completed",
      channels: { filename: "completed", fulltext: "completed", semantic: "unavailable" },
      results: [{
        file_id: "018f0000-0000-7000-8000-000000000711",
        name: "季度图表.docx",
        extension: "docx",
        display_path: "资料/季度图表.docx",
        modified_at: "2026-08-20T08:00:00Z",
        snippet: "图片说明：柱状图显示第二季度收入明显增长。",
        match_reasons: ["fulltext"],
        locator: null,
        revision_id: "018f0000-0000-7000-8000-000000000712",
        image_asset_id: "018f0000-0000-7000-8000-000000000713",
        scores: { filename: null, fulltext: 0.9, semantic: null, fused: 0.9 },
      }],
      next_cursor: null,
      elapsed_ms: 5,
    });
    render(<SearchPage />);
    fireEvent.change(screen.getByPlaceholderText("输入关键词"), { target: { value: "第二季度收入" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    const image = await screen.findByRole("img", { name: "搜索命中的图片：季度图表.docx" });
    expect(image).toHaveAttribute(
      "src",
      "http://fanfan-image.localhost/018f0000-0000-7000-8000-000000000713",
    );
    expect(screen.getByText("图片内容命中")).toBeInTheDocument();
  });
});
