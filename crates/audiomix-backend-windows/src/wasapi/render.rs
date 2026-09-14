//! WASAPI 渲染：事件驱动，按需回调引擎填充缓冲。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::HSTRING;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use audiomix_core::backend::{RenderCallback, StartedStream, StreamHandle};
use audiomix_core::error::{Error, Result};

use super::device::{com_init, mix_format_of_by_id};
use super::capture::check_float_format;

/// 共享模式缓冲时长。原来 200ms；实测这一项直接叠加到端到端延迟上，
/// 50ms 足够（事件回调 + MMCSS 下不会欠载），同时把延迟砍掉约 150ms。
const BUFFER_DURATION_HNS: i64 = 500_000;

/// 在指定输出设备上启动事件驱动的渲染流。
/// 调用线程先查一次设备格式（供引擎配置重采样），流线程内重新打开设备。
pub fn start_render(device_id: &str, on_fill: RenderCallback) -> Result<StartedStream> {
    let info = mix_format_of_by_id(device_id)?;
    let device_id = HSTRING::from(device_id);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = StreamHandle::spawn(
        stop.clone(),
        std::thread::Builder::new().name("audiomix-render".into()),
        move |stop| {
            if let Err(e) = render_thread(&device_id, stop, on_fill) {
                tracing::error!("渲染线程退出: {e}");
            }
        },
    )?;
    Ok(StartedStream { info, handle })
}

fn render_thread(device_id: &HSTRING, stop: Arc<AtomicBool>, mut on_fill: RenderCallback) -> Result<()> {
    com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(Error::backend)?;
        let dev = enumerator
            .GetDevice(windows::core::PCWSTR(device_id.as_ptr()))
            .map_err(Error::backend)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(Error::backend)?;

        let fmt = client.GetMixFormat().map_err(Error::backend)?;
        if fmt.is_null() {
            return Err(Error::Backend("GetMixFormat 返回空格式".into()));
        }
        let (channels, _rate) = check_float_format(fmt)?;
        let channels = channels as usize;

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_HNS,
                0,
                fmt,
                None,
            )
            .map_err(Error::backend)?;
        windows::Win32::System::Com::CoTaskMemFree(Some(fmt as *const _));

        let event = CreateEventW(None, false, false, None).map_err(Error::backend)?;
        client.SetEventHandle(event).map_err(Error::backend)?;
        let render: IAudioRenderClient = client.GetService().map_err(Error::backend)?;
        client.Start().map_err(Error::backend)?;

        let buffer_frames = client.GetBufferSize().map_err(Error::backend)? as usize;

        // 音频回调线程：注册 MMCSS 拿优先级（否则系统一忙就被抢占几百毫秒）
        let _pro_audio = super::ProAudio::join();
        let mut gap = super::CallbackGap::new(std::time::Duration::from_millis(50));

        while !stop.load(Ordering::SeqCst) {
            let w = WaitForSingleObject(event, 200);
            if w != WAIT_OBJECT_0 {
                continue; // 超时，回头检查 stop 标志
            }
            gap.tick("渲染");
            let Ok(padding) = client.GetCurrentPadding() else { continue };
            let avail = buffer_frames.saturating_sub(padding as usize);
            if avail == 0 {
                continue;
            }
            let data: *mut u8 = match render.GetBuffer(avail as u32) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !data.is_null() {
                let slice =
                    std::slice::from_raw_parts_mut(data as *mut f32, avail * channels);
                on_fill(slice);
            }
            let _ = render.ReleaseBuffer(avail as u32, 0);
        }

        let _ = client.Stop();
        let _ = windows::Win32::Foundation::CloseHandle(event);
        Ok(())
    }
}
