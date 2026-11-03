"""arena pods test: which auth Claude Code would use on this machine, as root's login shell has
it - by kind and fingerprint, never the value - and, where that is free, whether its service
accepts it. Prints only `claude.<fact>=<value>` lines (the deep check's contract). argv[1]: `1`
to also make the one call that uses the account's limits or money (`pods test --claude-call`).

Claude Code's order, first match wins: a cloud provider (Bedrock, Vertex, Foundry), then
ANTHROPIC_AUTH_TOKEN (a Bearer token, to ANTHROPIC_BASE_URL), ANTHROPIC_API_KEY, an
apiKeyHelper, CLAUDE_CODE_OAUTH_TOKEN (`arena claude`'s export, or a clods profile's
settings.json), and last a /login (`.credentials.json`). A settings file's `env` wins over the
shell's: that is how a clods profile's token beats an exported one."""

import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

KINDS = [
    ("bearer", "ANTHROPIC_AUTH_TOKEN"),
    ("api_key", "ANTHROPIC_API_KEY"),
    ("oauth", "CLAUDE_CODE_OAUTH_TOKEN"),
]
CLOUD = ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"]
VARS = [v for _, v in KINDS] + ["ANTHROPIC_BASE_URL"] + CLOUD
TIMEOUT = 10
# The cheapest current model, one token: the paid call's whole cost.
CALL_MODEL = "claude-haiku-5-5"

secrets = []


def one(v, n=300):
    return " ".join(str(v).split())[:n]


def scrub(text):
    for s in secrets:
        if len(s) >= 8:
            text = text.replace(s, "[secret]")
    text = re.sub(r"[Bb]earer\s+[^\s\"]+", "Bearer [secret]", text)
    return re.sub(r"(^|[^A-Za-z0-9])(sk|hf)[-_][A-Za-z0-9._~-]+", r"\1[secret]", text)


def kv(k, v):
    # Scrubbed whole, before anything is joined or clipped: no part of a secret survives a cut.
    print(f"claude.{k}={one(scrub(str(v)))}", flush=True)


def fingerprint(s):
    return hashlib.sha256(s.encode()).hexdigest()[:12]


def truthy(v):
    return str(v).strip().lower() not in ("", "0", "false", "no", "off")


def settings(path):
    """A settings file's `env` (strings only) and whether it names an apiKeyHelper."""
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, ValueError):
        return {}, False
    if not isinstance(data, dict):
        return {}, False
    env = data.get("env") if isinstance(data.get("env"), dict) else {}
    env = {k: v for k, v in env.items() if isinstance(v, str) and v.strip()}
    return env, bool(data.get("apiKeyHelper"))


def call(url, headers, body=None):
    """(HTTP status, the answer's message): 0 and why when nothing answered."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers, method="POST" if data else "GET")
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return r.status, ""
    except urllib.error.HTTPError as e:
        try:
            ans = json.loads(e.read(16384) or b"{}")
        except ValueError:
            ans = {}
        err = ans.get("error") if isinstance(ans, dict) else None
        msg = (err.get("message") if isinstance(err, dict) else err) or (ans.get("message") if isinstance(ans, dict) else "")
        return e.code, str(msg or e.reason)[:16384]
    except Exception as e:  # unreachable, a timeout, TLS
        return 0, str(getattr(e, "reason", e))[:16384]


def main():
    paid = len(sys.argv) > 1 and sys.argv[1] == "1"
    shell = {v: os.environ[v] for v in VARS if os.environ.get(v, "").strip()}
    cdir = os.environ.get("CLAUDE_CONFIG_DIR", "").strip()
    home = cdir or os.path.expanduser("~/.claude")
    if cdir:
        kv("config_dir", cdir)
    # A clods profile: ~/.clods/profiles/claudeN with its `.account`.
    m = re.search(r"/\.clods/profiles/claude(\d+)/?$", home)
    profile = m.group(1) if m and os.path.isfile(os.path.join(home, ".account")) else None
    eff, src = dict(shell), {v: "shell" for v in shell}
    helper = False
    for path in [os.path.join(home, "settings.json"), "/etc/claude-code/managed-settings.json"]:
        env, h = settings(path)
        helper = helper or h
        for v in VARS:
            if v in env:
                eff[v], src[v] = env[v], ("clods" if profile and path.startswith(home) else "settings")
    for v in [v for _, v in KINDS]:
        if v in eff:
            secrets.append(eff[v])
    present = [
        kind
        for kind, there in [
            ("cloud", any(truthy(eff.get(v, "")) for v in CLOUD)),
            ("bearer", "ANTHROPIC_AUTH_TOKEN" in eff),
            ("api_key", "ANTHROPIC_API_KEY" in eff),
            ("helper", helper),
            ("oauth", "CLAUDE_CODE_OAUTH_TOKEN" in eff),
            ("login", os.path.isfile(os.path.join(home, ".credentials.json"))),
        ]
        if there
    ]
    kind = present[0] if present else "none"
    kv("auth", kind)
    if present[1:]:
        kv("also", ",".join(present[1:]))
    if profile:
        kv("profile", profile)
    var = dict(KINDS).get(kind)
    base = eff.get("ANTHROPIC_BASE_URL", "").strip()
    if base:
        kv("base_host", urllib.parse.urlsplit(base).hostname or "?")
    if not var:
        if kind == "login":
            kv("call", "none")
        return
    secret = eff[var]
    kv("source", src.get(var, "shell"))
    kv("fingerprint", fingerprint(secret))
    if secret == os.environ.get("OPENROUTER_API_KEY", "\0"):
        kv("same_as", "openrouter")
    api = (base or os.environ.get("ARENA_ANTHROPIC_URL") or "https://api.anthropic.com").rstrip("/")
    host = (urllib.parse.urlsplit(base).hostname or "") if base else ""
    version = {"anthropic-version": "2023-06-01"}
    openrouter = kind == "bearer" and (host == "openrouter.ai" or host.endswith(".openrouter.ai"))
    if openrouter:
        # Free and exact: OpenRouter's key endpoint, as the AI key's check asks it (its limits
        # and money are that check's), with or without `--claude-call`.
        url = os.environ.get("ARENA_OPENROUTER_URL") or "https://openrouter.ai/api/v1/key"
        what, (code, msg) = "openrouter", call(url, {"Authorization": f"Bearer {secret}"})
    elif kind == "api_key" and not paid:
        # Free: the models list spends nothing.
        what, (code, msg) = "models", call(f"{api}/v1/models?limit=1", {"x-api-key": secret, **version})
    elif paid:
        headers = {"content-type": "application/json", **version}
        if kind == "api_key":
            headers["x-api-key"] = secret
        else:
            headers["Authorization"] = f"Bearer {secret}"
        if kind == "oauth":
            headers["anthropic-beta"] = "oauth-2025-04-20"
        body = {
            "model": CALL_MODEL,
            "max_tokens": 1,
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": "hi"}],
        }
        what, (code, msg) = "messages", call(f"{api}/v1/messages", headers, body)
    else:
        kv("call", "none")
        return
    kv("call", what)
    kv("http", code)
    if msg:
        # Scrubbed first (`kv`), then clipped for a NOTES cell.
        kv("error", one(scrub(msg), 160))


if __name__ == "__main__":
    try:
        main()
    except BaseException as e:  # say so; never a trace with a secret in it
        kv("auth_error", f"{type(e).__name__}")
