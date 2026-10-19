// use crate::capture_settings::CapturePos;
// use crate::dxgi_capture_rs::DxgiCaptureRs;
// use image::{DynamicImage, GenericImageView, ImageBuffer, Rgb};
// use ndarray::{Array3, Axis};
// use ort::session::builder::GraphOptimizationLevel;
// use ort::session::{Session, WorkloadType};
// use ort::value::Tensor;
// use ort::{ep, inputs};
// use show_image::{create_window, ImageInfo, ImageView, WindowOptions};
// use std::cmp::Ordering;
// use std::collections::BinaryHeap;
// use std::error::Error;
// use std::ops::{Deref, DerefMut};
// use std::sync::{mpsc, Arc};
// use std::thread;
// use windows::core::BOOL;
// use windows::Win32::Foundation::{HWND, LPARAM};
// use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW};
// struct WindowFinder {
//     title: String,
//     found_hwnd: Option<HWND>,
// }
//
// // 1. 定义发送给工作线程的任务数据结构
// struct Task {
//     seq: usize,
//     dynamic_img: DynamicImage, // 假设你的图片类型是这个
// }
//
// // 2. 定义工作线程处理完毕后返回的数据结构
// struct RenderFrame {
//     seq: usize,
//     display_img: DynamicImage,
// }
//
// // 为了让 BinaryHeap 实现小顶堆（按 seq 从小到大排序），需要实现 Ord
// impl PartialEq for RenderFrame {
//     fn eq(&self, other: &Self) -> bool { self.seq == other.seq }
// }
// impl Eq for RenderFrame {}
// impl PartialOrd for RenderFrame {
//     fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
// }
// impl Ord for RenderFrame {
//     fn cmp(&self, other: &Self) -> Ordering {
//         other.seq.cmp(&self.seq) // 倒序：让 seq 小的排在堆顶
//     }
// }
//
// extern "system" fn window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
//     // Here you would implement the logic to check the window title
//     // and compare it with the desired title passed via lparam.
//     // If a match is found, you can store the HWND in a location
//     // accessible via lparam.
//     let window_finder = unsafe { &mut *(lparam.0 as *mut WindowFinder) };
//     let mut buffer: [u16; 256] = [0; 256];
//     unsafe {
//         GetWindowTextW(hwnd, &mut buffer);
//     }
//     let window_title = String::from_utf16_lossy(&buffer);
//     if window_title.contains(&window_finder.title) {
//         window_finder.found_hwnd = Some(hwnd);
//         // 已经找到窗口, 返回false终止枚举
//         BOOL(0)
//     } else {
//         // 未找到, 返回true继续枚举
//         BOOL(1)
//     }
// }
// /// 遍历所有窗口，查找匹配的窗口名，返回HWND
// pub fn search_window_by_title(window_name: &str) -> Option<HWND> {
//     let mut finder = WindowFinder {
//         title: window_name.to_string(),
//         found_hwnd: None,
//     };
//     unsafe {
//         let _ = EnumWindows(
//             Some(window_callback),
//             LPARAM(&mut finder as *mut _ as isize),
//         );
//     }
//     finder.found_hwnd
// }
//
// pub fn to_animation_image(
//     // device:Arc<CudaDevice>,
//     img: &DynamicImage,
//     session: &mut Session,
// ) -> Result<DynamicImage, Box<dyn Error>> {
//     let (orig_width, orig_height) = img.dimensions();
//     let target_width = (orig_width / 32) * 32;
//     let target_height = (orig_height / 32) * 32;
//
//     // 快速轉換：直接獲取底層 Vec<u8> 並轉為 Array3
//     let img_rgb = img
//         .resize_exact(
//             target_width,
//             target_height,
//             image::imageops::FilterType::Triangle,
//         )
//         .to_rgb8();
//     let raw_data = img_rgb.as_raw();
//     // 向量化預處理：直接將 [u8] 轉成 [H, W, 3] 的 Array3，然後一次性運算
//     let input_array = Array3::from_shape_vec(
//         (target_height as usize, target_width as usize, 3),
//         raw_data.to_vec(),
//     )?
//     .mapv(|x| (x as f32 / 127.5) - 1.0)
//     .insert_axis(Axis(0)); // 變為 [1, H, W, 3]
//
//     // let device_data = device.htod_sync_copy(&input_array.into_raw_vec_and_offset().0)?;
//     // let tensor: TensorRefMut<'_, f32> = unsafe {
//     //     TensorRefMut::from_raw(
//     //         MemoryInfo::new(AllocationDevice::CUDA, 0, AllocatorType::Device, MemoryType::Default)?,
//     //         (*device_data.device_ptr() as usize as *mut ()).cast(),
//     //         Shape::from([1usize, 3, target_height as usize, target_width as usize])
//     //     )?
//     // };
//     // 推理
//     let input_tensor = Tensor::from_array(input_array)?;
//     let outputs =    session.run(inputs![input_tensor])?;
//     let output_tensor = outputs[0].try_extract_array::<f32>()?;
//
//     // 向量化後處理：一次性完成逆運算並轉回 u8
//     let binding = output_tensor.view();
//     let hwc_view = binding.index_axis(Axis(0), 0);
//     let out_raw: Vec<u8> = hwc_view
//         .mapv(|x| ((x + 1.0) * 127.5).clamp(0.0, 255.0) as u8)
//         .into_raw_vec();
//
//     let out_img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(target_width, target_height, out_raw)
//         .ok_or("Buffer error")?;
//
//     Ok(DynamicImage::ImageRgb8(out_img).resize_exact(
//         orig_width,
//         orig_height,
//         image::imageops::FilterType::Lanczos3,
//     ))
// }
//
// fn right_get_frame(capture: &DxgiCaptureRs, pos: &CapturePos) -> DynamicImage {
//     capture.grab(pos).unwrap()
// }
// pub fn animation_test() -> Result<(), Box<dyn std::error::Error>> {
//     match search_window_by_title("Dungeon") {
//         Some(hwnd) => {
//             let pos = CapturePos::full_window();
//             let capture = DxgiCaptureRs::new(hwnd)?;
//             // 建立顯示視窗
//             let (window_width, window_height) = (2134, 1200);
//             let options = WindowOptions {
//                 size: Some([window_width, window_height]), // 初始視窗大小
//                 resizable: true,                           // 允許用戶手動調整大小
//                 ..Default::default()
//             };
//             let window = create_window("Real-time AnimeGAN", options)?;
//
//             // 创建输入和输出通道
//             // task_tx: 主线程发任务给后台，res_rx: 主线程接收处理好的图像
//             let (task_tx, task_rx) = mpsc::sync_channel::<Task>(10); // 限制队列大小防止内存撑爆
//             let (res_tx, res_rx) = mpsc::channel::<RenderFrame>();
//
//             let task_rx = Arc::new(std::sync::Mutex::new(task_rx));
//             let worker_count = 1; // 根据 CPU 核心数调整线程数
//             // 3. 启动后台多线程消费者
//             for _ in 0..worker_count {
//                 let task_rx = Arc::clone(&task_rx);
//                 let res_tx = res_tx.clone();
//                 let mut session = Session::builder()?
//                     .with_intra_threads(8)?
//                     .with_execution_providers([
//                         ep::DirectML::default()
//                             .build()
//                             // exit the program with an error if the CUDA EP fails to register
//                             .error_on_failure()
//                     ])?
//                     .with_optimization_level(GraphOptimizationLevel::Level3)?
//                     .commit_from_file("AnimeGANv3_Hayao_36.onnx")?;
//                 session.set_workload_type(WorkloadType::Efficient)?;
//                 thread::spawn(move || {
//                     loop {
//                         // 获取任务
//                         let task = match task_rx.lock().unwrap().recv() {
//                             Ok(t) => t,
//                             Err(_) => break, // 通道关闭，线程退出
//                         };
//                         // 执行耗时的动画转换操作
//                         // let device = CudaDevice::new(0).unwrap();
//                         if let Ok(display_img) = to_animation_image(/*device, */&task.dynamic_img, &mut session) {
//                             // 将带序号的结果发回
//                             if res_tx.send(RenderFrame { seq: task.seq, display_img }).is_err() {
//                                 break;
//                             }
//                         }
//                     }
//                 });
//             }
//             drop(res_tx); // 释放主线程的这个副本，确保通道能正确关闭
//
//
//             let mut send_seq = 0;    // 发送序号计数器
//             let mut render_seq = 0;  // 期望渲染的序号计数器
//             let mut render_buffer = BinaryHeap::new(); // 缓存乱序到达的帧
//             println!("按 'q' 鍵退出...");
//             loop {
//                 // 4. 生产者：抓取帧并放入队列（非阻塞或带限流发送）
//                 // 注意：如果窗口有关闭事件，需要在这里 break 循环
//                 if task_tx.try_send(Task { seq: send_seq, dynamic_img: right_get_frame(&capture, &pos) }).is_ok() {
//                     send_seq += 1;
//                 }
//
//                 // 5. 收集已处理完毕的帧放入堆中
//                 while let Ok(frame) = res_rx.try_recv() {
//                     render_buffer.push(frame);
//                 }
//
//                 // 6. 按顺序渲染
//                 // 检查堆顶的帧是否是当前期望的 render_seq
//                 while let Some(frame) = render_buffer.peek() {
//                     if frame.seq == render_seq {
//                         // 弹出顺序列中的第一帧
//                         let frame = render_buffer.pop().unwrap();
//                         let display_img = frame.display_img;
//
//                         let (w, h) = display_img.dimensions();
//                         let image_view = ImageView::new(
//                             ImageInfo::rgb8(w, h),
//                             display_img.as_bytes(),
//                         );
//
//                         // 更新视窗内容
//                         window.set_image("live-result", image_view)?;
//
//                         render_seq += 1;
//                     } else {
//                         break; // 还在等待缺失的那一帧，先不渲染后续的
//                     }
//                 }
//
//                 // 防止主循环跑太快死循环占满 CPU，适当休眠
//                 thread::sleep(std::time::Duration::from_millis(5));
//             }
//
//             // loop {
//             //     let dynamic_img = right_get_frame(&capture, &pos);
//             //     let display_img = to_animation_image(&dynamic_img, &mut session)?;
//             //     // let display_img = dynamic_img.resize(window_width, window_height, image::imageops::FilterType::Nearest);
//             //     let (w, h) = display_img.dimensions();
//             //     let image_view = ImageView::new(
//             //         ImageInfo::rgb8(w, h),
//             //         display_img.as_bytes(), // 這裡動態獲取 RGB 字節流
//             //     );
//             //     // 3. 更新視窗內容
//             //     // 使用固定的名稱（如 "frame"）可以確保是在同一個畫布上刷新
//             //     window.set_image("live-result", image_view)?;
//             // }
//         }
//         None => {}
//     }
//     Ok(())
// }
//
// #[cfg(test)]
// mod tests {
//     use crate::image_animation::animation_test;
//
//     #[test]
//     fn test_animation_test() {
//         animation_test().unwrap();
//     }
// }
