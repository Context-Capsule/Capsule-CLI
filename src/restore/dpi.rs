type DpiAwarenessContext = isize;

const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: DpiAwarenessContext = -4;

#[link(name = "user32")]
unsafe extern "system" {
    fn SetThreadDpiAwarenessContext(
        dpi_context: DpiAwarenessContext,
    ) -> DpiAwarenessContext;
}

/// Temporarily makes the restore thread Per-Monitor-V2 DPI aware.
///
/// Capture uses DWM extended-frame bounds, which are physical screen coordinates.
/// The matching restore thread therefore needs a non-virtualized coordinate context
/// before it enumerates monitors and calls SetWindowPos on mixed-DPI desktops.
pub struct DpiAwarenessGuard {
    previous: DpiAwarenessContext,
}

impl DpiAwarenessGuard {
    pub fn per_monitor_v2() -> Option<Self> {
        let previous = unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        };
        (previous != 0).then_some(Self { previous })
    }
}

impl Drop for DpiAwarenessGuard {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.previous);
        }
    }
}
