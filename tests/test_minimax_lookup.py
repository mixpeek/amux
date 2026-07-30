"""MiniMax Look up inference-path checks.

Loads the real definitions out of amux-server.py via AST (the same technique
as test_peek_parity.py) so the session-aware Look up routing for a MiniMax
session is covered without booting the whole server.
"""

import ast
import os

_SERVER = os.path.join(os.path.dirname(__file__), "..", "amux-server.py")
_WANTED = {"_PROVIDER_LOOKUP_MODEL", "_MINIMAX_OPENAI_BASE", "_minimax_region"}


def _load():
    ns = {}
    tree = ast.parse(open(_SERVER, encoding="utf-8").read())
    for node in tree.body:
        name = None
        if isinstance(node, ast.FunctionDef):
            name = node.name
        elif isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
        if name in _WANTED:
            mod = ast.Module(body=[node], type_ignores=[])
            exec(compile(mod, "amux-server.py", "exec"), ns)
    missing = _WANTED - set(ns)
    assert not missing, f"definitions not found: {missing}"
    return ns


NS = _load()
_PROVIDER_LOOKUP_MODEL = NS["_PROVIDER_LOOKUP_MODEL"]
_MINIMAX_OPENAI_BASE = NS["_MINIMAX_OPENAI_BASE"]
_minimax_region = NS["_minimax_region"]


def test_minimax_lookup_model_registered():
    # The Look up shot rides the lighter model for a cheap, snappy answer.
    assert _PROVIDER_LOOKUP_MODEL["minimax"] == "MiniMax-M2.7"


def test_region_defaults_to_global():
    # No base URL configured -> global endpoint.
    assert _minimax_region("") == "global_en"
    assert _minimax_region(None) == "global_en"
    assert _MINIMAX_OPENAI_BASE["global_en"] == "https://api.minimax.io/v1"


def test_region_follows_global_base_url():
    assert _minimax_region("https://api.minimax.io/anthropic") == "global_en"
    assert _MINIMAX_OPENAI_BASE[_minimax_region("https://api.minimax.io/anthropic")] \
        == "https://api.minimax.io/v1"


def test_region_follows_china_base_url():
    assert _minimax_region("https://api.minimaxi.com/anthropic") == "cn_zh"
    assert _MINIMAX_OPENAI_BASE[_minimax_region("https://api.minimaxi.com/anthropic")] \
        == "https://api.minimaxi.com/v1"
