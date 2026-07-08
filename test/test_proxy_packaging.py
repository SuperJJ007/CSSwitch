import json
import pathlib
import shutil
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PROXY_DIR = ROOT / "proxy"
TAURI_CONF = ROOT / "desktop" / "src-tauri" / "tauri.conf.json"


class ProxyPackagingSmoke(unittest.TestCase):
    def test_tauri_proxy_resources_import_from_minimal_bundle_dir(self):
        conf = json.loads(TAURI_CONF.read_text())
        resources = conf["bundle"]["resources"]
        proxy_resources = {
            pathlib.Path(dst).name
            for dst in resources.values()
            if str(dst).startswith("proxy/")
        }
        needed = {
            "csswitch_proxy.py",
            "dsml_shim.py",
            "model_discovery.py",
            "openai_chat_compat.py",
            "provider_policy.py",
            "responses_compat.py",
            "anthropic_compat.py",
        }
        self.assertTrue(needed.issubset(proxy_resources))

        old_dont_write = sys.dont_write_bytecode
        sys.dont_write_bytecode = True
        with tempfile.TemporaryDirectory() as td:
            bundle_proxy = pathlib.Path(td)
            for name in needed:
                shutil.copy2(PROXY_DIR / name, bundle_proxy / name)
            sys.path.insert(0, str(bundle_proxy))
            try:
                for mod in [
                    "csswitch_proxy",
                    "model_discovery",
                    "openai_chat_compat",
                    "responses_compat",
                ]:
                    sys.modules.pop(mod, None)
                __import__("csswitch_proxy")
            finally:
                sys.path.remove(str(bundle_proxy))
                for mod in [
                    "csswitch_proxy",
                    "model_discovery",
                    "openai_chat_compat",
                    "responses_compat",
                ]:
                    sys.modules.pop(mod, None)
                sys.dont_write_bytecode = old_dont_write


if __name__ == "__main__":
    unittest.main()
