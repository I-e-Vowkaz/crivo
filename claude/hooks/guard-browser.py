#!/usr/bin/env python3
"""PreToolUse (Claude in Chrome): só navega em hosts permitidos.

Hosts permitidos vêm de PRO_ALLOWED_HOSTS (separados por vírgula; aceita
host ou host:porta). Padrão: localhost e 127.0.0.1.
"""
import json, os, re, sys
from urllib.parse import urlsplit

PREFIX = "mcp__claude-in-chrome__"
DENY_TOOLS = {"file_upload", "upload_image"}
JS_NAV = re.compile(r"location|window\.open|\bopen\s*\(|href\s*=|fetch\s*\(|XMLHttpRequest|sendBeacon|WebSocket|import\s*\(", re.I)


def allowed_hosts():
    raw = os.environ.get("PRO_ALLOWED_HOSTS", "localhost,127.0.0.1")
    return {h.strip().lower() for h in raw.split(",") if h.strip()}


def url_ok(url):
    if url in ("back", "forward"):
        return True
    u = urlsplit(url if "://" in url else "http://" + url)
    if u.scheme not in ("http", "https"):
        return False
    hosts = allowed_hosts()
    host = (u.hostname or "").lower()
    return host in hosts or (u.port and f"{host}:{u.port}" in hosts)


ADDRESS_BAR_KEYS = re.compile(r"(ctrl|cmd|meta)\+(l|t|n)\b|alt\+d\b|\bf6\b", re.I)


def walk(obj, path=""):
    """Percorre a entrada inteira: browser_batch aninha ações em `actions`."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            yield from walk(v, k)
    elif isinstance(obj, list):
        for v in obj:
            yield from walk(v, path)
    else:
        yield path, obj


def check_action(name, inp):
    if name in DENY_TOOLS:
        return f"{name} não é permitido nos testes"
    for key, val in walk(inp):
        if key in ("url", "href") and isinstance(val, str) and not url_ok(val):
            return f"URL fora da allowlist: {val}"
    if name == "javascript_tool" and JS_NAV.search(json.dumps(inp)):
        return "javascript que navega ou faz requisições não é permitido"
    if name == "computer":
        for key, val in walk(inp):
            if key in ("text", "key", "keys") and isinstance(val, str) and ADDRESS_BAR_KEYS.search(val):
                return "atalho para barra de endereço/nova aba não é permitido"
    return None


def reason_for(tool, inp):
    if not tool.startswith(PREFIX):
        return None
    name = tool[len(PREFIX):]
    r = check_action(name, inp)
    if r:
        return r
    # Ações dentro de browser_batch: cada uma é checada como se fosse chamada direta.
    for act in inp.get("actions", []) if isinstance(inp.get("actions"), list) else []:
        if isinstance(act, dict):
            r = check_action(str(act.get("name", "")).replace(PREFIX, ""), act.get("input", act))
            if r:
                return r
    return None


def main():
    ev = json.load(sys.stdin)
    r = reason_for(ev.get("tool_name", ""), ev.get("tool_input", {}) or {})
    if r:
        print(f"Bloqueado pelo PR Review Orchestrator: {r}.", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
