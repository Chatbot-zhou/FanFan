import { useCallback, useEffect, useRef, useState } from "react";
import { bridge } from "../../bridge";
import { TitleBar } from "../shell/TitleBar";
import { transitionWelcome, type WelcomeStage } from "./welcome-machine";

interface WelcomeScreenProps {
  interactive: boolean;
  on_completed: () => void;
  on_persist_error: (message: string) => void;
}

export function WelcomeScreen({ interactive, on_completed, on_persist_error }: WelcomeScreenProps) {
  const [stage, setStage] = useState<WelcomeStage>("intro");
  const stageRef = useRef(stage);
  stageRef.current = stage;
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const fadeMs = reducedMotion ? 150 : 250;

  const advance = useCallback(() => {
    if (!interactive) return;
    setStage((current) => transitionWelcome(current, "ADVANCE"));
  }, [interactive]);

  useEffect(() => {
    if (!interactive || stage !== "intro") return;
    const timer = window.setTimeout(advance, 3000);
    const watchdog = window.setTimeout(() => setStage("action"), 4000);
    return () => {
      window.clearTimeout(timer);
      window.clearTimeout(watchdog);
    };
  }, [advance, interactive, stage]);

  useEffect(() => {
    if (stage !== "transitioning") return;
    const timer = window.setTimeout(() => setStage((current) => transitionWelcome(current, "TRANSITION_DONE")), fadeMs);
    return () => window.clearTimeout(timer);
  }, [fadeMs, stage]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      if (stageRef.current === "intro") {
        event.preventDefault();
        advance();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [advance]);

  const start = async () => {
    if (stage !== "action") return;
    setStage((current) => transitionWelcome(current, "START"));
    try {
      await bridge.welcome_complete("1.0");
    } catch {
      on_persist_error("欢迎状态未能保存，下次可能再次显示");
    }
    window.setTimeout(() => {
      setStage("completed");
      on_completed();
    }, fadeMs);
  };

  const showSecond = stage === "action" || stage === "exiting" || stage === "completed";

  return (
    <div className={`welcome-screen welcome-screen--${stage}`} onClick={(event) => {
      const target = event.target as HTMLElement;
      if (!target.closest("button")) advance();
    }}>
      <TitleBar model_state={null} welcome />
      <main className="welcome-screen__content">
        <div className="welcome-copy" aria-live="polite">
          <h1 key={showSecond ? "second" : "first"} className={stage === "transitioning" ? "welcome-copy--fading" : ""}>
            {showSecond ? "想得到，搜得到，翻翻知道。" : "想不起来？翻翻就知道。"}
          </h1>
          {showSecond && (
            <button className="welcome-start" type="button" autoFocus disabled={stage !== "action"} onClick={(event) => {
              event.stopPropagation();
              void start();
            }}>
              开始使用
            </button>
          )}
        </div>
      </main>
    </div>
  );
}
