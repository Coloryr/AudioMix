//! WASAPI 采集（含 loopback）：事件驱动的共享模式捕获线程。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use audiomix_core::backend::{CaptureCallback, StartedStream, StreamHandle};
use audiomix_core::error::{Error, Result};

use super::device::{com_init, mix_format_of_by_id};

/// 共享模式缓冲时长（与 render.rs 一致：50ms，直接决定端到端延迟）
const BUFFER_DURATION_HNS: i64 = 500_000;
const IEEE_FLOAT_SUBFORMAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
// mmreg.h 格式标签
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 在指定设备上启动采集流；`loopback=true` 时捕获该**输出**设备正在播放的声音。
/// 调用线程先查一次设备格式（供引擎配置重采样），流线程内重新打开设备。
pub fn start_capture(
    device_id: &str,
    loopback: bool,
    on_data: CaptureCallback,
) -> Result<StartedStream> {
    // 在调用线程查询格式（供引擎配置重采样），流线程内重新打开设备
    let info = mix_format_of_by_id(device_id)?;
    let device_id = HSTRING::from(device_id);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = StreamHandle::spawn(
        stop.clone(),
        std::thread::Builder::new().name("audiomix-capture".into()),
        move |stop| {
            if let Err(e) = capture_thread(&device_id, loopback, stop, on_data) {
                tracing::error!("采集线程退出: {e}");
            }
        },
    )?;
    Ok(StartedStream { info, handle })
}

fn capture_thread(
    device_id: &HSTRING,
    loopback: bool,
    stop: Arc<AtomicBool>,
    mut on_data: CaptureCallback,
) -> Result<()> {
    com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(Error::backend)?;
        let dev = enumerator
            .GetDevice(PCWSTR(device_id.as_ptr()))
            .map_err(Error::backend)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(Error::backend)?;

        let fmt = client.GetMixFormat().map_err(Error::backend)?;
        let (channels, _rate) = check_float_format(fmt)?;
        let channels = channels as usize;

        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION_HNS,
                0,
                fmt,
                None,
            )
            .map_err(Error::backend)?;
        CoTaskMemFree(Some(fmt as *const _));

        let event = CreateEventW(None, false, false, None).map_err(Error::backend)?;
        client.SetEventHandle(event).map_err(Error::backend)?;
        let capture: IAudioCaptureClient = client.GetService().map_err(Error::backend)?;
        client.Start().map_err(Error::backend)?;

        let mut silent: Vec<f32> = Vec::new();
        // 采集回调线程同样注册 MMCSS：被抢占会丢数据（录进来的音频会缺口）
        let _pro_audio = super::ProAudio::join();
        let mut gap = super::CallbackGap::new(std::time::Duration::from_millis(50));
        while !stop.load(Ordering::SeqCst) {
            let w = WaitForSingleObject(event, 200);
            if w == WAIT_OBJECT_0 {
                gap.tick("采集");
                loop {
                    let Ok(packets) = capture.GetNextPacketSize() else {
                        break;
                    };
                    if packets == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    if capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .is_err()
                    {
                        break;
                    }
                    let samples = frames as usize * channels;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        silent.clear();
                        silent.resize(samples, 0.0);
                        on_data(&silent);
                    } else if !data.is_null() {
                        let slice = std::slice::from_raw_parts(data as *const f32, samples);
                        on_data(slice);
                    }
                    let _ = capture.ReleaseBuffer(frames);
                }
            }
        }

        let _ = client.Stop();
        let _ = windows::Win32::Foundation::CloseHandle(event);
        Ok(())
    }
}

/// 校验共享引擎格式为 32-bit float，返回 (channels, sample_rate)
/// WAVEFORMATEX 是 packed 结构，字段一律 read_unaligned。
pub(crate) unsafe fn check_float_format(fmt: *const WAVEFORMATEX) -> Result<(u16, u32)> {
    let tag = std::ptr::addr_of!((*fmt).wFormatTag).read_unaligned();
    let bits = std::ptr::addr_of!((*fmt).wBitsPerSample).read_unaligned();
    let channels = std::ptr::addr_of!((*fmt).nChannels).read_unaligned();
    let rate = std::ptr::addr_of!((*fmt).nSamplesPerSec).read_unaligned();
    let ok_tag = tag == WAVE_FORMAT_IEEE_FLOAT || tag == WAVE_FORMAT_EXTENSIBLE;
    if !ok_tag || bits != 32 {
        return Err(Error::Backend(format!(
            "不支持的设备格式: tag={tag} bits={bits}"
        )));
    }
    if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = fmt as *const WAVEFORMATEXTENSIBLE;
        let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
        if sub != IEEE_FLOAT_SUBFORMAT {
            return Err(Error::Backend("EXTENSIBLE 子格式不是 IEEE float".into()));
        }
    }
    Ok((channels, rate))
}
