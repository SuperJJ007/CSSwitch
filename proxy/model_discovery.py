"""Model discovery helpers for csswitch_proxy.

This module keeps /v1/models normalization independent from the HTTP handler
and provider globals. It intentionally does not know about keys, secrets, or
process state.
"""


CREATED_AT = "2026-01-01T00:00:00Z"


def normalize_models_response(raw):
    data = raw.get("data") if isinstance(raw, dict) else raw
    out, ids = [], []
    for m in data or []:
        mid = m.get("id") if isinstance(m, dict) else None
        if not mid:
            continue
        ids.append(mid)
        # 能力位：从上游 supported_parameters 推断，绝不臆测（无该字段 → None）。
        sp = m.get("supported_parameters") if isinstance(m, dict) else None
        supports_tools = ("tools" in sp) if isinstance(sp, list) else None
        out.append({
            "type": "model",
            "id": mid,
            "display_name": (m.get("display_name") if isinstance(m, dict) else None) or mid,
            "supports_tools": supports_tools,
            "created_at": CREATED_AT,
        })
    return out, ids


def force_shell_response(model):
    shell = [{
        "type": "model",
        "id": "claude-opus-4-8",
        "display_name": model,
        "supports_tools": None,
        "created_at": CREATED_AT,
    }]
    return 200, {
        "data": shell,
        "has_more": False,
        "first_id": "claude-opus-4-8",
        "last_id": "claude-opus-4-8",
    }


def static_models_response(models):
    data = [{
        "type": "model",
        "id": mid,
        "display_name": disp,
        "supports_tools": None,
        "created_at": CREATED_AT,
    } for mid, disp in models]
    return 200, {
        "data": data,
        "has_more": False,
        "first_id": data[0]["id"] if data else None,
        "last_id": data[-1]["id"] if data else None,
    }


def live_models_response(data):
    return 200, {
        "data": data,
        "has_more": False,
        "first_id": data[0]["id"] if data else None,
        "last_id": data[-1]["id"] if data else None,
    }
