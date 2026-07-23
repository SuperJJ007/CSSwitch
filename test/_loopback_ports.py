"""Test-only loopback port helpers.

In-process servers bind port 0 themselves.  Proxy subprocesses only accept a
numeric ``--port``, so those use a reservation released immediately before
``Popen``; the tiny release/exec TOCTOU window is unavoidable without changing
the production CLI.
"""
import socket
import subprocess


FORBIDDEN_PORTS = {8765}
MAX_BIND_ATTEMPTS = 32


def bind_loopback_listener(backlog=5):
    """Return a listening socket bound by the OS to an allowed dynamic port."""
    for _ in range(MAX_BIND_ATTEMPTS):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", 0))
            if sock.getsockname()[1] in FORBIDDEN_PORTS:
                sock.close()
                continue
            sock.listen(backlog)
            return sock
        except Exception:
            sock.close()
            raise
    raise RuntimeError("OS repeatedly selected a forbidden loopback port")


def bind_http_server(server_type, handler_type):
    """Create an HTTP server that owns its dynamic port before returning."""
    for _ in range(MAX_BIND_ATTEMPTS):
        server = server_type(("127.0.0.1", 0), handler_type)
        if server.server_address[1] not in FORBIDDEN_PORTS:
            return server
        server.server_close()
    raise RuntimeError("OS repeatedly selected a forbidden loopback port")


class LoopbackPortReservation:
    def __init__(self, port=None):
        if port is None:
            self._socket = bind_loopback_listener(backlog=1)
        else:
            if not isinstance(port, int) or not (1 <= port <= 65535) or port in FORBIDDEN_PORTS:
                raise ValueError("invalid exact loopback reservation port")
            self._socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self._socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                self._socket.bind(("127.0.0.1", port))
                self._socket.listen(1)
            except Exception:
                self._socket.close()
                self._socket = None
                raise
        self.port = self._socket.getsockname()[1]

    def release(self):
        if self._socket is not None:
            self._socket.close()
            self._socket = None


def popen_on_reserved_port(argv_for_port, **popen_kwargs):
    """Launch a numeric-port-only child with the shortest practical TOCTOU."""
    reservation = LoopbackPortReservation()
    port = reservation.port
    try:
        argv = argv_for_port(port)
        reservation.release()
        proc = subprocess.Popen(argv, **popen_kwargs)
    except Exception:
        reservation.release()
        raise
    return port, proc


def terminate_process(proc, timeout=5):
    """Stop exactly one owned child, escalating only if it misses the timeout."""
    if proc is None:
        return
    if proc.poll() is None:
        proc.terminate()
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=timeout)
