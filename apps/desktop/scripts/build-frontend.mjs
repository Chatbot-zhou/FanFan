import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const desktopRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function run(modulePath, args) {
  const result = spawnSync(process.execPath, [path.join(desktopRoot, modulePath), ...args], {
    cwd: desktopRoot,
    stdio: "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run("node_modules/typescript/bin/tsc", ["-b", "--pretty", "false"]);
run("node_modules/vite/bin/vite.js", ["build"]);
