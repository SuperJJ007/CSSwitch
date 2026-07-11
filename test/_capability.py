"""共享能力探测：loopback bind+connect 是否被环境允许。
被 sub-runner（bash 调 CLI 形态）与 unittest（import 形态）共用。"""
import socket

from _loopback_ports import bind_loopback_listener


def loopback_available():
    """能否在 127.0.0.1 上 bind 并 connect。禁 loopback 的沙箱返 False（不抛）。"""
    import os
    if os.environ.get("CSSWITCH_FORCE_NO_LOOPBACK") == "1":
        return False
    srv = cli = None
    try:
        srv = bind_loopback_listener(backlog=1)
        port = srv.getsockname()[1]
        cli = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        cli.settimeout(1.0)
        cli.connect(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        for s in (cli, srv):
            try:
                if s:
                    s.close()
            except OSError:
                pass


if __name__ == "__main__":
    import sys
    print("1" if loopback_available() else "0")
    sys.exit(0)
