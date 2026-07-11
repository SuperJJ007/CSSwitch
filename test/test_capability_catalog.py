import json
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
CATALOG = ROOT / "catalog" / "capabilities.v1.json"
SECTIONS = [
    "providers",
    "tool_rules",
    "mcp_servers",
    "skills",
    "science_versions",
    "transport_rules",
]

REQUIRED_FIELDS = {
    "id",
    "scope",
    "match",
    "status",
    "action",
    "reason",
    "evidence",
    "tests",
}

ALLOWED_SCOPES = {
    "provider",
    "model",
    "tool",
    "mcp",
    "skill",
    "science_version",
    "transport",
}

ALLOWED_STATUS = {
    "supported",
    "limited",
    "unsupported",
    "unknown",
}

ALLOWED_ACTIONS = {
    "none",
    "normalize",
    "drop",
    "disable",
    "degrade",
    "diagnose",
    "document",
}

REQUIRED_RULE_IDS = {
    "provider.relay.force-model-shell",
    "provider.kimi.relay-thinking-enabled",
    "provider.dashscope.responses-tools-cap",
    "tool.kimi.web_search.server-tool-filter",
    "tool.relay.input-schema-normalize",
    "tool.deepseek.forced-tool-choice-disable-thinking",
    "tool.dashscope.responses.web_search-drop",
    "tool.siliconflow.forced-named-to-any",
    "transport.connect.anthropic-fastfail-401",
    "transport.connect.non-anthropic-direct-tunnel",
}

RUNTIME_OBSERVABILITY_RULE_IDS = {
    "provider.relay.force-model-shell",
    "provider.kimi.relay-thinking-enabled",
    "provider.dashscope.responses-tools-cap",
    "tool.kimi.web_search.server-tool-filter",
    "tool.relay.input-schema-normalize",
    "tool.deepseek.forced-tool-choice-disable-thinking",
    "tool.dashscope.responses.web_search-drop",
    "tool.siliconflow.forced-named-to-any",
}


def load_catalog():
    with CATALOG.open(encoding="utf-8") as f:
        return json.load(f)


class CapabilityCatalogSchema(unittest.TestCase):
    def test_catalog_json_loads_and_has_v1_shape(self):
        data = load_catalog()
        self.assertEqual(data["schema_version"], 1)
        self.assertEqual(set(data), {"schema_version", *SECTIONS})
        for section in SECTIONS:
            self.assertIsInstance(data[section], list, section)

    def test_entries_have_required_fields_and_valid_enums(self):
        data = load_catalog()
        for section in SECTIONS:
            for entry in data[section]:
                with self.subTest(section=section, rule_id=entry.get("id")):
                    self.assertEqual(set(entry), REQUIRED_FIELDS)
                    self.assertIsInstance(entry["id"], str)
                    self.assertTrue(entry["id"].strip())
                    self.assertIn(entry["scope"], ALLOWED_SCOPES)
                    self.assertIn(entry["status"], ALLOWED_STATUS)
                    self.assertIn(entry["action"], ALLOWED_ACTIONS)
                    self.assertIsInstance(entry["match"], dict)
                    self.assertIsInstance(entry["reason"], str)
                    self.assertTrue(entry["reason"].strip())
                    self.assertIsInstance(entry["evidence"], list)
                    self.assertTrue(entry["evidence"], "evidence must not be empty")
                    self.assertTrue(all(isinstance(x, str) and x.strip() for x in entry["evidence"]))
                    self.assertIsInstance(entry["tests"], list)
                    self.assertTrue(all(isinstance(x, str) and x.strip() for x in entry["tests"]))

    def test_rule_ids_are_unique_and_key_rules_exist(self):
        data = load_catalog()
        ids = [
            entry["id"]
            for section in SECTIONS
            for entry in data[section]
        ]
        self.assertEqual(len(ids), len(set(ids)), "catalog rule ids must be unique")
        self.assertTrue(REQUIRED_RULE_IDS.issubset(set(ids)))

    def test_proxy_observability_rule_ids_are_cataloged(self):
        data = load_catalog()
        ids = {
            entry["id"]
            for section in SECTIONS
            for entry in data[section]
        }
        self.assertTrue(RUNTIME_OBSERVABILITY_RULE_IDS.issubset(ids))

    def test_dashscope_rules_use_exact_request_shape_hosts(self):
        data = load_catalog()
        rules = {
            entry["id"]: entry
            for section in SECTIONS
            for entry in data[section]
        }
        for rule_id in (
            "provider.dashscope.responses-tools-cap",
            "tool.dashscope.responses.web_search-drop",
        ):
            with self.subTest(rule_id=rule_id):
                match = rules[rule_id]["match"]
                self.assertEqual(match["provider"], "openai-responses")
                self.assertEqual(match["endpoint_hosts"], ["dashscope.aliyuncs.com"])
                self.assertNotIn("base_url_contains", match)

    def test_migrated_rules_include_rust_evidence_and_tests(self):
        data = load_catalog()
        rules = {
            entry["id"]: entry
            for section in SECTIONS
            for entry in data[section]
        }
        migrated = {
            "provider.deepseek.anthropic-native",
            "provider.relay.force-model-shell",
            "provider.kimi.relay-thinking-enabled",
            "provider.dashscope.responses-tools-cap",
            "tool.kimi.web_search.server-tool-filter",
            "tool.relay.input-schema-normalize",
            "tool.siliconflow.forced-named-to-any",
            "tool.deepseek.forced-tool-choice-disable-thinking",
            "tool.dashscope.responses.web_search-drop",
            "tool.dsml.deepseek-tooluse-rewrite",
            "transport.connect.anthropic-fastfail-401",
            "transport.connect.non-anthropic-direct-tunnel",
        }
        for rule_id in migrated:
            with self.subTest(rule_id=rule_id):
                self.assertTrue(
                    any(item.startswith("desktop/gateway/") for item in rules[rule_id]["evidence"]),
                    f"{rule_id} lacks Rust gateway evidence",
                )
                self.assertTrue(
                    any(
                        item.startswith("desktop/gateway/")
                        or "test_gateway_rust" in item
                        for item in rules[rule_id]["tests"]
                    ),
                    f"{rule_id} lacks a Rust test reference",
                )

    def test_local_evidence_uses_stable_paths_without_line_numbers(self):
        data = load_catalog()
        for section in SECTIONS:
            for entry in data[section]:
                for evidence in entry["evidence"]:
                    with self.subTest(rule_id=entry["id"], evidence=evidence):
                        if evidence.startswith(("http://", "https://")):
                            continue
                        suffix = evidence.rpartition(":")[2]
                        self.assertFalse(
                            suffix.isdigit(),
                            "local evidence should use a stable path without a line number",
                        )

    def test_python_unittest_references_resolve(self):
        python_refs = []

        def collect(value):
            if isinstance(value, str):
                if value.startswith("test."):
                    python_refs.append(value)
            elif isinstance(value, dict):
                for item in value.values():
                    collect(item)
            elif isinstance(value, list):
                for item in value:
                    collect(item)

        def test_cases(suite):
            for item in suite:
                if isinstance(item, unittest.TestSuite):
                    yield from test_cases(item)
                else:
                    yield item

        collect(load_catalog())
        refs = sorted(set(python_refs))
        self.assertTrue(refs, "catalog must contain Python unittest references")

        loader = unittest.defaultTestLoader
        failures = []
        for ref in refs:
            errors_before = len(loader.errors)
            suite = loader.loadTestsFromName(ref)
            loader_errors = loader.errors[errors_before:]
            failed_tests = [
                str(case)
                for case in test_cases(suite)
                if isinstance(case, unittest.loader._FailedTest)
            ]
            details = []
            if suite.countTestCases() == 0:
                details.append("loaded zero test cases")
            if failed_tests:
                details.append(f"failed loader cases: {failed_tests}")
            if loader_errors:
                details.append(f"loader errors: {loader_errors}")
            if details:
                failures.append(f"{ref}: {'; '.join(details)}")

        self.assertFalse(
            failures,
            "unloadable catalog unittest references:\n" + "\n".join(failures),
        )


if __name__ == "__main__":
    unittest.main()
