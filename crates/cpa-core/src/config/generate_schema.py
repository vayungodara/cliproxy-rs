#!/usr/bin/env python3
"""Generate YAML field shapes from the pinned Go reference, not example defaults.

Run: python3 generate_schema.py /path/to/CLIProxyAPI
The checked-in artifact makes normal Rust builds independent of Go and Python.
"""
import copy
import json
from pathlib import Path
import re
import subprocess
import sys

reference = Path(sys.argv[1])
revision = subprocess.check_output(["git", "-C", str(reference), "rev-parse", "HEAD"], text=True).strip()
assert revision.startswith("6fecc6e"), "Use the pinned parity reference"
structs = {}
aliases = {}
for directory, prefix in [("internal/config", ""), ("internal/registry", "registry."), ("internal/pluginstore", "sdkpluginstore.")]:
    for file in sorted((reference / directory).glob("*.go")):
        if file.name.endswith("_test.go"):
            continue
        text = file.read_text()
        for name, body in re.findall(r"type (\w+) struct \{(.*?)^\}", text, re.S | re.M):
            fields = {}
            for line in body.splitlines():
                match = re.match(r'\s*\w+\s+([^`]+?)\s+`[^`]*yaml:"([^",]+)', line)
                if match and match[2] != "-":
                    fields[match[2]] = match[1].strip()
            structs[prefix + name] = fields
        for name, target in re.findall(r"^type (\w+) = (\w+)$", text, re.M):
            aliases[prefix + name] = prefix + target

def shape(type_name):
    if type_name in aliases:
        return shape(aliases[type_name])
    if type_name.startswith("*"):
        return {"optional": shape(type_name[1:])}
    if type_name.startswith("[]"):
        return {"list": shape(type_name[2:])}
    if type_name.startswith("map[string]"):
        return {"map": shape(type_name[11:])}
    if type_name in structs:
        return {"fields": {key: shape(value) for key, value in structs[type_name].items()}}
    primitives = {"bool": "bool", "int": "int", "int64": "int", "uint16": "port", "string": "string",
                  "time.Duration": "duration", "any": "opaque", "yaml.Node": "opaque",
                  "DisableImageGenerationMode": "image-mode"}
    assert type_name in primitives, f"Unresolved Go type: {type_name}"
    return primitives[type_name]

schema = shape("Config")
schema["fields"].update(shape("SDKConfig")["fields"])

def take(path):
    node = schema
    parts = path.split(".")
    for part in parts[:-1]:
        node = node["fields"][part]
    return node["fields"].pop(parts[-1])

def put(path, value):
    node = schema
    parts = path.split(".")
    for part in parts[:-1]:
        node = node["fields"].setdefault(part, {"fields": {}})
    node["fields"][parts[-1]] = value

source = (reference / "internal/config/config_v8.go").read_text()
prefixes = source.split("prefixes := []configPath{", 1)[1].split("var out", 1)[0]
for old, current in re.findall(r'\{"([^"]+)", "([^"]+)"\}', prefixes):
    put(current, take(old))
# Client aliases are removed, not part of the canonical writable schema.
schema["fields"]["oauth"]["fields"]["providers"]["fields"]["codex"]["fields"].pop("optimize-multi-agent-v2", None)
# v8.0.35 52d5507d (buildV8Paths): shared Codex, Claude and xAI settings live under
# `upstream` and apply to API-key credentials too. Claude Code's field joins Claude's.
for name in ["disable-codex-cloaking", "stream-bootstrap-buffering", "stream-bootstrap-timeout",
             "orphan-delegation-compatibility", "model-level-cooling", "response-steering"]:
    put("upstream.codex." + name, take("oauth.providers.codex." + name))
claude = take("oauth.providers.claude")
claude["fields"].update(claude["fields"].pop("claude-code")["fields"])
put("upstream.claude", claude)
put("upstream.xai", take("oauth.providers.xai"))
families = source.split("var v8KeyFamilies = []configPath{", 1)[1].split("func buildV8Paths", 1)[0]
shared = ["priority", "prefix", "proxy-url", "headers", "models", "excluded-models", "disable-cooling", "request-retry", "request-scoped-errors"]
for old, family in re.findall(r'\{"([^"]+)", "([^"]+)"\}', families):
    key = take(old)["list"]
    if family == "openai-compatibility":
        group = key
        group["fields"]["keys"] = group["fields"].pop("api-key-entries")
    else:
        fields = key["fields"]
        group = {"fields": {"name": "string", "base-url": fields.pop("base-url", "string")}}
        for field in shared:
            if field in fields:
                group["fields"][field] = copy.deepcopy(fields[field])
        group["fields"]["keys"] = {"list": key}
    put("api-keys." + family, {"list": group})
schema["fields"]["config-version"] = "version"
# v8.0.35 345cc9d5 and b3dc6c3f (SDKConfig.TLSHandshakeTimeoutSeconds), with its legacy
# root spelling in buildV8Paths.
put("requests.tls-handshake-timeout-seconds", "int")
# v8.0.35 dd7b4334 (SDKConfig.MaxRequestBodyBytes), legacy root spelling in buildV8Paths.
put("server.max-request-body-bytes", "int")
# v8.0.35 7c41cd05 (Config.GitHubToken), legacy root spelling in buildV8Paths.
put("server.github-token", "string")
# v8.0.35 CatalogSources (internal/registry/catalog_config.go), Config.Models: the
# root `models` in both layouts.
for name in ["catalog", "codex-catalog", "devin-catalog"]:
    put("models." + name, "string")
# cliproxy-rs additions (docs/DIFFERENCES-FROM-GO.md). Go starts with them and ignores
# them; a config edit through its v8 Management API moves them into comments.
put("oauth.providers.codex.chatgpt-keep-alive", "bool")
put("observability.usage.claude-cache-telemetry", "bool")
put("routing.cooldown.max-trusted-cooldown", "string")
schema["fields"]["worker-threads"] = "int"
schema["fields"].pop("home", None)
Path(__file__).with_name("schema.json").write_text(json.dumps(schema, indent=2) + "\n")
print(f"Generated schema from {revision}")
