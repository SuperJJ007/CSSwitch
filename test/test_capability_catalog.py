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
    "tool.kimi.web_search.server-tool-filter",
    "tool.relay.input-schema-normalize",
    "tool.dashscope.responses.web_search-drop",
    "mcp.hosted-anthropic.hcls-boundary",
    "science.version.0_1_15_dev.route-diff",
    "transport.connect.anthropic-fastfail-401",
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


if __name__ == "__main__":
    unittest.main()
