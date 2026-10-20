//! 纹理直通（GPU 全程驻留，CPU 侧零整帧拷贝）
//!
//! 数据流：
//! ```text
//! 采集纹理(BGRA8, GPU)
//!   └─ D3D11 CS（缩放 + 归一化）──> 共享缓冲 A（D3D11 分配的显存）
//!                                     └─ D3D12 CopyBufferRegion（GPU 内拷贝，~0.05ms）
//!                                          └─ ORT/DirectML 的输入显存（ORT 自己分配，可直接绑定）
//!                                               DirectML 推理
//!                                          ┌─ ORT/DirectML 的输出显存
//!                                     ┌────┘  D3D12 CopyBufferRegion
//!   swapchain 后缓冲 <── D3D11 全屏三角形（顶点/像素着色器直接从缓冲取值）
//! ```
//!
//! 为什么不能「一块显存给 ORT 直接用」：
//! 1. ORT 的 `CreateGPUAllocationFromD3DResource` 包出来的张量交给 `IoBinding::bind_input` 会**死锁**
//!    （ORT DML 的 DataTransfer 只认自己 allocator 分配的显存）；
//! 2. 反过来，ORT 自己分配的显存无法导出共享句柄（不是 SHARED 堆，`CreateSharedHandle` 返回 E_INVALIDARG）。
//! 所以只能两端各用各的显存，中间由 D3D12 在 GPU 内部做两次 buffer 拷贝 —— CPU 完全不参与搬运。
//!
//! 注意：本模块只负责「推理 + 显示一帧」所需的全部 GPU 资源，抓帧见 `gpu_capture`。

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use ort::ep::directml::{DMLResource, DMLSessionBuilderExt};
use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{IoBinding, Session};
use ort::value::{Shape, Tensor, ValueType};
use windows::Win32::Foundation::{HANDLE, HMODULE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_BUFFER, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32_FLOAT,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCSTR};

/// ort 采用 `load-dynamic`：运行时加载自带的 DirectML 版 onnxruntime.dll。
/// 全局只初始化一次；优先用 ORT_DYLIB_PATH，其次 libs/ 与 exe 同目录。
fn ort_env() -> Result<&'static ort::environment::Environment, String> {
    static ENV: std::sync::OnceLock<Result<ort::environment::Environment, String>> =
        std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        let mut candidates = Vec::new();
        if let Some(p) = std::env::var_os("ORT_DYLIB_PATH") {
            candidates.push(std::path::PathBuf::from(p));
        }
        candidates.push(std::path::PathBuf::from("libs/onnxruntime.dll"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("onnxruntime.dll"));
                candidates.push(dir.join("libs").join("onnxruntime.dll"));
            }
        }
        let builder = match candidates.into_iter().find(|p| p.is_file()) {
            Some(path) => {
                log::info!("ONNX Runtime: {}", path.display());
                let shown = path.display().to_string();
                ort::init_from(&path).map_err(|e| format!("加载 {shown} 失败: {e}"))?
            }
            None => ort::init(),
        };
        builder
            .with_name("animation_imagee")
            .build()
            .map_err(|e| format!("初始化 ONNX Runtime 环境: {e}"))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// f32 → f16（仅测试用：造 HDR 测试纹理）。正常范围内的正规数，不处理非规格化/NaN。
#[cfg(test)]
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = (((bits >> 23) & 0xFF) as i32 - 127 + 15).clamp(1, 30) as u16;
    let mant = ((bits >> 13) & 0x3FF) as u16;
    sign | (exp << 10) | mant
}

/// f16 → f32（仅测试用：打印桌面复制帧的像素值）
#[cfg(test)]
fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1F) as u32;
    let mant = (h & 0x3FF) as u32;
    let bits = match (exp, mant) {
        (0, 0) => sign,
        (0, m) => {
            // 非规格化数：重新规格化到 f32
            let mut e = 127 - 15 + 1;
            let mut m = m;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((m & 0x3FF) << 13)
        }
        (0x1F, _) => sign | 0x7F80_0000 | (mant << 13),
        (e, _) => sign | ((e + 127 - 15) << 23) | (mant << 13),
    };
    f32::from_bits(bits)
}

/// 采集纹理 → f32 NHWC 归一化缓冲（一次 CS 派发完成缩放与通道转换）
///
/// 注意：`B8G8R8A8_UNORM` 只是**内存**排列，D3D 在采样时已按通道语义还原，
/// 所以 `c.r` 就是红、`c.b` 就是蓝，这里不能再手动交换（否则喂给模型的是 BGR）。
const CS_SRC: &str = r#"
Texture2D<float4> srcTex : register(t0);
RWBuffer<float> dstBuf : register(u0);
// 原图亮度（0..1 sRGB 域）：模型输出会提亮暗部，上屏时用它把亮度锚回原图
RWBuffer<float> lumaBuf : register(u1);
cbuffer Params : register(b0) {
    uint srcLeft; uint srcTop; uint srcW; uint srcH;
    uint dstW; uint dstH; uint texW; uint texH;
    uint toSrgb; uint exact; float hdrScale; float lumaAnchor;
};

// HDR（scRGB/FP16）桌面复制给的是线性值，需要编码回 sRGB
float3 linear_to_srgb(float3 c) {
    c = saturate(c);
    return lerp(c * 12.92, 1.055 * pow(max(c, 1e-6), 1.0 / 2.4) - 0.055, step(0.0031308, c));
}

[numthreads(8, 8, 1)]
void CSMain(uint3 id : SV_DispatchThreadID) {
    if (id.x >= dstW || id.y >= dstH || texW == 0 || texH == 0) return;

    // 目标像素在源图上覆盖的矩形，做面积平均（降采样时不丢细节、不闪烁）
    uint x0 = srcLeft + id.x * srcW / dstW;
    uint x1 = srcLeft + (id.x + 1u) * srcW / dstW;
    uint y0 = srcTop + id.y * srcH / dstH;
    uint y1 = srcTop + (id.y + 1u) * srcH / dstH;
    x1 = max(x1, x0 + 1u);
    y1 = max(y1, y0 + 1u);
    x0 = min(x0, texW - 1u);
    y0 = min(y0, texH - 1u);
    x1 = min(max(x1, x0 + 1u), texW);
    y1 = min(max(y1, y0 + 1u), texH);

    // 每个方向最多抽 4 次
    uint stepX = max(1u, (x1 - x0) / 4u);
    uint stepY = max(1u, (y1 - y0) / 4u);
    float3 sum = 0.0;
    uint n = 0u;
    for (uint sy = y0; sy < y1; sy += stepY) {
        for (uint sx = x0; sx < x1; sx += stepX) {
            sum += srcTex.Load(int3(sx, sy, 0)).rgb;
            n += 1u;
        }
    }
    float3 c = sum / max(n, 1u);
    if (toSrgb != 0u) {
        // scRGB 里 1.0 = 80nits，SDR 白点常被设成 200nits(≈2.5)，
        // 不先归一化就会整体抬亮 + 高光切白。
        c = saturate(c * hdrScale);
        c = linear_to_srgb(c);
    }

    uint idx = (id.y * dstW + id.x) * 3u;
    dstBuf[idx + 0u] = saturate(c.r) * 2.0 - 1.0; // R
    dstBuf[idx + 1u] = saturate(c.g) * 2.0 - 1.0; // G
    dstBuf[idx + 2u] = saturate(c.b) * 2.0 - 1.0; // B
    // 顺手记下原图亮度：风格化模型的输出会整体提亮暗部，上屏时按它拉回来
    lumaBuf[id.y * dstW + id.x] = dot(saturate(c), float3(0.299, 0.587, 0.114));
}
"#;

const VS_SRC: &str = r#"
cbuffer Params : register(b0) {
    uint srcLeft; uint srcTop; uint srcW; uint srcH;
    uint dstW; uint dstH; uint texW; uint texH;
    uint toSrgb; uint exact; float hdrScale; float lumaAnchor;
};
struct VSOut { float4 pos : SV_Position; };
VSOut VSMain(uint vid : SV_VertexID) {
    float2 p = float2((vid << 1) & 2, vid & 2);
    VSOut o;
    o.pos = float4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    return o;
}
"#;

/// 直接把推理结果画到后缓冲：Catmull-Rom 双三次放大，比最近邻/双线性锐利得多
const PS_SRC: &str = r#"
Buffer<float> srcBuf : register(t0);
// 原图亮度（由 CS 在送进模型时顺带写下）
Buffer<float> lumaBuf : register(t1);
cbuffer Params : register(b0) {
    uint srcLeft; uint srcTop; uint srcW; uint srcH;
    uint dstW; uint dstH; uint texW; uint texH;
    uint toSrgb; uint exact; float hdrScale; float lumaAnchor;
};

/// 亮度锚定：保留模型输出的色度比例，把亮度往原图拉
///   lumaAnchor = 1 → 完全用模型亮度（原始行为）
///   lumaAnchor = 0 → 亮度完全等于原图，模型只贡献颜色
float3 anchor_luma(float3 c01, float yOrig) {
    c01 = saturate(c01);
    float ym = dot(c01, float3(0.299, 0.587, 0.114));
    if (ym > 1e-4) {
        float yNew = yOrig + lumaAnchor * (ym - yOrig);
        c01 = saturate(c01 * (yNew / ym));
    }
    return c01;
}

float fetch_luma(int2 p) {
    p = clamp(p, int2(0, 0), int2((int)srcW - 1, (int)srcH - 1));
    return lumaBuf[(uint)p.y * srcW + (uint)p.x];
}

float3 fetch(int2 p) {
    p = clamp(p, int2(0, 0), int2((int)srcW - 1, (int)srcH - 1));
    uint idx = ((uint)p.y * srcW + (uint)p.x) * 3u;
    return float3(srcBuf[idx], srcBuf[idx + 1u], srcBuf[idx + 2u]);
}

void cr_weights(float t, out float w0, out float w1, out float w2, out float w3) {
    float t2 = t * t;
    float t3 = t2 * t;
    w0 = -0.5 * t3 + t2 - 0.5 * t;
    w1 = 1.5 * t3 - 2.5 * t2 + 1.0;
    w2 = -1.5 * t3 + 2.0 * t2 + 0.5 * t;
    w3 = 0.5 * t3 - 0.5 * t2;
}

float4 PSMain(float4 pos : SV_Position) : SV_Target {
    // 目标尺寸与源一致时直通：Catmull-Rom 在 t=0.5 是个轻微低通，1:1 也套它平白丢锐度
    if (exact != 0u) {
        int2 ip = int2(pos.xy);
        uint idx = ((uint)ip.y * srcW + (uint)ip.x) * 3u;
        float3 c01 = float3(srcBuf[idx], srcBuf[idx + 1u], srcBuf[idx + 2u]) * 0.5 + 0.5;
        return float4(anchor_luma(c01, fetch_luma(ip)), 1.0);
    }
    float2 p = pos.xy * float2(srcW, srcH) / float2(dstW, dstH) - 0.5;
    int2 base = (int2)floor(p);
    float2 f = p - (float2)base;

    float wx[4];
    float wy[4];
    cr_weights(f.x, wx[0], wx[1], wx[2], wx[3]);
    cr_weights(f.y, wy[0], wy[1], wy[2], wy[3]);

    float3 c = 0.0;
    [unroll] for (int j = 0; j < 4; ++j) {
        [unroll] for (int i = 0; i < 4; ++i) {
            c += fetch(base + int2(i - 1, j - 1)) * (wx[i] * wy[j]);
        }
    }
    // 模型输出是 tanh 的 [-1,1]，必须先映射回 [0,1]（c * 0.5 + 0.5）再上屏；
    // 直接 saturate 会把所有负值压成 0、正值砍半，整幅画面变成高对比死黑。
    return float4(anchor_luma(c * 0.5 + 0.5, fetch_luma(base)), 1.0);
}
"#;

/// 纹理 → 后缓冲：超分结果是现成的 BGRA 纹理，1:1 时直通，否则线性放大
const PS_TEX_SRC: &str = r#"
Texture2D<float4> srcTex : register(t0);
SamplerState linClamp : register(s0);
cbuffer Params : register(b0) {
    uint srcLeft; uint srcTop; uint srcW; uint srcH;
    uint dstW; uint dstH; uint texW; uint texH;
    uint toSrgb; uint exact; float hdrScale; float lumaAnchor;
};

float4 PSMain(float4 pos : SV_Position) : SV_Target {
    if (exact != 0u) {
        return float4(saturate(srcTex.Load(int3(int2(pos.xy), 0)).rgb), 1.0);
    }
    float2 uv = pos.xy / float2(dstW, dstH);
    return float4(saturate(srcTex.SampleLevel(linClamp, uv, 0).rgb), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    src_left: u32,
    src_top: u32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    /// 源纹理真实尺寸（桌面复制纹理可能和窗口/显示器矩形不一致，用它兜住越界）
    tex_w: u32,
    tex_h: u32,
    /// 1 = 源纹理是线性值（HDR scRGB），需要编码回 sRGB
    to_srgb: u32,
    /// 1 = 源与目标同尺寸，采样时直通（不做 Catmull-Rom 低通）
    exact: u32,
    /// HDR 下把 scRGB 线性值归一化到「SDR 白 = 1.0」的乘数（80 / SDRWhiteLevelNits）
    hdr_scale: f32,
    /// 亮度锚定强度：1 = 完全用模型亮度，0 = 亮度完全跟原图
    luma_anchor: f32,
}

/// 亮度锚定强度默认值。实测 AnimeGANv3 输出暗部比原图亮 ~10/255、亮部暗 ~13/255，
/// 这里把差值按比例保留一部分：0 = 亮度完全跟原图，1 = 完全跟模型（原始行为）。
const LUMA_ANCHOR: f32 = 0.3;

/// 允许用环境变量 `LUMA_ANCHOR` 覆盖（0..1），免得为了调这个值重新编译
fn luma_anchor() -> f32 {
    std::env::var("LUMA_ANCHOR")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(LUMA_ANCHOR)
}

fn shader_blob(src: &str, entry: &[u8], target: &[u8]) -> Result<ID3DBlob, String> {
    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            None,
            None,
            None,
            PCSTR::from_raw(entry.as_ptr()),
            PCSTR::from_raw(target.as_ptr()),
            0x0000_0003, // D3DCOMPILE_OPTIMIZATION_LEVEL3
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    match hr {
        Ok(()) => blob.ok_or_else(|| "D3DCompile 返回空 blob".to_string()),
        Err(e) => {
            let detail = err.map(|b| unsafe {
                let p = b.GetBufferPointer() as *const u8;
                let n = b.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned()
            });
            Err(format!("{e}: {}", detail.unwrap_or_default()))
        }
    }
}

unsafe fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

/// D3D11 分配、共享给 D3D12 的 buffer：
/// D3D11 侧用于 CS 写（输入）/ 显示着色器读（输出），D3D12 侧只作为 GPU 拷贝的源/目标。
struct SharedBuffer {
    d3d11: ID3D11Buffer,
    d3d12: ID3D12Resource,
    bytes: usize,
}

impl SharedBuffer {
    fn new(device11: &ID3D11Device, device12: &ID3D12Device, bytes: usize) -> Result<Self, String> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: bytes as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            // 实测：buffer 不支持 MISC_SHARED_NTHANDLE（E_INVALIDARG），只能用传统共享句柄
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
            StructureByteStride: 0,
        };
        let mut buffer: Option<ID3D11Buffer> = None;
        unsafe {
            device11
                .CreateBuffer(&desc, None, Some(&mut buffer))
                .map_err(|e| format!("CreateBuffer 失败: {e}"))?
        };
        let d3d11 = buffer.ok_or("CreateBuffer 返回空")?;

        let handle = unsafe {
            let res: IDXGIResource = d3d11.cast().map_err(|e| format!("IDXGIResource: {e}"))?;
            res.GetSharedHandle()
                .map_err(|e| format!("GetSharedHandle 失败: {e}"))?
        };

        let d3d12 = unsafe {
            let mut res: Option<ID3D12Resource> = None;
            device12
                .OpenSharedHandle(handle, &mut res)
                .map_err(|e| format!("D3D12 OpenSharedHandle 失败: {e}"))?;
            res.ok_or("OpenSharedHandle 返回空")?
        };

        Ok(Self { d3d11, d3d12, bytes })
    }
}

/// ORT 自己分配的显存（唯一能直接绑定到 IoBinding 的形式）
struct OrtTensors {
    /// 保持 ORT 输入张量存活（IoBinding 内部另有引用，这里兜底）
    _input: Tensor<f32>,
    input_res: ID3D12Resource,
    output_res: ID3D12Resource,
    _allocator: Allocator,
}

fn resource_of(tensor: &Tensor<f32>, allocator: &Allocator) -> Result<ID3D12Resource, String> {
    let ptr = tensor.data_ptr() as *mut c_void;
    if ptr.is_null() {
        return Err("ORT 张量数据指针为空".into());
    }
    // DMLResource 是 repr(transparent) 的裸指针包装；这里只借用，绝不能让它 Drop（会 FreeGPUAllocation）
    let dml: ManuallyDrop<DMLResource> = unsafe { ManuallyDrop::new(std::mem::transmute::<*mut c_void, DMLResource>(ptr)) };
    let raw = dml
        .d3d_resource(allocator)
        .map_err(|e| format!("GetD3D12ResourceFromAllocation: {e}"))?;
    // ORT 也持有该资源，AddRef 后再接管，避免双重释放
    let res = unsafe { ID3D12Resource::from_raw(raw.cast()) };
    let owned = res.clone();
    std::mem::forget(res);
    Ok(owned)
}

/// 在 ORT 的 D3D12 设备上自建一条拷贝通道（顺序执行，一次性提交 + 围栏等待）
struct CopyBridge {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    value: u64,
}

impl CopyBridge {
    fn new(device12: &ID3D12Device) -> Result<Self, String> {
        unsafe {
            let queue: ID3D12CommandQueue = device12
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    Priority: 0,
                    Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                    NodeMask: 0,
                })
                .map_err(|e| format!("CreateCommandQueue: {e}"))?;
            let allocator: ID3D12CommandAllocator = device12
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(|e| format!("CreateCommandAllocator: {e}"))?;
            let list: ID3D12GraphicsCommandList = device12
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .map_err(|e| format!("CreateCommandList: {e}"))?;
            list.Close().map_err(|e| format!("Close: {e}"))?;
            let fence: ID3D12Fence = device12
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| format!("CreateFence: {e}"))?;
            let event = CreateEventW(None, false, false, None)
                .map_err(|e| format!("CreateEventW: {e}"))?;
            Ok(Self {
                queue,
                allocator,
                list,
                fence,
                event,
                value: 0,
            })
        }
    }

    fn barrier(res: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: ManuallyDrop::new(Some(res.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    /// GPU 内部 buffer 拷贝：dst[0..bytes] = src[0..bytes]
    fn copy(&mut self, dst: &ID3D12Resource, src: &ID3D12Resource, bytes: u64) -> Result<(), String> {
        unsafe {
            self.allocator
                .Reset()
                .map_err(|e| format!("CommandAllocator::Reset: {e}"))?;
            self.list
                .Reset(&self.allocator, None)
                .map_err(|e| format!("CommandList::Reset: {e}"))?;
            self.list.ResourceBarrier(&[
                Self::barrier(src, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE),
                Self::barrier(dst, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST),
            ]);
            self.list.CopyBufferRegion(dst, 0, src, 0, bytes);
            self.list.ResourceBarrier(&[
                Self::barrier(src, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON),
                Self::barrier(dst, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON),
            ]);
            self.list.Close().map_err(|e| format!("CommandList::Close: {e}"))?;
            self.queue
                .ExecuteCommandLists(&[Some(self.list.cast().map_err(|e| format!("cast 命令列表: {e}"))?)]);
            self.value += 1;
            self.queue
                .Signal(&self.fence, self.value)
                .map_err(|e| format!("Queue::Signal: {e}"))?;
            if self.fence.GetCompletedValue() < self.value {
                self.fence
                    .SetEventOnCompletion(self.value, self.event)
                    .map_err(|e| format!("SetEventOnCompletion: {e}"))?;
                let ret = WaitForSingleObject(self.event, 2000);
                if ret != WAIT_OBJECT_0 {
                    return Err(format!("等待显存拷贝超时/失败: {ret:?}"));
                }
            }
        }
        Ok(())
    }
}

pub struct GpuStylePipeline {
    session: Session,
    binding: IoBinding,
    tensors: OrtTensors,
    bridge: CopyBridge,

    device11: ID3D11Device,
    ctx11: ID3D11DeviceContext,

    /// CS 写入的输入显存
    input: SharedBuffer,
    /// 显示着色器读取的输出显存
    output: SharedBuffer,
    uav_in: ID3D11UnorderedAccessView,
    srv_out: ID3D11ShaderResourceView,
    /// 原图亮度（CS 写 / 上屏 PS 读），用于把模型提亮的暗部拉回原图
    uav_luma: ID3D11UnorderedAccessView,
    srv_luma: ID3D11ShaderResourceView,
    cb: ID3D11Buffer,
    cs: ID3D11ComputeShader,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    /// 复用的同步事件查询（每帧新建一个 ID3D11Query 会白白拖慢主循环）
    query: ID3D11Query,
    /// HDR 桌面：scRGB 线性值的归一化乘数（1.0 = 不需要处理）
    sdr_white_scale: f32,

    pub width: u32,
    pub height: u32,
}

impl GpuStylePipeline {
    pub fn new(model_path: &str, width: u32, height: u32) -> Result<Self, String> {
        // 1) DirectML session；它内部那套 D3D12 设备也是我们后续建资源的地方
        let env = ort_env()?;
        let mut builder = Session::builder(env)
            .map_err(|e| format!("Session::builder: {e}"))?
            .with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()])
            .map_err(|e| format!("开启 DirectML 失败: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("优化级别: {e}"))?;

        let queue_ptr = builder
            .d3d_command_queue()
            .ok_or("这个 ONNX Runtime 构建不支持 DirectML")?
            .map_err(|e| format!("取 DML 命令队列失败: {e}"))?;
        // SAFETY: ORT 返回有效的 ID3D12CommandQueue*，这里加一次引用，避免误释放
        let ort_queue = unsafe { ID3D12CommandQueue::from_raw(queue_ptr.cast()) };
        let queue12 = ort_queue.clone();
        std::mem::forget(ort_queue);
        let mut device12_opt: Option<ID3D12Device> = None;
        unsafe { queue12.GetDevice(&mut device12_opt) }
            .map_err(|e| format!("CommandQueue::GetDevice: {e}"))?;
        let device12 = device12_opt.ok_or("取 DML D3D12 设备失败")?;

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| format!("加载模型失败: {e}"))?;

        let layout_ok = session.inputs().first().is_some_and(|o| {
            matches!(
                o.dtype(),
                ValueType::Tensor { shape, .. } if shape.len() == 4 && (shape[3] == 3 || shape[3] < 0)
            )
        });
        if !layout_ok {
            return Err("该直通路径只支持 NHWC [1,H,W,3] 输入模型".into());
        }

        // 模型自己声明的固定输入尺寸优先：固定尺寸的模型喂别的大小要么直接报错，
        // 要么被 DML 静默拉伸。只有 H/W 是动态（-1）时才用调用方给的值。
        let (width, height) = match session.inputs()[0].dtype() {
            ValueType::Tensor { shape, .. } if shape.len() == 4 => {
                let (mh, mw) = (shape[1], shape[2]);
                if mh > 0 && mw > 0 {
                    let (mw, mh) = (mw as u32, mh as u32);
                    if (mw, mh) != (width, height) {
                        log::info!(
                            "模型输入固定为 {mw}x{mh}，忽略请求的 {width}x{height}（按模型尺寸缩放）"
                        );
                    }
                    (mw, mh)
                } else {
                    (width, height)
                }
            }
            _ => (width, height),
        };

        let input_name = session.inputs()[0].name().to_string();
        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or("模型没有输出")?;
        match session.outputs()[0].dtype() {
            ValueType::Tensor { shape, .. } if shape.len() == 4 => {
                // 输入/输出共用同一块大小的显存，输出尺寸必须和输入一致
                if shape[1] > 0 && shape[2] > 0 && (shape[2] as u32, shape[1] as u32) != (width, height)
                {
                    return Err(format!(
                        "模型输出尺寸 {}x{} 与输入 {width}x{height} 不一致，这条直通路径不支持（只做等尺寸风格化）",
                        shape[2], shape[1]
                    ));
                }
            }
            other => return Err(format!("模型输出不是四维张量: {other:?}")),
        }

        // 2) ORT 自己分配的显存 + 绑定（这是唯一不会死锁的绑定方式）
        let bytes = width as usize * height as usize * 3 * size_of::<f32>();
        let mem = MemoryInfo::new(
            AllocationDevice::DIRECTML,
            0,
            AllocatorType::Device,
            MemoryType::Default,
        )
        .map_err(|e| format!("MemoryInfo: {e}"))?;
        let allocator = Allocator::new(&session, mem).map_err(|e| format!("创建 DML 分配器: {e}"))?;
        let shape_in = Shape::from([1usize, height as usize, width as usize, 3]);
        let input_tensor = Tensor::<f32>::new(&allocator, shape_in.clone())
            .map_err(|e| format!("分配输入显存: {e}"))?;
        let output_tensor = Tensor::<f32>::new(&allocator, shape_in.clone())
            .map_err(|e| format!("分配输出显存: {e}"))?;
        let input_res = resource_of(&input_tensor, &allocator)?;
        let output_res = resource_of(&output_tensor, &allocator)?;

        let mut binding = session
            .create_binding()
            .map_err(|e| format!("创建 IoBinding: {e}"))?;
        binding
            .bind_input(&input_name, &input_tensor)
            .map_err(|e| format!("绑定输入: {e}"))?;
        binding
            .bind_output(&output_name, output_tensor)
            .map_err(|e| format!("绑定输出: {e}"))?;

        // 3) 同一块显卡上的 D3D11 设备 + 我们自己的共享缓冲
        let luid = unsafe { device12.GetAdapterLuid() };
        let device11 = create_d3d11_device(luid)?;
        let ctx11 = unsafe { device11.GetImmediateContext() }
            .map_err(|e| format!("GetImmediateContext: {e}"))?;
        let input = SharedBuffer::new(&device11, &device12, bytes)?;
        let output = SharedBuffer::new(&device11, &device12, bytes)?;
        let bridge = CopyBridge::new(&device12)?;

        let mut uav: Option<ID3D11UnorderedAccessView> = None;
        let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_FLOAT,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: (bytes / 4) as u32,
                    Flags: 0,
                },
            },
        };
        unsafe {
            device11
                .CreateUnorderedAccessView(&input.d3d11, Some(&uav_desc), Some(&mut uav))
                .map_err(|e| format!("CreateUnorderedAccessView: {e}"))?;
        }

        let mut srv: Option<ID3D11ShaderResourceView> = None;
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_FLOAT,
            ViewDimension: D3D_SRV_DIMENSION_BUFFER,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_SRV {
                    Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                    Anonymous2: D3D11_BUFFER_SRV_1 {
                        NumElements: (bytes / 4) as u32,
                    },
                },
            },
        };
        unsafe {
            device11
                .CreateShaderResourceView(&output.d3d11, Some(&srv_desc), Some(&mut srv))
                .map_err(|e| format!("CreateShaderResourceView: {e}"))?;
        }

        // 原图亮度缓冲：CS 送进模型时顺手写，上屏 PS 用它把亮度锚回原图
        let mut luma: Option<ID3D11Buffer> = None;
        unsafe {
            device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: width * height * 4,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0
                            | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut luma),
                )
                .map_err(|e| format!("创建亮度缓冲失败: {e}"))?;
        }
        let luma = luma.ok_or("亮度缓冲创建失败")?;
        let luma_elems = width * height;
        let mut luma_uav: Option<ID3D11UnorderedAccessView> = None;
        let mut luma_srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            device11
                .CreateUnorderedAccessView(
                    &luma,
                    Some(&D3D11_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_FLOAT,
                        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D11_BUFFER_UAV {
                                FirstElement: 0,
                                NumElements: luma_elems,
                                Flags: 0,
                            },
                        },
                    }),
                    Some(&mut luma_uav),
                )
                .map_err(|e| format!("亮度缓冲 UAV: {e}"))?;
            device11
                .CreateShaderResourceView(
                    &luma,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_FLOAT,
                        ViewDimension: D3D_SRV_DIMENSION_BUFFER,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D11_BUFFER_SRV {
                                Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                                Anonymous2: D3D11_BUFFER_SRV_1 {
                                    NumElements: luma_elems,
                                },
                            },
                        },
                    }),
                    Some(&mut luma_srv),
                )
                .map_err(|e| format!("亮度缓冲 SRV: {e}"))?;
        }

        // 4) 着色器 + 常量缓冲
        let cs_blob = shader_blob(CS_SRC, b"CSMain\0", b"cs_5_0\0")?;
        let vs_blob = shader_blob(VS_SRC, b"VSMain\0", b"vs_5_0\0")?;
        let ps_blob = shader_blob(PS_SRC, b"PSMain\0", b"ps_5_0\0")?;
        let (mut cs, mut vs, mut ps) = (None, None, None);
        unsafe {
            device11
                .CreateComputeShader(blob_bytes(&cs_blob), None, Some(&mut cs))
                .map_err(|e| format!("CreateComputeShader: {e}"))?;
            device11
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .map_err(|e| format!("CreateVertexShader: {e}"))?;
            device11
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))
                .map_err(|e| format!("CreatePixelShader: {e}"))?;
        }

        let mut cb: Option<ID3D11Buffer> = None;
        unsafe {
            device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DYNAMIC,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut cb),
                )
                .map_err(|e| format!("创建常量缓冲失败: {e}"))?;
        }

        let mut query: Option<ID3D11Query> = None;
        unsafe {
            device11
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .map_err(|e| format!("CreateQuery: {e}"))?;
        }

        Ok(Self {
            session,
            binding,
            tensors: OrtTensors {
                _input: input_tensor,
                input_res,
                output_res,
                _allocator: allocator,
            },
            bridge,
            device11,
            ctx11,
            input,
            output,
            uav_in: uav.ok_or("UAV 创建失败")?,
            srv_out: srv.ok_or("SRV 创建失败")?,
            uav_luma: luma_uav.ok_or("亮度缓冲 UAV 创建失败")?,
            srv_luma: luma_srv.ok_or("亮度缓冲 SRV 创建失败")?,
            cb: cb.ok_or("常量缓冲创建失败")?,
            cs: cs.ok_or("CS 创建失败")?,
            vs: vs.ok_or("VS 创建失败")?,
            ps: ps.ok_or("PS 创建失败")?,
            query: query.ok_or("查询对象创建失败")?,
            sdr_white_scale: 1.0,
            width,
            height,
        })
    }

    pub fn device11(&self) -> &ID3D11Device {
        &self.device11
    }

    /// 设置 HDR 桌面的 scRGB → SDR 归一化乘数（80 / SDR 白点亮度）
    pub fn set_sdr_white_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.sdr_white_scale = scale;
        }
    }

    /// 采集纹理 → 输入显存（CS 一次派发：裁剪、缩放、归一化 NHWC）
    pub fn upload_texture(
        &mut self,
        src: &ID3D11Texture2D,
        src_left: u32,
        src_top: u32,
        src_w: u32,
        src_h: u32,
    ) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        let tex_w = desc.Width.max(1);
        let tex_h = desc.Height.max(1);
        // 桌面复制纹理的尺寸就是显示器像素尺寸；窗口矩形若因 DPI/多屏换算跑偏，
        // 这里兜住越界，避免采样到纹理外全黑。
        let left = src_left.min(tex_w - 1);
        let top = src_top.min(tex_h - 1);
        let w = src_w.clamp(1, tex_w - left);
        let h = src_h.clamp(1, tex_h - top);
        if left != src_left || top != src_top || w != src_w || h != src_h {
            eprintln!(
                "裁剪区域被裁到纹理范围内: 请求 ({src_left},{src_top},{src_w},{src_h}) → ({left},{top},{w},{h})，纹理 {tex_w}x{tex_h}"
            );
        }

        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device11
                .CreateShaderResourceView(src, None, Some(&mut srv))
                .map_err(|e| format!("采集纹理 SRV: {e}"))?;
        }
        let srv = srv.ok_or("采集纹理 SRV 为空")?;
        let params = Params {
            src_left: left,
            src_top: top,
            src_w: w,
            src_h: h,
            dst_w: self.width,
            dst_h: self.height,
            tex_w,
            tex_h,
            // HDR 桌面（scRGB）给的是线性 FP16，不转换会明显发灰/偏色
            to_srgb: matches!(desc.Format, DXGI_FORMAT_R16G16B16A16_FLOAT) as u32,
            exact: 0,
            hdr_scale: self.sdr_white_scale,
            luma_anchor: luma_anchor(),
        };
        self.update_params(&params)?;

        let ctx = &self.ctx11;
        unsafe {
            ctx.CSSetShader(&self.cs, None);
            ctx.CSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.CSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.CSSetUnorderedAccessViews(0, 1, Some(&[Some(self.uav_in.clone())] as *const _), None);
            // u1：原图亮度，CS 顺手写下来给上屏做亮度锚定
            ctx.CSSetUnorderedAccessViews(1, 1, Some(&[Some(self.uav_luma.clone())] as *const _), None);
            ctx.Dispatch(self.width.div_ceil(8), self.height.div_ceil(8), 1);
            // 立即解绑，避免同一资源同时被 SRV/UAV 绑定
            ctx.CSSetShaderResources(0, Some(&[None]));
            ctx.CSSetUnorderedAccessViews(0, 1, Some(&[None] as *const _), None);
            ctx.CSSetUnorderedAccessViews(1, 1, Some(&[None] as *const _), None);
            ctx.CSSetShader(None::<&ID3D11ComputeShader>, None);
        }
        Ok(())
    }

    fn update_params(&mut self, params: &Params) -> Result<(), String> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx11
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map 常量缓冲: {e}"))?;
            std::ptr::copy_nonoverlapping(
                params as *const Params as *const u8,
                mapped.pData as *mut u8,
                size_of::<Params>(),
            );
            self.ctx11.Unmap(&self.cb, 0);
        }
        Ok(())
    }

    /// 只等 GPU 完成，不搬任何数据
    pub fn sync(&mut self) -> Result<(), String> {
        let query = self.query.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        unsafe {
            self.ctx11.End(&query);
            self.ctx11.Flush();
            loop {
                let mut done: u32 = 0;
                match self.ctx11.GetData(
                    &query,
                    Some(&mut done as *mut u32 as *mut c_void),
                    4,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                ) {
                    Ok(()) if done != 0 => return Ok(()),
                    Ok(()) => {}
                    Err(e) => return Err(format!("等待 GPU 失败（设备可能已丢失）: {e}")),
                }
                if std::time::Instant::now() > deadline {
                    return Err("等待 GPU 完成超时（5s）".into());
                }
                std::thread::yield_now();
            }
        }
    }

    /// 一次完整推理：D3D11 输入显存 → ORT 输入显存 → DirectML → ORT 输出显存 → D3D11 输出显存
    pub fn infer(&mut self) -> Result<(), String> {
        let bytes = self.input.bytes as u64;
        self.sync()?;
        self.bridge
            .copy(&self.tensors.input_res, &self.input.d3d12, bytes)?;
        self.session
            .run_binding(&self.binding)
            .map_err(|e| format!("run_binding: {e}"))?;
        // DML 的推理实际跑在 ORT 自己的命令队列上，Run 返回并不保证显存已写完；
        // 必须显式同步，否则接下来读到的还是「上一帧」的结果。
        self.binding
            .synchronize()
            .map_err(|e| format!("synchronize: {e}"))?;
        self.bridge
            .copy(&self.output.d3d12, &self.tensors.output_res, bytes)?;
        Ok(())
    }

    /// 把输出显存画到传入的渲染目标（后缓冲），全屏三角形，不产生任何 CPU 拷贝
    pub fn present(&mut self, rtv: &ID3D11RenderTargetView, view_w: u32, view_h: u32) -> Result<(), String> {
        self.update_params(&Params {
            src_left: 0,
            src_top: 0,
            src_w: self.width,
            src_h: self.height,
            dst_w: view_w,
            dst_h: view_h,
            tex_w: self.width,
            tex_h: self.height,
            to_srgb: 0,
            // 源与目标同尺寸时直通（模型输出 → 同尺寸后缓冲是最常见的情况）
            exact: (view_w == self.width && view_h == self.height) as u32,
            hdr_scale: 1.0,
            luma_anchor: luma_anchor(),
        })?;
        let ctx = &self.ctx11;
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: view_w as f32,
                Height: view_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(self.srv_out.clone())]));
            // t1：原图亮度，用于把模型提亮的暗部拉回来
            ctx.PSSetShaderResources(1, Some(&[Some(self.srv_luma.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.PSSetShaderResources(1, Some(&[None]));
        }
        Ok(())
    }

}

/// 纹理级后处理：超分中转用的渲染目标、读回/上传、纹理上屏。
///
/// 不依赖模型（不需要 ORT session），所以「只超分」模式也能单独用；
/// 「模型 + 超分」模式下和 `GpuStylePipeline` 共用同一个 D3D11 设备。
pub struct GpuBlit {
    pub device11: ID3D11Device,
    ctx11: ID3D11DeviceContext,
    cb: ID3D11Buffer,
    vs: ID3D11VertexShader,
    ps_tex: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    query: ID3D11Query,
}

impl GpuBlit {
    pub fn new(device11: &ID3D11Device, ctx11: &ID3D11DeviceContext) -> Result<Self, String> {
        let vs_blob = shader_blob(VS_SRC, b"VSMain\0", b"vs_5_0\0")?;
        let ps_blob = shader_blob(PS_TEX_SRC, b"PSMain\0", b"ps_5_0\0")?;
        let mut vs: Option<ID3D11VertexShader> = None;
        let mut ps_tex: Option<ID3D11PixelShader> = None;
        let mut sampler: Option<ID3D11SamplerState> = None;
        let mut cb: Option<ID3D11Buffer> = None;
        let mut query: Option<ID3D11Query> = None;
        unsafe {
            device11
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .map_err(|e| format!("CreateVertexShader(blit): {e}"))?;
            device11
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps_tex))
                .map_err(|e| format!("CreatePixelShader(tex): {e}"))?;
            device11
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        ComparisonFunc: D3D11_COMPARISON_NEVER,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )
                .map_err(|e| format!("CreateSamplerState: {e}"))?;
            device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DYNAMIC,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut cb),
                )
                .map_err(|e| format!("创建常量缓冲(blit)失败: {e}"))?;
            device11
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .map_err(|e| format!("CreateQuery(blit): {e}"))?;
        }
        Ok(Self {
            device11: device11.clone(),
            ctx11: ctx11.clone(),
            cb: cb.ok_or("常量缓冲(blit)创建失败")?,
            vs: vs.ok_or("VS(blit) 创建失败")?,
            ps_tex: ps_tex.ok_or("PS(tex) 创建失败")?,
            sampler: sampler.ok_or("采样器创建失败")?,
            query: query.ok_or("查询对象(blit)创建失败")?,
        })
    }

    fn update_params(&mut self, params: &Params) -> Result<(), String> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx11
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map 常量缓冲(blit): {e}"))?;
            std::ptr::copy_nonoverlapping(
                params as *const Params as *const u8,
                mapped.pData as *mut u8,
                size_of::<Params>(),
            );
            self.ctx11.Unmap(&self.cb, 0);
        }
        Ok(())
    }

    /// 只等 GPU 完成，不搬数据
    pub fn sync(&mut self) -> Result<(), String> {
        let query = self.query.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        unsafe {
            self.ctx11.End(&query);
            self.ctx11.Flush();
            loop {
                let mut done: u32 = 0;
                match self.ctx11.GetData(
                    &query,
                    Some(&mut done as *mut u32 as *mut c_void),
                    4,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                ) {
                    Ok(()) if done != 0 => return Ok(()),
                    Ok(()) => {}
                    Err(e) => return Err(format!("等待 GPU(blit) 失败: {e}")),
                }
                if std::time::Instant::now() > deadline {
                    return Err("等待 GPU(blit) 完成超时（5s）".into());
                }
                std::thread::yield_now();
            }
        }
    }

    /// 建一张既能当渲染目标、又能被采样的 BGRA8 纹理（超分输入/输出中转用）
    pub fn create_target(
        &self,
        w: u32,
        h: u32,
    ) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), String> {
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    },
                    None,
                    Some(&mut tex),
                )
                .map_err(|e| format!("CreateTexture2D({w}x{h}): {e}"))?;
            let tex = tex.ok_or("目标纹理为空")?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device11
                .CreateRenderTargetView(&tex, None, Some(&mut rtv))
                .map_err(|e| format!("CreateRenderTargetView: {e}"))?;
            Ok((tex, rtv.ok_or("RTV 为空")?))
        }
    }

    /// 读回一张 BGRA8 纹理（紧密排列，行距 = w*4）
    pub fn read_bgra(&mut self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<Vec<u8>, String> {
        unsafe {
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    },
                    None,
                    Some(&mut staging),
                )
                .map_err(|e| format!("staging 纹理({w}x{h}): {e}"))?;
            let staging = staging.ok_or("staging 纹理为空")?;
            self.ctx11.CopyResource(&staging, tex);
            self.sync()?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map staging: {e}"))?;
            let row = (w * 4) as usize;
            let mut out = vec![0u8; row * h as usize];
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(y * row),
                    row,
                );
            }
            self.ctx11.Unmap(&staging, 0);
            Ok(out)
        }
    }

    /// 把 CPU 的 BGRA8 数据上传进纹理
    pub fn upload_bgra(
        &mut self,
        tex: &ID3D11Texture2D,
        data: &[u8],
        w: u32,
        h: u32,
    ) -> Result<(), String> {
        let need = (w as usize) * (h as usize) * 4;
        if data.len() < need {
            return Err(format!("上传数据不足：{} < {need}", data.len()));
        }
        unsafe {
            let res: ID3D11Resource = tex.cast().map_err(|e| format!("cast ID3D11Resource: {e}"))?;
            self.ctx11
                .UpdateSubresource(Some(&res), 0, None, data.as_ptr() as *const c_void, w * 4, 0);
        }
        Ok(())
    }

    /// 纹理 → 渲染目标：同尺寸直通，否则线性缩放（超分结果上屏）
    pub fn draw_texture(
        &mut self,
        tex: &ID3D11Texture2D,
        rtv: &ID3D11RenderTargetView,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<(), String> {
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device11
                .CreateShaderResourceView(tex, None, Some(&mut srv))
                .map_err(|e| format!("纹理 SRV: {e}"))?;
        }
        let srv = srv.ok_or("纹理 SRV 为空")?;
        self.update_params(&Params {
            src_left: 0,
            src_top: 0,
            src_w,
            src_h,
            dst_w,
            dst_h,
            tex_w: src_w,
            tex_h: src_h,
            to_srgb: 0,
            exact: (src_w == dst_w && src_h == dst_h) as u32,
            hdr_scale: 1.0,
            luma_anchor: luma_anchor(),
        })?;
        let ctx = &self.ctx11;
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: dst_w as f32,
                Height: dst_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetShader(&self.ps_tex, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }

    /// 把源纹理的某个子矩形拷进目标纹理（同尺寸，GPU 内部拷贝）
    pub fn copy_region(
        &mut self,
        dst: &ID3D11Texture2D,
        src: &ID3D11Texture2D,
        left: u32,
        top: u32,
        w: u32,
        h: u32,
    ) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            src.GetDesc(&mut desc);
        }
        // 越界就夹回纹理内，避免 CopySubresourceRegion 直接失败
        let left = left.min(desc.Width.saturating_sub(1));
        let top = top.min(desc.Height.saturating_sub(1));
        let w = w.clamp(1, desc.Width - left);
        let h = h.clamp(1, desc.Height - top);
        unsafe {
            self.ctx11.CopySubresourceRegion(
                dst,
                0,
                0,
                0,
                0,
                src,
                0,
                Some(&D3D11_BOX {
                    left,
                    top,
                    front: 0,
                    right: left + w,
                    bottom: top + h,
                    back: 1,
                }),
            );
        }
        Ok(())
    }

    /// 从纹理的某个子矩形读回 CPU（BGRA8，紧密排列）：抓窗口纹理要先裁掉边框。
    /// 仅超分路径目前走 copy_region + draw_texture（GPU 缩放），这里备用。
    #[allow(dead_code)]
    pub fn read_bgra_region(
        &mut self,
        tex: &ID3D11Texture2D,
        left: u32,
        top: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    },
                    None,
                    Some(&mut staging),
                )
                .map_err(|e| format!("staging 纹理({w}x{h}): {e}"))?;
            let staging = staging.ok_or("staging 纹理为空")?;
            self.ctx11.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                tex,
                0,
                Some(&D3D11_BOX {
                    left,
                    top,
                    front: 0,
                    right: left + w,
                    bottom: top + h,
                    back: 1,
                }),
            );
            self.sync()?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map staging: {e}"))?;
            let row = (w * 4) as usize;
            let mut out = vec![0u8; row * h as usize];
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(y * row),
                    row,
                );
            }
            self.ctx11.Unmap(&staging, 0);
            Ok(out)
        }
    }
}

impl GpuStylePipeline {
    /// 仅供自检：把模型输出显存存成 PNG（与上屏内容对照）
    #[cfg(test)]
    pub(crate) fn dump_output_png(&mut self, path: &str) -> Result<(), String> {
        use opencv::core::{CV_8UC3, Mat, MatTraitManual, Scalar, Vector};
        let data = self.readback()?;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut img = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0))
            .map_err(|e| format!("Mat: {e}"))?;
        {
            let buf = img.data_bytes_mut().map_err(|e| format!("data_bytes_mut: {e}"))?;
            let f = |v: f32| ((v.clamp(-1.0, 1.0) + 1.0) * 127.5 + 0.5) as u8;
            for (i, px) in data.chunks_exact(3).enumerate() {
                buf[i * 3] = f(px[2]);
                buf[i * 3 + 1] = f(px[1]);
                buf[i * 3 + 2] = f(px[0]);
            }
        }
        opencv::imgcodecs::imwrite(path, &img, &Vector::<i32>::new())
            .map_err(|e| format!("imwrite: {e}"))?;
        Ok(())
    }

    /// 仅供自检：把输出显存拷到 staging 再读回 CPU（生产路径不使用）
    #[cfg(test)]
    fn readback(&mut self) -> Result<Vec<f32>, String> {
        let mut staging: Option<ID3D11Buffer> = None;
        unsafe {
            self.device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: self.output.bytes as u32,
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut staging),
                )
                .map_err(|e| format!("staging buffer: {e}"))?;
        }
        let staging = staging.ok_or("staging 为空")?;
        unsafe {
            self.ctx11.CopyResource(&staging, &self.output.d3d11);
            self.sync()?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map staging: {e}"))?;
            let data = std::slice::from_raw_parts(
                mapped.pData as *const f32,
                self.output.bytes / size_of::<f32>(),
            )
            .to_vec();
            self.ctx11.Unmap(&staging, 0);
            Ok(data)
        }
    }

    /// 自检用：把任意 BGRA8 纹理（比如交换链后缓冲）存成 PNG，用于确认「真正上屏的像素」
    #[cfg(test)]
    pub(crate) fn dump_bgra_texture(&mut self, tex: &ID3D11Texture2D, path: &str) -> Result<(), String> {
        use opencv::core::{CV_8UC3, Mat, MatTraitManual, Scalar, Vector};
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let st = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device11
                .CreateTexture2D(&st, None, Some(&mut staging))
                .map_err(|e| format!("staging 纹理: {e}"))?;
        }
        let staging = staging.ok_or("staging 纹理为空")?;
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        unsafe {
            self.ctx11.CopyResource(&staging, tex);
            self.sync()?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map staging: {e}"))?;
            let mut img = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0))
                .map_err(|e| format!("Mat: {e}"))?;
            {
                let buf = img.data_bytes_mut().map_err(|e| format!("data_bytes_mut: {e}"))?;
                for y in 0..h {
                    let row = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                    for x in 0..w {
                        // 后缓冲是 BGRA8，Mat 要 BGR
                        let px = row.add(x * 4);
                        let o = (y * w + x) * 3;
                        buf[o] = *px;
                        buf[o + 1] = *px.add(1);
                        buf[o + 2] = *px.add(2);
                    }
                }
            }
            self.ctx11.Unmap(&staging, 0);
            opencv::imgcodecs::imwrite(path, &img, &Vector::<i32>::new())
                .map_err(|e| format!("imwrite: {e}"))?;
        }
        Ok(())
    }

    /// 自检用：造一张纯色 BGRA 纹理（u32 按小端写入，字节序是 B,G,R,A）
    #[cfg(test)]
    fn make_solid_texture(&self, bgra: u32, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
        let px: Vec<u32> = vec![bgra; (w * h) as usize];
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: px.as_ptr() as *const c_void,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    },
                    Some(&init),
                    Some(&mut tex),
                )
                .map_err(|e| format!("CreateTexture2D: {e}"))?;
        }
        tex.ok_or_else(|| "tex 为空".to_string())
    }

    /// 自检：用纯色纹理验证「CS → D3D12 拷贝 → DirectML → 拷贝回 → 可读」整条链路
    #[cfg(test)]
    fn run_with_solid_color(&mut self, bgra: u32) -> Result<(f32, Vec<f32>), String> {
        let (w, h) = (1600u32, 900u32);
        let tex = self.make_solid_texture(bgra, w, h)?;

        self.upload_texture(&tex, 0, 0, w, h)?;
        self.sync()?;
        let started = std::time::Instant::now();
        self.infer()?;
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        let data = self.readback()?;
        Ok((ms, data))
    }

    /// 自检用：造一张 R16G16B16A16_FLOAT（scRGB 线性）纹理，模拟 HDR 桌面复制的结果
    #[cfg(test)]
    fn make_texture_f16(&self, w: u32, h: u32, rgba_linear: &[f32]) -> Result<ID3D11Texture2D, String> {
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
        let half: Vec<u16> = rgba_linear
            .iter()
            .map(|&v| f32_to_f16(v))
            .collect();
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: half.as_ptr() as *const c_void,
            SysMemPitch: w * 8,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    },
                    Some(&init),
                    Some(&mut tex),
                )
                .map_err(|e| format!("CreateTexture2D(f16): {e}"))?;
        }
        let _ = half;
        tex.ok_or_else(|| "tex 为空".to_string())
    }

    /// 自检用：用给定的 BGRA 字节造纹理
    #[cfg(test)]
    fn make_texture(&self, w: u32, h: u32, bgra: &[u8]) -> Result<ID3D11Texture2D, String> {
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr() as *const c_void,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device11
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    },
                    Some(&init),
                    Some(&mut tex),
                )
                .map_err(|e| format!("CreateTexture2D: {e}"))?;
        }
        tex.ok_or_else(|| "tex 为空".to_string())
    }
}

/// 不跑模型（只做超分）时用的独立 D3D11 设备：默认适配器 + BGRA 支持
pub fn create_standalone_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let device = try_create_d3d11(
        LUID::default(),
        false,
        &[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0],
    )?;
    let ctx = unsafe { device.GetImmediateContext() }
        .map_err(|e| format!("GetImmediateContext: {e}"))?;
    Ok((device, ctx))
}

fn create_d3d11_device(luid: LUID) -> Result<ID3D11Device, String> {
    let mut errors = Vec::new();
    for use_luid in [true, false] {
        for levels in [
            &[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0][..],
            &[D3D_FEATURE_LEVEL_11_0][..],
        ] {
            match try_create_d3d11(luid, use_luid, levels) {
                Ok(device) => return Ok(device),
                Err(e) => errors.push(e),
            }
        }
    }
    Err(errors.join(" | "))
}

fn try_create_d3d11(
    luid: LUID,
    use_luid: bool,
    levels: &[D3D_FEATURE_LEVEL],
) -> Result<ID3D11Device, String> {
    unsafe {
        let adapter: Option<IDXGIAdapter> = if use_luid {
            let factory: IDXGIFactory4 = CreateDXGIFactory1::<IDXGIFactory4>()
                .map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
            Some(factory.EnumAdapterByLuid(luid).map_err(|e| {
                format!(
                    "EnumAdapterByLuid({:08X}:{:08X}): {e}",
                    luid.HighPart, luid.LowPart
                )
            })?)
        } else {
            None
        };
        let mut device: Option<ID3D11Device> = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        D3D11CreateDevice(
            adapter.as_ref(),
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            None,
        )
        .map_err(|e| format!("D3D11CreateDevice(luid={use_luid}, levels={levels:?}): {e}"))?;
        device.ok_or_else(|| "D3D11 设备返回空".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 采集侧自检：桌面复制 → CS → 直通推理，用真实屏幕内容验证（不打开窗口）
    #[test]
    fn capture_passthrough_smoke() {
        let mut pipeline = match GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", 512, 288) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("初始化失败: {e}");
                return;
            }
        };
        let center = windows::Win32::Foundation::POINT { x: 640, y: 360 };
        let (dup, monitor, _) = match crate::gpu_window::create_duplication(pipeline.device11(), center) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("桌面复制不可用: {e}");
                return;
            }
        };
        let w = (monitor.right - monitor.left) as u32;
        let h = (monitor.bottom - monitor.top) as u32;
        let mut last_ms = 0.0f32;
        for _ in 0..3 {
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                dup.AcquireNextFrame(500, &mut info, &mut resource).unwrap();
                let res = resource.expect("没有拿到桌面帧");
                let tex: ID3D11Texture2D = res.cast().unwrap();
                pipeline.upload_texture(&tex, 0, 0, w, h).unwrap();
                pipeline.sync().unwrap();
                dup.ReleaseFrame().unwrap();
            }
            let started = std::time::Instant::now();
            pipeline.infer().unwrap();
            last_ms = started.elapsed().as_secs_f32() * 1000.0;
        }
        eprintln!("采集 + 直通推理 {last_ms:.1} ms（第 3 帧，已排除 DML 首次编译）");
        let data = pipeline.readback().unwrap();
        let mean = data.iter().copied().sum::<f32>() / data.len() as f32;
        let nonzero = data.iter().filter(|v| v.abs() > 1e-4).count();
        eprintln!("输出均值 {mean:.4}，非零元素 {nonzero}/{}", data.len());
        assert!(nonzero > data.len() / 10, "输出几乎全 0，链路可能没通");
    }

    #[test]
    fn present_smoke() {
        match crate::gpu_window::selftest_frames(40) {
            Ok(ms) => eprintln!("上屏自检通过：平均 {ms:.1} ms/帧（含垂直同步）"),
            Err(e) => eprintln!("上屏自检失败: {e}"),
        }
    }

    /// 验证亮度锚定后的**最终上屏画面**：走 present 读回，再和原图分桶比亮度
    ///
    /// `cargo test --bin animation_imagee tone_anchor_check -- --ignored --nocapture`
    #[test]
    #[ignore = "手动排查用：需要模型 + GPU"]
    fn tone_anchor_check() {
        use opencv::core::{
            AlgorithmHint, Mat, MatTraitConstManual, MatTraitManual, Scalar, Size, Vector,
        };
        use opencv::{imgcodecs, imgproc};
        let (w, h) = (512usize, 288usize);
        let src = imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR).unwrap();
        let mut small = Mat::default();
        imgproc::resize(&src, &mut small, Size::new(w as i32, h as i32), 0.0, 0.0, imgproc::INTER_AREA)
            .unwrap();
        let mut bgra = Mat::default();
        imgproc::cvt_color(&small, &mut bgra, imgproc::COLOR_BGR2BGRA, 0, AlgorithmHint::ALGO_HINT_DEFAULT)
            .unwrap();
        let bytes = bgra.data_bytes().unwrap().to_vec();

        let mut p = GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w as u32, h as u32).unwrap();
        let tex = p.make_texture(w as u32, h as u32, &bytes).unwrap();
        p.upload_texture(&tex, 0, 0, w as u32, h as u32).unwrap();
        p.sync().unwrap();
        p.infer().unwrap();

        let ctx = unsafe { p.device11().GetImmediateContext().unwrap() };
        let mut blit = GpuBlit::new(p.device11(), &ctx).unwrap();
        let (rt, rtv) = blit.create_target(w as u32, h as u32).unwrap();
        p.present(&rtv, w as u32, h as u32).unwrap();
        let shown = blit.read_bgra(&rt, w as u32, h as u32).unwrap();

        let luma = |r: f32, g: f32, b: f32| 0.299 * r + 0.587 * g + 0.114 * b;
        let mut buckets = vec![[0f64; 3]; 4];
        for i in 0..w * h {
            let b = small.data_bytes().unwrap()[i * 3] as f32;
            let g = small.data_bytes().unwrap()[i * 3 + 1] as f32;
            let r = small.data_bytes().unwrap()[i * 3 + 2] as f32;
            let y_in = luma(r, g, b);
            let y_out = luma(
                shown[i * 4 + 2] as f32,
                shown[i * 4 + 1] as f32,
                shown[i * 4] as f32,
            );
            let idx = match y_in as u32 {
                0..=31 => 0,
                32..=63 => 1,
                64..=127 => 2,
                _ => 3,
            };
            buckets[idx][0] += y_in as f64;
            buckets[idx][1] += y_out as f64;
            buckets[idx][2] += 1.0;
        }
        eprintln!("上屏画面 vs 原图（lumaAnchor = {}）", luma_anchor());
        for (name, b) in ["0-31", "32-63", "64-127", "128-255"].iter().zip(buckets.iter()) {
            if b[2] > 0.0 {
                eprintln!(
                    "  {name:>8} → 原 {:6.1} / 上屏 {:6.1}（差 {:+.1}）",
                    b[0] / b[2],
                    b[1] / b[2],
                    (b[1] - b[0]) / b[2]
                );
            }
        }
        // 量化「有没有把值硬钳到 0 / 255」：统计触到两端的通道比例
        let (mut clipped_in, mut clipped_out, mut clipped_ch) = (0usize, 0usize, 0usize);
        let mut sum_abs = 0f64;
        for i in 0..w * h {
            let src = &small.data_bytes().unwrap()[i * 3..i * 3 + 3];
            let out = &shown[i * 4..i * 4 + 3];
            for c in 0..3 {
                if src[c] == 0 || src[c] == 255 {
                    clipped_in += 1;
                }
                if out[c] == 0 || out[c] == 255 {
                    clipped_out += 1;
                }
                sum_abs += (src[c] as f64 - out[c] as f64).abs();
                clipped_ch += 1;
            }
        }
        eprintln!(
            "触到 0/255 的通道：原图 {:.3}% / 上屏 {:.3}%；平均通道差 {:.2}",
            clipped_in as f64 * 100.0 / clipped_ch as f64,
            clipped_out as f64 * 100.0 / clipped_ch as f64,
            sum_abs / clipped_ch as f64
        );

        let mut img = Mat::new_rows_cols_with_default(
            h as i32,
            w as i32,
            opencv::core::CV_8UC3,
            Scalar::all(0.0),
        )
        .unwrap();
        {
            let buf = img.data_bytes_mut().unwrap();
            for i in 0..w * h {
                buf[i * 3] = shown[i * 4];
                buf[i * 3 + 1] = shown[i * 4 + 1];
                buf[i * 3 + 2] = shown[i * 4 + 2];
            }
        }
        imgcodecs::imwrite("target/tone_anchor.png", &img, &Vector::<i32>::new()).unwrap();
        eprintln!("已导出 target/tone_anchor.png");
    }

    /// 诊断：模型输出相对原图的亮度漂移（分桶统计，看暗部究竟被抬亮多少）
    ///
    /// `cargo test --bin animation_imagee tone_drift_model -- --ignored --nocapture`
    #[test]
    #[ignore = "手动排查用：需要模型 + GPU"]
    fn tone_drift_model() {
        use opencv::core::{AlgorithmHint, Mat, MatTraitConstManual, Size};
        use opencv::{imgcodecs, imgproc};
        let (w, h) = (512usize, 288usize);
        let src = imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR).unwrap();
        let mut small = Mat::default();
        imgproc::resize(&src, &mut small, Size::new(w as i32, h as i32), 0.0, 0.0, imgproc::INTER_AREA)
            .unwrap();
        let mut bgra = Mat::default();
        imgproc::cvt_color(&small, &mut bgra, imgproc::COLOR_BGR2BGRA, 0, AlgorithmHint::ALGO_HINT_DEFAULT)
            .unwrap();
        let bytes = bgra.data_bytes().unwrap().to_vec();

        let mut p = GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w as u32, h as u32).unwrap();
        let tex = p.make_texture(w as u32, h as u32, &bytes).unwrap();
        p.upload_texture(&tex, 0, 0, w as u32, h as u32).unwrap();
        p.sync().unwrap();
        p.infer().unwrap();
        let out = p.readback().unwrap();

        // 原图（BGR）与输出（NHWC RGB，[-1,1]）都换算成 0..255 的亮度
        let luma = |r: f32, g: f32, b: f32| 0.299 * r + 0.587 * g + 0.114 * b;
        let mut buckets = vec![[0f64; 4]; 4]; // [原均值, 输出均值, 计数, 0]
        for i in 0..w * h {
            let b = small.data_bytes().unwrap()[i * 3] as f32;
            let g = small.data_bytes().unwrap()[i * 3 + 1] as f32;
            let r = small.data_bytes().unwrap()[i * 3 + 2] as f32;
            let y_in = luma(r, g, b);
            let y_out = luma(
                (out[i * 3] + 1.0) * 127.5,
                (out[i * 3 + 1] + 1.0) * 127.5,
                (out[i * 3 + 2] + 1.0) * 127.5,
            );
            let idx = match y_in as u32 {
                0..=31 => 0,
                32..=63 => 1,
                64..=127 => 2,
                _ => 3,
            };
            buckets[idx][0] += y_in as f64;
            buckets[idx][1] += y_out as f64;
            buckets[idx][2] += 1.0;
        }
        eprintln!("原图亮度区间 → 原均值 / 输出均值 / 像素数");
        for (i, (name, b)) in ["0-31", "32-63", "64-127", "128-255"]
            .iter()
            .zip(buckets.iter())
            .enumerate()
        {
            if b[2] > 0.0 {
                eprintln!(
                    "  {name:>8} → {:6.1} / {:6.1} / {:8.0}（差 {:+.1}）",
                    b[0] / b[2],
                    b[1] / b[2],
                    b[2],
                    (b[1] - b[0]) / b[2]
                );
            } else {
                eprintln!("  {name:>8} → 无像素（bucket {i}）");
            }
        }
        let _ = Mat::default();
    }

    /// 离屏对照（手动排查用）：真实截图分别走 GPU 直通和 CPU 参考，导出 PNG 便于肉眼比对
    ///
    /// `cargo test --bin animation_imagee dump_style_preview -- --ignored --nocapture`
    #[test]
    #[ignore = "手动排查用，会导出 PNG 到 target/"]
    fn dump_style_preview() {
        use opencv::core::{AlgorithmHint, CV_8UC3, Mat, MatTraitConstManual, MatTraitManual, Scalar, Size, Vector};
        use opencv::{imgcodecs, imgproc};
        use ort::value::TensorRef;
        let (w, h) = (1024usize, 576usize);
        let src = imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR).unwrap();
        let mut resized = Mat::default();
        imgproc::resize(
            &src,
            &mut resized,
            Size::new(w as i32, h as i32),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )
        .unwrap();
        let mut bgra = Mat::default();
        imgproc::cvt_color(&resized, &mut bgra, imgproc::COLOR_BGR2BGRA, 0, AlgorithmHint::ALGO_HINT_DEFAULT).unwrap();
        imgcodecs::imwrite("target/check_input.png", &resized, &Vector::<i32>::new()).unwrap();
        let bgra_bytes = bgra.data_bytes().unwrap().to_vec();

        let save = |data: &[f32], path: &str| {
            let f = |v: f32| ((v + 1.0) * 127.5).clamp(0.0, 255.0) as u8;
            let mut img =
                Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0))
                    .unwrap();
            {
                let buf = img.data_bytes_mut().unwrap();
                for (i, px) in data.chunks_exact(3).enumerate() {
                    buf[i * 3] = f(px[2]); // B
                    buf[i * 3 + 1] = f(px[1]); // G
                    buf[i * 3 + 2] = f(px[0]); // R
                }
            }
            imgcodecs::imwrite(path, &img, &Vector::<i32>::new()).unwrap();
        };
        let mean_rgb = |d: &[f32]| {
            let n = (d.len() / 3) as f64;
            let mut m = [0.0f64; 3];
            for px in d.chunks_exact(3) {
                for c in 0..3 {
                    m[c] += px[c] as f64;
                }
            }
            [m[0] / n, m[1] / n, m[2] / n]
        };

        // CPU 参考：BGR Mat → NHWC RGB
        let bgr = resized.data_bytes().unwrap();
        let mut input = vec![0.0f32; w * h * 3];
        for (i, px) in bgr.chunks_exact(3).enumerate() {
            input[i * 3] = px[2] as f32 / 127.5 - 1.0;
            input[i * 3 + 1] = px[1] as f32 / 127.5 - 1.0;
            input[i * 3 + 2] = px[0] as f32 / 127.5 - 1.0;
        }
        let env = ort_env().unwrap();
        let mut cpu = Session::builder(env)
            .unwrap()
            .commit_from_file("AnimeGANv3_Hayao_36.onnx")
            .unwrap();
        let tensor = TensorRef::from_array_view(([1usize, h, w, 3], &input[..])).unwrap();
        let outs = cpu.run(ort::inputs![tensor]).unwrap();
        let (_, reference) = outs[0].try_extract_tensor::<f32>().unwrap();
        save(reference, "target/check_cpu.png");

        // GPU 直通
        let mut p = GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w as u32, h as u32).unwrap();
        let tex = p.make_texture(w as u32, h as u32, &bgra_bytes).unwrap();
        p.upload_texture(&tex, 0, 0, w as u32, h as u32).unwrap();
        p.sync().unwrap();
        p.infer().unwrap();
        let gpu = p.readback().unwrap();
        save(&gpu, "target/check_gpu.png");

        let diff: f32 = gpu
            .iter()
            .zip(reference)
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / gpu.len() as f32;
        eprintln!("输入均值 {:?}", mean_rgb(&input));
        eprintln!("CPU 输出均值 {:?}", mean_rgb(reference));
        eprintln!("GPU 输出均值 {:?}", mean_rgb(&gpu));
        eprintln!("GPU vs CPU 平均绝对差 {diff:.5}");
        eprintln!("已导出 target/check_input.png / check_cpu.png / check_gpu.png");
    }

    /// 已经是 sRGB 编码的值 → 8bit
    fn to_u8(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }

    fn srgb_to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// HDR 自检：模拟 scRGB 桌面（SDR 白点 ≈192nits → 线性值约 2.4），
    /// 经过 CS 归一化 + sRGB 编码后，模型输入应当还原成原始 sRGB 画面。
    #[test]
    fn hdr_scrgb_roundtrip() {
        use opencv::core::{AlgorithmHint, CV_8UC3, Mat, MatTraitConstManual, MatTraitManual, Size, Vector};
        use opencv::{imgcodecs, imgproc};
        let (w, h) = (512usize, 288usize);

        let src = imgcodecs::imread("input.png", imgcodecs::IMREAD_COLOR).unwrap();
        let mut resized = Mat::default();
        imgproc::resize(&src, &mut resized, Size::new(w as i32, h as i32), 0.0, 0.0, imgproc::INTER_AREA)
            .unwrap();
        let bgr = resized.data_bytes().unwrap().to_vec();

        // 假设 SDR 白点 192nits → scRGB 里 SDR 白 = 2.4
        let white = 2.4f32;
        let mut lin = vec![0.0f32; w * h * 4];
        for i in 0..w * h {
            let to_lin = |v: u8| srgb_to_linear(v as f32 / 255.0) * white;
            lin[i * 4] = to_lin(bgr[i * 3 + 2]); // R
            lin[i * 4 + 1] = to_lin(bgr[i * 3 + 1]); // G
            lin[i * 4 + 2] = to_lin(bgr[i * 3]); // B
            lin[i * 4 + 3] = 1.0;
        }

        let mut p = GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w as u32, h as u32).unwrap();
        p.set_sdr_white_scale(1.0 / white);
        let tex = p.make_texture_f16(w as u32, h as u32, &lin).unwrap();
        p.upload_texture(&tex, 0, 0, w as u32, h as u32).unwrap();
        p.sync().unwrap();

        // 读回模型输入
        let bytes = p.input.bytes;
        let mut staging: Option<ID3D11Buffer> = None;
        unsafe {
            p.device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: bytes as u32,
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut staging),
                )
                .unwrap();
        }
        let staging = staging.unwrap();
        unsafe {
            p.ctx11.CopyResource(&staging, &p.input.d3d11);
            p.sync().unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            p.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let data = std::slice::from_raw_parts(
                mapped.pData as *const f32,
                bytes / size_of::<f32>(),
            )
            .to_vec();
            p.ctx11.Unmap(&staging, 0);

            let mut out = vec![0u8; w * h * 3];
            let mut max_diff = 0i32;
            for i in 0..w * h {
                let f = |v: f32| ((v.clamp(0.0, 1.0) * 255.0) + 0.5) as i32;
                let r = f((data[i * 3] + 1.0) * 0.5);
                let g = f((data[i * 3 + 1] + 1.0) * 0.5);
                let b = f((data[i * 3 + 2] + 1.0) * 0.5);
                out[i * 3] = b as u8;
                out[i * 3 + 1] = g as u8;
                out[i * 3 + 2] = r as u8;
                for (a, e) in [(r, 2usize), (g, 1), (b, 0)] {
                    max_diff = max_diff.max((a - bgr[i * 3 + e] as i32).abs());
                }
            }
            let mut img =
                Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, opencv::core::Scalar::all(0.0))
                    .unwrap();
            img.data_bytes_mut().unwrap().copy_from_slice(&out);
            imgcodecs::imwrite("target/check_hdr_model_input.png", &img, &Vector::<i32>::new())
                .unwrap();
            let mean_diff = (0..w * h)
                .map(|i| {
                    (0..3)
                        .map(|c| (out[i * 3 + c] as i32 - bgr[i * 3 + c] as i32).abs())
                        .sum::<i32>() as f64
                })
                .sum::<f64>()
                / (w * h * 3) as f64;
            eprintln!("HDR 往返：模型输入与原始 sRGB 平均差 {mean_diff:.2}/255，最大差 {max_diff}");
            assert!(mean_diff < 3.0, "HDR 归一化后与原始画面对不上（{mean_diff:.2}）");
        }
        let _ = AlgorithmHint::ALGO_HINT_DEFAULT;

        // 再跑一遍推理，导出一张 HDR 通路下的风格化结果供肉眼比对
        p.infer().unwrap();
        let data = p.readback().unwrap();
        let mut out = vec![0u8; w * h * 3];
        for (i, px) in data.chunks_exact(3).enumerate() {
            out[i * 3] = to_u8((px[2] + 1.0) * 0.5);
            out[i * 3 + 1] = to_u8((px[1] + 1.0) * 0.5);
            out[i * 3 + 2] = to_u8((px[0] + 1.0) * 0.5);
        }
        {
            let mut img = Mat::new_rows_cols_with_default(
                h as i32,
                w as i32,
                CV_8UC3,
                opencv::core::Scalar::all(0.0),
            )
            .unwrap();
            img.data_bytes_mut().unwrap().copy_from_slice(&out);
            imgcodecs::imwrite("target/check_hdr_styled.png", &img, &Vector::<i32>::new()).unwrap();
        }
        eprintln!("已导出 target/check_hdr_model_input.png / check_hdr_styled.png");
    }

    fn to_srgb_u8(v: f32) -> u8 {
        let c = v.clamp(0.0, 1.0);
        let s = if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    }

    fn save_bgr(data: &[u8], w: usize, h: usize, path: &str) {
        use opencv::core::{CV_8UC3, Mat, MatTraitManual, Scalar, Vector};
        use opencv::imgcodecs;
        let mut img = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0))
            .unwrap();
        img.data_bytes_mut().unwrap().copy_from_slice(&data[..w * h * 3]);
        imgcodecs::imwrite(path, &img, &Vector::<i32>::new()).unwrap();
    }

    /// 离屏对照（手动排查用）：抓一帧真实桌面，导出「原始帧」「模型输入」「风格化输出」
    ///
    /// `cargo test --bin animation_imagee dump_capture_check -- --ignored --nocapture`
    /// 游戏/桌面可见时运行，可据此判断偏色出在采集、CS 还是模型。
    #[test]
    #[ignore = "手动排查用，会导出 PNG 到 target/"]
    fn dump_capture_check() {
        let (w, h) = (1024u32, 576u32);
        let mut p = match GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w, h) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("初始化失败: {e}");
                return;
            }
        };
        let center = windows::Win32::Foundation::POINT { x: 640, y: 360 };
        let (dup, monitor, display_name) = match crate::gpu_window::create_duplication(p.device11(), center) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("桌面复制不可用: {e}");
                return;
            }
        };
        let mw = (monitor.right - monitor.left) as u32;
        let mh = (monitor.bottom - monitor.top) as u32;
        let hdr = crate::gpu_window::hdr_info(&display_name);
        let white_scale = hdr.white_scale;
        p.set_sdr_white_scale(white_scale);
        eprintln!(
            "{display_name}: HDR={} SDR 白点乘数 {white_scale:.3}（≈{:.0} nits）",
            hdr.is_hdr,
            80.0 / white_scale
        );
        if let Some(ci) = crate::gpu_window::output_color_info(&display_name) {
            eprintln!("输出色彩: {ci:?}");
        }

        unsafe {
            // 新建复制后的前几帧是空白的，先丢几帧拿到真实内容
            for _ in 0..5 {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                if dup.AcquireNextFrame(500, &mut info, &mut resource).is_ok() {
                    let _ = dup.ReleaseFrame();
                }
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            dup.AcquireNextFrame(1000, &mut info, &mut resource).unwrap();
            let res = resource.expect("没有拿到桌面帧");
            let tex: ID3D11Texture2D = res.cast().unwrap();
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            let ddesc = dup.GetDesc();
            let bpp = if matches!(desc.Format, DXGI_FORMAT_R16G16B16A16_FLOAT) { 8 } else { 4 };
            eprintln!(
                "桌面复制纹理 {}x{} 格式 {}，旋转 {}，每像素 {bpp} 字节",
                desc.Width, desc.Height, desc.Format.0, ddesc.Rotation.0
            );

            let mut st = desc;
            st.Usage = D3D11_USAGE_STAGING;
            st.BindFlags = 0;
            st.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            st.MiscFlags = 0;
            let mut staging: Option<ID3D11Texture2D> = None;
            p.device11.CreateTexture2D(&st, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();
            p.ctx11.CopyResource(&staging, &tex);
            p.sync().unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            p.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let row_bytes = (desc.Width * bpp) as usize;
            let mut packed = vec![0u8; row_bytes * desc.Height as usize];
            for y in 0..desc.Height {
                let src = (mapped.pData as *const u8).add((y * mapped.RowPitch) as usize);
                let dst = packed.as_mut_ptr().add(y as usize * row_bytes);
                std::ptr::copy_nonoverlapping(src, dst, row_bytes);
            }
            p.ctx11.Unmap(&staging, 0);

            // 原始帧按「线性 → sRGB」正确解码后保存，作为肉眼基准
            let full_w = desc.Width as usize;
            let full_h = desc.Height as usize;
            let mut raw_bgr = vec![0u8; full_w * full_h * 3];
            if bpp == 8 {
                for i in 0..full_w * full_h {
                    let px = &packed[i * 8..i * 8 + 8];
                    let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
                    let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
                    let b = f16_to_f32(u16::from_le_bytes([px[4], px[5]]));
                    raw_bgr[i * 3] = to_srgb_u8(b * white_scale);
                    raw_bgr[i * 3 + 1] = to_srgb_u8(g * white_scale);
                    raw_bgr[i * 3 + 2] = to_srgb_u8(r * white_scale);
                }
            } else {
                for i in 0..full_w * full_h {
                    raw_bgr[i * 3] = packed[i * 4];
                    raw_bgr[i * 3 + 1] = packed[i * 4 + 1];
                    raw_bgr[i * 3 + 2] = packed[i * 4 + 2];
                }
            }
            save_bgr(&raw_bgr, full_w, full_h, "target/check_capture_raw.png");
            if bpp == 8 {
                let sample = |pix: usize| -> (u16, u16, u16) {
                    let o = pix * 8;
                    (
                        u16::from_le_bytes([packed[o], packed[o + 1]]),
                        u16::from_le_bytes([packed[o + 2], packed[o + 3]]),
                        u16::from_le_bytes([packed[o + 4], packed[o + 5]]),
                    )
                };
                let mid = (full_h / 2) * full_w + full_w / 2;
                for (tag, pix) in [("左上", 0usize), ("中间", mid), ("右下", full_w * full_h - 1)] {
                    let (r, g, b) = sample(pix);
                    eprintln!(
                        "原始帧 {tag}: u16=({r:#06x},{g:#06x},{b:#06x}) → ({:.3},{:.3},{:.3})",
                        f16_to_f32(r),
                        f16_to_f32(g),
                        f16_to_f32(b)
                    );
                }
            }

            p.upload_texture(&tex, 0, 0, mw, mh).unwrap();
            p.sync().unwrap();
            dup.ReleaseFrame().unwrap();
        }

        // ② 模型输入（CS 写进显存的东西）
        unsafe {
            let bytes = p.input.bytes;
            let mut staging: Option<ID3D11Buffer> = None;
            p.device11
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: bytes as u32,
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut staging),
                )
                .unwrap();
            let staging = staging.unwrap();
            p.ctx11.CopyResource(&staging, &p.input.d3d11);
            p.sync().unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            p.ctx11
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let data = std::slice::from_raw_parts(
                mapped.pData as *const f32,
                bytes / size_of::<f32>(),
            )
            .to_vec();
            p.ctx11.Unmap(&staging, 0);
            let mut bgr = vec![0u8; (w * h * 3) as usize];
            for (i, px) in data.chunks_exact(3).enumerate() {
                bgr[i * 3] = to_u8((px[2] + 1.0) * 0.5);
                bgr[i * 3 + 1] = to_u8((px[1] + 1.0) * 0.5);
                bgr[i * 3 + 2] = to_u8((px[0] + 1.0) * 0.5);
            }
            save_bgr(&bgr, w as usize, h as usize, "target/check_model_input.png");
            eprintln!("模型输入前 6 个 f32: {:?}", &data[0..6]);
            let mean = |c: usize| {
                data.chunks_exact(3).map(|p| p[c] as f64).sum::<f64>() / (data.len() / 3) as f64
            };
            eprintln!(
                "模型输入均值 R={:.3} G={:.3} B={:.3}",
                mean(0),
                mean(1),
                mean(2)
            );
        }

        p.infer().unwrap();
        let data = p.readback().unwrap();
        let mut bgr = vec![0u8; (w * h * 3) as usize];
        for (i, px) in data.chunks_exact(3).enumerate() {
            bgr[i * 3] = to_u8((px[2] + 1.0) * 0.5);
            bgr[i * 3 + 1] = to_u8((px[1] + 1.0) * 0.5);
            bgr[i * 3 + 2] = to_u8((px[0] + 1.0) * 0.5);
        }
        save_bgr(&bgr, w as usize, h as usize, "target/check_capture_styled.png");
        eprintln!("已导出 target/check_capture_raw.png / check_model_input.png / check_capture_styled.png");
    }

    /// 通道顺序 + 一帧延迟自检：
    /// 把同一组纯色同时喂给 CPU 参考实现和 GPU 直通，通道均值必须对得上。
    /// 曾经的两个坑都会在这里暴露：
    /// 1. CS 里把 BGRA 的 `.b` 当红、`.r` 当蓝（模型吃到 BGR，成片偏色）；
    /// 2. `run_binding` 后没有同步 ORT 的命令队列（读到的永远是上一帧）。
    #[test]
    fn matches_cpu_reference() {
        use ort::value::TensorRef;
        let (w, h) = (512usize, 288usize);
        let mut p = match GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", w as u32, h as u32) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("初始化失败（无 GPU/驱动时属正常）: {e}");
                return;
            }
        };
        let env = ort_env().unwrap();
        let mut cpu = Session::builder(env)
            .unwrap()
            .commit_from_file("AnimeGANv3_Hayao_36.onnx")
            .unwrap();
        let mean_rgb = |d: &[f32]| {
            let n = (d.len() / 3) as f64;
            let mut m = [0.0f64; 3];
            for px in d.chunks_exact(3) {
                for c in 0..3 {
                    m[c] += px[c] as f64;
                }
            }
            [m[0] / n, m[1] / n, m[2] / n]
        };
        // (名字, 与 CS 输出一致的归一化 RGB, BGRA 纹理的 u32)
        let cases = [
            ("红", [1.0f32, -1.0, -1.0], 0xFF_FF_00_00u32),
            ("蓝", [-1.0f32, -1.0, 1.0], 0xFF_00_00_FFu32),
        ];
        for (name, rgb, bgra) in cases {
            let mut data = vec![0.0f32; w * h * 3];
            for px in data.chunks_exact_mut(3) {
                px.copy_from_slice(&rgb);
            }
            let input = TensorRef::from_array_view(([1usize, h, w, 3], &data[..])).unwrap();
            let outs = cpu.run(ort::inputs![input]).unwrap();
            let (_, reference) = outs[0].try_extract_tensor::<f32>().unwrap();
            let cpu_mean = mean_rgb(reference);

            let (ms, gpu) = p.run_with_solid_color(bgra).unwrap();
            let gpu_mean = mean_rgb(&gpu);
            eprintln!(
                "{name}: CPU {cpu_mean:?} / GPU {gpu_mean:?}（{ms:.1}ms，含同步）"
            );
            for c in 0..3 {
                assert!(
                    (cpu_mean[c] - gpu_mean[c]).abs() < 0.02,
                    "{name} 第 {c} 通道与 CPU 参考对不上：CPU {cpu_mean:?} / GPU {gpu_mean:?}"
                );
            }
            if name == "红" {
                assert!(gpu_mean[0] > gpu_mean[2], "红输入却输出偏蓝：R/B 通道反了");
            } else {
                assert!(gpu_mean[2] > gpu_mean[0], "蓝输入却输出偏红：R/B 通道反了");
            }
        }
    }

    #[test]
    fn gpu_zero_copy_smoke() {
        let mut pipeline = match GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", 512, 288) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("初始化失败（无 GPU/驱动时属正常）: {e}");
                return;
            }
        };
        // 首次推理包含 DirectML 算子编译（~1s），先热身一次，避免把编译时间算进延迟
        let _ = pipeline.run_with_solid_color(0xFF_80_80_80).unwrap();
        let (_, _) = pipeline.run_with_solid_color(0xFF_80_80_80).unwrap();
        // 纹理内存是 B,G,R,A 小端排列：0xFF_FF_00_00 = 红，0xFF_00_00_FF = 蓝
        let (ms_b, blue) = pipeline.run_with_solid_color(0xFF_FF_00_00).unwrap();
        let (ms_r, red) = pipeline.run_with_solid_color(0xFF_00_00_FF).unwrap();
        let mean = |d: &[f32]| d.iter().copied().sum::<f32>() / d.len() as f32;
        eprintln!(
            "零拷贝推理 {ms_b:.1}/{ms_r:.1} ms | 均值 红={:.4} 蓝={:.4}",
            mean(&blue),
            mean(&red)
        );
        let diff: f32 = blue.iter().zip(&red).map(|(a, b)| (a - b).abs()).sum::<f32>()
            / blue.len() as f32;
        eprintln!("两次输入不同的平均差 {diff:.4}");
        assert!(blue.iter().any(|v| v.abs() > 1e-3), "输出全 0：DirectML 没写进显存");
        assert!(diff > 1e-3, "不同输入得到相同输出：输入显存没真正传进去（{diff}）");
    }
}
