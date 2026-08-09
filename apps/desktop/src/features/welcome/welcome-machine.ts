export type WelcomeStage = "intro" | "transitioning" | "action" | "exiting" | "completed";
export type WelcomeEvent = "ADVANCE" | "TRANSITION_DONE" | "START" | "COMPLETE" | "RECOVER";

export function transitionWelcome(stage: WelcomeStage, event: WelcomeEvent): WelcomeStage {
  if (event === "RECOVER") return "action";
  const transitions: Partial<Record<WelcomeStage, Partial<Record<WelcomeEvent, WelcomeStage>>>> = {
    intro: { ADVANCE: "transitioning" },
    transitioning: { TRANSITION_DONE: "action" },
    action: { START: "exiting" },
    exiting: { COMPLETE: "completed" },
  };
  return transitions[stage]?.[event] ?? stage;
}
