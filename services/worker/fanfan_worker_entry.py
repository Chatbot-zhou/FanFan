"""PyInstaller入口：让打包后的Worker保持与开发模式相同的JSONL协议。"""

from fanfan_worker.__main__ import main


if __name__ == "__main__":
    raise SystemExit(main())
