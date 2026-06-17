from __future__ import annotations

import argparse
import asyncio
import socket
from contextlib import closing

import uvicorn
from fastapi import FastAPI

from .api.clip import router as clip_router
from .api.health import router as health_router
from .api.models import router as models_router
from .api.tagger import router as tagger_router


app = FastAPI(title="PhotoView+ AI Worker", version="0.0.1")
app.include_router(health_router)
app.include_router(clip_router)
app.include_router(tagger_router)
app.include_router(models_router)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the PhotoView+ AI worker")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    return parser.parse_args()


def reserve_socket(host: str, port: int) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((host, port))
    sock.listen(socket.SOMAXCONN)
    sock.set_inheritable(True)
    return sock


async def serve(host: str, port: int) -> None:
    with closing(reserve_socket(host, port)) as sock:
        actual_port = sock.getsockname()[1]
        config = uvicorn.Config(
            app,
            host=host,
            port=actual_port,
            log_level="info",
            access_log=False,
        )
        server = uvicorn.Server(config)
        app.state.server = server
        print(f"PVP_AI_WORKER_PORT={actual_port}", flush=True)
        await server.serve(sockets=[sock])


def main() -> None:
    args = parse_args()
    asyncio.run(serve(args.host, args.port))


if __name__ == "__main__":
    main()
