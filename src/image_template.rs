// 找T标记并实时显示

use crate::capture_settings::CapturePos;
use crate::dxgi_capture_rs::DxgiCaptureRs;
use opencv::core::{min_max_loc, no_array, AlgorithmHint, Mat, MatTraitConst, Point, Rect, Scalar};
use opencv::highgui::{destroy_all_windows, imshow, named_window, wait_key, WINDOW_AUTOSIZE};
use opencv::imgcodecs::{imdecode, imread, IMREAD_GRAYSCALE};
use opencv::imgproc::{cvt_color, match_template, rectangle, COLOR_BGR2GRAY, TM_CCOEFF_NORMED};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::error::Error;
use std::ffi::c_void;
use std::sync::{mpsc, Arc};
use std::thread;
use log::error;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, SetProcessDPIAware};
struct WindowFinder {
    title: String,
    found_hwnd: Option<HWND>,
}

// 1. 定义发送给工作线程的任务数据结构
struct Task {
    seq: usize,
    dynamic_img: Mat, // 假设你的图片类型是这个
}

// 2. 定义工作线程处理完毕后返回的数据结构
struct RenderFrame {
    seq: usize,
    display_img: Mat,
}

// 为了让 BinaryHeap 实现小顶堆（按 seq 从小到大排序），需要实现 Ord
impl PartialEq for RenderFrame {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Eq for RenderFrame {}
impl PartialOrd for RenderFrame {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RenderFrame {
    fn cmp(&self, other: &Self) -> Ordering {
        other.seq.cmp(&self.seq) // 倒序：让 seq 小的排在堆顶
    }
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

pub fn draw_rect_on_image(
    // device:Arc<CudaDevice>,
    img: &Mat,
    template_gray: &Mat,
) -> Result<Mat, Box<dyn Error>> {
    // 1. 将原图转换为灰度图用于算法匹配

    let mut img_gray = Mat::default();
    cvt_color(
        img,
        &mut img_gray,
        COLOR_BGR2GRAY,
        0,
        AlgorithmHint::ALGO_HINT_DEFAULT,
    )?;

    // 2. 执行模板匹配（使用平方差匹配算法）
    let mut match_result = Mat::default();
    match_template(
        &img_gray,
        &template_gray,
        &mut match_result,
        TM_CCOEFF_NORMED,
        &no_array(),
    )?;

    // 3. 获取最佳匹配位置（最大值所在位置即为最匹配点）
    let mut max_val = 0.0;
    let mut max_loc = Point::default();
    min_max_loc(
        &match_result,
        None,
        Some(&mut max_val),
        None,
        Some(&mut max_loc),
        &no_array(),
    )?;

    // 4. 获取模板的宽高，并在原图上绘制矩形框
    let template_size = template_gray.size()?;
    let (w, h) = (template_size.width, template_size.height);

    // 克隆一份原图用来画线，避免修改输入源
    let mut output_img = img.clone();
    let top_left = max_loc;

    // 绘制红色矩形框（BGR格式中红色为 Scalar(0.0, 0.0, 255.0)），线条粗细为 2
    rectangle(&mut output_img, Rect::new(top_left.x, top_left.y, w, h), Scalar::new(0.0, 0.0, 255.0, 0.0), 2, 8, 0)?;

    #[cfg(test)]{
        imshow("test", &output_img)?;
        wait_key(0)?;
        destroy_all_windows()?;
    }

    // 5. 将 OpenCV 的 Mat 转换为 image 库的 DynamicImage
    // 确保图像是连续存储的
    if !output_img.is_continuous() {
        return Err("Mat is not continuous".into());
    }

    Ok(output_img)
}

#[inline]
fn right_get_frame(capture: &DxgiCaptureRs, pos: &CapturePos) -> Mat {
    capture.grab(pos)
}

pub fn get_window_client_offset(hwnd: HWND) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::RECT;
    let mut rect = RECT::default();
    unsafe {
        match DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        ) {
            Ok(_) => {
                let mut point = POINT::default();
                if ClientToScreen(hwnd, &mut point).as_bool() {
                    let offset_x = point.x - rect.left;
                    let offset_y = point.y - rect.top;
                    Some((offset_x, offset_y))
                } else {
                    error!("ClientToScreen failed for hwnd: {:?}", hwnd);
                    None
                }
            }
            Err(e) => {
                error!("DwmGetWindowAttribute error: {:?}", e);
                None
            }
        }
    }
}
pub fn animation_test() -> Result<(), Box<dyn Error>> {
    let _ = unsafe { SetProcessDPIAware()};
    match search_window_by_title("PHANTASY") {
        Some(hwnd) => {
            let pos = if let Some((offset_x, offset_y)) = get_window_client_offset(hwnd){
                CapturePos::new(offset_x,offset_y,1600,900)
            }else{
                CapturePos::full_window()
            };
            let capture = DxgiCaptureRs::new(hwnd)?;
            // 建立顯示視窗
            // 创建输入和输出通道
            // task_tx: 主线程发任务给后台，res_rx: 主线程接收处理好的图像
            let (task_tx, task_rx) = mpsc::sync_channel::<Task>(10); // 限制队列大小防止内存撑爆
            let (res_tx, res_rx) = mpsc::channel::<RenderFrame>();

            let task_rx = Arc::new(std::sync::Mutex::new(task_rx));
            let worker_count = 1; // 根据 CPU 核心数调整线程数
            // 3. 启动后台多线程消费者
            for _ in 0..worker_count {
                let task_rx = Arc::clone(&task_rx);
                let res_tx = res_tx.clone();
                let template_bytes = include_bytes!("t_template_blue_small_gray.png");

                // 2. 从内存的字节数组中直接解码成灰色 Mat
                // 传入的字节数组必须先转换为 OpenCV 兼容的 core::Vector<u8> 或切片
                let template_gray = imdecode(&opencv::core::Vector::from_slice(template_bytes), IMREAD_GRAYSCALE)?;

                thread::spawn(move || {
                    loop {
                        // 获取任务
                        let task = match task_rx.lock().unwrap().recv() {
                            Ok(t) => t,
                            Err(_) => break, // 通道关闭，线程退出
                        };
                        // 执行耗时的动画转换操作
                        if let Ok(display_img) =
                            draw_rect_on_image(&task.dynamic_img, &template_gray)
                        {
                            // 将带序号的结果发回
                            if res_tx
                                .send(RenderFrame {
                                    seq: task.seq,
                                    display_img,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                });
            }
            drop(res_tx); // 释放主线程的这个副本，确保通道能正确关闭

            let mut send_seq = 0; // 发送序号计数器
            let mut render_seq = 0; // 期望渲染的序号计数器
            let mut render_buffer = BinaryHeap::new(); // 缓存乱序到达的帧
            println!("按 'q' 鍵退出...");
            // 预先创建窗口（可选，但推荐）
            let window_name = "Video Playback";
            named_window(window_name, WINDOW_AUTOSIZE)?;
            loop {
                // 4. 生产者：抓取帧并放入队列（非阻塞或带限流发送）
                // 注意：如果窗口有关闭事件，需要在这里 break 循环
                if task_tx
                    .try_send(Task {
                        seq: send_seq,
                        dynamic_img: right_get_frame(&capture, &pos),
                    })
                    .is_ok()
                {
                    send_seq += 1;
                }

                // 5. 收集已处理完毕的帧放入堆中
                while let Ok(frame) = res_rx.try_recv() {
                    render_buffer.push(frame);
                }

                // 6. 按顺序渲染
                // 检查堆顶的帧是否是当前期望的 render_seq
                while let Some(frame) = render_buffer.peek() {
                    if frame.seq == render_seq {
                        // 弹出顺序列中的第一帧
                        let frame = render_buffer.pop().unwrap();
                        let display_img = frame.display_img;
                        imshow(window_name, &display_img)?;
                        render_seq += 1;
                    } else {
                        break; // 还在等待缺失的那一帧，先不渲染后续的
                    }
                }
                let key = wait_key(33)?;
                if key == 'q' as i32 {
                    println!("用户终止播放。");
                    break;
                }
                // 防止主循环跑太快死循环占满 CPU，适当休眠
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::capture_settings::CapturePos;
    use crate::dxgi_capture_rs::DxgiCaptureRs;
    use crate::image_template::{animation_test, draw_rect_on_image, right_get_frame, search_window_by_title};
    use opencv::imgcodecs::{imread, IMREAD_GRAYSCALE};

    #[test]
    fn test_animation_test() {
        animation_test().unwrap();
    }

    #[test]
    fn test_draw_rect_on_image() {
        let template_path = "t_template_blue_small_gray.png";
        let template_gray = imread(template_path, IMREAD_GRAYSCALE).unwrap();
        match search_window_by_title("PHANTASY") {
            Some(hwnd) => {
                let dxgi = DxgiCaptureRs::new(hwnd).unwrap();
                let img = right_get_frame(&dxgi, &CapturePos::full_window());
                let output = draw_rect_on_image(&img, &template_gray).unwrap();
            },
            None => {}
        }
    }
}
