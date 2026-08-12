from __future__ import annotations

import json
import sys

from .protocol import WorkerError, WorkerRequest, WorkerResponse
from .service import WorkerService


def main() -> int:
    # JSONL协议固定使用UTF-8，不能依赖Windows区域设置或父进程代码页。
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    service = WorkerService()
    for line in sys.stdin:
        try:
            request = WorkerRequest.from_dict(json.loads(line))
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
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
