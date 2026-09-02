from pathlib import Path


def patch_windows() -> None:
    path = Path("src/restore/windows.rs")
    text = path.read_text(encoding="utf-8")

    # Remove the first experimental guard, if present.
    text = text.replace("            !force_layout,\n", "", 1)
    text = text.replace("    repair_layout_after_order: bool,\n", "", 1)
    guard = """    // The forced final placement pass has already proved native Snap/maximize state.\n    // Re-running placement here can destroy a valid Snap group while shell Z-order\n    // state is still settling. The top-level restore performs a later fresh,\n    // geometry-free order/foreground pass, so do not touch layout again here.\n    if !repair_layout_after_order {\n        return;\n    }\n\n"""
    text = text.replace(guard, "", 1)

    # The forced final physical pass must only converge geometry/native window state.
    # A separate fresh, geometry-free order/foreground pass already runs afterward.
    old = "    if !dry_run && !matched_for_order.is_empty() {\n        reconcile_order_and_foreground("
    new = "    if !dry_run && !force_layout && !matched_for_order.is_empty() {\n        reconcile_order_and_foreground("
    if old in text:
        text = text.replace(old, new, 1)
    elif new not in text:
        raise RuntimeError("final reconciliation condition not found")

    path.write_text(text, encoding="utf-8")


def patch_live_test() -> None:
    path = Path("tests/windows_snap_live.rs")
    text = path.read_text(encoding="utf-8")

    # Existing main test helper has an unescaped apostrophe character literal.
    text = text.replace("replace(''', \"''\")", "replace('\\'', \"''\")")

    # Remove the earlier scenario from the broad live suite. It now has its own
    # focused test so unrelated Snap-layout failures cannot mask this regression.
    old_scenario = """    run_stock_scenario(\n        &host,\n        &display,\n        &executable,\n        &out_dir,\n        \"07-top-half-foreground-stability\",\n        vec![stock_spec(\n            \"Live Top Half Foreground\",\n            \"snapped:top-half\",\n            SnapSlot::TopHalf,\n            display.work,\n        )],\n        &mut log,\n    );\n\n"""
    text = text.replace(old_scenario, "", 1)

    if "fn live_forced_final_pass_does_not_unsnap_after_foreground_reconciliation()" not in text:
        anchor = "#[test]\n#[ignore = \"interactive Windows shell validation; run only on a desktop self-hosted runner\"]\nfn live_restore_rejects_near_floating_windows_and_restores_real_snap() {"
        focused = r'''#[test]
#[ignore = "interactive Windows shell validation; run only on a desktop self-hosted runner"]
fn live_forced_final_pass_does_not_unsnap_after_foreground_reconciliation() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let out_dir = std::env::var_os("SNAP_LIVE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("context-capsule-post-restore-unsnap"));
    fs::create_dir_all(&out_dir).expect("create focused live output directory");

    let host = WindowHost::start(2);
    let display = display_for(host.windows[0].hwnd()).expect("monitor info");
    let executable = std::env::current_exe().expect("current test executable");
    let normal = SavedRect {
        left: display.work.left + 80,
        top: display.work.top + 80,
        right: (display.work.left + 880).min(display.work.right - 80),
        bottom: (display.work.top + 680).min(display.work.bottom - 80),
    };
    let top_half = snap_rect(display.work, SnapSlot::TopHalf);
    let specs = vec![
        SlotSpec {
            title: "Foreground Floating Window".to_owned(),
            state: "normal".to_owned(),
            target: normal,
            normalized: [
                (normal.left - display.work.left) as f64 / display.work.width() as f64,
                (normal.top - display.work.top) as f64 / display.work.height() as f64,
                normal.width() as f64 / display.work.width() as f64,
                normal.height() as f64 / display.work.height() as f64,
            ],
        },
        stock_spec(
            "Background Top Half",
            "snapped:top-half",
            SnapSlot::TopHalf,
            display.work,
        ),
    ];
    let titles = specs.iter().map(|spec| spec.title.clone()).collect::<Vec<_>>();
    host.prepare(&titles);
    stage_frame(host.windows[0].hwnd(), near_target(normal, display.work))
        .expect("stage foreground floating window");
    stage_frame(host.windows[1].hwnd(), near_target(top_half, display.work))
        .expect("stage top-half window near target");
    assert_eq!(unsafe { IsWindowArranged(host.windows[1].hwnd()) }, 0);

    screenshot(&out_dir, "post-restore-unsnap-before.png");
    let snapshot = snapshot_for(&display, &executable, &specs);
    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    fs::write(
        out_dir.join("post-restore-unsnap-report.txt"),
        format!("{report:#?}"),
    )
    .expect("write focused restore report");
    assert!(report.success(), "focused restore failed: {report:#?}");

    // The reported bug happened after the window initially looked correct.
    thread::sleep(Duration::from_millis(1200));
    let observed = frame_bounds(host.windows[1].hwnd()).expect("final top-half DWM bounds");
    assert_ne!(
        unsafe { IsWindowArranged(host.windows[1].hwnd()) },
        0,
        "top-half window was unsnapped after final foreground/Z-order handling"
    );
    assert!(
        rect_close_px(observed.into(), top_half, 3),
        "top-half geometry drifted after final foreground/Z-order handling: observed={observed:?}, target={top_half:?}"
    );
    screenshot(&out_dir, "post-restore-unsnap-after-1200ms.png");
}

'''
        if anchor not in text:
            raise RuntimeError("focused live-test insertion point not found")
        text = text.replace(anchor, focused + anchor, 1)

    path.write_text(text, encoding="utf-8")


if __name__ == "__main__":
    patch_windows()
    patch_live_test()
