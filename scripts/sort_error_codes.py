from __future__ import annotations

import json
from pathlib import Path


def main() -> None:
    path = Path(__file__).resolve().parents[1] / "contracts" / "error-codes.json"
    catalog = json.loads(path.read_text(encoding="utf-8-sig"))
    catalog["codes"] = sorted(catalog["codes"], key=lambda entry: entry["code"])
    lines = ["{", f'  "version": {json.dumps(catalog["version"])},', '  "codes": [']
    for index, entry in enumerate(catalog["codes"]):
        suffix = "," if index + 1 < len(catalog["codes"]) else ""
        rendered = json.dumps(entry, ensure_ascii=False, separators=(", ", ": "))
        lines.append(f"    {rendered}{suffix}")
    lines.extend(["  ]", "}"])
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
