from pathlib import Path

# 1) Add a one-shot canonical two-window pair restore primitive.
p = Path("src/windows_snap.rs")
s = p.read_text()
marker = "pub(crate) fn restore_resized_pair(\n"
if "pub(crate) fn restore_equal_pair(" not in s:
    insert = r'''pub(crate) fn restore_equal_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
) -> Result<(), String> {
    let first = first_hwnd as Hwnd;
    let second = second_hwnd as Hwnd;
    if first.is_null() || second.is_null() || first == second {
        return Err("stock snap pair has invalid window handles".to_owned());
    }

    let width = work_area[2].saturating_sub(work_area[0]);
    let height = work_area[3].saturating_sub(work_area[1]);
    if width <= 0 || height <= 0 {
        return Err("stock snap pair has an invalid monitor work area".to_owned());
    }

    let _foreground_restore = ForegroundRestoreGuard {
        hwnd: unsafe { GetForegroundWindow() },
    };

    // A canonical 50/50 pair needs no divider drag. Each member gets exactly
    // one native Snap attempt, followed by verification. If Windows makes no
    // progress, return immediately rather than replaying identical input.
    establish_pair(first, second, orientation)?;
    thread::sleep(SNAP_SETTLE);

    if equal_pair_matches_work_area(first, second, orientation, work_area, 3) {
        Ok(())
    } else {
        Err(format!(
            "Windows created the stock snap pair, but it did not settle into the expected 50/50 work-area halves: {}",
            pair_mismatch_description(
                first,
                second,
                orientation,
                match orientation {
                    SplitOrientation::SideBySide => work_area[0] + width / 2,
                    SplitOrientation::Stacked => work_area[1] + height / 2,
                },
            )
        ))
    }
}

fn equal_pair_matches_work_area(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    tolerance: i32,
) -> bool {
    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return false;
    }
    let Some(first_rect) = frame_bounds(first) else {
        return false;
    };
    let Some(second_rect) = frame_bounds(second) else {
        return false;
    };

    let close = |left: i32, right: i32| (left - right).abs() <= tolerance;
    match orientation {
        SplitOrientation::SideBySide => {
            let divider = work_area[0] + (work_area[2] - work_area[0]) / 2;
            close(first_rect.left, work_area[0])
                && close(first_rect.top, work_area[1])
                && close(first_rect.right, divider)
                && close(first_rect.bottom, work_area[3])
                && close(second_rect.left, divider)
                && close(second_rect.top, work_area[1])
                && close(second_rect.right, work_area[2])
                && close(second_rect.bottom, work_area[3])
        }
        SplitOrientation::Stacked => {
            let divider = work_area[1] + (work_area[3] - work_area[1]) / 2;
            close(first_rect.left, work_area[0])
                && close(first_rect.top, work_area[1])
                && close(first_rect.right, work_area[2])
                && close(first_rect.bottom, divider)
                && close(second_rect.left, work_area[0])
                && close(second_rect.top, divider)
                && close(second_rect.right, work_area[2])
                && close(second_rect.bottom, work_area[3])
        }
    }
}

'''
    if marker not in s:
        raise SystemExit("windows_snap insertion marker missing")
    s = s.replace(marker, insert + marker, 1)
    p.write_text(s)

# 2) Detect an unambiguous portrait top/bottom stock pair, suppress independent
# native Snap during generic placement, then restore the pair exactly once.
p = Path("src/restore/custom_snap.rs")
s = p.read_text()
old_use = "    SavedApplication, SavedDesktop, SavedNormalizedRect, SavedRect, SavedWindow, title_match_score,\n"
new_use = "    SavedApplication, SavedDesktop, SavedDisplay, SavedNormalizedRect, SavedRect, SavedWindow,\n    SnapSlot, WindowStateSpec, title_match_score,\n"
if old_use in s:
    s = s.replace(old_use, new_use, 1)
elif "SavedDisplay" not in s.split("};", 1)[0]:
    raise SystemExit("custom_snap import marker missing")

pair_struct = '''struct SavedPair<'a> {\n    first: SavedCustom<'a>,\n    second: SavedCustom<'a>,\n    orientation: SplitOrientation,\n    divider_fraction: f64,\n}\n'''
if "struct SavedStockPairIndex" not in s:
    addition = pair_struct + r'''

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedStockPairIndex {
    top_app: usize,
    top_window: usize,
    bottom_app: usize,
    bottom_window: usize,
}
'''
    if pair_struct not in s:
        raise SystemExit("SavedPair marker missing")
    s = s.replace(pair_struct, addition, 1)

restore_marker = "    report\n}\n\nfn saved_custom_windows"
if "pub(super) fn geometry_only_portrait_stacked_pairs" not in s:
    block = r'''    report
}

/// Returns a placement-only clone for a complete, unambiguous top/bottom pair
/// captured on a portrait monitor. Generic placement restores the exact two
/// non-overlapping rectangles without independently injecting TopHalf/BottomHalf
/// shortcuts. The original desktop remains authoritative for native pair replay.
pub(super) fn geometry_only_portrait_stacked_pairs(desktop: &SavedDesktop) -> SavedDesktop {
    let pairs = portrait_stacked_pair_indices(desktop);
    if pairs.is_empty() {
        return desktop.clone();
    }

    let mut placement = desktop.clone();
    for pair in pairs {
        placement.applications[pair.top_app].windows[pair.top_window].state = "normal".to_owned();
        placement.applications[pair.bottom_app].windows[pair.bottom_window].state = "normal".to_owned();
    }
    placement
}

/// Rebuilds a canonical portrait 50/50 top/bottom layout as one native pair.
/// Each shortcut is attempted once; an unchanged failure is reported immediately.
pub(super) fn restore_portrait_stacked_pairs(desktop: &SavedDesktop) -> CustomSnapRestoreReport {
    let mut report = CustomSnapRestoreReport::default();
    let pairs = portrait_stacked_pair_indices(desktop);
    if pairs.is_empty() {
        return report;
    }

    let current = match enumerate_windows() {
        Ok(windows) => windows,
        Err(error) => {
            report
                .failures
                .push(format!("portrait stock snap inventory failed: {error}"));
            return report;
        }
    };
    let mut used = HashSet::new();

    for pair in pairs {
        let top_app = &desktop.applications[pair.top_app];
        let top_window = &top_app.windows[pair.top_window];
        let bottom_app = &desktop.applications[pair.bottom_app];
        let bottom_window = &bottom_app.windows[pair.bottom_window];

        let top_saved = SavedCustom {
            app: top_app,
            window: top_window,
            normalized: stock_normalized(SnapSlot::TopHalf),
        };
        let bottom_saved = SavedCustom {
            app: bottom_app,
            window: bottom_window,
            normalized: stock_normalized(SnapSlot::BottomHalf),
        };

        let Some(top) = match_saved_window(top_saved, &current, &used) else {
            report.failures.push(format!(
                "portrait stock snap restore could not find the current top-half window '{}'",
                top_window.title
            ));
            continue;
        };
        used.insert(top.hwnd);

        let Some(bottom) = match_saved_window(bottom_saved, &current, &used) else {
            used.remove(&top.hwnd);
            report.failures.push(format!(
                "portrait stock snap restore could not find the current bottom-half window '{}'",
                bottom_window.title
            ));
            continue;
        };
        used.insert(bottom.hwnd);

        if !top
            .monitor
            .device_name
            .eq_ignore_ascii_case(&bottom.monitor.device_name)
        {
            report.failures.push(format!(
                "portrait stock snap pair '{}' / '{}' landed on different monitors before native grouping",
                top_window.title, bottom_window.title
            ));
            continue;
        }

        let work = top.monitor.work_area;
        if work.width() >= work.height() {
            report.failures.push(format!(
                "saved portrait stock snap pair '{}' / '{}' resolved to a non-portrait monitor at restore time",
                top_window.title, bottom_window.title
            ));
            continue;
        }

        let top_target = normalized_target(work, top_saved.normalized);
        let bottom_target = normalized_target(work, bottom_saved.normalized);
        let already_correct = top_target.is_some_and(|target| rect_distance(target, top.bounds) <= 12)
            && bottom_target.is_some_and(|target| rect_distance(target, bottom.bounds) <= 12)
            && windows_snap::is_arranged(top.hwnd as Hwnd) == Some(true)
            && windows_snap::is_arranged(bottom.hwnd as Hwnd) == Some(true);
        if already_correct {
            continue;
        }

        if let Err(error) = windows_snap::restore_equal_pair(
            top.hwnd,
            bottom.hwnd,
            SplitOrientation::Stacked,
            [work.left, work.top, work.right, work.bottom],
        ) {
            report.failures.push(format!(
                "portrait stock snap restore failed for '{}' + '{}': {error}",
                top_window.title, bottom_window.title
            ));
        }
    }

    report
}

fn portrait_stacked_pair_indices(desktop: &SavedDesktop) -> Vec<SavedStockPairIndex> {
    let mut tops = Vec::new();
    let mut bottoms = Vec::new();

    for (app_index, app) in desktop.applications.iter().enumerate() {
        for (window_index, window) in app.windows.iter().enumerate() {
            if !saved_window_is_on_portrait_display(desktop, window) {
                continue;
            }
            match window.state_spec() {
                WindowStateSpec::Snapped(SnapSlot::TopHalf) => {
                    tops.push((app_index, window_index, window));
                }
                WindowStateSpec::Snapped(SnapSlot::BottomHalf) => {
                    bottoms.push((app_index, window_index, window));
                }
                _ => {}
            }
        }
    }

    let mut result = Vec::new();
    for (top_app, top_window, top) in &tops {
        let compatible_bottoms = bottoms
            .iter()
            .filter(|(_, _, bottom)| same_saved_display(top, bottom))
            .collect::<Vec<_>>();
        if compatible_bottoms.len() != 1 {
            continue;
        }
        let (bottom_app, bottom_window, bottom) = *compatible_bottoms[0];
        let compatible_tops = tops
            .iter()
            .filter(|(_, _, candidate)| same_saved_display(candidate, bottom))
            .count();
        if compatible_tops != 1 {
            continue;
        }
        result.push(SavedStockPairIndex {
            top_app: *top_app,
            top_window: *top_window,
            bottom_app,
            bottom_window,
        });
    }
    result
}

fn saved_window_is_on_portrait_display(desktop: &SavedDesktop, window: &SavedWindow) -> bool {
    desktop
        .displays
        .iter()
        .find(|display| {
            display
                .device_name
                .eq_ignore_ascii_case(&window.display_device)
                || (!window.display_relation.is_empty()
                    && display
                        .relation_to_primary
                        .eq_ignore_ascii_case(&window.display_relation))
        })
        .is_some_and(|display| display.work_area.height() > display.work_area.width())
}

fn stock_normalized(slot: SnapSlot) -> SavedNormalizedRect {
    match slot {
        SnapSlot::TopHalf => SavedNormalizedRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 0.5,
        },
        SnapSlot::BottomHalf => SavedNormalizedRect {
            x: 0.0,
            y: 0.5,
            width: 1.0,
            height: 0.5,
        },
        _ => unreachable!("stock portrait pair only supports top/bottom halves"),
    }
}

fn saved_custom_windows'''
    if restore_marker not in s:
        raise SystemExit("custom_snap restore marker missing")
    s = s.replace(restore_marker, block, 1)

if "portrait_top_bottom_pair_is_deferred_as_one_unit" not in s:
    test_marker = "    #[test]\n    fn arbitrary_side_by_side_ratio_forms_pair() {"
    test_block = r'''    #[test]
    fn portrait_top_bottom_pair_is_deferred_as_one_unit() {
        let mut top_app = app("Top");
        let mut bottom_app = app("Bottom");
        let mut top = custom_window("Top", 0.0, 1.0);
        top.state = "snapped:top-half".to_owned();
        top.normalized_bounds = Some(stock_normalized(SnapSlot::TopHalf));
        top.bounds = SavedRect { left: 0, top: 0, right: 1080, bottom: 960 };
        let mut bottom = custom_window("Bottom", 0.0, 1.0);
        bottom.state = "snapped:bottom-half".to_owned();
        bottom.normalized_bounds = Some(stock_normalized(SnapSlot::BottomHalf));
        bottom.bounds = SavedRect { left: 0, top: 960, right: 1080, bottom: 1920 };
        top_app.windows.push(top);
        bottom_app.windows.push(bottom);
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![SavedDisplay {
                device_name: "DISPLAY1".to_owned(),
                bounds: SavedRect { left: 0, top: 0, right: 1080, bottom: 1920 },
                work_area: SavedRect { left: 0, top: 0, right: 1080, bottom: 1920 },
                is_primary: true,
                scale_percent: 100,
                orientation: "portrait".to_owned(),
                relation_to_primary: "primary".to_owned(),
            }],
            applications: vec![top_app, bottom_app],
        };

        assert_eq!(portrait_stacked_pair_indices(&desktop).len(), 1);
        let placement = geometry_only_portrait_stacked_pairs(&desktop);
        assert_eq!(placement.applications[0].windows[0].state, "normal");
        assert_eq!(placement.applications[1].windows[0].state, "normal");
        assert_eq!(desktop.applications[0].windows[0].state, "snapped:top-half");
        assert_eq!(desktop.applications[1].windows[0].state, "snapped:bottom-half");
    }

    #[test]
    fn landscape_top_bottom_pair_keeps_existing_restore_path() {
        let mut top_app = app("Top");
        let mut bottom_app = app("Bottom");
        let mut top = custom_window("Top", 0.0, 1.0);
        top.state = "snapped:top-half".to_owned();
        top.normalized_bounds = Some(stock_normalized(SnapSlot::TopHalf));
        let mut bottom = custom_window("Bottom", 0.0, 1.0);
        bottom.state = "snapped:bottom-half".to_owned();
        bottom.normalized_bounds = Some(stock_normalized(SnapSlot::BottomHalf));
        top_app.windows.push(top);
        bottom_app.windows.push(bottom);
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![SavedDisplay {
                device_name: "DISPLAY1".to_owned(),
                bounds: SavedRect { left: 0, top: 0, right: 1920, bottom: 1080 },
                work_area: SavedRect { left: 0, top: 0, right: 1920, bottom: 1040 },
                is_primary: true,
                scale_percent: 100,
                orientation: "landscape".to_owned(),
                relation_to_primary: "primary".to_owned(),
            }],
            applications: vec![top_app, bottom_app],
        };

        assert!(portrait_stacked_pair_indices(&desktop).is_empty());
        let placement = geometry_only_portrait_stacked_pairs(&desktop);
        assert_eq!(placement.applications[0].windows[0].state, "snapped:top-half");
        assert_eq!(placement.applications[1].windows[0].state, "snapped:bottom-half");
    }

'''
    if test_marker not in s:
        raise SystemExit("custom_snap test marker missing")
    s = s.replace(test_marker, test_block + test_marker, 1)
p.write_text(s)

# 3) Use geometry-only clones in both generic passes and run the one-shot pair
# reconstruction before the final geometry-free Z-order operation.
p = Path("src/restore/mod.rs")
s = p.read_text()
old = "                report.desktop = windows::restore_desktop(&prerequisite, options.dry_run);"
new = '''                let prerequisite_placement =
                    custom_snap::geometry_only_portrait_stacked_pairs(&prerequisite);
                report.desktop =
                    windows::restore_desktop(&prerequisite_placement, options.dry_run);'''
if old in s:
    s = s.replace(old, new, 1)
elif "geometry_only_portrait_stacked_pairs(&prerequisite)" not in s:
    raise SystemExit("prerequisite restore marker missing")

old = "            let mut final_desktop = windows::restore_desktop_forced(desktop, false);"
new = '''            let final_placement = custom_snap::geometry_only_portrait_stacked_pairs(desktop);
            let mut final_desktop = windows::restore_desktop_forced(&final_placement, false);'''
if old in s:
    s = s.replace(old, new, 1)
elif "geometry_only_portrait_stacked_pairs(desktop)" not in s:
    raise SystemExit("final restore marker missing")

custom_marker = '''            let custom = custom_snap::restore(desktop);
            final_desktop.warnings.extend(custom.warnings);
            final_desktop.failures.extend(custom.failures);
'''
if "restore_portrait_stacked_pairs(desktop)" not in s:
    replacement = '''            let portrait_stock = custom_snap::restore_portrait_stacked_pairs(desktop);
            final_desktop.warnings.extend(portrait_stock.warnings);
            final_desktop.failures.extend(portrait_stock.failures);

''' + custom_marker
    if custom_marker not in s:
        raise SystemExit("custom restore marker missing")
    s = s.replace(custom_marker, replacement, 1)
p.write_text(s)
