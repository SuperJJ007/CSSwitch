import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PRODUCTION_RUST = ROOT / "desktop" / "src-tauri" / "src"


class ProductionProcessOwnershipPolicy(unittest.TestCase):
    def test_tauri_runtime_has_no_global_name_or_argv_kill(self):
        forbidden = ("pkill", "killall")
        matches = []
        for path in sorted(PRODUCTION_RUST.rglob("*.rs")):
            text = path.read_text(encoding="utf-8")
            for needle in forbidden:
                if needle.casefold() in text.casefold():
                    matches.append(f"{path.relative_to(ROOT)}: {needle}")
        self.assertEqual(
            matches,
            [],
            "Tauri source must not invoke or shell out to global name/argv killers; "
            "the launch-identity behavior test separately proves an old listener survives",
        )


if __name__ == "__main__":
    unittest.main()
