#![cfg(windows)]

use context_capsule::restore::{RestoreOptions, SavedRect, SnapSlot, restore_snapshot, snap_rect};
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    fs,
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::mpsc,
    thread,
    time::Duration,
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Hmonitor = *mut c_void;
type Bool = i32;
type Hresult = i32;
type WndProc = Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>;

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SW_RESTORE: i32 = 9;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const MONITORINFOF_PRIMARY: u32 = 1;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
const WM_CLOSE: u32 = 0x0010;
const WM_QUIT: u32 = 0x0012;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl From<Rect> for SavedRect {
    fn from(value: Rect) -> Self {
        Self {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}

#[repr(C)]
struct WndClassW {
    style: u32,
    wnd_proc: WndProc,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct MonitorInfoExW {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
    device: [u16; 32],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(class: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(message: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(message: *const Msg) -> Bool;
    fn DispatchMessageW(message: *const Msg) -> isize;
    fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> Bool;
    fn PostThreadMessageW(thread_id: u32, message: u32, wparam: usize, lparam: isize) -> Bool;
    fn ShowWindow(hwnd: Hwnd, command: i32) -> Bool;
    fn SetWindowPos(
        hwnd: Hwnd,
        insert_after: Hwnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> Bool;
    fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Hmonitor;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfoExW) -> Bool;
    fn SetThreadDpiAwarenessContext(context: isize) -> isize;
    fn IsWindowArranged(hwnd: Hwnd) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
    fn GetCurrentThreadId() -> u32;
    fn GetLastError() -> u32;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: Hwnd,
        attribute: u32,
        value: *mut c_void,
        value_size: u32,
    ) -> Hresult;
}

unsafe extern "system" fn window_proc(
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[derive(Clone, Copy)]
struct LiveWindow {
    hwnd: usize,
}

impl LiveWindow {
    fn hwnd(self) -> Hwnd {
        self.hwnd as Hwnd
    }
}

struct WindowHost {
    thread_id: u32,
    windows: Vec<LiveWindow>,
    join: Option<thread::JoinHandle<()>>,
}

impl WindowHost {
    fn start(count: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            let class_name = wide("ContextCapsuleLiveSnapValidationWindow");
            let instance = unsafe { GetModuleHandleW(ptr::null()) };
            assert!(!instance.is_null(), "GetModuleHandleW failed");
            let class = WndClassW {
                style: 0,
                wnd_proc: Some(window_proc),
                cls_extra: 0,
                wnd_extra: 0,
                instance,
                icon: ptr::null_mut(),
                cursor: ptr::null_mut(),
                background: ptr::null_mut(),
                menu_name: ptr::null(),
                class_name: class_name.as_ptr(),
            };
            let atom = unsafe { RegisterClassW(&class) };
            if atom == 0 {
                let error = unsafe { GetLastError() };
                assert_eq!(
                    error, 1410,
                    "RegisterClassW failed with Win32 error {error}"
                );
            }

            let mut windows = Vec::new();
            for index in 0..count {
                let title = wide(&format!("Capsule Snap Live {}", index + 1));
                let hwnd = unsafe {
                    CreateWindowExW(
                        0,
                        class_name.as_ptr(),
                        title.as_ptr(),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        180 + index as i32 * 45,
                        140 + index as i32 * 35,
                        760,
                        520,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        instance,
                        ptr::null_mut(),
                    )
                };
                assert!(!hwnd.is_null(), "CreateWindowExW failed at index {index}");
                windows.push(LiveWindow {
                    hwnd: hwnd as usize,
                });
            }
            let thread_id = unsafe { GetCurrentThreadId() };
            sender.send((thread_id, windows)).expect("send live HWNDs");

            let mut message: Msg = unsafe { zeroed() };
            loop {
                let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
                if result <= 0 {
                    break;
                }
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
        let (thread_id, windows) = receiver.recv().expect("receive live HWNDs");
        thread::sleep(Duration::from_millis(250));
        Self {
            thread_id,
            windows,
            join: Some(join),
        }
    }

    fn prepare(&self, titles: &[String]) {
        for (index, window) in self.windows.iter().copied().enumerate() {
            if index < titles.len() {
                let title = wide(&titles[index]);
                assert_ne!(unsafe { SetWindowTextW(window.hwnd(), title.as_ptr()) }, 0);
                unsafe {
                    ShowWindow(window.hwnd(), SW_SHOW);
                }
            } else {
                unsafe {
                    ShowWindow(window.hwnd(), SW_HIDE);
                }
            }
        }
        thread::sleep(Duration::from_millis(160));
    }
}

impl Drop for WindowHost {
    fn drop(&mut self) {
        for window in &self.windows {
            unsafe {
                PostMessageW(window.hwnd(), WM_CLOSE, 0, 0);
            }
        }
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
struct SlotSpec {
    title: String,
    state: String,
    target: SavedRect,
    normalized: [f64; 4],
}

#[derive(Clone)]
struct DisplayInfo {
    device: String,
    bounds: SavedRect,
    work: SavedRect,
    primary: bool,
}

#[test]
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
    let top_half = snap_rect(SnapSlot::TopHalf, display.work);
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

#[test]
#[ignore = "interactive Windows shell validation; run only on a desktop self-hosted runner"]
fn live_restore_rejects_near_floating_windows_and_restores_real_snap() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let out_dir = std::env::var_os("SNAP_LIVE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("context-capsule-snap-live"));
    fs::create_dir_all(&out_dir).expect("create live output directory");

    let host = WindowHost::start(4);
    let display = display_for(host.windows[0].hwnd()).expect("monitor info");
    let executable = std::env::current_exe().expect("current test executable");
    let mut log = String::new();
    log.push_str(&format!(
        "machine={}\ndevice={}\nbounds={:?}\nwork_area={:?}\n\n",
        std::env::var("COMPUTERNAME").unwrap_or_default(),
        display.device,
        display.bounds,
        display.work
    ));

    run_stock_scenario(
        &host,
        &display,
        &executable,
        &out_dir,
        "01-left-half",
        vec![stock_spec(
            "Live Left Half",
            "snapped:left-half",
            SnapSlot::LeftHalf,
            display.work,
        )],
        &mut log,
    );

    run_stock_scenario(
        &host,
        &display,
        &executable,
        &out_dir,
        "02-four-quarters",
        vec![
            stock_spec(
                "Live Top Left",
                "snapped:top-left-quarter",
                SnapSlot::TopLeftQuarter,
                display.work,
            ),
            stock_spec(
                "Live Top Right",
                "snapped:top-right-quarter",
                SnapSlot::TopRightQuarter,
                display.work,
            ),
            stock_spec(
                "Live Bottom Left",
                "snapped:bottom-left-quarter",
                SnapSlot::BottomLeftQuarter,
                display.work,
            ),
            stock_spec(
                "Live Bottom Right",
                "snapped:bottom-right-quarter",
                SnapSlot::BottomRightQuarter,
                display.work,
            ),
        ],
        &mut log,
    );

    run_stock_scenario(
        &host,
        &display,
        &executable,
        &out_dir,
        "03-three-thirds",
        vec![
            stock_spec(
                "Live Left Third",
                "snapped:left-third",
                SnapSlot::LeftThird,
                display.work,
            ),
            stock_spec(
                "Live Center Third",
                "snapped:center-third",
                SnapSlot::CenterThird,
                display.work,
            ),
            stock_spec(
                "Live Right Third",
                "snapped:right-third",
                SnapSlot::RightThird,
                display.work,
            ),
        ],
        &mut log,
    );

    run_stock_scenario(
        &host,
        &display,
        &executable,
        &out_dir,
        "04-two-thirds-left",
        vec![
            stock_spec(
                "Live Left Two Thirds",
                "snapped:left-two-thirds",
                SnapSlot::LeftTwoThirds,
                display.work,
            ),
            stock_spec(
                "Live Right Third Pair",
                "snapped:right-third",
                SnapSlot::RightThird,
                display.work,
            ),
        ],
        &mut log,
    );

    run_stock_scenario(
        &host,
        &display,
        &executable,
        &out_dir,
        "05-two-thirds-right",
        vec![
            stock_spec(
                "Live Left Third Pair",
                "snapped:left-third",
                SnapSlot::LeftThird,
                display.work,
            ),
            stock_spec(
                "Live Right Two Thirds",
                "snapped:right-two-thirds",
                SnapSlot::RightTwoThirds,
                display.work,
            ),
        ],
        &mut log,
    );

    run_custom_pair_scenario(&host, &display, &executable, &out_dir, &mut log);
    fs::write(out_dir.join("runtime.txt"), log).expect("write runtime log");
}

fn stock_spec(title: &str, state: &str, slot: SnapSlot, work: SavedRect) -> SlotSpec {
    let target = snap_rect(work, slot);
    let normalized = normalized_for_slot(slot);
    SlotSpec {
        title: title.to_owned(),
        state: state.to_owned(),
        target,
        normalized,
    }
}

fn run_stock_scenario(
    host: &WindowHost,
    display: &DisplayInfo,
    executable: &Path,
    out_dir: &Path,
    name: &str,
    specs: Vec<SlotSpec>,
    log: &mut String,
) {
    let titles = specs
        .iter()
        .map(|spec| spec.title.clone())
        .collect::<Vec<_>>();
    host.prepare(&titles);

    for (window, spec) in host.windows.iter().copied().zip(specs.iter()) {
        let near = near_target(spec.target, display.work);
        stage_frame(window.hwnd(), near).expect("stage near-floating window");
        let observed = frame_bounds(window.hwnd()).expect("near-floating DWM bounds");
        assert!(
            rect_close_px(observed.into(), spec.target, 14),
            "{} must reproduce the old close-enough path: observed={observed:?} target={:?}",
            spec.title,
            spec.target
        );
        assert!(
            !rect_close_px(observed.into(), spec.target, 3),
            "{} must be outside the new snap tolerance: observed={observed:?} target={:?}",
            spec.title,
            spec.target
        );
        assert_eq!(
            unsafe { IsWindowArranged(window.hwnd()) },
            0,
            "{} unexpectedly arranged before restore",
            spec.title
        );
    }

    screenshot(out_dir, &format!("{name}-before-near-floating.png"));
    let snapshot = snapshot_for(display, executable, &specs);
    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    log.push_str(&format!(
        "[{name}] success={} warnings={:?} failures={:?} desktop_failures={:?}\n",
        report.success(),
        report.warnings,
        report.failures,
        report.desktop.failures
    ));
    assert!(report.success(), "{name} restore failed: {report:#?}");

    thread::sleep(Duration::from_millis(350));
    for (window, spec) in host.windows.iter().copied().zip(specs.iter()) {
        let observed = frame_bounds(window.hwnd()).expect("final DWM bounds");
        let arranged = unsafe { IsWindowArranged(window.hwnd()) } != 0;
        log.push_str(&format!(
            "  {} arranged={} observed={:?} target={:?}\n",
            spec.title, arranged, observed, spec.target
        ));
        assert!(
            arranged,
            "{} has target-like geometry but is not truly Windows-arranged",
            spec.title
        );
        assert!(
            rect_close_px(observed.into(), spec.target, 3),
            "{} native Snap geometry is outside strict tolerance: observed={observed:?}, target={:?}",
            spec.title,
            spec.target
        );
    }
    screenshot(out_dir, &format!("{name}-after-native.png"));
    log.push('\n');
}

fn run_custom_pair_scenario(
    host: &WindowHost,
    display: &DisplayInfo,
    executable: &Path,
    out_dir: &Path,
    log: &mut String,
) {
    let width = display.work.width();
    let divider = display.work.left + (width as f64 * 0.27).round() as i32;
    let specs = vec![
        SlotSpec {
            title: "Live Custom 27".to_owned(),
            state: "snapped:custom".to_owned(),
            target: SavedRect {
                left: display.work.left,
                top: display.work.top,
                right: divider,
                bottom: display.work.bottom,
            },
            normalized: [0.0, 0.0, 0.27, 1.0],
        },
        SlotSpec {
            title: "Live Custom 73".to_owned(),
            state: "snapped:custom".to_owned(),
            target: SavedRect {
                left: divider,
                top: display.work.top,
                right: display.work.right,
                bottom: display.work.bottom,
            },
            normalized: [0.27, 0.0, 0.73, 1.0],
        },
    ];
    let titles = specs
        .iter()
        .map(|spec| spec.title.clone())
        .collect::<Vec<_>>();
    host.prepare(&titles);
    for (window, spec) in host.windows.iter().copied().zip(specs.iter()) {
        stage_frame(window.hwnd(), near_target(spec.target, display.work))
            .expect("stage custom pair near-floating");
        assert_eq!(unsafe { IsWindowArranged(window.hwnd()) }, 0);
    }
    screenshot(out_dir, "06-custom-27-73-before.png");
    let snapshot = snapshot_for(display, executable, &specs);
    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    log.push_str(&format!(
        "[06-custom-27-73] success={} warnings={:?} failures={:?} desktop_failures={:?}\n",
        report.success(),
        report.warnings,
        report.failures,
        report.desktop.failures
    ));
    assert!(report.success(), "custom 27/73 restore failed: {report:#?}");
    thread::sleep(Duration::from_millis(400));
    for (window, spec) in host.windows.iter().copied().zip(specs.iter()) {
        let observed = frame_bounds(window.hwnd()).expect("custom final DWM bounds");
        let arranged = unsafe { IsWindowArranged(window.hwnd()) } != 0;
        log.push_str(&format!(
            "  {} arranged={} observed={:?} target={:?}\n",
            spec.title, arranged, observed, spec.target
        ));
        assert!(arranged, "{} custom pair is not arranged", spec.title);
        assert!(
            rect_close_px(observed.into(), spec.target, 24),
            "{} custom pair geometry missed divider target",
            spec.title
        );
    }
    screenshot(out_dir, "06-custom-27-73-after-native.png");
}

fn snapshot_for(display: &DisplayInfo, executable: &Path, specs: &[SlotSpec]) -> Value {
    let exe = executable.to_string_lossy().to_string();
    let name = executable
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("windows_snap_live");
    let windows = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            json!({
                "title": spec.title,
                "bounds": rect_json(spec.target),
                "restore_bounds": null,
                "normalized_bounds": {
                    "x": spec.normalized[0], "y": spec.normalized[1],
                    "width": spec.normalized[2], "height": spec.normalized[3]
                },
                "state": spec.state,
                "display_device": display.device,
                "display_relation": "primary",
                "display_scale_percent": 100,
                "is_foreground": index == 0,
                "z_order": index,
                "virtual_desktop_id": null,
                "is_on_current_virtual_desktop": true,
                "taskbar_candidate": true
            })
        })
        .collect::<Vec<_>>();

    json!({
        "desktop": {
            "status": "available",
            "displays": [{
                "device_name": display.device,
                "bounds": rect_json(display.bounds),
                "work_area": rect_json(display.work),
                "is_primary": display.primary,
                "scale_percent": 100,
                "orientation": if display.bounds.width() >= display.bounds.height() { "landscape" } else { "portrait" },
                "relation_to_primary": "primary"
            }],
            "applications": [{
                "name": name,
                "executable_path": exe,
                "app_user_model_id": null,
                "file_version": null,
                "classification": "user-application",
                "launch": null,
                "windows": windows,
                "discovered_as_background": false
            }]
        }
    })
}

fn rect_json(rect: SavedRect) -> Value {
    json!({ "left": rect.left, "top": rect.top, "right": rect.right, "bottom": rect.bottom })
}

fn normalized_for_slot(slot: SnapSlot) -> [f64; 4] {
    match slot {
        SnapSlot::LeftHalf => [0.0, 0.0, 0.5, 1.0],
        SnapSlot::RightHalf => [0.5, 0.0, 0.5, 1.0],
        SnapSlot::TopHalf => [0.0, 0.0, 1.0, 0.5],
        SnapSlot::BottomHalf => [0.0, 0.5, 1.0, 0.5],
        SnapSlot::TopLeftQuarter => [0.0, 0.0, 0.5, 0.5],
        SnapSlot::TopRightQuarter => [0.5, 0.0, 0.5, 0.5],
        SnapSlot::BottomLeftQuarter => [0.0, 0.5, 0.5, 0.5],
        SnapSlot::BottomRightQuarter => [0.5, 0.5, 0.5, 0.5],
        SnapSlot::LeftThird => [0.0, 0.0, 1.0 / 3.0, 1.0],
        SnapSlot::CenterThird => [1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0],
        SnapSlot::RightThird => [2.0 / 3.0, 0.0, 1.0 / 3.0, 1.0],
        SnapSlot::LeftTwoThirds => [0.0, 0.0, 2.0 / 3.0, 1.0],
        SnapSlot::RightTwoThirds => [1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0],
    }
}

fn display_for(hwnd: Hwnd) -> Option<DisplayInfo> {
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

fn near_target(target: SavedRect, work: SavedRect) -> SavedRect {
    let dx = if target.right >= work.right - 1 {
        -6
    } else {
        6
    };
    let dy = if target.bottom >= work.bottom - 1 {
        -6
    } else {
        6
    };
    SavedRect {
        left: target.left + dx,
        top: target.top + dy,
        right: target.right + dx,
        bottom: target.bottom + dy,
    }
}

fn stage_frame(hwnd: Hwnd, desired: SavedRect) -> Result<(), String> {
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
    }
    thread::sleep(Duration::from_millis(100));
    let mut outer = Rect::default();
    if unsafe { GetWindowRect(hwnd, &mut outer) } == 0 {
        return Err("GetWindowRect failed".to_owned());
    }
    let frame = frame_bounds(hwnd).unwrap_or(outer);
    let left_inset = frame.left - outer.left;
    let top_inset = frame.top - outer.top;
    let right_inset = outer.right - frame.right;
    let bottom_inset = outer.bottom - frame.bottom;
    let left = desired.left - left_inset;
    let top = desired.top - top_inset;
    let right = desired.right + right_inset;
    let bottom = desired.bottom + bottom_inset;
    if unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            left,
            top,
            (right - left).max(1),
            (bottom - top).max(1),
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    } == 0
    {
        return Err("SetWindowPos failed".to_owned());
    }
    thread::sleep(Duration::from_millis(160));
    Ok(())
}

fn frame_bounds(hwnd: Hwnd) -> Option<Rect> {
    let mut rect = Rect::default();
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut Rect).cast(),
            size_of::<Rect>() as u32,
        )
    } >= 0
    {
        Some(rect)
    } else if unsafe { GetWindowRect(hwnd, &mut rect) } != 0 {
        Some(rect)
    } else {
        None
    }
}

fn rect_close_px(actual: SavedRect, expected: SavedRect, tolerance: i32) -> bool {
    (actual.left - expected.left).abs() <= tolerance
        && (actual.top - expected.top).abs() <= tolerance
        && (actual.right - expected.right).abs() <= tolerance
        && (actual.bottom - expected.bottom).abs() <= tolerance
}

fn screenshot(out_dir: &Path, file_name: &str) {
    let path = out_dir.join(file_name);
    let escaped = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $v=[System.Windows.Forms.SystemInformation]::VirtualScreen; $b=New-Object System.Drawing.Bitmap($v.Width,$v.Height); $g=[System.Drawing.Graphics]::FromImage($b); $g.CopyFromScreen($v.Left,$v.Top,0,0,$b.Size); $g.Dispose(); $b.Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); $b.Dispose()",
        escaped
    );
    let status = Command::new("pwsh")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .expect("start screenshot PowerShell");
    assert!(
        status.success(),
        "screenshot command failed for {}",
        path.display()
    );
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
