import os, socket, sys, unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import _loopback_ports
from _capability import loopback_available

class TestCapability(unittest.TestCase):
    def test_returns_bool(self):
        self.assertIn(loopback_available(), (True, False))

    def test_matches_actual_bind(self):
        import os
        if os.environ.get("CSSWITCH_FORCE_NO_LOOPBACK") == "1":
            # FORCE 模拟下探针本就应该强制返回 False，与真实 bind 是否可行无关。
            self.assertFalse(loopback_available())
            return
        try:
            s = _loopback_ports.bind_loopback_listener(); s.close()
            can = True
        except OSError:
            can = False
        self.assertEqual(loopback_available(), can)

    def test_force_no_loopback_env(self):
        import os
        name = "CSSWITCH_FORCE_NO_LOOPBACK"
        prev = os.environ.get(name)
        os.environ[name] = "1"
        try:
            self.assertFalse(loopback_available())
        finally:
            if prev is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = prev

    @unittest.skipUnless(loopback_available(), "env-blocked: loopback bind/connect not permitted")
    def test_reserved_port_is_held_through_argv_callback_then_released_for_popen(self):
        events = []
        selected = {}
        fake_proc = object()

        def argv_for_port(port):
            events.append("argv")
            selected["port"] = port
            self.assertNotEqual(port, 8765)
            probe = socket.socket()
            try:
                with self.assertRaises(OSError):
                    probe.bind(("127.0.0.1", port))
            finally:
                probe.close()
            return ["fake-proxy", "--port", str(port)]

        def fake_popen(argv, **_kwargs):
            events.append("popen")
            self.assertEqual(argv, ["fake-proxy", "--port", str(selected["port"])])
            probe = socket.socket()
            try:
                probe.bind(("127.0.0.1", selected["port"]))
            finally:
                probe.close()
            return fake_proc

        with mock.patch.object(_loopback_ports.subprocess, "Popen", side_effect=fake_popen):
            port, proc = _loopback_ports.popen_on_reserved_port(argv_for_port)

        self.assertEqual(events, ["argv", "popen"])
        self.assertEqual(port, selected["port"])
        self.assertIs(proc, fake_proc)

    @unittest.skipUnless(loopback_available(), "env-blocked: loopback bind/connect not permitted")
    def test_reserved_port_is_released_when_argv_callback_raises(self):
        selected = {}

        def fail_argv(port):
            selected["port"] = port
            raise RuntimeError("synthetic argv failure")

        with self.assertRaisesRegex(RuntimeError, "synthetic argv failure"):
            _loopback_ports.popen_on_reserved_port(fail_argv)

        probe = socket.socket()
        try:
            probe.bind(("127.0.0.1", selected["port"]))
        finally:
            probe.close()

if __name__ == "__main__":
    unittest.main()
