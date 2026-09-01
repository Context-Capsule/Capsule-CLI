from pathlib import Path

path = Path("tests/windows_snap_live.rs")
text = path.read_text(encoding="utf-8")
old = "replace(''', \"''\")"
new = "replace('\\'', \"''\")"
if old not in text:
    raise SystemExit("live test quoting fragment was not found")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("Patched live screenshot quoting")
