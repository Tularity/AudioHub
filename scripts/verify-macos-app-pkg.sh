#!/bin/zsh
# Metadata-only package inspection shared by the PKG and DMG builders.
set -euo pipefail
ROOT="$(cd "${0:a:h}/.." && pwd)"
EXPANDED="${1:?expanded package required}"
EXPECTED="$ROOT/app/installer/macos/pkg-scripts/preinstall"
/usr/bin/python3 - "$EXPANDED" "$EXPECTED" <<'PY'
import pathlib
import sys
import xml.etree.ElementTree as ET

root, expected = map(pathlib.Path, sys.argv[1:])
component = root / "AudioHubApp.pkg"
scripts = component / "Scripts"
if sorted(root.rglob("Scripts")) != [scripts]:
    raise SystemExit("The App package must contain exactly one preinstall script directory")
entries = list(scripts.iterdir())
preinstall = scripts / "preinstall"
if entries != [preinstall] or preinstall.is_symlink() or not preinstall.is_file():
    raise SystemExit("Unexpected App package lifecycle scripts")
if preinstall.read_bytes() != expected.read_bytes():
    raise SystemExit("The packaged preinstall differs from the reviewed App shutdown script")
infos = list(root.rglob("PackageInfo"))
if infos != [component / "PackageInfo"]:
    raise SystemExit("Unexpected package components")
info = ET.parse(infos[0]).getroot()
if info.get("relocatable") != "false" or any(len(node) or node.attrib for node in info.findall(".//relocate")):
    raise SystemExit("The App package must not relocate")
hooks = info.findall("scripts")
if len(hooks) != 1 or len(hooks[0]) != 1:
    raise SystemExit("Unexpected App package lifecycle metadata")
hook = hooks[0][0]
if hook.tag != "preinstall" or hook.get("file") != "./preinstall":
    raise SystemExit("Only the App shutdown preinstall is allowed")
PY
