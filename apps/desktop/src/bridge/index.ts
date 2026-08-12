import type { ReminBridge } from "./contracts";
import { browserBridge } from "./browser-bridge";
import { observedTauriBridge } from "./observed-bridge";

export const bridge: ReminBridge = window.__TAURI_INTERNALS__ ? observedTauriBridge : browserBridge;

export * from "./contracts";
