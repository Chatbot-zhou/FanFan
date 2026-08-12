import { describe, expect, it } from "vitest";
import { errorMessage, normalizeAppError } from "./app-error";

describe("normalizeAppError", () => {
  it("preserves structured AppError messages", () => {
    expect(errorMessage({ code: "INBOX_UPDATE_FAILED", message: "数据库暂时繁忙", retryable: true })).toBe("数据库暂时繁忙");
  });

  it("never exposes object stringification", () => {
    const normalized = normalizeAppError({ unexpected: { nested: true } });
    expect(normalized.message).not.toContain("[object Object]");
    expect(errorMessage("[object Object]")).not.toContain("[object Object]");
  });

  it("normalizes a structured Tauri error serialized as JSON", () => {
    expect(errorMessage('{"code":"DATABASE_TRANSACTION_FAILED","message":"资料库繁忙","retryable":true}')).toBe("资料库繁忙");
  });
});
