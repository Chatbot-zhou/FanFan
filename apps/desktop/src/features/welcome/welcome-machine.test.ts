import { describe, expect, it } from "vitest";
import { transitionWelcome } from "./welcome-machine";

describe("welcome state machine", () => {
  it("follows the confirmed happy path", () => {
    expect(transitionWelcome("intro", "ADVANCE")).toBe("transitioning");
    expect(transitionWelcome("transitioning", "TRANSITION_DONE")).toBe("action");
    expect(transitionWelcome("action", "START")).toBe("exiting");
    expect(transitionWelcome("exiting", "COMPLETE")).toBe("completed");
  });

  it("ignores duplicate or out-of-order events", () => {
    expect(transitionWelcome("transitioning", "ADVANCE")).toBe("transitioning");
    expect(transitionWelcome("intro", "START")).toBe("intro");
    expect(transitionWelcome("completed", "START")).toBe("completed");
  });

  it("recovers to the safe action screen", () => {
    expect(transitionWelcome("intro", "RECOVER")).toBe("action");
  });
});
