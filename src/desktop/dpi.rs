type DpiAwarenessContext = isize;

const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: DpiAwarenessContext = -4;

#[link(name = "user32")]
unsafe extern "system" {
    fn SetThreadDpiAwarenessContext(dpi_context: DpiAwarenessContext) -> DpiAwarenessContext;
}

/// Temporarily switches desktop capture into Per-Monitor-V2 DPI awareness.
///
/// `DWMWA_EXTENDED_FRAME_BOUNDS` is expressed in physical screen pixels. The
/// USER32 monitor/work-area APIs used by capture must be queried from the same
/// coordinate space so normalized bounds, size and snap classification are not
/// distorted on displays using scaling above 100%.
pub struct DpiAwarenessGuard {
    previous: DpiAwarenessContext,
}

impl DpiAwarenessGuard {
    pub fn per_monitor_v2() -> Option<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
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
