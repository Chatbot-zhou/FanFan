import type { ReminBridge } from "./contracts";
import { browserBridge } from "./browser-bridge";
import { tauriBridge } from "./tauri-bridge";

export const bridge: ReminBridge = window.__TAURI_INTERNALS__ ? tauriBridge : browserBridge;

export * from "./contracts";
