use bytes::{BufMut, BytesMut};
use log::{debug, error};
use opencv::core::{CV_8UC4, Mat};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::thread::sleep;
use std::time::Duration;
// use image::{DynamicImage, ImageBuffer, Rgb, RgbImage, Rgba};
use crate::capture_settings::CapturePos;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
};

pub struct GrabItem {
    buffer: Mutex<BytesMut>,
    left: AtomicU32,
    top: AtomicU32,
    right: AtomicU32,
    bottom: AtomicU32,
    // 需要进行截屏操作
    should_capture: AtomicBool,
    capture_finished: AtomicBool,
    // 需要停止这个session
    should_stop: AtomicBool,
    stop_succeeded: AtomicBool,
}
struct CaptureHandler {
    grab_item: Arc<GrabItem>,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Arc<GrabItem>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            grab_item: ctx.flags,
        })
    }

    // 當新幀到達時觸發
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // 有停止请求发送过来，停止
        if self.grab_item.should_stop.load(Ordering::Acquire) {
            eprintln!("stop capture");
            #[cfg(test)]
            {
                use windows_capture::frame::ImageFormat;
                let mut buffer = frame.buffer()?;
                let mut file_name = (self.grab_item.as_ref() as *const _ as usize).to_string();
                file_name.push_str(".png");
                buffer.save_as_image(file_name, ImageFormat::Png)?;
            }
            _capture_control.stop(); // 正確停止捕獲循環 [3, 4]
            return Ok(());
        }
        // 没有截图请求发送过来，空跑
        if !self.grab_item.should_capture.load(Ordering::Acquire) {
            return Ok(());
        }
        // 开始截图
        // 獲取原始像素數據 (Bgra8 格式)
        debug!("frame arrived");
        let grab_item = self.grab_item.clone();
        let left = grab_item.left.load(Ordering::Acquire);
        let top = grab_item.top.load(Ordering::Acquire);
        let right = grab_item.right.load(Ordering::Acquire);
        let bottom = grab_item.bottom.load(Ordering::Acquire);

        let mut frame_buffer = Frame::buffer_crop(frame, left, top, right, bottom)?;
        debug!(
            "frame cropped, left: {}, top: {}, right: {}, bottom: {}",
            left, top, right, bottom
        );

        // 這裡直接進行高效拷貝。Rust 編譯器會自動對此循環進行 SIMD (AVX2) 優化
        let data = frame_buffer.as_nopadding_buffer()?;
        debug!("frame as_no_padding_buffer");
        {
            match grab_item.buffer.lock() {
                Ok(mut buffer) => {
                    debug!(
                        "frame start copy to buffer, buffer len:{}, data len: {}",
                        buffer.len(),
                        data.len()
                    );
                    buffer.clear();
                    // 避免拷贝
                    buffer.put_slice(data);
                    debug!(
                        "copy frame to buffer, buffer len:{}, data len: {}",
                        buffer.len(),
                        data.len()
                    );
                }
                Err(e) => eprintln!("grab failed, get buffer lock error: {:?}", e),
            }
        }
        #[cfg(test)]
        println!("frame copied");
        grab_item.capture_finished.store(true, Ordering::Release);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session closed");
        self.grab_item.stop_succeeded.store(true, Ordering::Release);
        Ok(())
    }
}

pub struct DxgiCaptureRs {
    handler_arc: Arc<GrabItem>,
}
impl DxgiCaptureRs {
    fn init(hwnd: HWND) -> Arc<GrabItem> {
        let _ = unsafe { SetProcessDPIAware() };
        let window = Window::from_raw_hwnd(hwnd.0);
        let handler = GrabItem {
            buffer: Mutex::new(BytesMut::new()),
            left: AtomicU32::new(0),
            top: AtomicU32::new(0),
            right: AtomicU32::new(100),
            bottom: AtomicU32::new(100),
            should_capture: AtomicBool::new(false),
            capture_finished: AtomicBool::new(true),
            should_stop: AtomicBool::new(false),
            stop_succeeded: AtomicBool::new(true),
        };
        let handler_arc = Arc::new(handler);
        // 2. 設定 (對應你設置 IsCursorCaptureEnabled 和 IsBorderRequired)
        let settings = Settings::new(
            window,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8, // 與你 C++ 代碼一致
            handler_arc.clone(),
        );

        // 開一個新線程跑捕獲，不要阻塞主線程
        thread::spawn(move || {
            if let Err(e) = CaptureHandler::start(settings) {
                eprintln!("Capture session failed to start: {:?}", e);
            }
        });
        eprintln!(
            "capture session started, handler: {}",
            handler_arc.as_ref() as *const _ as usize
        );
        handler_arc
    }
    pub fn new(hwnd: HWND) -> Result<Self, windows::core::Error> {
        let handler_arc = Self::init(hwnd);
        Ok(Self { handler_arc })
    }

    fn grab_internal(&self, left: u32, top: u32, right: u32, bottom: u32) -> bool {
        let handler_arc = self.handler_arc.clone();
        handler_arc.left.store(left, Ordering::Release);
        handler_arc.top.store(top, Ordering::Release);
        handler_arc.right.store(right, Ordering::Release);
        handler_arc.bottom.store(bottom, Ordering::Release);
        // 確保在設置 should_capture 之前，capture_finished 是 false
        handler_arc.capture_finished.store(false, Ordering::Release);
        handler_arc.should_capture.store(true, Ordering::Release);
        while !handler_arc.capture_finished.load(Ordering::Acquire) {
            thread::yield_now();
        }
        // 这里需要保证是顺序调用的，否则会读取到错误数据
        handler_arc.should_capture.store(false, Ordering::Release);
        true
    }

    pub fn grab(&self, capture_pos: &CapturePos) -> Mat {
        let (left, top, width, height) = capture_pos.rect;
        let right = width + left;
        let bottom = height + top;
        self.grab_internal(left as u32, top as u32, right as u32, bottom as u32);
        match self.handler_arc.buffer.lock() {
            Ok(mut buffer) => unsafe {
                Mat::new_rows_cols_with_data_unsafe_def(
                    height,
                    width,
                    CV_8UC4,
                    buffer.as_mut_ptr() as *mut _,
                )
                .unwrap_or_else(|_e| {
                    error!("grab failed, create mat from buffer error: {:?}", _e);
                    Mat::default()
                })
            },
            Err(e) => {
                error!("grab failed, get buffer lock error: {:?}", e);
                Mat::default()
            }
        }
    }

    pub fn stop(&self) {
        let handler_arc = self.handler_arc.clone();
        handler_arc.should_stop.store(true, Ordering::Release);
        while !handler_arc.stop_succeeded.load(Ordering::Acquire) {
            sleep(Duration::from_millis(1));
        }
    }

    pub fn update_hwnd(&mut self, hwnd: HWND) {
        self.stop();
        let handler_arc = Self::init(hwnd);
        self.handler_arc = handler_arc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 模擬 grab() 函數調用
    fn grab_monitor(handler_arc: Arc<GrabItem>) {
        let mut z = 0;
        loop {
            sleep(Duration::from_millis(16));
            let buffer = &handler_arc.buffer;
            let data = buffer.lock().unwrap();
            if !data.is_empty() {
                println!("Captured frame size: {} bytes", data.len());
                // 這裡可以根據 region (left, top, width, height) 進行切片
            }
            handler_arc.left.store(z, Ordering::Relaxed);
            handler_arc.top.store(z, Ordering::Relaxed);
            handler_arc.right.store(z + 100, Ordering::Relaxed);
            handler_arc.bottom.store(z + 100, Ordering::Relaxed);
            println!("z: {}", z);
            z += 10;
            if z % 50 == 0 {
                handler_arc.should_capture.store(true, Ordering::Relaxed);
            }
            if z > 500 {
                handler_arc.should_stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    #[test]
    pub fn test_grab() {
        // 1. 查找窗口 (根據標題，替代 HWnd 手動查找)
        let window_qq = Window::enumerate()
            .expect("Failed to enumerate windows")
            .into_iter()
            .find(|w| w.title().expect("REASON").contains("PHANTASY"))
            .expect("Window not found");

        let handler_arc_qq = DxgiCaptureRs::new(HWND(window_qq.as_raw_hwnd())).unwrap();

        let window_phantom = Window::enumerate()
            .expect("Failed to enumerate windows")
            .into_iter()
            .find(|w| w.title().expect("REASON").contains("PHAN"))
            .expect("Window not found");
        let handler_arc_phantom = DxgiCaptureRs::new(HWND(window_phantom.as_raw_hwnd())).unwrap();
        sleep(Duration::from_secs(5));

        thread::spawn(|| grab_monitor(handler_arc_phantom.handler_arc));
        grab_monitor(handler_arc_qq.handler_arc);

        sleep(Duration::from_secs(3));
    }
}
