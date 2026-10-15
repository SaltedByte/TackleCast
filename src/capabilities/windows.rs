use super::CaptureMode;
use crate::frame_rate::FrameRate;
use crate::settings::VideoFormat;
use std::io::Read;
use std::mem::{size_of, ManuallyDrop};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, HSTRING, VARIANT};
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IBaseFilter, ICreateDevEnum, PINDIR_OUTPUT, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory, AM_MEDIA_TYPE, VIDEOINFOHEADER,
    VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CreateBindCtx, IMoniker,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::Debug::{
    SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
};

struct ComApartment(bool);
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
struct MediaType(*mut AM_MEDIA_TYPE);
impl Drop for MediaType {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                let mt = &mut *self.0;
                CoTaskMemFree(Some(mt.pbFormat.cast()));
                ManuallyDrop::drop(&mut mt.pUnk);
                CoTaskMemFree(Some(self.0.cast()));
            }
        }
    }
}

pub fn moniker_id(moniker: &IMoniker) -> Option<String> {
    unsafe {
        let context = CreateBindCtx(0).ok()?;
        let value = moniker.GetDisplayName(&context, None).ok()?;
        let name = value.to_string().ok();
        CoTaskMemFree(Some(value.0.cast()));
        name.map(|name| name.replace(':', "_"))
    }
}

/// Vendor drivers execute inside the probe process. A hung or crashing driver
/// must not take down the viewer, its audio, or an already active capture.
pub fn query(device: &str) -> Result<Vec<CaptureMode>, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(executable)
        .arg("--probe-capture-modes")
        .arg(device)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start device mode query: {e}"))?;
    let output = child
        .stdout
        .take()
        .ok_or("Mode query output is unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(8 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(match result {
                    Err(error) => format!("Cannot wait for device mode query: {error}"),
                    _ => "The device mode query timed out. Check whether another app is using the device, then refresh.".into(),
                });
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Device mode query reader failed")?
        .map_err(|e| e.to_string())?;
    if !status?.success() {
        return Err("The device driver failed during mode discovery. Check whether another app is using the device, then refresh.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid device mode query result: {e}"))?
}

pub fn handle_probe_args() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--probe-capture-modes") {
        return false;
    }
    // A vendor fault belongs in the parent's error message, not a blocking OS
    // crash dialog. This applies only to the disposable probe process.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
    let result = args
        .next()
        .ok_or_else(|| "Missing capture device".to_string())
        .and_then(|device| query_inline(&device));
    let _ = serde_json::to_writer(std::io::stdout().lock(), &result);
    true
}

fn query_inline(device: &str) -> Result<Vec<CaptureMode>, String> {
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _apartment = ComApartment(initialized.is_ok());
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
        let mut enumerator = None;
        dev_enum
            .CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut enumerator, 0)
            .map_err(|e| e.to_string())?;
        let enumerator = enumerator.ok_or("No capture devices found")?;
        let mut moniker = [None];
        while enumerator.Next(&mut moniker, None).is_ok() {
            let Some(moniker) = moniker[0].take() else {
                break;
            };
            let Ok(bag) = moniker.BindToStorage::<_, _, IPropertyBag>(None, None) else {
                continue;
            };
            let mut value = VARIANT::default();
            let name = if bag
                .Read(&HSTRING::from("FriendlyName"), &mut value, None)
                .is_ok()
            {
                value.to_string()
            } else {
                String::new()
            };
            if moniker_id(&moniker).as_deref() == Some(device) || name == device {
                return query_moniker(&moniker);
            }
        }
        Err("The selected capture device is no longer connected".into())
    }
}

const VIDEO_INFO: GUID = GUID::from_u128(0x05589f80_c356_11ce_bf01_00aa0055595a);
const VIDEO_INFO2: GUID = GUID::from_u128(0xf72a76a0_eb0a_11d0_ace4_0000c0cc16ba);

unsafe fn description(mt: &AM_MEDIA_TYPE) -> Option<(u32, u32, FrameRate)> {
    if mt.pbFormat.is_null() {
        return None;
    }
    let (width, height, interval) = if mt.formattype == VIDEO_INFO
        && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER>()
    {
        let header = &*mt.pbFormat.cast::<VIDEOINFOHEADER>();
        (
            header.bmiHeader.biWidth,
            header.bmiHeader.biHeight,
            header.AvgTimePerFrame,
        )
    } else if mt.formattype == VIDEO_INFO2 && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>()
    {
        let header = &*mt.pbFormat.cast::<VIDEOINFOHEADER2>();
        (
            header.bmiHeader.biWidth,
            header.bmiHeader.biHeight,
            header.AvgTimePerFrame,
        )
    } else {
        return None;
    };
    Some((
        u32::try_from(width).ok()?,
        height.unsigned_abs(),
        FrameRate::from_interval_100ns(interval)?,
    ))
}

unsafe fn set_interval(mt: &mut AM_MEDIA_TYPE, interval: i64) {
    if mt.formattype == VIDEO_INFO {
        (*mt.pbFormat.cast::<VIDEOINFOHEADER>()).AvgTimePerFrame = interval;
    } else if mt.formattype == VIDEO_INFO2 {
        (*mt.pbFormat.cast::<VIDEOINFOHEADER2>()).AvgTimePerFrame = interval;
    }
}

unsafe fn query_moniker(moniker: &IMoniker) -> Result<Vec<CaptureMode>, String> {
    let filter: IBaseFilter = moniker
        .BindToObject(None, None)
        .map_err(|e| e.to_string())?;
    let pins = filter.EnumPins().map_err(|e| e.to_string())?;
    let mut pin = [None];
    let mut modes = Vec::new();
    while pins.Next(&mut pin, None).is_ok() {
        let Some(pin) = pin[0].take() else { break };
        if pin.QueryDirection().ok() != Some(PINDIR_OUTPUT) {
            continue;
        }
        let Ok(config) = pin.cast::<IAMStreamConfig>() else {
            continue;
        };
        let (mut count, mut size) = (0, 0);
        if config
            .GetNumberOfCapabilities(&mut count, &mut size)
            .is_err()
            || size < size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32
            || size > 1024 * 1024
        {
            continue;
        }
        let original = config.GetFormat().ok().map(MediaType);
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        // Snapshot before SetFormat: drivers may change enumeration after a format request.
        let mut entries = Vec::new();
        for index in 0..count.min(4096) {
            let mut raw = std::ptr::null_mut();
            if config
                .GetStreamCaps(index, &mut raw, buffer.as_mut_ptr().cast())
                .is_err()
                || raw.is_null()
            {
                continue;
            }
            let media = MediaType(raw);
            let Some((width, height, rate)) = description(&*raw) else {
                continue;
            };
            let format = match (*raw).subtype.data1 {
                0x3231564e => VideoFormat::Nv12,
                0x47504a4d => VideoFormat::Mjpeg,
                0x32595559 => VideoFormat::Yuy2,
                0x59565955 => VideoFormat::Uyvy,
                0x30323449 => VideoFormat::Yuv420p,
                // YV12 has a different U/V plane order. It is not I420/yuv420p.
                _ => continue,
            };
            let caps = &*buffer.as_ptr().cast::<VIDEO_STREAM_CONFIG_CAPS>();
            entries.push((
                media,
                width,
                height,
                rate,
                format,
                caps.MinFrameInterval,
                caps.MaxFrameInterval,
            ));
        }
        for (media, width, height, advertised, format, min_interval, max_interval) in entries {
            modes.push(CaptureMode {
                width,
                height,
                rate: advertised,
                format,
            });
            if min_interval <= 0 || max_interval < min_interval {
                continue;
            }
            let mut candidates = FrameRate::common();
            candidates.extend(FrameRate::from_interval_100ns(min_interval));
            candidates.extend(FrameRate::from_interval_100ns(max_interval));
            for requested in candidates {
                let interval = requested.interval_100ns();
                if interval < min_interval
                    || interval > max_interval
                    || requested.equivalent(advertised)
                {
                    continue;
                }
                set_interval(&mut *media.0, interval);
                if config.SetFormat(media.0).is_err() {
                    continue;
                }
                let Ok(actual) = config.GetFormat().map(MediaType) else {
                    continue;
                };
                if let Some((actual_w, actual_h, rate)) = description(&*actual.0) {
                    if actual_w == width
                        && actual_h == height
                        && (*actual.0).subtype == (*media.0).subtype
                        && rate.equivalent(requested)
                    {
                        modes.push(CaptureMode {
                            width,
                            height,
                            rate,
                            format,
                        });
                    }
                }
            }
        }
        if let Some(original) = original {
            let _ = config.SetFormat(original.0);
        }
    }
    Ok(modes)
}
