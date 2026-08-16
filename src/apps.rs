#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApp {
    pub pid: u32,
    pub executable: Option<String>,
    pub window_title: String,
}

#[cfg(windows)]
pub fn list_open_apps() -> Result<Vec<OpenApp>, String> {
    windows::list_open_apps()
}

#[cfg(not(windows))]
pub fn list_open_apps() -> Result<Vec<OpenApp>, String> {
    Err("open application discovery is currently supported on Windows only".to_owned())
}

#[cfg(windows)]
mod windows {
    use super::OpenApp;
    use std::{ffi::c_void, io, path::Path};

    type Hwnd = *mut c_void;
    type Handle = *mut c_void;
    type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> i32>;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        fn QueryFullProcessImageNameW(
            process: Handle,
            flags: u32,
            executable_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    pub fn list_open_apps() -> Result<Vec<OpenApp>, String> {
        let mut apps = Vec::<OpenApp>::new();
        let lparam = (&mut apps as *mut Vec<OpenApp>) as isize;

        let result = unsafe { EnumWindows(Some(enum_window), lparam) };
        if result == 0 {
            return Err(format!(
                "failed to enumerate Windows desktop windows: {}",
                io::Error::last_os_error()
            ));
        }

        apps.sort_by(|left, right| {
            left.executable
                .cmp(&right.executable)
                .then_with(|| left.pid.cmp(&right.pid))
                .then_with(|| left.window_title.cmp(&right.window_title))
        });

        Ok(apps)
    }

    unsafe extern "system" fn enum_window(hwnd: Hwnd, lparam: isize) -> i32 {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }

        let title_length = unsafe { GetWindowTextLengthW(hwnd) };
        if title_length <= 0 {
            return 1;
        }

        let mut title_buffer = vec![0_u16; title_length as usize + 1];
        let copied = unsafe {
            GetWindowTextW(
                hwnd,
                title_buffer.as_mut_ptr(),
                title_buffer.len() as i32,
            )
        };
        if copied <= 0 {
            return 1;
        }

        let window_title = String::from_utf16_lossy(&title_buffer[..copied as usize]);
        let window_title = window_title.trim().to_owned();
        if window_title.is_empty() {
            return 1;
        }

        let mut pid = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == 0 {
            return 1;
        }

        let executable = process_executable_name(pid);
        let apps = unsafe { &mut *(lparam as *mut Vec<OpenApp>) };
        apps.push(OpenApp {
            pid,
            executable,
            window_title,
        });

        1
    }

    fn process_executable_name(pid: u32) -> Option<String> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }

        let mut buffer = vec![0_u16; 32_768];
        let mut size = buffer.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size)
        };
        unsafe { CloseHandle(process) };

        if result == 0 || size == 0 {
            return None;
        }

        let full_path = String::from_utf16_lossy(&buffer[..size as usize]);
        Path::new(&full_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| {
                if full_path.is_empty() {
                    None
                } else {
                    Some(full_path)
                }
            })
    }
}
