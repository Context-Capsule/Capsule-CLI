from pathlib import Path

p = Path("tests/windows_snap_live.rs")
s = p.read_text()
new_name = "live_restore_portrait_top_bottom_prefers_native_or_falls_back_exactly"
retained_name = "live_restore_portrait_top_bottom_as_one_native_pair"
if new_name in s:
    s = s.replace(new_name, retained_name, 1)
elif retained_name not in s:
    raise SystemExit("portrait live regression entrypoint was not found")
p.write_text(s)
