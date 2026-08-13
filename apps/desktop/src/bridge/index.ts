import type { FanFanBridge } from "./contracts";
import { browserBridge } from "./browser-bridge";
import { observedTauriBridge } from "./observed-bridge";

export const bridge: FanFanBridge = window.__TAURI_INTERNALS__ ? observedTauriBridge : browserBridge;

export * from "./contracts";
