use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;
use std::num::{NonZeroU16, NonZeroU32};
use std::{io, iter, mem, ptr};

use dpi::{PhysicalPosition, PhysicalSize};
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_PIXELFORMAT_8BPP,
    DISPLAYCONFIG_PIXELFORMAT_16BPP, DISPLAYCONFIG_PIXELFORMAT_24BPP,
    DISPLAYCONFIG_PIXELFORMAT_32BPP, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LPARAM, POINT, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, EnumDisplayMonitors,
    EnumDisplaySettingsExW, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
};
use windows_sys::core::BOOL;
use winit_core::monitor::{MonitorHandleProvider, VideoMode};

use super::util::decode_wide;
use crate::dpi::{dpi_to_scale_factor, get_monitor_dpi};
use crate::util::has_flag;

#[derive(Clone)]
pub struct VideoModeHandle {
    pub(crate) mode: VideoMode,
    // DEVMODEW is huge so we box it to avoid blowing up the size of winit::window::Fullscreen
    pub(crate) native_video_mode: Box<DEVMODEW>,
}

impl PartialEq for VideoModeHandle {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
    }
}

impl Eq for VideoModeHandle {}

impl std::hash::Hash for VideoModeHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.mode.hash(state);
    }
}

impl std::fmt::Debug for VideoModeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoMode").field("mode", &self.mode).finish()
    }
}

impl VideoModeHandle {
    fn new(native_video_mode: DEVMODEW) -> Self {
        const REQUIRED_FIELDS: u32 =
            DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        assert!(has_flag(native_video_mode.dmFields, REQUIRED_FIELDS));

        let mode = VideoMode::new(
            (native_video_mode.dmPelsWidth, native_video_mode.dmPelsHeight).into(),
            NonZeroU16::new(native_video_mode.dmBitsPerPel as u16),
            NonZeroU32::new(native_video_mode.dmDisplayFrequency * 1000),
        );

        VideoModeHandle { mode, native_video_mode: Box::new(native_video_mode) }
    }
}

thread_local! {
    // Scratch buffers for `query_current_video_mode`, kept around to avoid allocating on every
    // query.
    static DISPLAY_CONFIG_BUFFERS: RefCell<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> =
        const { RefCell::new((Vec::new(), Vec::new())) };
}

/// Query the video mode that the display with the given GDI device name (e.g. `\\.\DISPLAY1`,
/// see `szDevice` in [`MONITORINFOEXW`]) is currently running in.
///
/// This uses `QueryDisplayConfig`, which reports the refresh rate as a rational number, unlike
/// `EnumDisplaySettingsExW`, which only knows whole hertz (so 59.94 Hz shows up as 60 Hz).
///
/// Returns `None` if the display isn't part of an active path (e.g. because it was disconnected
/// in the meantime), or the system doesn't report a source mode for it.
fn query_current_video_mode(gdi_device_name: &[u16]) -> Option<VideoMode> {
    // `modeInfoIdx` value for a path that has no mode information.
    const MODE_IDX_INVALID: u32 = 0xffff_ffff;

    DISPLAY_CONFIG_BUFFERS.with_borrow_mut(|(paths, modes)| {
        // The configuration can change between getting the buffer sizes and querying it, in which
        // case `QueryDisplayConfig` fails with `ERROR_INSUFFICIENT_BUFFER` and we have to retry.
        loop {
            let (mut num_paths, mut num_modes) = (0u32, 0u32);
            let status = unsafe {
                GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
            };
            if status != ERROR_SUCCESS {
                tracing::warn!("Error from GetDisplayConfigBufferSizes: {status}");
                return None;
            }

            paths.resize(num_paths as usize, DISPLAYCONFIG_PATH_INFO::default());
            modes.resize(num_modes as usize, DISPLAYCONFIG_MODE_INFO::default());

            let status = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut num_paths,
                    paths.as_mut_ptr(),
                    &mut num_modes,
                    modes.as_mut_ptr(),
                    ptr::null_mut(),
                )
            };
            match status {
                ERROR_SUCCESS => {
                    paths.truncate(num_paths as usize);
                    modes.truncate(num_modes as usize);
                    break;
                },
                ERROR_INSUFFICIENT_BUFFER => continue,
                status => {
                    tracing::warn!("Error from QueryDisplayConfig: {status}");
                    return None;
                },
            }
        }

        let wanted_name = gdi_device_name.split(|&c| c == 0).next().unwrap_or_default();
        let path = paths.iter().find(|path| {
            // Find the path whose source corresponds to the GDI device (i.e. the monitor).
            let mut source_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source_name.header.size = mem::size_of_val(&source_name) as u32;
            source_name.header.adapterId = path.sourceInfo.adapterId;
            source_name.header.id = path.sourceInfo.id;
            // This returns a plain `i32` rather than a `WIN32_ERROR`, but 0 is still success.
            if unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header) } != 0 {
                return false;
            }
            let name = source_name.viewGdiDeviceName.split(|&c| c == 0).next();
            name.unwrap_or_default() == wanted_name
        })?;

        // The source mode holds the size and pixel format of the desktop on this display. We don't
        // pass `QDC_VIRTUAL_MODE_AWARE`, so `modeInfoIdx` is a plain index into `modes`.
        let mode_idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let mode_info =
            modes.get(usize::try_from(mode_idx).ok()?).filter(|_| mode_idx != MODE_IDX_INVALID)?;
        if mode_info.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        let source_mode = unsafe { mode_info.Anonymous.sourceMode };

        let bit_depth = match source_mode.pixelFormat {
            DISPLAYCONFIG_PIXELFORMAT_8BPP => NonZeroU16::new(8),
            DISPLAYCONFIG_PIXELFORMAT_16BPP => NonZeroU16::new(16),
            DISPLAYCONFIG_PIXELFORMAT_24BPP => NonZeroU16::new(24),
            DISPLAYCONFIG_PIXELFORMAT_32BPP => NonZeroU16::new(32),
            _ => None,
        };

        let rate = path.targetInfo.refreshRate;
        let refresh_rate_millihertz = (rate.Denominator != 0)
            .then(|| {
                // Round to the nearest millihertz.
                let millihertz = (u64::from(rate.Numerator) * 1000
                    + u64::from(rate.Denominator) / 2)
                    / u64::from(rate.Denominator);
                NonZeroU32::new(u32::try_from(millihertz).ok()?)
            })
            .flatten();

        Some(VideoMode::new(
            PhysicalSize::new(source_mode.width, source_mode.height),
            bit_depth,
            refresh_rate_millihertz,
        ))
    })
}

/// Whether `mode`, as enumerated by `EnumDisplaySettingsExW` (and thus with a rounded refresh
/// rate), describes the same mode as `current`, which has the exact refresh rate.
fn is_current_mode(mode: &VideoMode, current: &VideoMode) -> bool {
    // Be lenient about the rounding (up, down or to nearest) that the driver applied.
    let same_refresh_rate =
        match (mode.refresh_rate_millihertz(), current.refresh_rate_millihertz()) {
            (Some(mode), Some(current)) => mode.get().abs_diff(current.get()) < 1000,
            (mode, current) => mode == current,
        };
    mode.size() == current.size()
        && (current.bit_depth().is_none() || mode.bit_depth() == current.bit_depth())
        && same_refresh_rate
}

unsafe extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _place: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = data as *mut VecDeque<MonitorHandle>;
    unsafe { (*monitors).push_back(MonitorHandle::new(hmonitor)) };
    true.into() // continue enumeration
}

pub fn available_monitors() -> VecDeque<MonitorHandle> {
    let mut monitors: VecDeque<MonitorHandle> = VecDeque::new();
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(monitor_enum_proc),
            &mut monitors as *mut _ as LPARAM,
        );
    }
    monitors
}

pub fn primary_monitor() -> MonitorHandle {
    const ORIGIN: POINT = POINT { x: 0, y: 0 };
    let hmonitor = unsafe { MonitorFromPoint(ORIGIN, MONITOR_DEFAULTTOPRIMARY) };
    MonitorHandle::new(hmonitor)
}

pub fn current_monitor(hwnd: HWND) -> MonitorHandle {
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    MonitorHandle::new(hmonitor)
}

pub(crate) fn get_monitor_info(hmonitor: HMONITOR) -> Result<MONITORINFOEXW, io::Error> {
    let mut monitor_info: MONITORINFOEXW = unsafe { mem::zeroed() };
    monitor_info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
    let status = unsafe {
        GetMonitorInfoW(hmonitor, &mut monitor_info as *mut MONITORINFOEXW as *mut MONITORINFO)
    };
    if status == false.into() { Err(io::Error::last_os_error()) } else { Ok(monitor_info) }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct MonitorHandle(HMONITOR);

// Send and Sync are not implemented for HMONITOR, we have to wrap it and implement them manually.

unsafe impl Send for MonitorHandle {}
unsafe impl Sync for MonitorHandle {}

impl MonitorHandle {
    pub(crate) fn new(hmonitor: HMONITOR) -> Self {
        MonitorHandle(hmonitor)
    }

    pub(crate) fn size(&self) -> PhysicalSize<u32> {
        let rc_monitor = get_monitor_info(self.0).unwrap().monitorInfo.rcMonitor;
        PhysicalSize {
            width: (rc_monitor.right - rc_monitor.left) as u32,
            height: (rc_monitor.bottom - rc_monitor.top) as u32,
        }
    }

    /// The monitor's work area, i.e. its bounds minus space reserved by the system for things
    /// like the taskbar (see `rcWork` in [`MONITORINFO`]).
    pub(crate) fn work_area(&self) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>)> {
        let rc_work = get_monitor_info(self.0).ok()?.monitorInfo.rcWork;
        Some((PhysicalPosition { x: rc_work.left, y: rc_work.top }, PhysicalSize {
            width: (rc_work.right - rc_work.left) as u32,
            height: (rc_work.bottom - rc_work.top) as u32,
        }))
    }

    pub(crate) fn video_mode_handles(&self) -> Box<dyn Iterator<Item = VideoModeHandle>> {
        // EnumDisplaySettingsExW can return duplicate values (or some of the
        // fields are probably changing, but we aren't looking at those fields
        // anyway), so we're using a BTreeSet deduplicate
        let mut modes = HashSet::<VideoModeHandle>::new();

        let monitor_info = match get_monitor_info(self.0) {
            Ok(monitor_info) => monitor_info,
            Err(error) => {
                tracing::warn!("Error from get_monitor_info: {error}");
                return Box::new(iter::empty());
            },
        };

        let device_name = monitor_info.szDevice.as_ptr();

        // `EnumDisplaySettingsExW` only knows whole hertz. Use the exact refresh rate for the
        // entry that describes the mode that's currently active.
        let current = query_current_video_mode(&monitor_info.szDevice);

        let mut i = 0;
        loop {
            let mut mode: DEVMODEW = unsafe { mem::zeroed() };
            mode.dmSize = mem::size_of_val(&mode) as u16;
            if unsafe { EnumDisplaySettingsExW(device_name, i, &mut mode, 0) } == false.into() {
                break;
            }

            let mut handle = VideoModeHandle::new(mode);
            if let Some(current) = current.filter(|current| is_current_mode(&handle.mode, current))
            {
                handle.mode = current;
            }

            // Use Ord impl of RootVideoModeHandle
            modes.insert(handle);

            i += 1;
        }

        Box::new(modes.into_iter())
    }
}

impl MonitorHandleProvider for MonitorHandle {
    fn id(&self) -> u128 {
        self.native_id() as _
    }

    fn native_id(&self) -> u64 {
        self.0 as _
    }

    fn name(&self) -> Option<std::borrow::Cow<'_, str>> {
        let monitor_info = get_monitor_info(self.0).unwrap();
        Some(decode_wide(&monitor_info.szDevice).to_string_lossy().to_string().into())
    }

    fn position(&self) -> Option<PhysicalPosition<i32>> {
        get_monitor_info(self.0)
            .map(|info| {
                let rc_monitor = info.monitorInfo.rcMonitor;
                PhysicalPosition { x: rc_monitor.left, y: rc_monitor.top }
            })
            .ok()
    }

    fn scale_factor(&self) -> f64 {
        dpi_to_scale_factor(get_monitor_dpi(self.0).unwrap_or(96))
    }

    fn current_video_mode(&self) -> Option<winit_core::monitor::VideoMode> {
        let monitor_info = get_monitor_info(self.0).ok()?;
        query_current_video_mode(&monitor_info.szDevice)
    }

    fn video_modes(&self) -> Box<dyn Iterator<Item = VideoMode>> {
        Box::new(self.video_mode_handles().map(|mode| mode.mode))
    }
}
