"""Phase 1 audit smoke test — Jupyter wire protocol end-to-end.

Stands up a kernel via HTTP, opens the messaging WebSocket, sends an
execute_request for `1 + 1`, collects iopub messages until idle, and
prints the MIME bundles we got back. Proves the protocol surface
Phase 2's Rust client will need to drive is actually drivable.

Run with the gateway already exposed on 127.0.0.1:18888:
    docker run --rm -d --name sandbox-smoke -p 127.0.0.1:18888:8888 \\
        execlaw/python-sandbox-fast:0.1.0
    python plugins/python-sandbox/smoke_execute.py

Not shipped with the image. This file lives in the plugin dir so it's
co-located with what it tests; it is excluded from the docker build
because COPY only pulls Dockerfile / requirements / config / schemas.
"""

from __future__ import annotations

import asyncio
import datetime as dt
import json
import sys
import urllib.parse
import urllib.request
import uuid
from typing import Any

import websockets

GATEWAY = "http://127.0.0.1:18888"


def http(method: str, path: str, body: dict[str, Any] | None = None) -> Any:
    """Tiny HTTP helper — keeps the smoke test free of extra deps."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        urllib.parse.urljoin(GATEWAY, path),
        data=data,
        headers={"Content-Type": "application/json"} if data else {},
        method=method,
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        raw = r.read()
        return json.loads(raw) if raw else None


def make_message(msg_type: str, content: dict[str, Any]) -> dict[str, Any]:
    return {
        "header": {
            "msg_id": str(uuid.uuid4()),
            "username": "smoke",
            "session": str(uuid.uuid4()),
            "msg_type": msg_type,
            "version": "5.3",
            "date": dt.datetime.now(dt.UTC).isoformat().replace("+00:00", "Z"),
        },
        "parent_header": {},
        "metadata": {},
        "content": content,
        "channel": "shell",
        "buffers": [],
    }


async def execute(kernel_id: str, code: str, *, timeout: float = 10.0) -> list[dict[str, Any]]:
    """Open the channels WS, send execute_request, collect outputs."""
    # Derive the WS URL from GATEWAY so a `--gateway` override / monkey-
    # patch reaches both HTTP and WS paths (earlier this was hardcoded
    # to :18888 and silently desynced from the override).
    ws_base = GATEWAY.replace("http://", "ws://").replace("https://", "wss://")
    ws_uri = f"{ws_base}/api/kernels/{kernel_id}/channels"
    outputs: list[dict[str, Any]] = []
    request = make_message(
        "execute_request",
        {
            "code": code,
            "silent": False,
            "store_history": True,
            "user_expressions": {},
            "allow_stdin": False,
            "stop_on_error": True,
        },
    )
    request_msg_id = request["header"]["msg_id"]
    async with websockets.connect(ws_uri) as ws:
        await ws.send(json.dumps(request))
        while True:
            raw = await asyncio.wait_for(ws.recv(), timeout=timeout)
            envelope = json.loads(raw)
            parent = envelope.get("parent_header", {}).get("msg_id")
            if parent != request_msg_id:
                # Stray broadcast from a sibling kernel — ignore.
                continue
            mt = envelope["header"]["msg_type"]
            content = envelope["content"]
            channel = envelope.get("channel")
            print(f"  [{channel:<6}] {mt:<20} {content}")
            if mt in {"stream", "display_data", "execute_result", "error"}:
                outputs.append({"msg_type": mt, "content": content})
            if mt == "status" and content.get("execution_state") == "idle":
                # The iopub idle that follows the same execute_request
                # marks the end of output. Don't break on the
                # execute_reply on shell — iopub can still deliver
                # display_data after the reply.
                break
    return outputs


async def main() -> int:
    print("=== creating kernel ===")
    kernel = http("POST", "/api/kernels", {"name": "python3"})
    kid = kernel["id"]
    print(f"  kernel_id = {kid}\n")

    try:
        # Test 1: simple eval — should yield one execute_result with text/plain "2"
        print("=== execute `1 + 1` ===")
        outs = await execute(kid, "1 + 1")
        result = next((o for o in outs if o["msg_type"] == "execute_result"), None)
        assert result is not None, "no execute_result for `1 + 1`"
        plain = result["content"]["data"].get("text/plain")
        assert plain == "2", f"expected text/plain '2', got {plain!r}"
        print(f"  -> text/plain = {plain!r}  OK\n")

        # Test 2: pandas DataFrame — confirms our packaged libs work in the kernel
        # and the gateway returns rich MIME (text/html for the DataFrame).
        print("=== execute pandas DataFrame ===")
        outs = await execute(
            kid,
            "import pandas as pd; pd.DataFrame({'a':[1,2],'b':[3,4]})",
        )
        result = next((o for o in outs if o["msg_type"] == "execute_result"), None)
        assert result is not None, "no execute_result for DataFrame"
        mimes = sorted(result["content"]["data"].keys())
        assert "text/html" in mimes, f"DataFrame should yield text/html; got {mimes}"
        assert "text/plain" in mimes, f"DataFrame should yield text/plain; got {mimes}"
        print(f"  -> MIME bundle: {mimes}  OK\n")

        # Test 3: stderr surfaces as a stream message — confirms error paths.
        print("=== execute `print('hello', file=__import__('sys').stderr)` ===")
        outs = await execute(
            kid, "import sys; print('hello-stderr', file=sys.stderr)"
        )
        streams = [o for o in outs if o["msg_type"] == "stream"]
        stderr_msg = next(
            (o for o in streams if o["content"].get("name") == "stderr"), None
        )
        assert stderr_msg is not None, f"no stderr stream; got {[o['content'] for o in streams]}"
        assert "hello-stderr" in stderr_msg["content"]["text"]
        print(f"  -> stderr stream OK\n")

        # Test 4: error path — SyntaxError should yield an `error` iopub
        # message with traceback. Our Rust client will translate this to
        # status=error in the tool result.
        print("=== execute `1 / 0` (ZeroDivisionError) ===")
        outs = await execute(kid, "1 / 0")
        err = next((o for o in outs if o["msg_type"] == "error"), None)
        assert err is not None, "no error message for 1/0"
        assert err["content"]["ename"] == "ZeroDivisionError", err["content"]
        print(f"  -> error: {err['content']['ename']}: {err['content']['evalue']}  OK\n")

    finally:
        print("=== deleting kernel ===")
        http("DELETE", f"/api/kernels/{kid}")
        print("  OK")

    print("\nAll 4 protocol checks passed.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
