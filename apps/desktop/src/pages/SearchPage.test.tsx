import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { useAppStore } from "../state/app-store";
import { SearchPage } from "./SearchPage";


describe("SearchPage", () => {
  beforeEach(() => {
    useAppStore.setState({ search_query: "" });
  });

  it("shows channel availability, match reasons and a real source locator", async () => {
    render(<SearchPage />);
    const input = screen.getByPlaceholderText("例如：去年那个关于RAG召回率优化的文档");
    fireEvent.change(input, { target: { value: "RAG" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    expect(await screen.findByText("RAG项目总结.docx")).toBeInTheDocument();
    expect(screen.getByText(/语义搜索未启用，已自动使用名称与全文/)).toBeInTheDocument();
    expect(screen.getByText("第 18 段")).toBeInTheDocument();
    expect(screen.getByText(/匹配：文件名 · 正文/)).toBeInTheDocument();
    fireEvent.click(screen.getAllByRole("button", { name: "查看内容" })[0]!);
    expect(await screen.findByLabelText("RAG项目总结.docx内容预览")).toHaveTextContent("混合召回和重排");
  });

  it("passes the selected mode, type, time and sort to the search contract", async () => {
    render(<SearchPage />);
    fireEvent.change(screen.getByLabelText("搜索方式"), { target: { value: "filename" } });
    fireEvent.change(screen.getByLabelText("资料类型"), { target: { value: "pdf" } });
    fireEvent.change(screen.getByLabelText("修改时间"), { target: { value: "30" } });
    fireEvent.change(screen.getByLabelText("结果排序"), { target: { value: "modified_desc" } });
    fireEvent.change(screen.getByPlaceholderText("例如：去年那个关于RAG召回率优化的文档"), { target: { value: "设计" } });
    fireEvent.click(screen.getByRole("button", { name: "搜索" }));

    expect(await screen.findByText(/找到/)).toBeInTheDocument();
    expect(screen.getByDisplayValue("文件名")).toBeInTheDocument();
    expect(screen.getByDisplayValue("PDF")).toBeInTheDocument();
    expect(screen.getByDisplayValue("最近30天")).toBeInTheDocument();
    expect(screen.getByDisplayValue("最近修改")).toBeInTheDocument();
  });
});
