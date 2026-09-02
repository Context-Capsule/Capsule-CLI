from pathlib import Path

p = Path('tests/windows_snap_live.rs')
s = p.read_text()

# Add the monitor-enumeration callback type and FFI entry point.
if 'type MonitorEnumProc' not in s:
    s = s.replace(
        'type WndProc = Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>;\n',
        'type WndProc = Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>;\n'
        'type MonitorEnumProc = Option<unsafe extern "system" fn(Hmonitor, Handle, *mut Rect, isize) -> Bool>;\n',
        1,
    )

if 'fn EnumDisplayMonitors(' not in s:
    marker = '    fn IsWindowArranged(hwnd: Hwnd) -> Bool;\n'
    addition = marker + '''    fn EnumDisplayMonitors(\n        hdc: Handle,\n        clip: *const Rect,\n        callback: MonitorEnumProc,\n        data: isize,\n    ) -> Bool;\n'''
    if marker not in s:
        raise SystemExit('user32 FFI marker missing')
    s = s.replace(marker, addition, 1)

# Add the focused real-HWND regression before the broader stock matrix.
if 'live_restore_portrait_top_bottom_as_one_native_pair' not in s:
    marker = '''#[test]\n#[ignore = "interactive Windows shell validation; run only on a desktop self-hosted runner"]\nfn live_restore_rejects_near_floating_windows_and_restores_real_snap() {'''
    test = r'''#[test]
#[ignore = "interactive Windows shell validation; run only on a desktop self-hosted runner"]
fn live_restore_portrait_top_bottom_as_one_native_pair() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let out_dir = std::env::var_os("SNAP_LIVE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("context-capsule-portrait-stacked"));
    fs::create_dir_all(&out_dir).expect("create portrait live output directory");

    let display = portrait_display().expect("this regression requires a real portrait monitor");
    let host = WindowHost::start(2);
    let executable = std::env::current_exe().expect("current test executable");
    let top_target = snap_rect(display.work, SnapSlot::TopHalf);
    let bottom_target = snap_rect(display.work, SnapSlot::BottomHalf);
    let specs = vec![
        stock_spec(
            "Portrait Top Half",
            "snapped:top-half",
            SnapSlot::TopHalf,
            display.work,
        ),
        stock_spec(
            "Portrait Bottom Half",
            "snapped:bottom-half",
            SnapSlot::BottomHalf,
            display.work,
        ),
    ];
    let titles = specs.iter().map(|spec| spec.title.clone()).collect::<Vec<_>>();
    host.prepare(&titles);

    // Reproduce the user's bad starting state: both windows are close to their
    // saved halves but are ordinary floating windows, not Windows-arranged.
    stage_frame(host.windows[0].hwnd(), near_target(top_target, display.work))
        .expect("stage portrait top window");
    stage_frame(host.windows[1].hwnd(), near_target(bottom_target, display.work))
        .expect("stage portrait bottom window");
    assert_eq!(unsafe { IsWindowArranged(host.windows[0].hwnd()) }, 0);
    assert_eq!(unsafe { IsWindowArranged(host.windows[1].hwnd()) }, 0);

    screenshot(&out_dir, "portrait-stacked-before.png");
    let snapshot = snapshot_for(&display, &executable, &specs);
    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    fs::write(
        out_dir.join("portrait-stacked-report.txt"),
        format!("{report:#?}"),
    )
    .expect("write portrait restore report");
    assert!(report.success(), "portrait stacked restore failed: {report:#?}");

    // The bug was visible after repeated attempts and final ordering, so verify
    // only after the restore has fully settled.
    thread::sleep(Duration::from_millis(1200));
    let top = frame_bounds(host.windows[0].hwnd()).expect("final top DWM bounds");
    let bottom = frame_bounds(host.windows[1].hwnd()).expect("final bottom DWM bounds");
    assert_ne!(
        unsafe { IsWindowArranged(host.windows[0].hwnd()) },
        0,
        "portrait top window is only floating at the target rectangle"
    );
    assert_ne!(
        unsafe { IsWindowArranged(host.windows[1].hwnd()) },
        0,
        "portrait bottom window is only floating at the target rectangle"
    );
    assert!(
        rect_close_px(top.into(), top_target, 3),
        "portrait top geometry missed its strict target: observed={top:?}, target={top_target:?}"
    );
    assert!(
        rect_close_px(bottom.into(), bottom_target, 3),
        "portrait bottom geometry missed its strict target: observed={bottom:?}, target={bottom_target:?}"
    );
    assert!(
        (top.bottom - bottom.top).abs() <= 3,
        "portrait pair has a gap or overlap at the divider: top={top:?}, bottom={bottom:?}"
    );
    assert!(
        top.bottom <= bottom.top + 3,
        "portrait windows overlap instead of halving the monitor: top={top:?}, bottom={bottom:?}"
    );

    screenshot(&out_dir, "portrait-stacked-after-1200ms.png");
}

'''
    if marker not in s:
        raise SystemExit('live test insertion marker missing')
    s = s.replace(marker, test + marker, 1)

# Factor display lookup and add enumeration of a real portrait monitor.
if 'fn portrait_display() -> Option<DisplayInfo>' not in s:
    old = r'''fn display_for(hwnd: Hwnd) -> Option<DisplayInfo> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info = MonitorInfoExW {
        size: size_of::<MonitorInfoExW>() as u32,
        monitor: Rect::default(),
        work: Rect::default(),
        flags: 0,
        device: [0; 32],
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let length = info.device.iter().position(|unit| *unit == 0).unwrap_or(info.device.len());
    Some(DisplayInfo {
        device: String::from_utf16_lossy(&info.device[..length]),
        bounds: info.monitor.into(),
        work: info.work.into(),
        primary: info.flags & MONITORINFOF_PRIMARY != 0,
    })
}
'''
    new = r'''fn display_for(hwnd: Hwnd) -> Option<DisplayInfo> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    display_from_monitor(monitor)
}

fn display_from_monitor(monitor: Hmonitor) -> Option<DisplayInfo> {
    if monitor.is_null() {
        return None;
    }
    let mut info = MonitorInfoExW {
        size: size_of::<MonitorInfoExW>() as u32,
        monitor: Rect::default(),
        work: Rect::default(),
        flags: 0,
        device: [0; 32],
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let length = info
        .device
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(info.device.len());
    Some(DisplayInfo {
        device: String::from_utf16_lossy(&info.device[..length]),
        bounds: info.monitor.into(),
        work: info.work.into(),
        primary: info.flags & MONITORINFOF_PRIMARY != 0,
    })
}

unsafe extern "system" fn collect_monitor(
    monitor: Hmonitor,
    _hdc: Handle,
    _rect: *mut Rect,
    data: isize,
) -> Bool {
    let displays = unsafe { &mut *(data as *mut Vec<DisplayInfo>) };
    if let Some(display) = display_from_monitor(monitor) {
        displays.push(display);
    }
    1
}

fn portrait_display() -> Option<DisplayInfo> {
    let mut displays = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(collect_monitor),
            (&mut displays as *mut Vec<DisplayInfo>) as isize,
        )
    };
    assert_ne!(ok, 0, "EnumDisplayMonitors failed");
    displays
        .into_iter()
        .find(|display| display.work.height() > display.work.width())
}
'''
    if old not in s:
        raise SystemExit('display_for block marker missing')
    s = s.replace(old, new, 1)

p.write_text(s)
