use crate::capture_settings::CapturePos;
use crate::dxgi_capture_rs::DxgiCaptureRs;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgb};
use ndarray::{Array3, Array4, Axis};
use ort::ep::{CUDA, CUDAExecutionProvider, DirectML, DirectMLExecutionProvider};
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{Session, WorkloadType};
use ort::value::Tensor;
use show_image::{ImageInfo, ImageView, create_window, WindowOptions};
use std::error::Error;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW};
use windows::core::BOOL;
struct WindowFinder {
    title: String,
    found_hwnd: Option<HWND>,
}

extern "system" fn window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // Here you would implement the logic to check the window title
    // and compare it with the desired title passed via lparam.
    // If a match is found, you can store the HWND in a location
    // accessible via lparam.
    let window_finder = unsafe { &mut *(lparam.0 as *mut WindowFinder) };
    let mut buffer: [u16; 256] = [0; 256];
    unsafe {
        GetWindowTextW(hwnd, &mut buffer);
    }
    let window_title = String::from_utf16_lossy(&buffer);
    if window_title.contains(&window_finder.title) {
        window_finder.found_hwnd = Some(hwnd);
        // 已经找到窗口, 返回false终止枚举
        BOOL(0)
    } else {
        // 未找到, 返回true继续枚举
        BOOL(1)
    }
}
/// 遍历所有窗口，查找匹配的窗口名，返回HWND
pub fn search_window_by_title(window_name: &str) -> Option<HWND> {
    let mut finder = WindowFinder {
        title: window_name.to_string(),
        found_hwnd: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(window_callback),
            LPARAM(&mut finder as *mut _ as isize),
        );
    }
    finder.found_hwnd
}

pub fn to_animation_image(
    img: &DynamicImage,
    session: &mut Session,
) -> Result<DynamicImage, Box<dyn Error>> {
    let (orig_width, orig_height) = img.dimensions();
    let target_width = (orig_width / 32) * 32;
    let target_height = (orig_height / 32) * 32;

    // 快速轉換：直接獲取底層 Vec<u8> 並轉為 Array3
    let img_rgb = img
        .resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();
    let raw_data = img_rgb.as_raw();

    // 向量化預處理：直接將 [u8] 轉成 [H, W, 3] 的 Array3，然後一次性運算
    let input_array = Array3::from_shape_vec(
        (target_height as usize, target_width as usize, 3),
        raw_data.to_vec(),
    )?
    .mapv(|x| (x as f32 / 127.5) - 1.0)
    .insert_axis(Axis(0)); // 變為 [1, H, W, 3]

    // 推理
    let input_tensor = Tensor::from_array(input_array)?;
    let outputs = session.run(inputs![input_tensor])?;
    let output_tensor = outputs[0].try_extract_array::<f32>()?;

    // 向量化後處理：一次性完成逆運算並轉回 u8
    let binding = output_tensor.view();
    let hwc_view = binding.index_axis(Axis(0), 0);
    let out_raw: Vec<u8> = hwc_view
        .mapv(|x| ((x + 1.0) * 127.5).clamp(0.0, 255.0) as u8)
        .into_raw_vec();

    let out_img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(target_width, target_height, out_raw)
        .ok_or("Buffer error")?;

    Ok(DynamicImage::ImageRgb8(out_img).resize_exact(
        orig_width,
        orig_height,
        image::imageops::FilterType::Lanczos3,
    ))
}

fn right_get_frame(capture: &DxgiCaptureRs, pos: &CapturePos) -> DynamicImage {
    capture.grab(pos).unwrap()
}
pub fn animation_test() -> Result<(), Box<dyn std::error::Error>> {
    match search_window_by_title("PHANTASY STAR") {
        Some(hwnd) => {
            let mut session = Session::builder()?
                // 啟用 DirectML (Windows 最佳實踐，支援大部分顯卡)
                .with_execution_providers([CUDA::default().build(), DirectML::default().build()])?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                .commit_from_file("AnimeGANv3_Hayao_36.onnx")?;
            session.set_workload_type(WorkloadType::Efficient)?;
            let pos = CapturePos::full_window();
            let capture = DxgiCaptureRs::new(hwnd)?;
            // 建立顯示視窗
            let (window_width,window_height) = (1602, 980);
            let options = WindowOptions {
                size: Some([window_width, window_height]), // 初始視窗大小
                resizable: true,   // 允許用戶手動調整大小
                ..Default::default()
            };
            let window = create_window("Real-time AnimeGAN", options)?;
            println!("按 'q' 鍵退出...");
            loop {
                let dynamic_img = right_get_frame(&capture, &pos);
                let display_img = to_animation_image(&dynamic_img, &mut session)?;
                // let display_img = dynamic_img.resize(window_width, window_height, image::imageops::FilterType::Nearest);
                let (w, h) = display_img.dimensions();
                let image_view = ImageView::new(
                    ImageInfo::rgb8(w, h),
                    display_img.as_bytes(), // 這裡動態獲取 RGB 字節流
                );
                // 3. 更新視窗內容
                // 使用固定的名稱（如 "frame"）可以確保是在同一個畫布上刷新
                window.set_image("live-result", image_view)?;
            }
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::image_animation::animation_test;

    #[test]
    fn test_animation_test() {
        animation_test().unwrap();
    }
}
