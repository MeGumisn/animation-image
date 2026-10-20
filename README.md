# animation_imagee

把一个已存在的窗口（游戏、播放器、任何顶层窗口）**实时**捕获下来，做风格化推理和 / 或 AI 超分，然后在另一个独立窗口里上屏。

```
目标窗口 ──WGC──> 纹理 ──CS 缩放/归一化──> AnimeGANv3 (DirectML)
                                          │
                           ┌──────────────┴──────────────┐
                           │                             │
                     直接上屏（仅模型）            缩放到「超分输入」
                                                         │
                                              NVIDIA Upscale（2/3/4x）
                                                         │
                                                    1:1 上屏
```

- 捕获走 **WGC**（Windows Graphics Capture），从窗口的重定向表面取内容，**被遮挡也有画面**，不依赖屏幕坐标。
- 推理走 **ONNX Runtime + DirectML**，全程 GPU，模型输入/输出都是显存里的 buffer（零拷贝）。
- 超分走 **NVIDIA Video Effects SDK 的 `Upscale`**（可选，需要 RTX 卡）。

---

## 1. 运行环境（使用打包好的 dist）

| 项 | 要求 |
|---|---|
| 系统 | **Windows 10 1809 及以上**（WGC 要求），推荐 Windows 11 |
| 显卡 | 支持 DirectX 11 的 GPU；DirectML 需要 Windows 10 1903+（`DirectML.dll` 随系统提供） |
| 磁盘 | 约 19 MB（不含模型；带 NVIDIA 超分运行库再加约 1.1 GB） |
| 运行库 | `VCRUNTIME140.dll`（`package.ps1` 会自动放进 dist；或安装 VC++ 2015-2022 运行库） |
| ONNX Runtime | `libs\onnxruntime.dll`（DirectML 版，API 24 / ORT 1.24.x），随包提供 |
| 可选：AI 超分 | **RTX 20 系及以上**（需要 Tensor Core）+ **NVIDIA Video Effects 运行库**，需单独安装，见 [1.1](#11-nvidia-超分运行库可选) |

> 没有 NVIDIA 显卡也能用：仅模型模式走 DirectML，A 卡 / 核显均可；只是超分选项不可用。

### 拷贝到另一台电脑

```powershell
# 开发机
.\package.ps1 -IncludeModels          # 想带模型就加，产物在 dist\

# 目标机：整个 dist 目录拷过去，直接双击 animation_imagee.exe
```

dist 结构：

```
dist\
  animation_imagee.exe
  VCRUNTIME140.dll
  libs\onnxruntime.dll
  libs\onnxruntime_providers_shared.dll
  AnimeGANv3_Hayao_36.onnx        (可选，-IncludeModels)
  nvvfx\                          (可选，-IncludeVfx：NVIDIA 超分运行库 + models)
```

### 1.1 NVIDIA 超分运行库（可选）

「仅超分」「模型+超分」两种模式依赖 **NVIDIA Maxine Video Effects SDK 的运行库**。它不在 Windows 里，也**不随显卡驱动安装**，是一个独立安装项。

装好后长这样：

```
C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects\
  NVVideoEffects.dll                    ← 特效本体，本程序 LoadLibrary 它
  NVCVImage.dll                         ← 图像/显存辅助（格式转换、上传下载）
  nvinfer_10.dll / nvinfer_plugin_10.dll / nvonnxparser_10.dll
  cudart64_12.dll / cublas*_12.dll / npp*_12.dll / nvrtc*     ← 内置 TensorRT 10 + CUDA
  models\SR_2x_agg_100.engine.trtpkg    ← 2x / 3x / 4x 超分引擎（共 130 MB）
  uninstall.exe  version.h  NVIDIA Maxine EULA.pdf
```

「应用和功能」里对应的条目名是 **NVIDIA Video Effects**（本机验证版本 **0.7.6**），整个目录约 **1.1 GB**。

#### 从哪儿下载

| 渠道 | 下载/获取的东西 | 说明 |
|---|---|---|
| **官方 SDK（推荐）** | **NVIDIA Maxine Video Effects SDK**，NGC 上的条目叫 *Maxine Windows Video Effects SDK*：<br>`https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/collections/maxine_windows_vfx_sdk_collection_ga` | 需要 NVIDIA 账号；GA 版可能要 **NVIDIA AI Enterprise 订阅或 90 天评估许可**。下载得到的是 SDK 压缩包（形如 `VideoEffects_SDK_<版本>.zip`，具体文件名以页面为准），**解压后运行包内的运行库安装器即可，不需要编译 SDK** |
| **从已有机器复制** | 把整个 `C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects` 目录拷走 | **必须连 `models\` 和那些 TensorRT/CUDA dll 一起拷**，只拷 `NVVideoEffects.dll` + `NVCVImage.dll` 会加载失败。放到目标机 exe 同级的 `nvvfx\` 就能被自动找到 |
| **随其他 NVIDIA 应用** | 部分带 Maxine 特效的应用（如 NVIDIA Broadcast）安装时可能一并装上 | 不保证；装完去上面那个目录看 `NVVideoEffects.dll` 在不在。**NVIDIA 显卡驱动本身不含它** |

> **分发注意**：该运行库受目录里的 `NVIDIA Maxine EULA.pdf` 约束。自己两台机器之间拷贝没问题；如果要随程序对外分发，先确认条款。

快速确认有没有装：

```powershell
Test-Path 'C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects\NVVideoEffects.dll'
```

#### 程序怎么找到它

按顺序查找，命中即用（也可以用环境变量 `NVVFX_DIR` 强制指定）：

1. 环境变量 `NVVFX_DIR`
2. `exe 同级\nvvfx\`
3. `exe 同级\NVIDIA Video Effects\`
4. `C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects`
5. `C:\Program Files (x86)\NVIDIA Corporation\NVIDIA Video Effects`

打包时一起带上：

```powershell
.\package.ps1 -IncludeVfx      # 复制到 dist\nvvfx\，体积 +1.1 GB
```

#### 没装会怎样

不影响「仅模型」模式。选择「仅超分」或「模型+超分」时会直接弹错误框并列出上述查找路径，不会静默降级成别的效果。

---

## 2. 模型要求

只支持**等尺寸**的直通模型（当前实现的硬约束）：

- 格式：`.onnx`，float32
- 输入：**NHWC** `[1, H, W, 3]`，取值范围 `[-1, 1]`（即 `sRGB/127.5 - 1`）
- 输出：与输入**尺寸完全相同**的 `[1, H, W, 3]`，`[-1, 1]`

不满足时会直接报错提示，不会静默出怪图。仓库默认用 `AnimeGANv3_Hayao_36.onnx`。

超分模型是 NVIDIA 内置的 TensorRT 引擎（`SR_2x/3x/4x_*.engine.trtpkg`），不需要额外下载。

---

## 3. 使用

启动后是选择界面：

| 项 | 说明 |
|---|---|
| ① 目标窗口 | 列出可见的顶层窗口，点一行选中 |
| ② 模型文件 | `.onnx` 路径；「仅超分」模式可以留空 |
| ③ 模型输入宽高 | 留空 = 用目标窗口客户区（自动对齐到 8 的倍数）。固定输入尺寸的模型必须照填 |
| ④ 上屏宽高 | 不含超分：上屏窗口尺寸（留空 = 跟随目标窗口）。**含超分：这是超分输入尺寸，最终尺寸 = 它 × 倍率** |
| ⑤ 处理方式 | 仅模型 / 仅超分 / 模型+超分 |
| ⑥ 超分倍率 | 2x / 3x / 4x（由 NVIDIA `Upscale` 提供，倍率由输出尺寸决定） |
| ⑦ 开始渲染 | 校验通过后创建上屏窗口 |

尺寸链（「模型+超分」）：

```
模型输入 → 风格化(等尺寸) → Catmull-Rom 缩放到「超分输入」 → ×倍率 = 上屏尺寸
```

超分之后**不再做任何缩放**：上屏窗口客户区就是最终尺寸，内容是 1:1 直通（源和目标同尺寸时走像素直通，不做低通滤波）。

例：④ 填 `960×540` + 6 选 `4x` → 上屏窗口 `3840×2160`。

**退出**：关掉上屏窗口即回到选择界面；在选择界面点取消则退出程序。
**日志**：`logs\animegan_rCURRENT.log`（按天切割，保留 30 天）。

---

## 4. 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `LUMA_ANCHOR` | 亮度锚定强度 `0..1`。风格化模型会把暗部抬高约 10/255，这里把亮度往原图拉：`0` = 亮度完全等于原图（模型只贡献颜色），`1` = 完全用模型亮度 | `0.3` |
| `ORT_DYLIB_PATH` | 指定 `onnxruntime.dll` 的完整路径 | 依次找 `libs\`、exe 同级、`exe\libs\` |
| `NVVFX_DIR` | 指定 NVIDIA Video Effects 运行库目录 | 依次找 `exe\nvvfx\`、`exe\NVIDIA Video Effects\`、`C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects` |

---

## 5. 已知限制

- **目标窗口有内容保护**（`SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` 或 DRM）→ 抓到全黑，无解。
- **独占全屏游戏**不经过 DWM 合成，WGC 通常拿不到帧；请用无边框窗口模式。
- **跨 GPU**：目标窗口由另一块显卡渲染时拿不到纹理，需要在同一张卡上。
- **超分尺寸**：`上屏宽高 × 倍率` 超过屏幕时窗口会超出桌面，请把 ④ 调小。
- **性能**：仅模型是纯 GPU 零拷贝；含超分的路径每帧有一次 GPU→CPU→GPU 往返（1080p 约 8 MB×2），2x 比 4x 明显轻松。
- 上屏窗口可以被手动拖动缩放，此时会退化为线性缩放；正常使用（不改窗口大小）是 1:1。
- 程序不做桌面复制兜底：WGC 不可用时直接报错退出，避免出现「抓到的是被遮挡后的画面」这种错误结果。
