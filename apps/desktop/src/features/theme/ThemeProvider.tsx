import { ConfigProvider, theme as antdTheme } from "antd";
import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import zhCN from "antd/locale/zh_CN";
import { bridge, type EffectiveTheme, type ThemePreference, type ThemeState } from "../../bridge";

interface ThemeContextValue extends ThemeState {
  setPreference: (preference: ThemePreference) => Promise<void>;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

const systemDark = () => typeof window.matchMedia === "function"
  && window.matchMedia("(prefers-color-scheme: dark)").matches;

const resolveEffective = (preference: ThemePreference, dark: boolean): EffectiveTheme =>
  preference === "night_dark" || (preference === "system" && dark) ? "night_dark" : "day_gradient";

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<ThemeState>(() => ({
    preference: "system",
    effective_theme: resolveEffective("system", systemDark()),
    updated_at: null,
  }));

  useEffect(() => {
    let cancelled = false;
    void bridge.theme_get_state(systemDark()).then((next) => {
      if (!cancelled) setState(next);
    }).catch(() => undefined);
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = state.effective_theme;
    document.documentElement.style.colorScheme = state.effective_theme === "night_dark" ? "dark" : "light";
  }, [state.effective_theme]);

  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (event: MediaQueryListEvent) => setState((current) => current.preference === "system"
      ? { ...current, effective_theme: resolveEffective("system", event.matches) }
      : current);
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);

  const value = useMemo<ThemeContextValue>(() => ({
    ...state,
    setPreference: async (preference) => {
      const next = await bridge.theme_set_preference(preference, systemDark());
      setState(next);
    },
  }), [state]);
  const dark = state.effective_theme === "night_dark";

  return (
    <ThemeContext.Provider value={value}>
      <ConfigProvider
        locale={zhCN}
        theme={{
          algorithm: dark ? antdTheme.darkAlgorithm : antdTheme.defaultAlgorithm,
          token: {
            colorPrimary: dark ? "#a9a2ff" : "#6057c9",
            colorText: dark ? "#f7f7fb" : "#1f2233",
            colorBgBase: dark ? "#07080b" : "#fbfafc",
            borderRadius: 14,
            fontFamily: '"Microsoft YaHei UI", "PingFang SC", "Noto Sans CJK SC", sans-serif',
          },
        }}
      >
        {children}
      </ConfigProvider>
    </ThemeContext.Provider>
  );
}

export function useThemePreference() {
  const context = useContext(ThemeContext);
  if (!context) throw new Error("ThemeProvider is missing");
  return context;
}
