#!/usr/bin/env python3
"""Compare two vcontrol-mqtt configs by meaning: comments, order and formatting are ignored.

Default: the installed config against the package default dpkg left beside it. From this repo:
  ssh heizomat python3 - < scripts/config-diff.py
  ssh heizomat python3 - /etc/vcontrol/a.toml /etc/vcontrol/b.toml < scripts/config-diff.py
"""
import sys
import tomllib

ETC = "/etc/vcontrol/vcontrol-mqtt.toml"
yours_path, default_path = (sys.argv[1:3] if len(sys.argv) > 2 else (ETC, ETC + ".dpkg-dist"))


def load(path):
  with open(path, "rb") as f:
    return tomllib.load(f)


def fmt(value):
  return "(unset)" if value is None else repr(value)


def compare_dicts(a, b, indent="  "):
  lines = []
  for key in sorted(a.keys() | b.keys()):
    if a.get(key) != b.get(key):
      lines.append(f"{indent}{key}: {fmt(a.get(key))} -> {fmt(b.get(key))}")
  return lines


def keyed(entries):
  # [[command]] entries are identified by name, [[ferienprogramm]] by id.
  return {e.get("name", e.get("id")): e for e in entries}


yours, default = load(yours_path), load(default_path)
print(f"yours:   {yours_path}\ndefault: {default_path}\n(only settings that differ; '->' reads yours -> default)\n")

scalars = {k: v for k, v in yours.items() if not isinstance(v, (dict, list)) or k == "writable"}
default_scalars = {k: v for k, v in default.items() if not isinstance(v, (dict, list)) or k == "writable"}
if lines := compare_dicts(scalars, default_scalars):
  print("top level\n" + "\n".join(lines) + "\n")

for table in sorted(k for k in yours.keys() | default.keys() if isinstance(yours.get(k, default.get(k)), dict)):
  if lines := compare_dicts(yours.get(table, {}), default.get(table, {})):
    print(f"[{table}]\n" + "\n".join(lines) + "\n")

for array in sorted(k for k in yours.keys() | default.keys()
                    if isinstance(yours.get(k, default.get(k)), list) and k != "writable"):
  a, b = keyed(yours.get(array, [])), keyed(default.get(array, []))
  if only := [n for n in a if n not in b]:
    print(f"[[{array}]] only in yours:\n" + "\n".join(f"  {n}" for n in only) + "\n")
  if only := [n for n in b if n not in a]:
    print(f"[[{array}]] only in default:\n"
          + "\n".join(f"  {n}  {fmt(b[n].get('display_name', ''))}" for n in only) + "\n")
  changed = [(n, compare_dicts(a[n], b[n], "    ")) for n in a if n in b and a[n] != b[n]]
  if changed:
    print(f"[[{array}]] in both, settings differ:\n" + "\n".join(f"  {n}\n" + "\n".join(l) for n, l in changed) + "\n")
