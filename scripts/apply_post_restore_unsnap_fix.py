from pathlib import Path


def patch_windows() -> None:
    path = Path("src/restore/windows.rs")
    text = path.read_text(encoding="utf-8")

    call_start = text.index("        reconcile_order_and_foreground(\n", text.index("fn restore_desktop_with_policy("))
    call_end = text.index("        );\n", call_start) + len("        );\n")
    call = text[call_start:call_end]
    if "!force_layout," not in call:
        needle = "            &mut report.failures,\n"
        if needle not in call:
            raise RuntimeError("reconcile call failure argument not found")
        call = call.replace(needle, needle + "            !force_layout,\n", 1)
        text = text[:call_start] + call + text[call_end:]

    fn_start = text.index("fn reconcile_order_and_foreground(")
    fn_end = text.index("\nfn process_entries()", fn_start)
    block = text[fn_start:fn_end]

    if "repair_layout_after_order: bool," not in block:
        needle = "    failures: &mut Vec<String>,\n"
        if needle not in block:
            raise RuntimeError("reconcile signature failure argument not found")
        block = block.replace(needle, needle + "    repair_layout_after_order: bool,\n", 1)

    guard = """    // The forced final placement pass has already proved native Snap/maximize state.\n    // Re-running placement here can destroy a valid Snap group while shell Z-order\n    // state is still settling. The top-level restore performs a later fresh,\n    // geometry-free order/foreground pass, so do not touch layout again here.\n    if !repair_layout_after_order {\n        return;\n    }\n\n"""
    if "if !repair_layout_after_order {" not in block:
        loop = "    for (saved, current) in &desired {\n"
        if loop not in block:
            raise RuntimeError("post-foreground layout verification loop not found")
        block = block.replace(loop, guard + loop, 1)

    text = text[:fn_start] + block + text[fn_end:]
    path.write_text(text, encoding="utf-8")


def patch_live_test() -> None:
    path = Path("tests/windows_snap_live.rs")
    text = path.read_text(encoding="utf-8")

    # Existing main test helper has an unescaped apostrophe character literal.
    # Fix the test-only typo so the live validation target can compile.
    text = text.replace("replace(''', \"''\")", "replace('\\'', \"''\")")

    if "07-top-half-foreground-stability" not in text:
        anchor = """    run_stock_scenario(\n        &host,\n        &display,\n        &executable,\n        &out_dir,\n        \"02-four-quarters\",\n"""
        addition = """    run_stock_scenario(\n        &host,\n        &display,\n        &executable,\n        &out_dir,\n        \"07-top-half-foreground-stability\",\n        vec![stock_spec(\n            \"Live Top Half Foreground\",\n            \"snapped:top-half\",\n            SnapSlot::TopHalf,\n            display.work,\n        )],\n        &mut log,\n    );\n\n"""
        if anchor not in text:
            raise RuntimeError("live regression insertion point not found")
        text = text.replace(anchor, addition + anchor, 1)

    path.write_text(text, encoding="utf-8")


if __name__ == "__main__":
    patch_windows()
    patch_live_test()
