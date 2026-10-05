use std::collections::{HashSet, VecDeque};
use std::hash::Hash;
use std::num::{NonZeroU16, NonZeroU32};
use std::{io, iter, mem, ptr};

use dpi::{PhysicalPosition, PhysicalSize};
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LPARAM, POINT, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsExW, GetMonitorInfoW, HDC,
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW,
    MonitorFromPoint, MonitorFromWindow,
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
    /// Create a handle from a `DEVMODEW`.
    ///
    /// `DEVMODEW` only carries the refresh rate rounded to whole hertz. If a more precise rate is
    /// known (see [`query_refresh_rate_millihertz`]), pass it as `refresh_rate_millihertz` to
    /// use it for the public [`VideoMode`] instead. The `DEVMODEW` is kept as-is, since that is
    /// what must be handed back to the system when changing display settings.
    fn new(native_video_mode: DEVMODEW, refresh_rate_millihertz: Option<NonZeroU32>) -> Self {
        const REQUIRED_FIELDS: u32 =
            DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        assert!(has_flag(native_video_mode.dmFields, REQUIRED_FIELDS));

        let mode = VideoMode::new(
            (native_video_mode.dmPelsWidth, native_video_mode.dmPelsHeight).into(),
            NonZeroU16::new(native_video_mode.dmBitsPerPel as u16),
            refresh_rate_millihertz
                .or_else(|| NonZeroU32::new(native_video_mode.dmDisplayFrequency * 1000)),
        );

        VideoModeHandle { mode, native_video_mode: Box::new(native_video_mode) }
    }
}

/// Query the exact refresh rate, in millihertz, that the display with the given GDI device name
/// (e.g. `\\.\DISPLAY1`, see `szDevice` in [`MONITORINFOEXW`]) is currently running at.
///
/// This uses `QueryDisplayConfig`, which reports the refresh rate as a rational number, unlike
/// `EnumDisplaySettingsExW`, which rounds it to whole hertz (so 59.94 Hz shows up as 60 Hz).
///
/// Returns `None` if the display isn't part of an active path, or no rate is reported.
fn query_refresh_rate_millihertz(gdi_device_name: &[u16]) -> Option<NonZeroU32> {
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::new();

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

        paths.clear();
        paths.resize_with(num_paths as usize, || unsafe { mem::zeroed() });
        modes.clear();
        modes.resize_with(num_modes as usize, || unsafe { mem::zeroed() });

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
    paths.iter().find_map(|path| {
        // Find the path whose source corresponds to the GDI device (i.e. the monitor).
        let mut source_name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { mem::zeroed() };
        source_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source_name.header.size = mem::size_of_val(&source_name) as u32;
        source_name.header.adapterId = path.sourceInfo.adapterId;
        source_name.header.id = path.sourceInfo.id;
        // This function returns a (non-`WIN32_ERROR`) `LONG`, where 0 is `ERROR_SUCCESS`.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header) } != 0 {
            return None;
        }
        let name = source_name.viewGdiDeviceName.split(|&c| c == 0).next().unwrap_or_default();
        if name != wanted_name {
            return None;
        }

        let rate = path.targetInfo.refreshRate;
        if rate.Denominator == 0 {
            return None;
        }
        // Round to the nearest millihertz.
        let millihertz = (u64::from(rate.Numerator) * 1000 + u64::from(rate.Denominator) / 2)
            / u64::from(rate.Denominator);
        NonZeroU32::new(u32::try_from(millihertz).ok()?)
    })
}

/// Whether two `DEVMODEW`s describe the same size, bit depth and (rounded) refresh rate.
fn is_same_mode(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    a.dmPelsWidth == b.dmPelsWidth
        && a.dmPelsHeight == b.dmPelsHeight
        && a.dmBitsPerPel == b.dmBitsPerPel
        && a.dmDisplayFrequency == b.dmDisplayFrequency
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

        // `EnumDisplaySettingsExW` only knows whole hertz. Get the exact rate of the mode that's
        // currently active, and use that for the entry describing that mode.
        let current = unsafe {
            let mut mode: DEVMODEW = mem::zeroed();
            mode.dmSize = mem::size_of_val(&mode) as u16;
            (EnumDisplaySettingsExW(device_name, ENUM_CURRENT_SETTINGS, &mut mode, 0)
                != false.into())
            .then_some(mode)
        };
        let current_refresh_rate = current
            .is_some()
            .then(|| query_refresh_rate_millihertz(&monitor_info.szDevice))
            .flatten();

        let mut i = 0;
        loop {
            let mut mode: DEVMODEW = unsafe { mem::zeroed() };
            mode.dmSize = mem::size_of_val(&mode) as u16;
            if unsafe { EnumDisplaySettingsExW(device_name, i, &mut mode, 0) } == false.into() {
                break;
            }

            let refresh_rate = current
                .as_ref()
                .filter(|current| is_same_mode(current, &mode))
                .and(current_refresh_rate);

            // Use Ord impl of RootVideoModeHandle
            modes.insert(VideoModeHandle::new(mode, refresh_rate));

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
        let device_name = monitor_info.szDevice.as_ptr();
        let mode = unsafe {
            let mut mode: DEVMODEW = mem::zeroed();
            mode.dmSize = mem::size_of_val(&mode) as u16;
            if EnumDisplaySettingsExW(device_name, ENUM_CURRENT_SETTINGS, &mut mode, 0)
                == false.into()
            {
                return None;
            }
            mode
        };
        let refresh_rate = query_refresh_rate_millihertz(&monitor_info.szDevice);
        Some(VideoModeHandle::new(mode, refresh_rate).mode)
    }

    fn video_modes(&self) -> Box<dyn Iterator<Item = VideoMode>> {
        Box::new(self.video_mode_handles().map(|mode| mode.mode))
    }
}
