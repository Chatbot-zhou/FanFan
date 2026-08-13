import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import appIconUrl from "../assets/fanfan-logo.png";
import { BrandMark } from "./BrandMark";

describe("BrandMark", () => {
  it("uses the canonical application icon instead of a duplicated inline SVG", () => {
    const { container } = render(<BrandMark compact />);
    const icon = container.querySelector<HTMLImageElement>("img.brand-mark__symbol");

    expect(icon).not.toBeNull();
    expect(icon?.getAttribute("src")).toBe(appIconUrl);
    expect(icon).toHaveAttribute("draggable", "false");
    expect(container.querySelector("svg.brand-mark__symbol")).toBeNull();
  });
});
