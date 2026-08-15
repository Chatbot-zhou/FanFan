from __future__ import annotations

import argparse
import json
import sys
from contextlib import redirect_stdout
from pathlib import Path
from threading import Event, Thread
from time import monotonic

from .protocol import WorkerError, WorkerRequest, WorkerResponse
from .service import WorkerService

HEARTBEAT_INTERVAL_SECONDS = 2.0


def _heartbeat_loop(heartbeat_file: Path, stop: Event) -> None:
    """向心跳文件周期写入内容以更新 mtime，供 Rust 侧 watchdog 探测进程活性。

    心跳必须独立于 JSONL 协议通道：Rust 侧读响应是同步阻塞的，管道里不能混入
    额外数据；文件 mtime 是零协议的进程级心跳，PyInstaller 打包版同样适用。
    """
    while not stop.wait(HEARTBEAT_INTERVAL_SECONDS):
        try:
            heartbeat_file.write_text(f"{monotonic():.3f}", encoding="ascii")
        except OSError:
            # 心跳文件不可写（临时目录被清理等）时静默退出，主循环不受影响。
            return


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fanfan_worker")
    parser.add_argument(
        "--role",
        choices=["parse", "onnx", "ocr", "speech"],
        default=None,
        help="sidecar 角色：只注册该角色所需操作；缺省为全量（测试/兼容模式）",
    )
    parser.add_argument(
        "--heartbeat-file",
        default=None,
        help="心跳文件路径：进程存活期间每 2 秒更新其 mtime",
    )
    args = parser.parse_args(argv)

    # JSONL协议固定使用UTF-8，不能依赖Windows区域设置或父进程代码页。
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    service = WorkerService(role=args.role)

    stop_heartbeat = Event()
    if args.heartbeat_file:
        heartbeat_path = Path(args.heartbeat_file)
        try:
            heartbeat_path.parent.mkdir(parents=True, exist_ok=True)
            heartbeat_path.write_text("0", encoding="ascii")
        except OSError:
            heartbeat_path = None
        if heartbeat_path is not None:
            heartbeat_thread = Thread(
                target=_heartbeat_loop,
                args=(heartbeat_path, stop_heartbeat),
                name="fanfan-heartbeat",
                daemon=True,
            )
            heartbeat_thread.start()

    try:
        for line in sys.stdin:
            try:
                request = WorkerRequest.from_dict(json.loads(line))
                # Native/ML libraries sometimes print banners or warnings to stdout.
                # stdout is the JSONL protocol channel, so any such text would make
                # the Rust client report WORKER_RESPONSE_INVALID and lose the real
                # structured error. Route third-party output away from the protocol.
                with redirect_stdout(sys.stderr):
                    response = service.handle(request)
            except (ValueError, TypeError, json.JSONDecodeError) as error:
                response = WorkerResponse(
                    request_id="unknown",
                    ok=False,
                    result=None,
                    error=WorkerError("REQUEST_INVALID", str(error), False),
                )
            except Exception as error:
                # 兜底：任何未预料的异常都不能让整个 worker 进程退出，否则排队中的
                # 所有任务都会以 WORKER_RESPONSE_INVALID 丢失并反复重试。
                response = WorkerResponse(
                    request_id="unknown",
                    ok=False,
                    result=None,
                    error=WorkerError(
                        "WORKER_INTERNAL_ERROR",
                        f"{type(error).__name__}: {error}",
                        True,
                    ),
                )
            print(json.dumps(response.to_dict(), ensure_ascii=False), flush=True)
    finally:
        stop_heartbeat.set()
        if args.heartbeat_file and heartbeat_path is not None:
            try:
                heartbeat_path.unlink(missing_ok=True)
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
