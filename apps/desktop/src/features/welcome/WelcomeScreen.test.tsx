import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WelcomeScreen } from "./WelcomeScreen";

describe("WelcomeScreen", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows no start button on the first screen", () => {
    render(<WelcomeScreen interactive on_completed={() => undefined} on_persist_error={() => undefined} />);
    expect(screen.getByText("拾起你被遗忘的记忆")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "开始使用" })).not.toBeInTheDocument();
  });

  it("allows a blank-area click to reveal the second screen exactly once", () => {
    const { container } = render(<WelcomeScreen interactive on_completed={() => undefined} on_persist_error={() => undefined} />);
    fireEvent.click(container.querySelector(".welcome-screen__content")!);
    fireEvent.click(container.querySelector(".welcome-screen__content")!);
    act(() => vi.advanceTimersByTime(300));
    expect(screen.getByText("拾起散落的信息，连接过去的自己")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "开始使用" })).toBeEnabled();
  });

  it("persists completion before entering the main application", async () => {
    const completed = vi.fn();
    const { container } = render(<WelcomeScreen interactive on_completed={completed} on_persist_error={() => undefined} />);
    fireEvent.click(container.querySelector(".welcome-screen__content")!);
    act(() => vi.advanceTimersByTime(300));
    await act(async () => screen.getByRole("button", { name: "开始使用" }).click());
    act(() => vi.advanceTimersByTime(300));
    expect(completed).toHaveBeenCalledOnce();
    expect(JSON.parse(window.localStorage.getItem("remin.welcome.v1") ?? "{}").welcome_completed).toBe(true);
  });
});
