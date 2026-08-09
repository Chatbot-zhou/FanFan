import { useEffect, useState } from "react";
import { bridge } from "../bridge";
import { WelcomeScreen } from "../features/welcome/WelcomeScreen";
import { AppShell } from "../features/shell/AppShell";

type BootState = "checking" | "welcome" | "ready";

export function App() {
  const [bootState, setBootState] = useState<BootState>("checking");
  const [startupNotice, setStartupNotice] = useState<string | null>(null);

  const enterApp = () => {
    setBootState("ready");
  };

  useEffect(() => {
    let cancelled = false;
    void bridge.welcome_get_state().then((state) => {
      if (cancelled) return;
      if (state.welcome_completed) enterApp();
      else setBootState("welcome");
    }).catch(() => {
      if (!cancelled) setBootState("welcome");
    });
    return () => { cancelled = true; };
  }, []);

  if (bootState !== "ready") {
    return (
      <WelcomeScreen
        interactive={bootState === "welcome"}
        on_completed={enterApp}
        on_persist_error={setStartupNotice}
      />
    );
  }

  return <AppShell startup_notice={startupNotice} />;
}
