use image::{ImageReader, GenericImageView, ImageBuffer, Rgb, DynamicImage};
use ndarray::{Array4, Axis};
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::error::Error;
use ort::ep::CUDAExecutionProvider;

pub fn animation_test(input_image_path:&str) -> Result<(), Box<dyn Error>> {
    let mut session = Session::builder()?
        // 啟用 DirectML (Windows 最佳實踐，支援大部分顯卡)
        .with_execution_providers([CUDAExecutionProvider::default().with_device_id(0).build()])?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .commit_from_file("AnimeGANv3_Hayao_36.onnx")?;

    // 1. 獲取原圖尺寸
    let img = ImageReader::open(input_image_path)?.decode()?;
    let (orig_width, orig_height) = img.dimensions();

    // 2. 調整為 32 的倍數（模型通常要求，否則會報錯）
    let target_width = (orig_width / 32) * 32;
    let target_height = (orig_height / 32) * 32;
    let img_resized = img.resize_exact(target_width, target_height, image::imageops::FilterType::Triangle);

    // 3. 構建動態尺寸的 Tensor [1, H, W, 3]
    let mut input_array = Array4::<f32>::zeros((1, target_height as usize, target_width as usize, 3));
    for (x, y, pixel) in img_resized.pixels() {
        // 索引順序：[Batch, Y, X, Channel]
        input_array[[0, y as usize, x as usize, 0]] = (pixel[0] as f32 / 127.5) - 1.0;
        input_array[[0, y as usize, x as usize, 1]] = (pixel[1] as f32 / 127.5) - 1.0;
        input_array[[0, y as usize, x as usize, 2]] = (pixel[2] as f32 / 127.5) - 1.0;
    }

    // 4. 執行推理
    let input_tensor = Tensor::from_array(input_array)?;
    let outputs = session.run(inputs![input_tensor])?;

    // 5. 處理輸出 (假設模型輸出也是 NHWC)
    let output_tensor = outputs[0].try_extract_array::<f32>()?;
    let view = output_tensor.view();
    let hwc_view = view.index_axis(Axis(0), 0);

    // 6. 建立與 target 尺寸一致的緩衝區
    let mut out_img = ImageBuffer::new(target_width, target_height);
    for (y, row) in hwc_view.axis_iter(Axis(0)).enumerate() {
        for (x, pixel) in row.axis_iter(Axis(0)).enumerate() {
            let r = ((pixel[0] + 1.0) * 127.5).clamp(0.0, 255.0) as u8;
            let g = ((pixel[1] + 1.0) * 127.5).clamp(0.0, 255.0) as u8;
            let b = ((pixel[2] + 1.0) * 127.5).clamp(0.0, 255.0) as u8;
            out_img.put_pixel(x as u32, y as u32, Rgb([r, g, b]));
        }
    }

    // 7. 最後縮放回原始解析度 (如果 target 尺寸與原圖有微小差異)
    let final_img = DynamicImage::ImageRgb8(out_img)
        .resize_exact(orig_width, orig_height, image::imageops::FilterType::Lanczos3);

    final_img.save("output_anime.png")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::image_animation::animation_test;

    #[test]
    fn test_animation(){
        animation_test("NgsShot_20260104_045544.png").unwrap();
    }
}