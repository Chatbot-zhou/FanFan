import { describe, expect, it } from "vitest";
import type { ExecutionUnit, ValidationCheckpoint } from "./contracts";
import { canTransitionJobStatus, isValidRuntimeEventName, validateCheckpoint, validateExecutionUnit } from "./contract-validators";
import { RUNTIME_EVENTS } from "./runtime-events";

const uuid7 = "018f0000-0000-7000-8000-000000000001";

const unit = (): ExecutionUnit => ({
  unit_id: uuid7,
  unit_type: "document.probe",
  input_schema: "remin://schema/document-probe-input/v1",
  output_schema: "remin://schema/document-probe-output/v1",
  inputs: {},
  preconditions: [],
  postconditions: [],
  timeout_ms: 5_000,
  retry_policy: { max_attempts: 2, backoff_ms: 250, backoff_multiplier: 2, retryable_codes: [] },
  idempotency_key: "document.probe:test",
  risk_level: "low",
  checkpoint_policy: "on_success",
  fallback_unit_types: [],
});

describe("public contract validators", () => {
  it("accepts a complete atomic execution unit", () => {
    expect(validateExecutionUnit(unit())).toEqual([]);
  });

  it("requires always checkpoints for high-risk units", () => {
    const value = unit();
    value.risk_level = "high";
    expect(validateExecutionUnit(value).map((item) => item.code)).toContain("SCHEMA_CHECKPOINT_REQUIRED");
  });

  it("does not restart terminal jobs", () => {
    expect(canTransitionJobStatus("succeeded", "running")).toBe(false);
    expect(canTransitionJobStatus("running", "failed")).toBe(true);
  });

  it("requires an error for failed checkpoints", () => {
    const checkpoint: ValidationCheckpoint = {
      checkpoint_id: uuid7,
      job_id: "018f0000-0000-7000-8000-000000000002",
      unit_id: "018f0000-0000-7000-8000-000000000003",
      checkpoint_type: "schema",
      status: "failed",
      rules_version: "1.0",
      metrics: {},
      error: null,
      created_at: new Date().toISOString(),
      resume_token: null,
    };
    expect(validateCheckpoint(checkpoint).map((item) => item.code)).toEqual(["SCHEMA_CHECKPOINT_ERROR_REQUIRED"]);
  });

  it("uses only Tauri-compatible runtime event names", () => {
    expect(Object.values(RUNTIME_EVENTS).every(isValidRuntimeEventName)).toBe(true);
    expect(isValidRuntimeEventName("startup.state")).toBe(false);
  });
});
