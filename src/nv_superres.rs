//! NVIDIA Video Effects SDK 的 SuperRes（AI 超分），动态加载运行库，不新增链接依赖。
//!
//! 依赖 NVIDIA App / 驱动自带的 Maxine 运行库目录（默认
//! `C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects`），里面已有
//! `NVVideoEffects.dll`、`NvCVImage.dll` 和 `models\SR_2x/3x/4x_*.engine.trtpkg`。
//!
//! 调用链（C API，签名取自 SDK 的 `nvVideoEffects.h` / `nvCVImage.h`）：
//! ```text
//! NvVFX_CreateEffect("SuperRes") → SetString("ModelDir") → SetU32("Scale")
//!   → SetImage("SrcImage0"/"DstImage0") → NvVFX_Load → 每帧 NvVFX_Run
//! ```
//!
//! 目前图像走 CPU 中转（`NvCVImage_Transfer` 负责上传/下载），够用来验证质量与耗时；
//! 运行库里还有 `NvCVImage_InitFromD3D11Texture`，后续可以让采集/上屏纹理直接对接、免掉中转。
#![allow(dead_code)]

use std::ffi::{c_char, c_void};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LoadLibraryExW,
};

// ---- nvCVImage.h 里的常量 ----
/// 像素格式（`NvCVImage_PixelFormat`）
const NVCV_BGR: i32 = 5;
const NVCV_RGBA: i32 = 6;
const NVCV_BGRA: i32 = 7;
/// Upscale 的锐化/细节强度（官方示例默认 0.2）
const STRENGTH: f32 = 0.2;
/// 分量类型（`NvCVImage_ComponentType`）。
/// 实测 0.7.6 运行库里 `0` 是 UNKNOWN（直接返回 NVCV_ERR_PIXELFORMAT），`1` 才是 8bit unsigned。
const NVCV_UNSIGNED: i32 = 1;
/// 布局/存储位置
const NVCV_CHUNKY: u32 = 0;
const NVCV_CPU_PINNED: u32 = 2;
const NVCV_GPU: u32 = 1;

/// `NvCVImage`：布局照抄 `nvCVImage.h`（x64 下 64 字节）。
/// 字段顺序不能改，C 侧会按这块内存直接读写。
#[repr(C)]
#[derive(Clone, Copy)]
struct NvCVImage {
    width: u32,
    height: u32,
    /// 行字节跨度（可能大于 width * pixel_bytes）
    pitch: i32,
    pixel_format: i32,
    component_type: i32,
    pixel_bytes: u8,
    component_bytes: u8,
    num_components: u8,
    /// NVCV_CHUNKY / NVCV_PLANAR / YUV 布局
    planar: u8,
    /// NVCV_CPU / NVCV_CPU_PINNED / NVCV_GPU / NVCV_CUDA_ARRAY
    gpu_mem: u8,
    colorspace: u8,
    reserved: [u8; 2],
    pixels: *mut c_void,
    delete_ptr: *mut c_void,
    delete_proc: Option<unsafe extern "system" fn(*mut c_void)>,
    buffer_bytes: u64,
}

impl NvCVImage {
    const fn zeroed() -> Self {
        Self {
            width: 0,
            height: 0,
            pitch: 0,
            pixel_format: 0,
            component_type: 0,
            pixel_bytes: 0,
            component_bytes: 0,
            num_components: 0,
            planar: 0,
            gpu_mem: 0,
            colorspace: 0,
            reserved: [0; 2],
            pixels: std::ptr::null_mut(),
            delete_ptr: std::ptr::null_mut(),
            delete_proc: None,
            buffer_bytes: 0,
        }
    }
}

// ---- 函数指针类型 ----
type FnAlloc = unsafe extern "system" fn(
    *mut NvCVImage,
    u32,
    u32,
    i32,
    i32,
    u32,
    u32,
    u32,
) -> i32;
type FnDealloc = unsafe extern "system" fn(*mut NvCVImage);
type FnTransfer =
    unsafe extern "system" fn(*const NvCVImage, *mut NvCVImage, f32, *mut c_void, *mut NvCVImage) -> i32;
type FnCreateEffect = unsafe extern "system" fn(*const c_char, *mut *mut c_void) -> i32;
type FnSetImage = unsafe extern "system" fn(*mut c_void, *const c_char, *mut NvCVImage) -> i32;
type FnSetU32 = unsafe extern "system" fn(*mut c_void, *const c_char, u32) -> i32;
type FnSetF32 = unsafe extern "system" fn(*mut c_void, *const c_char, f32) -> i32;
type FnGetU32 = unsafe extern "system" fn(*mut c_void, *const c_char, *mut u32) -> i32;
type FnSetString = unsafe extern "system" fn(*mut c_void, *const c_char, *const c_char) -> i32;
type FnLoad = unsafe extern "system" fn(*mut c_void) -> i32;
type FnRun = unsafe extern "system" fn(*mut c_void, i32) -> i32;
type FnDestroyEffect = unsafe extern "system" fn(*mut c_void);

struct Api {
    image_alloc: FnAlloc,
    image_dealloc: FnDealloc,
    image_transfer: FnTransfer,
    create_effect: FnCreateEffect,
    set_image: FnSetImage,
    set_u32: FnSetU32,
    set_f32: FnSetF32,
    get_u32: FnGetU32,
    set_string: FnSetString,
    load: FnLoad,
    run: FnRun,
    destroy_effect: FnDestroyEffect,
}

/// 运行库目录：`NVVFX_DIR` 优先，其次 exe 自带的 `nvvfx\`，最后 NVIDIA App 的默认安装位置
fn runtime_dir() -> Result<String, String> {
    if let Some(dir) = std::env::var_os("NVVFX_DIR") {
        let dir = dir.to_string_lossy().into_owned();
        if std::path::Path::new(&dir).join("NVVideoEffects.dll").is_file() {
            return Ok(dir);
        }
        return Err(format!("NVVFX_DIR 下没有 NVVideoEffects.dll：{dir}"));
    }
    let mut candidates = Vec::new();
    // 打包时用 package.ps1 -IncludeVfx 会把运行库放进 exe 同级的 nvvfx\
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("nvvfx").to_string_lossy().into_owned());
            candidates.push(
                dir.join("NVIDIA Video Effects")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    candidates.push(r"C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects".to_string());
    candidates
        .push(r"C:\Program Files (x86)\NVIDIA Corporation\NVIDIA Video Effects".to_string());
    candidates
        .into_iter()
        .find(|dir| std::path::Path::new(dir).join("NVVideoEffects.dll").is_file())
        .ok_or_else(|| {
            "没找到 NVIDIA Video Effects 运行库（NVVideoEffects.dll）：\n\
             - 装 NVIDIA App（会自带该运行库），或\n\
             - 把运行库目录放到 exe 同级的 nvvfx\\ 或 NVIDIA Video Effects\\，或\n\
             - 用环境变量 NVVFX_DIR 指向运行库目录"
                .to_string()
        })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从 DLL 取符号。用 `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` 加载，让 DLL 自己的依赖
/// （cudart64_12 / nvinfer_10 等）从它所在目录解析，不用改进程的搜索路径。
unsafe fn load_dll(dir: &str, name: &str) -> Result<HMODULE, String> {
    let path = wide(&format!("{dir}\\{name}"));
    unsafe {
        LoadLibraryExW(
            PCWSTR::from_raw(path.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
        .map_err(|e| format!("加载 {name} 失败（文件在但依赖缺失？）: {e}"))
    }
}

unsafe fn sym<T: Copy>(dll: HMODULE, name: &str) -> Result<T, String> {
    let mut buf = Vec::with_capacity(name.len() + 1);
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    let f = unsafe { GetProcAddress(dll, PCSTR::from_raw(buf.as_ptr())) }
        .ok_or_else(|| format!("{name} 没有导出"))?;
    if std::mem::size_of::<T>() != std::mem::size_of_val(&f) {
        return Err(format!("{name} 的签名大小不匹配"));
    }
    Ok(unsafe { std::mem::transmute_copy(&f) })
}

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} 失败，NvCV_Status = {status:#x}"))
    }
}

fn cstr(s: &'static [u8]) -> *const c_char {
    s.as_ptr() as *const c_char
}

/// 效果名：`NVVFX_FX_SR_UPSCALE`
///
/// 注意不是 `"SuperRes"` —— 那个是另一套 VSR 特效，实测它对本机这几组 RGB(A) 图像
/// 一律返回 `NVCV_ERR_PIXELFORMAT`；官方 `UpscalePipelineApp` 示例用的就是 `"Upscale"`。
const EFFECT_NAME: &[u8] = b"Upscale\0";

const K_MODEL_DIR: &[u8] = b"ModelDir\0";
const K_SCALE: &[u8] = b"Scale\0";
const K_MODE: &[u8] = b"Mode\0";
const K_STRENGTH: &[u8] = b"Strength\0";
const K_MAX_IN_W: &[u8] = b"MaxInputWidth\0";
const K_MAX_IN_H: &[u8] = b"MaxInputHeight\0";
const K_SRC: &[u8] = b"SrcImage0\0";
const K_DST: &[u8] = b"DstImage0\0";

/// 一路 SuperRes 特效（2x/3x/4x 由 `scale` 决定，模型由 SDK 自己按倍率挑）
pub struct SuperRes {
    _vfx: HMODULE,
    _img: HMODULE,
    api: Api,
    handle: *mut c_void,
    pub scale: u32,
    /// 模型目录（SDK 的 `models\*.engine.trtpkg` 所在处）
    pub models_dir: String,
    /// 当前图像已按哪个输入尺寸分配
    size: (u32, u32),
    cpu_in: NvCVImage,
    gpu_in: NvCVImage,
    gpu_out: NvCVImage,
    cpu_out: NvCVImage,
    /// 旧版调用约定：所有图像都在这一步先清零，Drop 时按 pixels 是否为空决定要不要 Dealloc
    ready: bool,
}

impl SuperRes {
    /// `scale` 只支持 2/3/4，且必须能在 `models` 目录里找到对应引擎
    pub fn new(scale: u32) -> Result<Self, String> {
        if !(2..=4).contains(&scale) {
            return Err(format!("超分倍率只支持 2/3/4，收到 {scale}"));
        }
        let dir = runtime_dir()?;
        let models_dir = format!("{dir}\\models");
        if !std::path::Path::new(&models_dir).is_dir() {
            return Err(format!("模型目录不存在：{models_dir}"));
        }

        unsafe {
            let vfx = load_dll(&dir, "NVVideoEffects.dll")?;
            let img = match load_dll(&dir, "NvCVImage.dll") {
                Ok(h) => h,
                Err(e) => {
                    let _ = FreeLibrary(vfx);
                    return Err(e);
                }
            };
            let api = Api {
                image_alloc: sym(img, "NvCVImage_Alloc")?,
                image_dealloc: sym(img, "NvCVImage_Dealloc")?,
                image_transfer: sym(img, "NvCVImage_Transfer")?,
                create_effect: sym(vfx, "NvVFX_CreateEffect")?,
                set_image: sym(vfx, "NvVFX_SetImage")?,
                set_u32: sym(vfx, "NvVFX_SetU32")?,
                set_f32: sym(vfx, "NvVFX_SetF32")?,
                get_u32: sym(vfx, "NvVFX_GetU32")?,
                set_string: sym(vfx, "NvVFX_SetString")?,
                load: sym(vfx, "NvVFX_Load")?,
                run: sym(vfx, "NvVFX_Run")?,
                destroy_effect: sym(vfx, "NvVFX_DestroyEffect")?,
            };

            let mut handle: *mut c_void = std::ptr::null_mut();
            check(
                (api.create_effect)(cstr(EFFECT_NAME), &mut handle),
                "NvVFX_CreateEffect(Upscale)",
            )
            .inspect_err(|_| {
                let _ = FreeLibrary(img);
                let _ = FreeLibrary(vfx);
            })?;

            let mut this = Self {
                _vfx: vfx,
                _img: img,
                api,
                handle,
                scale,
                models_dir,
                size: (0, 0),
                cpu_in: NvCVImage::zeroed(),
                gpu_in: NvCVImage::zeroed(),
                gpu_out: NvCVImage::zeroed(),
                cpu_out: NvCVImage::zeroed(),
                ready: false,
            };
            this.configure()?;
            Ok(this)
        }
    }

    /// 设置模型目录 + 倍率 + 探测输入尺寸上限
    unsafe fn configure(&mut self) -> Result<(), String> {
        let api = &self.api;
        let h = self.handle;
        unsafe {
            // 模型目录：官方示例里只有 ArtifactReduction 会设它，Upscale 用默认目录；
            // 这里能设就设，返回 SELECTOR 说明不支持，不影响使用。
            let dir = std::ffi::CString::new(self.models_dir.as_str())
                .map_err(|e| format!("模型目录含 NUL: {e}"))?;
            let rmd = (api.set_string)(h, cstr(K_MODEL_DIR), dir.as_ptr());
            if rmd != 0 {
                eprintln!("NvVFX_SetString(ModelDir) 不支持（{rmd:#x}），用运行时默认模型目录");
            }
            // 放大倍率不通过参数给：由输入/输出图像的尺寸差决定（2x/3x/4x）。
            // 质量档用 Strength（浮点，官方示例默认 0.2），不是 Mode。
            let rs = (api.set_f32)(h, cstr(K_STRENGTH), STRENGTH);
            if rs != 0 {
                eprintln!("NvVFX_SetF32(Strength) 不受支持（{rs:#x}），用默认档");
            }
            for (key, tag) in [(K_MAX_IN_W, "MaxInputWidth"), (K_MAX_IN_H, "MaxInputHeight")] {
                let mut v = 0u32;
                if (api.get_u32)(h, cstr(key), &mut v) == 0 {
                    log::info!("SuperRes {tag} = {v}");
                }
            }
        }
        Ok(())
    }

    /// 按输入尺寸准备 4 张图（CPU 进 / GPU 进 / GPU 出 / CPU 出），并绑定到特效
    unsafe fn ensure_images(&mut self, w: u32, h: u32) -> Result<(), String> {
        if self.size == (w, h) {
            return Ok(());
        }
        let out_w = w * self.scale;
        let out_h = h * self.scale;
        let api = &self.api;

        if self.ready {
            unsafe {
                (api.image_dealloc)(&mut self.cpu_in);
                (api.image_dealloc)(&mut self.gpu_in);
                (api.image_dealloc)(&mut self.gpu_out);
                (api.image_dealloc)(&mut self.cpu_out);
            }
            self.cpu_in = NvCVImage::zeroed();
            self.gpu_in = NvCVImage::zeroed();
            self.gpu_out = NvCVImage::zeroed();
            self.cpu_out = NvCVImage::zeroed();
        }

        unsafe {
            // 官方 Upscale 示例：RGBA / U8 / 交错 / GPU / 对齐 32。
            // CPU 侧对齐 1，pitch 正好 w*4，打包解包最简单。
            check(
                (api.image_alloc)(&mut self.cpu_in, w, h, NVCV_RGBA, NVCV_UNSIGNED, NVCV_CHUNKY, NVCV_CPU_PINNED, 1),
                "NvCVImage_Alloc(cpu_in)",
            )?;
            check(
                (api.image_alloc)(&mut self.gpu_in, w, h, NVCV_RGBA, NVCV_UNSIGNED, NVCV_CHUNKY, NVCV_GPU, 32),
                "NvCVImage_Alloc(gpu_in)",
            )?;
            check(
                (api.image_alloc)(&mut self.gpu_out, out_w, out_h, NVCV_RGBA, NVCV_UNSIGNED, NVCV_CHUNKY, NVCV_GPU, 32),
                "NvCVImage_Alloc(gpu_out)",
            )?;
            check(
                (api.image_alloc)(&mut self.cpu_out, out_w, out_h, NVCV_RGBA, NVCV_UNSIGNED, NVCV_CHUNKY, NVCV_CPU_PINNED, 1),
                "NvCVImage_Alloc(cpu_out)",
            )?;
            self.ready = true;

            check(
                (api.set_image)(self.handle, cstr(K_SRC), &mut self.gpu_in),
                "NvVFX_SetImage(SrcImage0)",
            )?;
            check(
                (api.set_image)(self.handle, cstr(K_DST), &mut self.gpu_out),
                "NvVFX_SetImage(DstImage0)",
            )?;
            check((api.load)(self.handle), "NvVFX_Load")?;
            eprintln!("NVIDIA Upscale 就绪：{}x{} → {}x{}（{}x）", w, h, out_w, out_h, self.scale);
        }
        self.size = (w, h);
        Ok(())
    }

    /// 输入 BGRA8（紧密排列），返回放大后的 BGRA8 与其尺寸
    pub fn run_bgra(&mut self, src: &[u8], w: u32, h: u32) -> Result<(Vec<u8>, u32, u32), String> {
        if src.len() < (w as usize) * (h as usize) * 4 {
            return Err(format!("输入缓冲太小：{} < {}", src.len(), w as usize * h as usize * 4));
        }
        unsafe { self.ensure_images(w, h)? };

        let api = &self.api;
        unsafe {
            // CPU 进 → GPU 进：采集是 BGRA，特效只吃 RGBA，这一遍顺手换 R/B
            let row = (w * 4) as usize;
            let pitch = self.cpu_in.pitch as usize;
            for y in 0..h as usize {
                let s = src.as_ptr().add(y * row);
                let d = (self.cpu_in.pixels as *mut u8).add(y * pitch);
                for x in 0..(w as usize) {
                    let (sp, dp) = (s.add(x * 4), d.add(x * 4));
                    *dp = *sp.add(2);
                    *dp.add(1) = *sp.add(1);
                    *dp.add(2) = *sp;
                    *dp.add(3) = *sp.add(3);
                }
            }
            check(
                (api.image_transfer)(&self.cpu_in, &mut self.gpu_in, 1.0, std::ptr::null_mut(), std::ptr::null_mut()),
                "NvCVImage_Transfer(cpu→gpu)",
            )?;
            // 异步标志 0 = 内部同步，返回时结果已可读
            check((api.run)(self.handle, 0), "NvVFX_Run")?;
            check(
                (api.image_transfer)(&self.gpu_out, &mut self.cpu_out, 1.0, std::ptr::null_mut(), std::ptr::null_mut()),
                "NvCVImage_Transfer(gpu→cpu)",
            )?;

            let (out_w, out_h) = (w * self.scale, h * self.scale);
            let out_row = (out_w * 4) as usize;
            let out_pitch = self.cpu_out.pitch as usize;
            let mut out = vec![0u8; out_row * out_h as usize];
            for y in 0..out_h as usize {
                let s = (self.cpu_out.pixels as *const u8).add(y * out_pitch);
                let d = out.as_mut_ptr().add(y * out_row);
                // RGBA → BGRA，交还给后续上屏链路
                for x in 0..(out_w as usize) {
                    let (sp, dp) = (s.add(x * 4), d.add(x * 4));
                    *dp = *sp.add(2);
                    *dp.add(1) = *sp.add(1);
                    *dp.add(2) = *sp;
                    *dp.add(3) = *sp.add(3);
                }
            }
            Ok((out, out_w, out_h))
        }
    }
}

impl Drop for SuperRes {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                (self.api.destroy_effect)(self.handle);
                self.handle = std::ptr::null_mut();
            }
            if self.ready {
                (self.api.image_dealloc)(&mut self.cpu_in);
                (self.api.image_dealloc)(&mut self.gpu_in);
                (self.api.image_dealloc)(&mut self.gpu_out);
                (self.api.image_dealloc)(&mut self.cpu_out);
            }
            let _ = FreeLibrary(self._img);
            let _ = FreeLibrary(self._vfx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ABI 探针：把 (像素格式, 分量类型, 布局, 存储位置, 对齐) 扫一遍，
    /// 看 NvCVImage_Alloc 到底认哪一组（用于确认装着的 0.7.6 运行库的枚举值）
    ///
    /// `cargo test --bin animation_imagee nv_superres_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "手动排查 ABI 用"]
    fn nv_superres_probe() {
        let dir = runtime_dir().expect("运行库");
        unsafe {
            let img = load_dll(&dir, "NvCVImage.dll").unwrap();
            let alloc: FnAlloc = sym(img, "NvCVImage_Alloc").unwrap();
            let dealloc: FnDealloc = sym(img, "NvCVImage_Dealloc").unwrap();
            for &format in &[4i32, 5, 6, 7] {
                for &ctype in &[0i32, 1, 2] {
                    for &(layout, mem) in &[(0u32, 0u32), (0, 2), (0, 1), (2, 0), (1, 0)] {
                        for &align in &[0u32, 1] {
                            let mut im = NvCVImage::zeroed();
                            let s = alloc(&mut im, 64, 64, format, ctype, layout, mem, align);
                            if s == 0 {
                                eprintln!(
                                    "OK   fmt={format} type={ctype} layout={layout} mem={mem} align={align} -> pitch={} bytes={}",
                                    im.pitch, im.buffer_bytes
                                );
                                dealloc(&mut im);
                            } else if align == 1 {
                                eprintln!("FAIL fmt={format} type={ctype} layout={layout} mem={mem} align={align} -> {s:#x}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// 探针：SuperRes 到底接受哪个像素格式/存储位置的图像
    ///
    /// `cargo test --bin animation_imagee nv_superres_formats -- --ignored --nocapture`
    #[test]
    #[ignore = "手动排查 ABI 用"]
    fn nv_superres_formats() {
        let dir = runtime_dir().expect("运行库");
        unsafe {
            let vfx = load_dll(&dir, "NVVideoEffects.dll").unwrap();
            let imgdll = load_dll(&dir, "NvCVImage.dll").unwrap();
            let api = Api {
                image_alloc: sym(imgdll, "NvCVImage_Alloc").unwrap(),
                image_dealloc: sym(imgdll, "NvCVImage_Dealloc").unwrap(),
                image_transfer: sym(imgdll, "NvCVImage_Transfer").unwrap(),
                create_effect: sym(vfx, "NvVFX_CreateEffect").unwrap(),
                set_image: sym(vfx, "NvVFX_SetImage").unwrap(),
                set_u32: sym(vfx, "NvVFX_SetU32").unwrap(),
                set_f32: sym(vfx, "NvVFX_SetF32").unwrap(),
                get_u32: sym(vfx, "NvVFX_GetU32").unwrap(),
                set_string: sym(vfx, "NvVFX_SetString").unwrap(),
                load: sym(vfx, "NvVFX_Load").unwrap(),
                run: sym(vfx, "NvVFX_Run").unwrap(),
                destroy_effect: sym(vfx, "NvVFX_DestroyEffect").unwrap(),
            };
            let mut h: *mut c_void = std::ptr::null_mut();
            assert_eq!((api.create_effect)(cstr(b"SuperRes\0"), &mut h), 0);
            let models = std::ffi::CString::new(format!("{dir}\\models")).unwrap();
            eprintln!("ModelDir -> {:#x}", (api.set_string)(h, cstr(K_MODEL_DIR), models.as_ptr()));
            let mut v = 0u32;
            if (api.get_u32)(h, cstr(K_MAX_IN_W), &mut v) == 0 {
                eprintln!("MaxInputWidth = {v}");
            }
            if (api.get_u32)(h, cstr(K_MAX_IN_H), &mut v) == 0 {
                eprintln!("MaxInputHeight = {v}");
            }
            eprintln!("SetU32(Scale,4) -> {:#x}", (api.set_u32)(h, cstr(K_SCALE), 4));
            eprintln!("SetU32(Mode,1)  -> {:#x}", (api.set_u32)(h, cstr(K_MODE), 1));
            for &format in &[1i32, 4, 5, 6, 7] {
                for &ctype in &[1i32, 2, 3, 4] {
                    let (w, hh) = (64u32, 64u32);
                    let mut src = NvCVImage::zeroed();
                    let mut dst = NvCVImage::zeroed();
                    let a = (api.image_alloc)(&mut src, w, hh, format, ctype, NVCV_CHUNKY, NVCV_GPU, 1);
                    let b = (api.image_alloc)(&mut dst, w * 4, hh * 4, format, ctype, NVCV_CHUNKY, NVCV_GPU, 1);
                    if a != 0 || b != 0 {
                        continue;
                    }
                    let rs = (api.set_image)(h, cstr(K_SRC), &mut src);
                    let rd = (api.set_image)(h, cstr(K_DST), &mut dst);
                    let load = (api.load)(h);
                    eprintln!(
                        "fmt={format} type={ctype} pitch={} bpp={} -> Src={rs:#x} Dst={rd:#x} Load={load:#x}",
                        src.pitch, src.pixel_bytes
                    );
                    (api.image_dealloc)(&mut src);
                    (api.image_dealloc)(&mut dst);
                }
            }
            (api.destroy_effect)(h);
        }
    }

    /// 灰阶探针：喂 0..255 灰阶，看 Upscale 有没有偷偷改亮度/gamma
    ///
    /// `cargo test --bin animation_imagee nv_superres_gamma_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "需要 NVIDIA 显卡 + Video Effects 运行库，手动跑"]
    fn nv_superres_gamma_probe() {
        let (w, h) = (512u32, 128u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1)) as u8;
                let i = ((y * w + x) * 4) as usize;
                bgra[i] = v;
                bgra[i + 1] = v;
                bgra[i + 2] = v;
                bgra[i + 3] = 255;
            }
        }
        let mut sr = SuperRes::new(2).expect("SuperRes");
        let (out, ow, oh) = sr.run_bgra(&bgra, w, h).unwrap();
        let row = (oh / 2) as usize;
        eprintln!("输入灰度 → 输出灰度（{}x{} → {}x{}）", w, h, ow, oh);
        for xi in [0u32, 16, 32, 64, 96, 128, 160, 192, 224, 240, 255] {
            let xo = (xi * 2).min(ow - 1) as usize;
            let i = (row * ow as usize + xo) * 4;
            eprintln!("  {xi:3} → {:3}（差 {}）", out[i], out[i] as i32 - xi as i32);
        }
    }

    /// 离线验证「仅超分」整条 GPU 链路（与 gpu_window 里的生产路径一致）：
    /// 纹理上传 → 读回 → NVIDIA 超分 → 上传结果 → 1:1 上屏 → 读回存 PNG
    ///
    /// `cargo test --bin animation_imagee nv_superres_gpu_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore = "需要 NVIDIA 显卡 + Video Effects 运行库，手动跑"]
    fn nv_superres_gpu_roundtrip() {
        use crate::gpu_style::{create_standalone_device, GpuBlit};
        use opencv::core::{AlgorithmHint, Mat, MatTraitConstManual, MatTraitManual, Scalar, Size, Vector};
        use opencv::{imgcodecs, imgproc};

        let (w, h) = (512u32, 288u32);
        let src = imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR).unwrap();
        let mut small = Mat::default();
        imgproc::resize(
            &src,
            &mut small,
            Size::new(w as i32, h as i32),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )
        .unwrap();
        let mut bgra = Mat::default();
        imgproc::cvt_color(&small, &mut bgra, imgproc::COLOR_BGR2BGRA, 0, AlgorithmHint::ALGO_HINT_DEFAULT)
            .unwrap();
        let bgra = bgra.data_bytes().unwrap().to_vec();

        let (device, ctx) = create_standalone_device().expect("D3D11 设备");
        let mut blit = GpuBlit::new(&device, &ctx).expect("GpuBlit");
        let mut sr = SuperRes::new(2).expect("SuperRes");

        // 上传 → 读回（验证 upload_bgra / read_bgra 对称）
        let (t_src, _) = blit.create_target(w, h).unwrap();
        blit.upload_bgra(&t_src, &bgra, w, h).unwrap();
        let back = blit.read_bgra(&t_src, w, h).unwrap();
        assert_eq!(&back[..], &bgra[..], "上传后读回与原图不一致");

        let (out, ow, oh) = sr.run_bgra(&back, w, h).unwrap();
        let (t_dst, _) = blit.create_target(ow, oh).unwrap();
        blit.upload_bgra(&t_dst, &out, ow, oh).unwrap();

        // 1:1 上屏（draw_texture 的 exact 分支）
        let (t_show, rtv) = blit.create_target(ow, oh).unwrap();
        blit.draw_texture(&t_dst, &rtv, ow, oh, ow, oh).unwrap();
        let shown = blit.read_bgra(&t_show, ow, oh).unwrap();
        // 只比 RGB：上屏会强制 alpha = 1，超分输出的 alpha 不参与显示
        let diff = shown
            .chunks_exact(4)
            .zip(out.chunks_exact(4))
            .flat_map(|(a, b)| (0..3).map(move |c| (a[c] as i32 - b[c] as i32).abs()))
            .max()
            .unwrap();
        eprintln!("原始 {:?} → 上屏 {:?}", &out[..4], &shown[..4]);
        assert!(diff <= 1, "1:1 上屏后像素被改动（最大差 {diff}）");

        let mut img = Mat::new_rows_cols_with_default(
            oh as i32,
            ow as i32,
            opencv::core::CV_8UC3,
            Scalar::all(0.0),
        )
        .unwrap();
        {
            let buf = img.data_bytes_mut().unwrap();
            for i in 0..(ow * oh) as usize {
                buf[i * 3] = shown[i * 4];
                buf[i * 3 + 1] = shown[i * 4 + 1];
                buf[i * 3 + 2] = shown[i * 4 + 2];
            }
        }
        imgcodecs::imwrite("target/nv_superres_show.png", &img, &Vector::<i32>::new()).unwrap();
        eprintln!(
            "仅超分链路通过：{w}x{h} → {ow}x{oh}（2x），1:1 上屏最大像素差 {diff}，已导出 target/nv_superres_show.png"
        );
    }

    /// 离线冒烟：input.png → 缩小到 512x288 → VSR 超分 → 导出 PNG
    ///
    /// `cargo test --bin animation_imagee nv_superres_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "需要 NVIDIA 显卡 + Video Effects 运行库，手动跑"]
    fn nv_superres_smoke() {
        use opencv::core::{AlgorithmHint, Mat, MatTraitConstManual, MatTraitManual, Scalar, Size, Vector};
        use opencv::{imgcodecs, imgproc};

        let (w, h) = (512i32, 288i32);
        let src = match imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("读不到 input.png: {e}");
                return;
            }
        };
        let mut small = Mat::default();
        imgproc::resize(&src, &mut small, Size::new(w, h), 0.0, 0.0, imgproc::INTER_AREA).unwrap();
        let mut bgra = Mat::default();
        imgproc::cvt_color(&small, &mut bgra, imgproc::COLOR_BGR2BGRA, 0, AlgorithmHint::ALGO_HINT_DEFAULT)
            .unwrap();
        let bytes = bgra.data_bytes().unwrap().to_vec();

        let mut sr = match SuperRes::new(4) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SuperRes 初始化失败: {e}");
                return;
            }
        };
        let started = std::time::Instant::now();
        let (out, ow, oh) = match sr.run_bgra(&bytes, w as u32, h as u32) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("超分失败: {e}");
                return;
            }
        };
        let first = started.elapsed().as_secs_f32() * 1000.0;
        // 第二帧排除引擎首次加载/编译
        let started = std::time::Instant::now();
        let _ = sr.run_bgra(&bytes, w as u32, h as u32).unwrap();
        let steady = started.elapsed().as_secs_f32() * 1000.0;

        // BGRA → BGR 存 PNG
        let mut img = Mat::new_rows_cols_with_default(oh as i32, ow as i32, opencv::core::CV_8UC3, Scalar::all(0.0))
            .unwrap();
        {
            let buf = img.data_bytes_mut().unwrap();
            for i in 0..(ow * oh) as usize {
                buf[i * 3] = out[i * 4];
                buf[i * 3 + 1] = out[i * 4 + 1];
                buf[i * 3 + 2] = out[i * 4 + 2];
            }
        }
        imgcodecs::imwrite("target/nv_superres_out.png", &img, &Vector::<i32>::new()).unwrap();
        eprintln!(
            "SuperRes {}x：{}x{} → {}x{}，首帧 {first:.1}ms，稳定 {steady:.1}ms，已导出 target/nv_superres_out.png",
            sr.scale, w, h, ow, oh
        );
    }
}
