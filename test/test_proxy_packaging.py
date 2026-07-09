import ast
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PROXY_DIR = ROOT / "proxy"
TAURI_CONF = ROOT / "desktop" / "src-tauri" / "tauri.conf.json"


def local_proxy_module_closure(entry):
    pending = [entry]
    seen = set()
    while pending:
        name = pending.pop()
        if name in seen:
            continue
        seen.add(name)
        path = PROXY_DIR / f"{name}.py"
        tree = ast.parse(path.read_text(), filename=str(path))
        for node in ast.walk(tree):
            candidates = []
            if isinstance(node, ast.Import):
                candidates.extend(alias.name.split(".", 1)[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                candidates.append(node.module.split(".", 1)[0])
            for candidate in candidates:
                if (PROXY_DIR / f"{candidate}.py").exists() and candidate not in seen:
                    pending.append(candidate)
    return {f"{name}.py" for name in seen}


class ProxyPackagingSmoke(unittest.TestCase):
    def test_tauri_proxy_resources_import_from_minimal_bundle_dir(self):
        conf = json.loads(TAURI_CONF.read_text())
        resources = conf["bundle"]["resources"]
        proxy_resources = {
            pathlib.Path(dst).name
            for dst in resources.values()
            if str(dst).startswith("proxy/")
        }
        needed = local_proxy_module_closure("csswitch_proxy")
        self.assertTrue(needed.issubset(proxy_resources))

        old_dont_write = sys.dont_write_bytecode
        sys.dont_write_bytecode = True
        with tempfile.TemporaryDirectory() as td:
            bundle_proxy = pathlib.Path(td)
            for name in needed:
                shutil.copy2(PROXY_DIR / name, bundle_proxy / name)
            env = {
                "PYTHONDONTWRITEBYTECODE": "1",
                "PYTHONPATH": str(bundle_proxy),
            }
            try:
                result = subprocess.run(
                    [sys.executable, "-S", "-c", "import csswitch_proxy"],
                    cwd=td,
                    env=env,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            finally:
                sys.dont_write_bytecode = old_dont_write

    def test_tauri_bundle_includes_ops_scripts_for_packaged_smoke(self):
        conf = json.loads(TAURI_CONF.read_text())
        resources = set(conf["bundle"]["resources"].values())
        self.assertTrue(
            {
                "scripts/doctor.sh",
                "scripts/verify-proxy.sh",
                "scripts/launch-virtual-sandbox.sh",
                "scripts/stop-science-sandbox.sh",
            }.issubset(resources)
        )


if __name__ == "__main__":
    unittest.main()
