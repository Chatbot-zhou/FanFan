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
    const input = screen.getByPlaceholderText("例如：去年那个关于RAG召回率优化的文档");
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
    fireEvent.change(screen.getByPlaceholderText("例如：去年那个关于RAG召回率优化的文档"), { target: { value: "设计" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    expect(await screen.findByText(/找到/)).toBeInTheDocument();
    expect(searchStart).toHaveBeenCalledWith(expect.objectContaining({
      mode: "filename",
      sort: "modified_desc",
      scope: expect.objectContaining({ extensions: ["pdf"], modified_from: expect.any(String) }),
    }));
  });
});
