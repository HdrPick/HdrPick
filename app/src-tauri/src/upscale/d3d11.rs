//! GPU 推理引擎（P2 + P5）：D3D11 Compute Shader 全层类型支持
//!
//! 设计：
//! - 特征图：Texture2DArray<float4>（每 4 通道一个 slice，RGBA 通道打包）
//! - 权重：StructuredBuffer<float4>（权重 + bias 连续存放，biasOff 偏移）
//! - conv（stride 1）：每线程 2×2 输出像素（4×4 输入块共享，Load 数 ÷2.25）
//! - conv（stride>1）：每线程 1 像素（cunet 下采样 conv）
//! - deconv：gather 形式（scatter 的逆读）+ leaky ReLU
//! - SE 算子（P5）：GlobalAvgPool（dispatch z=ch4，每线程全图累加）/
//!   Sigmoid / Scale（data×vec[c]）/ Axpy（a[c]·x+y）/ EltwiseAdd / Crop（偏移拷贝）
//! - 分块：GpuSession 按 tile 尺寸预分配各槽位纹理（跨 tile 复用），
//!   blob 最后一次消费后纹理进空闲池复用（cunet 深网显存控制）
//!
//! 精度：每线程内累加顺序与 CPU 一致（ic→ky→kx 循环），
//! 差异仅来自 FMA 收缩，对拍 PSNR >> 60dB。

use std::sync::OnceLock;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, ID3DInclude, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_SRV_DIMENSION_BUFFER, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

use super::cpu::Tensor;
use super::model::{ExecLayer, WaifuModel};

// ==================== HLSL 着色器 ====================

/// 常量布局（96 字节 = 6×16 对齐；conv/pointwise/crop 共用）
const SHADER_COMMON: &str = r#"
cbuffer P : register(b0) {
    uint inW, inH, inCh, outCh;      // 逻辑通道数
    uint outW, outH, kernel, pad;
    uint dilation, stride, biasOff;  // biasOff 单位 = float4 元素
    float leakySlope;
    uint inCh4, outCh4, offX, offY; // offX/offY = crop 源偏移
    uint pad0, pad1, pad2, pad3;
    uint pad4, pad5, pad6, pad7;
};
StructuredBuffer<float4> w : register(t0);     // 权重 + bias 连续（bias 从 biasOff 起）
Texture2DArray<float4> texIn : register(t2);
"#;

const CONV_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    // 每线程 2×2 输出像素；tid.x/y 以 2 像素步进
    uint px = tid.x * 2, py = tid.y * 2;
    if (px >= outW || py >= outH) return;
    uint ocg = tid.z;
    float4 acc00 = w[biasOff + ocg];
    float4 acc10 = acc00, acc01 = acc00, acc11 = acc00;
    [loop]
    for (uint icg = 0; icg < inCh4; icg++) {
        [loop]
        for (uint ky = 0; ky < kernel; ky++) {
            int sy = int(py) + int(ky) * int(dilation) - int(pad);
            [loop]
            for (uint kx = 0; kx < kernel; kx++) {
                int sx = int(px) + int(kx) * int(dilation) - int(pad);
                // 权重 4×4 矩阵（行 = oc，列 = ic）：连续 4 个 float4
                uint wbase = ((ocg * inCh4 + icg) * kernel * kernel + ky * kernel + kx) * 4;
                float4 wr0 = w[wbase + 0], wr1 = w[wbase + 1];
                float4 wr2 = w[wbase + 2], wr3 = w[wbase + 3];
                // 2×2 输出共享 4×4 输入块
                if (sy >= 0 && sy < int(inH)) {
                    if (sx >= 0 && sx < int(inW)) {
                        float4 v = texIn.Load(int4(sx, sy, icg, 0));
                        acc00 += float4(dot(wr0, v), dot(wr1, v), dot(wr2, v), dot(wr3, v));
                    }
                    if (sx + 1 < int(inW)) {
                        float4 v = texIn.Load(int4(sx + 1, sy, icg, 0));
                        acc10 += float4(dot(wr0, v), dot(wr1, v), dot(wr2, v), dot(wr3, v));
                    }
                }
                if (sy + 1 >= 0 && sy + 1 < int(inH)) {
                    if (sx >= 0 && sx < int(inW)) {
                        float4 v = texIn.Load(int4(sx, sy + 1, icg, 0));
                        acc01 += float4(dot(wr0, v), dot(wr1, v), dot(wr2, v), dot(wr3, v));
                    }
                    if (sx + 1 < int(inW)) {
                        float4 v = texIn.Load(int4(sx + 1, sy + 1, icg, 0));
                        acc11 += float4(dot(wr0, v), dot(wr1, v), dot(wr2, v), dot(wr3, v));
                    }
                }
            }
        }
    }
    float4 r00 = acc00 >= 0.0 ? acc00 : acc00 * leakySlope;
    float4 r10 = acc10 >= 0.0 ? acc10 : acc10 * leakySlope;
    float4 r01 = acc01 >= 0.0 ? acc01 : acc01 * leakySlope;
    float4 r11 = acc11 >= 0.0 ? acc11 : acc11 * leakySlope;
    if (px < outW && py < outH) texOut[int3(px, py, ocg)] = r00;
    if (px + 1 < outW && py < outH) texOut[int3(px + 1, py, ocg)] = r10;
    if (px < outW && py + 1 < outH) texOut[int3(px, py + 1, ocg)] = r01;
    if (px + 1 < outW && py + 1 < outH) texOut[int3(px + 1, py + 1, ocg)] = r11;
}
"#;

/// stride>1 卷积（cunet 下采样 conv k2 s2；每线程 1 像素）
/// 权重打包与 CONV_CS 相同（[ocg][icg][k*k]×4 行 float4）
const STRIDE_CONV_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    uint ocg = tid.z;
    float4 acc = w[biasOff + ocg];
    [loop]
    for (uint icg = 0; icg < inCh4; icg++) {
        [loop]
        for (uint ky = 0; ky < kernel; ky++) {
            int sy = int(tid.y) * int(stride) + int(ky) * int(dilation) - int(pad);
            if (sy < 0 || sy >= int(inH)) continue;
            [loop]
            for (uint kx = 0; kx < kernel; kx++) {
                int sx = int(tid.x) * int(stride) + int(kx) * int(dilation) - int(pad);
                if (sx < 0 || sx >= int(inW)) continue;
                uint wbase = ((ocg * inCh4 + icg) * kernel * kernel + ky * kernel + kx) * 4;
                float4 v = texIn.Load(int4(sx, sy, icg, 0));
                acc += float4(dot(w[wbase + 0], v), dot(w[wbase + 1], v),
                              dot(w[wbase + 2], v), dot(w[wbase + 3], v));
            }
        }
    }
    float4 r = acc >= 0.0 ? acc : acc * leakySlope;
    texOut[int3(tid.xy, ocg)] = r;
}
"#;

/// deconv gather：out[sy,sx,oc] += w[ic,oc,ky,kx]·in[ic,y,x]
/// 其中 y*stride+ky-pad=sy（整数解才有效）；packed 版（每线程 1 像素）+ leaky
///
/// 权重打包布局（Rust 端预排）：[(ky*k+kx)][icg][ocg][i] 连续 float4
/// = M[i][j]（行 i=ic，列 j=oc），shader 里 acc += mrow_i * v[i]
const DECONV_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    uint ocg = tid.z;
    float4 acc = w[biasOff + ocg];
    [loop]
    for (uint icg = 0; icg < inCh4; icg++) {
        [loop]
        for (uint ky = 0; ky < kernel; ky++) {
            int t = int(tid.y) + int(pad) - int(ky);
            if (t < 0 || t % int(stride) != 0) continue;
            int y = t / int(stride);
            if (y >= int(inH)) continue;
            [loop]
            for (uint kx = 0; kx < kernel; kx++) {
                int u = int(tid.x) + int(pad) - int(kx);
                if (u < 0 || u % int(stride) != 0) continue;
                int x = u / int(stride);
                if (x >= int(inW)) continue;
                float4 v = texIn.Load(int4(x, y, icg, 0));
                uint wbase = ((ky * kernel + kx) * inCh4 + icg) * outCh4 * 4 + ocg * 4;
                [unroll]
                for (int i = 0; i < 4; i++) {
                    acc += w[wbase + i] * v[i];
                }
            }
        }
    }
    float4 r = acc >= 0.0 ? acc : acc * leakySlope;
    texOut[int3(tid.xy, ocg)] = r;
}
"#;

/// 全局平均池化（SE squeeze）：每线程（z=ch4 组）全图累加 → 1×1
const GAP_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    float4 acc = float4(0.0, 0.0, 0.0, 0.0);
    [loop]
    for (uint y = 0; y < inH; y++) {
        [loop]
        for (uint x = 0; x < inW; x++) {
            acc += texIn.Load(int4(x, y, tid.z, 0));
        }
    }
    texOut[int3(0, 0, tid.z)] = acc / float(inW * inH);
}
"#;

/// 逐元素 sigmoid（SE excite）
const SIGMOID_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 v = texIn.Load(int4(tid.x, tid.y, tid.z, 0));
    texOut[int3(tid.xy, tid.z)] = 1.0 / (1.0 + exp(-v));
}
"#;

/// 通道缩放（SE gate）：data(t2) × vec[c](t3，1×1×C 广播)
const SCALE_CS: &str = r#"
Texture2DArray<float4> texVec : register(t3);
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 v = texIn.Load(int4(tid.x, tid.y, tid.z, 0));
    float4 s = texVec.Load(int4(0, 0, tid.z, 0));
    texOut[int3(tid.xy, tid.z)] = v * s;
}
"#;

/// out = alpha[c]·x + y（upresnet10 Axpy：alpha=t2 1×1×C，x=t3，y=t4）
const AXPY_CS: &str = r#"
Texture2DArray<float4> texA : register(t3);
Texture2DArray<float4> texY : register(t4);
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 a = texIn.Load(int4(0, 0, tid.z, 0));           // t2 = alpha
    float4 x = texA.Load(int4(tid.x, tid.y, tid.z, 0));    // t3 = x
    float4 y = texY.Load(int4(tid.x, tid.y, tid.z, 0));    // t4 = y
    texOut[int3(tid.xy, tid.z)] = a * x + y;
}
"#;

/// 逐元素相加（残差 SUM）：t2 + t3
const ADD_CS: &str = r#"
Texture2DArray<float4> texB : register(t3);
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 a = texIn.Load(int4(tid.x, tid.y, tid.z, 0));
    float4 b = texB.Load(int4(tid.x, tid.y, tid.z, 0));
    texOut[int3(tid.xy, tid.z)] = a + b;
}
"#;

/// 通道拼接（RRDB 密集连接）：t2/t3/t4 = ≤3 个 bottom，
/// pad0/pad1/pad2 = 各 bottom 的起始逻辑通道（0xFFFFFFFF = 无该 bottom）
const CONCAT_CS: &str = r#"
Texture2DArray<float4> texB1 : register(t3);
Texture2DArray<float4> texB2 : register(t4);
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 r = float4(0.0, 0.0, 0.0, 0.0);
    [unroll]
    for (int j = 0; j < 4; j++) {
        uint c = tid.z * 4 + uint(j);
        if (c >= outCh) break;
        if (pad1 == 0xFFFFFFFFu || c < pad1) {
            uint cl = c - pad0;
            r[j] = texIn.Load(int4(tid.x, tid.y, cl / 4, 0))[cl % 4];
        } else if (pad2 == 0xFFFFFFFFu || c < pad2) {
            uint cl = c - pad1;
            r[j] = texB1.Load(int4(tid.x, tid.y, cl / 4, 0))[cl % 4];
        } else {
            uint cl = c - pad2;
            r[j] = texB2.Load(int4(tid.x, tid.y, cl / 4, 0))[cl % 4];
        }
    }
    texOut[int3(tid.xy, tid.z)] = r;
}
"#;

/// 最近邻 ×2 上采样（RRDBNet）：out[x,y] = in[x/2, y/2]
const NEARESTUP2_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    texOut[int3(tid.xy, tid.z)] = texIn.Load(int4(tid.x / 2, tid.y / 2, tid.z, 0));
}
"#;

/// out = β·a + b（RDB 残差缩放）：β = leakySlope（参数复用），t2 = a，t3 = b
const SCALEDADD_CS: &str = r#"
Texture2DArray<float4> texB : register(t3);
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 a = texIn.Load(int4(tid.x, tid.y, tid.z, 0));
    float4 b = texB.Load(int4(tid.x, tid.y, tid.z, 0));
    texOut[int3(tid.xy, tid.z)] = leakySlope * a + b;
}
"#;

/// 中心裁剪拷贝：out[y,x] = in[y+offY, x+offX]
const CROP_CS: &str = r#"
RWTexture2DArray<float4> texOut : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= outW || tid.y >= outH) return;
    float4 v = texIn.Load(int4(tid.x + offX, tid.y + offY, tid.z, 0));
    texOut[int3(tid.xy, tid.z)] = v;
}
"#;

/// Rust 侧常量（与 HLSL cbuffer 字段一一对应；16 字节对齐）
#[repr(C)]
#[derive(Clone, Copy)]
struct LayerParams {
    in_w: u32,
    in_h: u32,
    in_ch: u32,
    out_ch: u32,
    out_w: u32,
    out_h: u32,
    kernel: u32,
    pad: u32,
    dilation: u32,
    stride: u32,
    bias_off: u32,
    leaky_slope: f32,
    in_ch4: u32,
    out_ch4: u32,
    off_x: u32,
    off_y: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    _pad3: u32,
    _pad4: u32,
    _pad5: u32,
    _pad6: u32,
    _pad7: u32,
}

impl Default for LayerParams {
    fn default() -> Self {
        LayerParams {
            in_w: 0,
            in_h: 0,
            in_ch: 0,
            out_ch: 0,
            out_w: 0,
            out_h: 0,
            kernel: 0,
            pad: 0,
            dilation: 1,
            stride: 1,
            bias_off: 0,
            leaky_slope: 0.0,
            in_ch4: 0,
            out_ch4: 0,
            off_x: 0,
            off_y: 0,
            pad0: 0,
            pad1: 0,
            pad2: 0,
            _pad3: 0,
            _pad4: 0,
            _pad5: 0,
            _pad6: 0,
            _pad7: 0,
        }
    }
}

// ==================== 引擎（设备 + 着色器，进程级单例） ====================

pub struct GpuEngine {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// true = WARP 软渲染（无硬件加速）
    pub(crate) is_warp: bool,
    conv_cs: ID3D11ComputeShader,
    stride_conv_cs: ID3D11ComputeShader,
    deconv_cs: ID3D11ComputeShader,
    gap_cs: ID3D11ComputeShader,
    sigmoid_cs: ID3D11ComputeShader,
    scale_cs: ID3D11ComputeShader,
    axpy_cs: ID3D11ComputeShader,
    add_cs: ID3D11ComputeShader,
    crop_cs: ID3D11ComputeShader,
    concat_cs: ID3D11ComputeShader,
    nearest_up2_cs: ID3D11ComputeShader,
    scaled_add_cs: ID3D11ComputeShader,
}

// 设备/上下文跨线程共享（D3D11 immediate context 多线程安全需加锁；
// 这里用 Mutex 串行化所有 GPU 调用）
pub struct SyncGpuEngine(pub std::sync::Mutex<GpuEngine>);
unsafe impl Send for SyncGpuEngine {}
unsafe impl Sync for SyncGpuEngine {}

impl GpuEngine {
    /// 供其他模块的一次性 compute 任务访问（hybrid GPU IDCT 等）
    pub(crate) fn device_ctx(&self) -> (&ID3D11Device, &ID3D11DeviceContext) {
        (&self.device, &self.ctx)
    }

    /// 提交并触发 D3D11 延迟析构（会话 drop 后立即归还显存/内存）
    pub fn flush(&self) {
        unsafe {
            self.ctx.Flush();
        }
    }
}

static ENGINE: OnceLock<Result<SyncGpuEngine, String>> = OnceLock::new();

/// 获取全局 GPU 引擎（硬件优先，失败回退 WARP；再失败返回 Err）
pub fn engine() -> &'static Result<SyncGpuEngine, String> {
    ENGINE.get_or_init(|| GpuEngine::new().map(|e| SyncGpuEngine(std::sync::Mutex::new(e))))
}

impl GpuEngine {
    fn new() -> Result<GpuEngine, String> {
        unsafe {
            let (device, ctx, is_warp) = create_device()?;
            let compile = |src: &str, name: &[u8]| -> Result<ID3D11ComputeShader, String> {
                compile_cs(&device, &format!("{}\n{}", SHADER_COMMON, src), name)
            };
            let conv_cs = compile(CONV_CS, b"conv\0")?;
            let stride_conv_cs = compile(STRIDE_CONV_CS, b"stride_conv\0")?;
            let deconv_cs = compile(DECONV_CS, b"deconv\0")?;
            let gap_cs = compile(GAP_CS, b"gap\0")?;
            let sigmoid_cs = compile(SIGMOID_CS, b"sigmoid\0")?;
            let scale_cs = compile(SCALE_CS, b"scale\0")?;
            let axpy_cs = compile(AXPY_CS, b"axpy\0")?;
            let add_cs = compile(ADD_CS, b"add\0")?;
            let crop_cs = compile(CROP_CS, b"crop\0")?;
            let concat_cs = compile(CONCAT_CS, b"concat\0")?;
            let nearest_up2_cs = compile(NEARESTUP2_CS, b"nearest_up2\0")?;
            let scaled_add_cs = compile(SCALEDADD_CS, b"scaled_add\0")?;
            log::info!(
                "[upscale/GPU] D3D11 引擎就绪（{}，12 个 compute shader 已编译）",
                if is_warp { "WARP 软渲染" } else { "硬件" }
            );
            Ok(GpuEngine {
                device,
                ctx,
                is_warp,
                conv_cs,
                stride_conv_cs,
                deconv_cs,
                gap_cs,
                sigmoid_cs,
                scale_cs,
                axpy_cs,
                add_cs,
                crop_cs,
                concat_cs,
                nearest_up2_cs,
                scaled_add_cs,
            })
        }
    }
}

unsafe fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, bool), String> {
    let fl = [D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut ctx = None;
    let ok = D3D11CreateDevice(
        None::<&IDXGIAdapter>,
        D3D_DRIVER_TYPE_HARDWARE,
        windows::Win32::Foundation::HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&fl),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut ctx),
    )
    .is_ok();
    let warp = !ok;
    if !ok {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_WARP,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&fl),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .map_err(|e| format!("D3D11CreateDevice(WARP): {}", e))?;
    }
    match (device, ctx) {
        (Some(d), Some(c)) => Ok((d, c, warp)),
        _ => Err("D3D11 设备创建失败".into()),
    }
}

/// 进程级 shader 缓存（同名只编译一次）——hybrid 等高频调用路径避免每帧 D3DCompile
pub(crate) fn compile_cs_cached(
    device: &ID3D11Device,
    src: &str,
    name: &[u8],
) -> Result<ID3D11ComputeShader, String> {
    use std::collections::HashMap;
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, ID3D11ComputeShader>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = String::from_utf8_lossy(name).into_owned();
    if let Some(cs) = cache.lock().unwrap().get(&key) {
        return Ok(cs.clone());
    }
    let cs = unsafe { compile_cs(device, src, name) }?;
    cache.lock().unwrap().insert(key, cs.clone());
    Ok(cs)
}

pub(crate) unsafe fn compile_cs(
    device: &ID3D11Device,
    src: &str,
    name: &[u8],
) -> Result<ID3D11ComputeShader, String> {
    use windows::core::PCSTR;
    let bytes = src.as_bytes();
    let mut code = None;
    let mut errs = None;
    let hr = D3DCompile(
        bytes.as_ptr() as *const core::ffi::c_void,
        bytes.len(),
        PCSTR(name.as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(b"main\0".as_ptr()),
        PCSTR(b"cs_5_0\0".as_ptr()),
        0,
        0,
        &mut code,
        Some(&mut errs),
    );
    if hr.is_err() {
        let msg = errs
            .map(|b: ID3DBlob| {
                let p = b.GetBufferPointer() as *const u8;
                String::from_utf8_lossy(std::slice::from_raw_parts(p, b.GetBufferSize()))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("D3DCompile(cs): {:?} {}", hr, msg));
    }
    let blob = code.ok_or("编译无产物")?;
    let mut cs = None;
    let bytecode = {
        let p = blob.GetBufferPointer() as *const u8;
        std::slice::from_raw_parts(p, blob.GetBufferSize())
    };
    device
        .CreateComputeShader(bytecode, None, Some(&mut cs))
        .map_err(|e| format!("CreateComputeShader: {}", e))?;
    cs.ok_or_else(|| "CS 创建失败".to_string())
}

// ==================== 纹理/缓冲辅助 ====================

struct TexBundle {
    #[allow(dead_code)]
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
    w: u32,
    h: u32,
    /// packed slice 数 = ceil(逻辑通道 / 4)
    ch4: u32,
    /// 逻辑通道数（readback unpack 用；复用匹配条件之一）
    ch: u32,
}

/// RGBA32F 通道打包纹理（每 slice 4 个通道）
unsafe fn make_tex_array(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    ch4: u32,
    ch: u32,
) -> Result<TexBundle, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: ch4,
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS).0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut tex))
        .map_err(|e| format!("CreateTexture2D({w}x{h}x{ch4}): {}", e))?;
    let tex: ID3D11Texture2D = tex.ok_or("纹理创建失败")?;

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                ArraySize: ch4,
            },
        },
    };
    let mut srv = None;
    device
        .CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv))
        .map_err(|e| format!("CreateSRV: {}", e))?;
    let srv: ID3D11ShaderResourceView = srv.ok_or("SRV 创建失败")?;

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_UAV {
                MipSlice: 0,
                FirstArraySlice: 0,
                ArraySize: ch4,
            },
        },
    };
    let mut uav = None;
    device
        .CreateUnorderedAccessView(&tex, Some(&uav_desc), Some(&mut uav))
        .map_err(|e| format!("CreateUAV: {}", e))?;
    let uav: ID3D11UnorderedAccessView = uav.ok_or("UAV 创建失败")?;

    Ok(TexBundle {
        tex,
        srv,
        uav,
        w,
        h,
        ch4,
        ch,
    })
}

unsafe fn make_weight_buffer(
    device: &ID3D11Device,
    layer: &ExecLayer,
) -> Result<(ID3D11Buffer, ID3D11ShaderResourceView, u32), String> {
    let mut w4: Vec<[f32; 4]> = Vec::new();
    let mut bias4: Vec<[f32; 4]> = Vec::new();

    match layer {
        ExecLayer::Conv {
            weight,
            bias,
            in_ch,
            out_ch,
            kernel,
            ..
        } => {
            let (icg_n, ocg_n) = (in_ch.div_ceil(4), out_ch.div_ceil(4));
            // [ocg][icg][ky][kx] × 4 行 float4
            w4.reserve(ocg_n * icg_n * kernel * kernel * 4);
            for ocg in 0..ocg_n {
                for icg in 0..icg_n {
                    for ky in 0..*kernel {
                        for kx in 0..*kernel {
                            // 4 行（oc = ocg*4+j），每行 float4 = ic 0..3 的权重
                            for j in 0..4 {
                                let oc = ocg * 4 + j;
                                let mut row = [0.0f32; 4];
                                if oc < *out_ch {
                                    for i in 0..4 {
                                        let ic = icg * 4 + i;
                                        if ic < *in_ch {
                                            row[i] = weight[(oc * in_ch + ic) * kernel * kernel
                                                + ky * kernel
                                                + kx];
                                        }
                                    }
                                }
                                w4.push(row);
                            }
                        }
                    }
                }
            }
            for ocg in 0..ocg_n {
                let mut b = [0.0f32; 4];
                for j in 0..4 {
                    let oc = ocg * 4 + j;
                    if oc < *out_ch {
                        b[j] = bias[oc];
                    }
                }
                bias4.push(b);
            }
        }
        ExecLayer::Deconv {
            weight,
            bias,
            in_ch,
            out_ch,
            kernel,
            ..
        } => {
            let (icg_n, ocg_n) = (in_ch.div_ceil(4), out_ch.div_ceil(4));
            // [(ky*k+kx)][icg][ocg] × 4 行 float4（行=ic，分量=oc）
            w4.reserve(kernel * kernel * icg_n * ocg_n * 4);
            for ky in 0..*kernel {
                for kx in 0..*kernel {
                    for icg in 0..icg_n {
                        for ocg in 0..ocg_n {
                            for i in 0..4 {
                                let ic = icg * 4 + i;
                                let mut row = [0.0f32; 4];
                                if ic < *in_ch {
                                    for j in 0..4 {
                                        let oc = ocg * 4 + j;
                                        if oc < *out_ch {
                                            row[j] = weight[(ic * out_ch + oc) * kernel * kernel
                                                + ky * kernel
                                                + kx];
                                        }
                                    }
                                }
                                w4.push(row);
                            }
                        }
                    }
                }
            }
            for ocg in 0..ocg_n {
                let mut b = [0.0f32; 4];
                for j in 0..4 {
                    let oc = ocg * 4 + j;
                    if oc < *out_ch {
                        b[j] = bias[oc];
                    }
                }
                bias4.push(b);
            }
        }
        _ => return Err("该层无权重（pointwise 不走权重缓冲）".into()),
    }

    let bias_off = w4.len() as u32;
    let mut all: Vec<[f32; 4]> = w4;
    all.extend(bias4);
    let floats: Vec<f32> = all.iter().flat_map(|r| r.iter().copied()).collect();
    let elem_count = all.len() as u32; // float4 元素数

    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (all.len() * 16) as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: 16,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: floats.as_ptr() as *const core::ffi::c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut buf = None;
    device
        .CreateBuffer(&desc, Some(&init), Some(&mut buf))
        .map_err(|e| format!("CreateBuffer(weights): {}", e))?;
    let buf: ID3D11Buffer = buf.ok_or("权重缓冲创建失败")?;

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D_SRV_DIMENSION_BUFFER,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_SRV {
                // structured buffer：FirstElement/NumElements（ElementOffset/ElementWidth 是 raw byte 语义）
                Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                Anonymous2: D3D11_BUFFER_SRV_1 {
                    NumElements: elem_count,
                },
            },
        },
    };
    let mut srv = None;
    device
        .CreateShaderResourceView(&buf, Some(&srv_desc), Some(&mut srv))
        .map_err(|e| format!("CreateSRV(weights): {}", e))?;
    let srv: ID3D11ShaderResourceView = srv.ok_or("权重 SRV 创建失败")?;
    Ok((buf, srv, bias_off))
}

// ==================== 会话（模型常驻 GPU 资源 + 分块执行） ====================

enum LayerKind {
    Conv,
    StrideConv,
    Deconv,
    Gap,
    Sigmoid,
    Scale,
    Axpy,
    Add,
    Crop,
    Concat,
    NearestUp2,
    ScaledAdd,
}

struct LayerGpu {
    kind: LayerKind,
    weight_srv: Option<ID3D11ShaderResourceView>,
    bias_off: u32,
    /// bottom 纹理索引（conv/deconv/sigmoid/crop 1 个；scale/add 2 个；axpy 3 个）
    bottom_tex: Vec<usize>,
    /// 输出纹理索引
    top_tex: usize,
}

pub struct GpuSession<'a> {
    engine: &'a GpuEngine,
    texs: Vec<TexBundle>,
    /// 槽位 → 纹理索引（Split/Flatten 别名同纹理）
    slot_tex: Vec<usize>,
    layers: Vec<LayerGpu>,
    /// 每层 dispatch 参数（构造时缓存）
    params_cache: Vec<LayerParams>,
    cbuf: ID3D11Buffer,
    /// 输入 tile 尺寸（padded 后）与最终输出尺寸
    pub in_w: usize,
    pub in_h: usize,
    pub in_ch: usize,
    pub out_w: usize,
    pub out_h: usize,
    pub out_ch: usize,
    output_tex: usize,
}

impl<'a> GpuSession<'a> {
    /// 按模型 + tile 尺寸建立会话（分配各槽位纹理，跨 tile 复用）
    ///
    /// tile_w/tile_h 为未 pad 的有效 tile 尺寸；内部加 2×net_offset。
    /// 纹理按 blob 生命周期复用（最后一次消费后进空闲池）。
    pub fn new(
        engine: &'a GpuEngine,
        model: &WaifuModel,
        tile_w: usize,
        tile_h: usize,
    ) -> Result<Self, String> {
        unsafe {
            let pw = tile_w + 2 * model.net_offset;
            let ph = tile_h + 2 * model.net_offset;
            let shapes = model.slot_shapes(pw, ph)?;
            for (i, (w, h, _)) in shapes.iter().enumerate() {
                if *w == 0 || *h == 0 || *w > 16384 || *h > 16384 {
                    return Err(format!("槽位 {i} 形状异常 {w}x{h}"));
                }
            }

            // 纹理分配（带生命周期复用）
            let mut texs: Vec<TexBundle> = Vec::new();
            let mut slot_tex: Vec<usize> = vec![usize::MAX; model.n_slots];
            let mut free: Vec<usize> = Vec::new(); // 空闲纹理池（索引）
                                                   // 每纹理的"最后使用层"（所有映射到它的槽位取 max；MAX = 永不释放）
            let mut tex_live_until: Vec<usize> = Vec::new();

            // 输入槽 0
            let (iw0, ih0, ic0) = shapes[0];
            texs.push(make_tex_array(
                &engine.device,
                iw0 as u32,
                ih0 as u32,
                ic0.div_ceil(4) as u32,
                ic0 as u32,
            )?);
            tex_live_until.push(usize::MAX);
            slot_tex[0] = 0;

            let mut layers: Vec<LayerGpu> = Vec::with_capacity(model.layers.len());
            let mut params_cache: Vec<LayerParams> = Vec::with_capacity(model.layers.len());

            // 预计算每个槽位的最后消费层（全网络前瞻）。
            // 生命周期必须前瞻：逐层增量 max 会在"未来才被再次消费"的纹理
            // 中途误释放并复用 → 数据损坏。槽位唯一写入（Split/Flatten 在
            // 模型构建期已解析为别名），按槽位计算即无歧义。
            // 与 estimate_session_mb 的释放语义一致。
            let mut last_use = vec![0usize; model.n_slots];
            for (i, l) in model.layers.iter().enumerate() {
                for b in l.bottoms() {
                    last_use[b] = last_use[b].max(i);
                }
            }

            for (i, l) in model.layers.iter().enumerate() {
                // 1. 释放上轮消费完的纹理（live_until == i-1）进空闲池。
                //    前瞻 last_use 保证释放点之后绝无读者，可安全复用。
                if i > 0 {
                    for t in 0..texs.len() {
                        if tex_live_until[t] == i - 1 && !free.contains(&t) {
                            free.push(t);
                        }
                    }
                }

                let top_slot = l.top();
                let (ow, oh, oc) = shapes[top_slot];
                let och4 = oc.div_ceil(4);
                // 本槽纹理的释放点 = 最后一次被消费的层；
                // 0 = 无人消费（输出槽/死槽）→ 永不释放（含 run_tile 末尾读取的输出）
                let live = if last_use[top_slot] > 0 {
                    last_use[top_slot]
                } else {
                    usize::MAX
                };

                // 2. 分配 top 纹理（空闲池按 (w,h,ch4,ch) 复用）。
                //    此前新纹理误置 live=usize::MAX 且 max() 无法下调 →
                //    全部纹理永不回收 → 每层一张纹理（Real-ESRGAN 837 层
                //    ≈ 20GB，深网显存爆炸的根源）
                let tex_idx = match free.iter().position(|&t| {
                    texs[t].w == ow as u32
                        && texs[t].h == oh as u32
                        && texs[t].ch4 == och4 as u32
                        && texs[t].ch == oc as u32
                }) {
                    Some(pos) => {
                        let t = free.remove(pos);
                        tex_live_until[t] = live;
                        t
                    }
                    None => {
                        texs.push(make_tex_array(
                            &engine.device,
                            ow as u32,
                            oh as u32,
                            och4 as u32,
                            oc as u32,
                        )?);
                        tex_live_until.push(live);
                        texs.len() - 1
                    }
                };
                slot_tex[top_slot] = tex_idx;

                // 3. bottom 纹理（生命周期已前瞻计算，无需再延长）
                let bottom_slots = l.bottoms();
                let mut bottom_tex: Vec<usize> = Vec::with_capacity(bottom_slots.len());
                for b in &bottom_slots {
                    bottom_tex.push(slot_tex[*b]);
                }

                // 4. 构建层描述 + 参数
                let (kind, weight_srv, bias_off, mut p) = match l {
                    ExecLayer::Conv {
                        out_ch,
                        kernel,
                        pad,
                        stride,
                        dilation,
                        leaky_slope,
                        ..
                    } => {
                        let (iw, ih, ic) = shapes[bottom_slots[0]];
                        let (_, srv, off) = make_weight_buffer(&engine.device, l)?;
                        let kind = if *stride > 1 {
                            LayerKind::StrideConv
                        } else {
                            LayerKind::Conv
                        };
                        let p = LayerParams {
                            in_w: iw as u32,
                            in_h: ih as u32,
                            in_ch: ic as u32,
                            out_ch: *out_ch as u32,
                            out_w: ow as u32,
                            out_h: oh as u32,
                            kernel: *kernel as u32,
                            pad: *pad as u32,
                            dilation: *dilation as u32,
                            stride: *stride as u32,
                            bias_off: off,
                            leaky_slope: *leaky_slope,
                            in_ch4: ic.div_ceil(4) as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (kind, Some(srv), off, p)
                    }
                    ExecLayer::Deconv {
                        out_ch,
                        kernel,
                        stride,
                        pad,
                        leaky_slope,
                        ..
                    } => {
                        let (iw, ih, ic) = shapes[bottom_slots[0]];
                        let (_, srv, off) = make_weight_buffer(&engine.device, l)?;
                        let p = LayerParams {
                            in_w: iw as u32,
                            in_h: ih as u32,
                            in_ch: ic as u32,
                            out_ch: *out_ch as u32,
                            out_w: ow as u32,
                            out_h: oh as u32,
                            kernel: *kernel as u32,
                            pad: *pad as u32,
                            dilation: 1,
                            stride: *stride as u32,
                            bias_off: off,
                            leaky_slope: *leaky_slope,
                            in_ch4: ic.div_ceil(4) as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Deconv, Some(srv), off, p)
                    }
                    ExecLayer::GlobalAvgPool { .. } => {
                        let (iw, ih, ic) = shapes[bottom_slots[0]];
                        let p = LayerParams {
                            in_w: iw as u32,
                            in_h: ih as u32,
                            in_ch: ic as u32,
                            out_ch: oc as u32,
                            out_w: 1,
                            out_h: 1,
                            in_ch4: ic.div_ceil(4) as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Gap, None, 0, p)
                    }
                    ExecLayer::Sigmoid { .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Sigmoid, None, 0, p)
                    }
                    ExecLayer::Scale { .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Scale, None, 0, p)
                    }
                    ExecLayer::Axpy { .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Axpy, None, 0, p)
                    }
                    ExecLayer::CropCenter { crop_h, crop_w, .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            off_x: *crop_w as u32,
                            off_y: *crop_h as u32,
                            ..Default::default()
                        };
                        (LayerKind::Crop, None, 0, p)
                    }
                    ExecLayer::Crop { offset, .. } => {
                        // 输出尺寸 = reference 形状（shapes[top_slot] 已由 layer_out_shape 给出）
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            off_x: *offset as u32,
                            off_y: *offset as u32,
                            ..Default::default()
                        };
                        (LayerKind::Crop, None, 0, p)
                    }
                    ExecLayer::EltwiseAdd { .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::Add, None, 0, p)
                    }
                    ExecLayer::Concat { bottoms, .. } => {
                        // pad0/1/2 = 各 bottom 起始逻辑通道（<3 输入补 0xFFFFFFFF）
                        let mut starts = [0xFFFFFFFFu32; 3];
                        let mut acc = 0usize;
                        for (k, b) in bottoms.iter().enumerate().take(3) {
                            starts[k] = acc as u32;
                            acc += shapes[*b].2;
                        }
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            pad0: starts[0],
                            pad1: starts[1],
                            pad2: starts[2],
                            ..Default::default()
                        };
                        (LayerKind::Concat, None, 0, p)
                    }
                    ExecLayer::NearestUp2 { bottom, .. } => {
                        let (iw, ih, ic) = shapes[*bottom];
                        let p = LayerParams {
                            in_w: iw as u32,
                            in_h: ih as u32,
                            in_ch: ic as u32,
                            in_ch4: ic.div_ceil(4) as u32,
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            ..Default::default()
                        };
                        (LayerKind::NearestUp2, None, 0, p)
                    }
                    ExecLayer::ScaledAdd { beta, .. } => {
                        let p = LayerParams {
                            out_w: ow as u32,
                            out_h: oh as u32,
                            out_ch: oc as u32,
                            out_ch4: och4 as u32,
                            leaky_slope: *beta, // 参数复用：β
                            ..Default::default()
                        };
                        (LayerKind::ScaledAdd, None, 0, p)
                    }
                };
                p.bias_off = bias_off;
                layers.push(LayerGpu {
                    kind,
                    weight_srv,
                    bias_off,
                    bottom_tex,
                    top_tex: tex_idx,
                });
                params_cache.push(p);
            }

            // 常量缓冲（动态 + WRITE_DISCARD）
            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<LayerParams>() as u32, // 96 = 6×16 ✓
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut cbuf = None;
            engine
                .device
                .CreateBuffer(&cb_desc, None, Some(&mut cbuf))
                .map_err(|e| format!("CreateBuffer(CB): {}", e))?;
            let cbuf: ID3D11Buffer = cbuf.ok_or("常量缓冲创建失败")?;

            let (out_w, out_h, out_ch) = shapes[model.output_slot];
            let output_tex = slot_tex[model.output_slot];
            let total_mb: f64 = texs
                .iter()
                .map(|t| (t.w as f64 * t.h as f64 * t.ch4 as f64 * 16.0))
                .sum::<f64>()
                / 1024.0
                / 1024.0;
            log::info!(
                "[upscale/GPU] 会话建立：{} 层 / {} 纹理（{:.0}MB），tile {}x{}",
                layers.len(),
                texs.len(),
                total_mb,
                tile_w,
                tile_h
            );

            Ok(GpuSession {
                engine,
                texs,
                slot_tex,
                layers,
                params_cache,
                cbuf,
                in_w: pw,
                in_h: ph,
                in_ch: model.in_ch,
                out_w,
                out_h,
                out_ch,
                output_tex,
            })
        }
    }

    /// CHW Tensor → packed float4 内存（[slice4][h][w][4]，不足 4 通道补 0）
    fn pack_input(&self, t: &Tensor) -> Vec<f32> {
        let (w, h, c) = (t.w, t.h, t.c);
        let mut packed = vec![0.0f32; c.div_ceil(4) * h * w * 4];
        for ch in 0..c {
            let g = ch / 4;
            let comp = ch % 4;
            for y in 0..h {
                for x in 0..w {
                    packed[(g * h + y) * w * 4 + x * 4 + comp] = t.at(ch, y, x);
                }
            }
        }
        packed
    }

    /// 执行一次 tile 推理：输入 padded CHW Tensor → 输出整网输出（未裁剪）
    pub fn run_tile(&self, padded: &Tensor) -> Result<Tensor, String> {
        if padded.w != self.in_w || padded.h != self.in_h || padded.c != self.in_ch {
            return Err(format!(
                "tile 尺寸不符: 输入 {}x{}x{} 期望 {}x{}x{}",
                padded.w, padded.h, padded.c, self.in_w, self.in_h, self.in_ch
            ));
        }
        unsafe {
            let ctx = &self.engine.ctx;
            // 1. 上传输入（pack 后每 packed-slice 一个 subresource）
            let packed = self.pack_input(padded);
            let slice4 = self.texs[0].ch4 as usize;
            for g in 0..slice4 {
                ctx.UpdateSubresource(
                    &self.texs[0].tex,
                    g as u32, // D3D11CalcSubresource(mip=0, slice=g, mipLevels=1) = g
                    None,
                    packed[g * padded.w * padded.h * 4..].as_ptr() as *const core::ffi::c_void,
                    (padded.w * 16) as u32,
                    0,
                );
            }

            // 2. 逐层 dispatch
            self.dispatch_layers(self.layers.len())?;

            // 3. 读回输出槽位纹理
            self.readback(self.output_tex)
        }
    }

    /// 逐层 dispatch（run_tile / debug 共用；n = 跑前 n 层）
    unsafe fn dispatch_layers(&self, n: usize) -> Result<(), String> {
        let ctx = &self.engine.ctx;
        for (i, layer) in self.layers.iter().take(n).enumerate() {
            let p = self.params_cache[i];
            let out_tex = &self.texs[layer.top_tex];
            // 写常量
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map(CB): {}", e))?;
            std::ptr::copy_nonoverlapping(
                &p as *const LayerParams as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<LayerParams>(),
            );
            ctx.Unmap(&self.cbuf, 0);

            // 绑定：conv/deconv t0=权重+bias（同一 SRV，biasOff 区分）
            // pointwise：t2/t3/t4 = 输入纹理；u0 = 输出纹理
            let shader = match layer.kind {
                LayerKind::Conv => &self.engine.conv_cs,
                LayerKind::StrideConv => &self.engine.stride_conv_cs,
                LayerKind::Deconv => &self.engine.deconv_cs,
                LayerKind::Gap => &self.engine.gap_cs,
                LayerKind::Sigmoid => &self.engine.sigmoid_cs,
                LayerKind::Scale => &self.engine.scale_cs,
                LayerKind::Axpy => &self.engine.axpy_cs,
                LayerKind::Add => &self.engine.add_cs,
                LayerKind::Crop => &self.engine.crop_cs,
                LayerKind::Concat => &self.engine.concat_cs,
                LayerKind::NearestUp2 => &self.engine.nearest_up2_cs,
                LayerKind::ScaledAdd => &self.engine.scaled_add_cs,
            };
            let w_srv = layer
                .weight_srv
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or_else(|| {
                    // pointwise 无权重：借用第一个 bottom SRV 占位（shader 不读 t0）
                    self.texs[layer.bottom_tex[0]].srv.clone()
                });
            let t2 = self.texs[layer.bottom_tex[0]].srv.clone();
            let t3 = layer.bottom_tex.get(1).map(|&t| self.texs[t].srv.clone());
            let t4 = layer.bottom_tex.get(2).map(|&t| self.texs[t].srv.clone());
            ctx.CSSetShader(shader, None);
            ctx.CSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.CSSetShaderResources(0, Some(&[Some(w_srv), None, Some(t2), t3, t4]));
            let uavs = [Some(out_tex.uav.clone())];
            ctx.CSSetUnorderedAccessViews(0, uavs.len() as u32, Some(uavs.as_ptr()), None);

            // dispatch 尺寸：conv 2×2/线程（group 覆盖 16×16）；GAP (1,1,ch4)；其余 1 像素/线程
            let (gx, gy) = match layer.kind {
                LayerKind::Conv => ((out_tex.w + 15) / 16, (out_tex.h + 15) / 16),
                LayerKind::Gap => (1, 1),
                _ => ((out_tex.w + 7) / 8, (out_tex.h + 7) / 8),
            };
            ctx.Dispatch(gx, gy, out_tex.ch4);

            // 解绑（下轮该纹理将作为 SRV）
            let null_uav: Option<ID3D11UnorderedAccessView> = None;
            ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
            ctx.CSSetShaderResources(0, Some(&[None, None, None, None, None]));
            ctx.CSSetConstantBuffers(0, Some(&[None]));
        }
        ctx.CSSetShader(None, None);
        Ok(())
    }

    /// 调试：只跑前 n 层并读回该层输出（n=0 仅上传+读回输入，验证上传）
    pub fn debug_run_layers(&self, padded: &Tensor, n: usize) -> Result<Tensor, String> {
        assert!(n <= self.layers.len());
        unsafe {
            let ctx = &self.engine.ctx;
            let packed = self.pack_input(padded);
            let slice4 = self.texs[0].ch4 as usize;
            for g in 0..slice4 {
                ctx.UpdateSubresource(
                    &self.texs[0].tex,
                    g as u32,
                    None,
                    packed[g * padded.w * padded.h * 4..].as_ptr() as *const core::ffi::c_void,
                    (padded.w * 16) as u32,
                    0,
                );
            }
            self.dispatch_layers(n)?;
            let tex_idx = if n == 0 {
                0
            } else {
                self.layers[n - 1].top_tex
            };
            self.readback(tex_idx)
        }
    }

    /// 读回指定纹理（packed RGBA）→ unpack 成 CHW Tensor
    unsafe fn readback(&self, tex_idx: usize) -> Result<Tensor, String> {
        let src = &self.texs[tex_idx];
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.w,
            Height: src.h,
            MipLevels: 1,
            ArraySize: src.ch4,
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        self.engine
            .device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .map_err(|e| format!("CreateTexture2D(staging): {}", e))?;
        let staging: ID3D11Texture2D = staging.ok_or("staging 创建失败")?;

        self.engine.ctx.CopyResource(&staging, &src.tex);

        let ch = src.ch as usize;
        let (w, h) = (src.w as usize, src.h as usize);
        let mut t = Tensor::new(ch, h, w);
        // 每 packed slice 读 h×w 个 float4 → unpack 到 4 个逻辑通道
        for g in 0..src.ch4 as usize {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.engine
                .ctx
                .Map(&staging, g as u32, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map(staging[{}]): {}", g, e))?;
            let row_pitch = mapped.RowPitch as usize; // 字节
            let rows = std::slice::from_raw_parts(mapped.pData as *const f32, h * row_pitch / 4);
            for y in 0..h {
                let row_off = y * (row_pitch / 4);
                for x in 0..w {
                    let base = row_off + x * 4;
                    for j in 0..4 {
                        let c = g * 4 + j;
                        if c < ch {
                            t.data[(c * h + y) * w + x] = rows[base + j];
                        }
                    }
                }
            }
            self.engine.ctx.Unmap(&staging, g as u32);
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_root() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap_or_default();
        exe.parent()
            .map(|p| p.join("models"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| std::path::PathBuf::from(r"e:\jietu\图片放大器\models"))
    }

    /// GPU vs CPU 对拍（SE 模型全层类型：conv/stride-conv/deconv/GAP/sigmoid/scale/axpy/crop/add）
    /// 单 tile 整图前向，逐像素最大误差应 < 1e-4（FMA 收缩级差异）
    #[test]
    fn test_gpu_vs_cpu_se_models() {
        let root = model_root();
        let engine = match engine() {
            Ok(e) => e.0.lock().unwrap(),
            Err(e) => {
                eprintln!("[test] GPU 不可用，跳过: {}", e);
                return;
            }
        };
        for (kind, file) in [
            (
                super::super::model::ModelKind::Upresnet10,
                "scale2.0x_model.json.caffemodel",
            ),
            (
                super::super::model::ModelKind::Cunet,
                "scale2.0x_model.json.caffemodel",
            ),
            (
                super::super::model::ModelKind::Upconv7AnimeStyleArtRgb,
                "scale2.0x_model.json.caffemodel",
            ),
        ] {
            let m = super::super::model::WaifuModel::load(kind, &root, file).unwrap();
            // 48×48 渐变输入（含非线性变化，覆盖 sigmoid/SE 分支）
            let mut inp = Tensor::new(3, 48, 48);
            for y in 0..48 {
                for x in 0..48 {
                    let v = (x * 47 + y * 31) as f32 / (48.0 * 78.0);
                    for c in 0..3 {
                        inp.set(c, y, x, v * (0.5 + 0.25 * c as f32));
                    }
                }
            }
            let padded = super::super::pipeline::pad_replicate(&inp, m.net_offset);
            let cpu_out = super::super::cpu::forward(&m, &padded).unwrap();

            // GPU：单 tile（48×48 有效区域）
            let sess = GpuSession::new(&engine, &m, 48, 48)
                .unwrap_or_else(|e| panic!("{} 会话失败: {}", kind.dir_name(), e));
            let gpu_out = sess
                .run_tile(&padded)
                .unwrap_or_else(|e| panic!("{} GPU 前向失败: {}", kind.dir_name(), e));

            assert_eq!(
                (gpu_out.c, gpu_out.h, gpu_out.w),
                (cpu_out.c, cpu_out.h, cpu_out.w)
            );
            let mut max_err: f32 = 0.0;
            let mut sum_sq = 0.0f64;
            for (g, c) in gpu_out.data.iter().zip(cpu_out.data.iter()) {
                let d = (g - c).abs();
                max_err = max_err.max(d);
                sum_sq += (d as f64) * (d as f64);
            }
            let mse = sum_sq / cpu_out.data.len() as f64;
            let psnr = if mse > 0.0 {
                10.0 * (1.0 / mse).log10()
            } else {
                f64::INFINITY
            };
            eprintln!(
                "[test] {} GPU 对拍: max_err={:.2e} PSNR={:.1}dB",
                kind.dir_name(),
                max_err,
                psnr
            );
            assert!(
                max_err < 1e-3,
                "{} GPU/CPU 差异过大: max_err={}",
                kind.dir_name(),
                max_err
            );
        }
    }

    /// Real-ESRGAN GPU vs CPU 对拍（RRDBNet：conv/concat/nearest-up2/scaled-add 全算子）
    ///
    /// 深网（x4plus 837 层）对渐变输入的端到端一致；max_err 阈值放宽（深网误差累积 +
    /// GAN 权重对分布外输入产生大值 → 相对误差更合理）
    #[test]
    fn test_gpu_vs_cpu_realesrgan() {
        let root = model_root();
        let engine = match engine() {
            Ok(e) => e.0.lock().unwrap(),
            Err(e) => {
                eprintln!("[test] GPU 不可用，跳过: {}", e);
                return;
            }
        };
        for kind in [
            super::super::model::ModelKind::RealEsrganAnime6B,
            super::super::model::ModelKind::RealEsrganX4,
        ] {
            let f = kind.scale_file();
            let exists = root.join(f).exists() || root.join("RealESRGAN").join(f).exists();
            if !exists {
                eprintln!("[test] {} 缺失，跳过", f);
                continue;
            }
            let m = super::super::model::WaifuModel::load(kind, &root, f).unwrap();
            // 20×20 渐变输入（×4 → 80×80）
            let (iw, ih) = (20usize, 20usize);
            let mut inp = Tensor::new(3, ih, iw);
            for y in 0..ih {
                for x in 0..iw {
                    let v = (x * 13 + y * 7) as f32 / (iw as f32 * 20.0);
                    for c in 0..3 {
                        inp.set(c, y, x, v * (0.6 + 0.2 * c as f32));
                    }
                }
            }
            let cpu_out = super::super::cpu::forward(&m, &inp).unwrap();
            let sess = GpuSession::new(&engine, &m, iw, ih)
                .unwrap_or_else(|e| panic!("{} 会话失败: {}", f, e));
            let gpu_out = sess
                .run_tile(&inp)
                .unwrap_or_else(|e| panic!("{} GPU 前向失败: {}", f, e));

            assert_eq!(
                (gpu_out.c, gpu_out.h, gpu_out.w),
                (cpu_out.c, cpu_out.h, cpu_out.w),
                "{} 输出尺寸",
                f
            );
            let (mut max_err, mut cpu_max) = (0.0f32, 1e-6f32);
            for (g, c) in gpu_out.data.iter().zip(cpu_out.data.iter()) {
                max_err = max_err.max((g - c).abs());
                cpu_max = cpu_max.max(c.abs());
            }
            let rel = max_err / cpu_max;
            eprintln!(
                "[test] {} GPU 对拍: max_err={:.2e}（相对 {:.2e}，CPU 值域 ±{:.1}）",
                f, max_err, rel, cpu_max
            );
            assert!(
                rel < 1e-4,
                "{} GPU/CPU 相对误差过大: {} / {}",
                f,
                max_err,
                cpu_max
            );
        }
    }

    /// 微型网络对拍：Conv + Concat(2/3 输入) + ScaledAdd + NearestUp2 全算子组合
    /// （手工拓扑 + 确定性权重，直接验证新算子 GPU 实现）
    #[test]
    fn test_gpu_vs_cpu_micro_rrdb() {
        let engine = match engine() {
            Ok(e) => e.0.lock().unwrap(),
            Err(e) => {
                eprintln!("[test] GPU 不可用，跳过: {}", e);
                return;
            }
        };
        let mut seed = 42u64;
        let mut rnd = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / u32::MAX as f32) * 0.2 - 0.1
        };
        let mut conv = |in_ch, out_ch, leaky: f32, bottom, top| {
            let n = out_ch * in_ch * 9;
            let mut w = Vec::with_capacity(n);
            for _ in 0..n {
                w.push(rnd());
            }
            let b: Vec<f32> = (0..out_ch).map(|_| rnd() * 0.5).collect();
            ExecLayer::Conv {
                weight: w,
                bias: b,
                in_ch,
                out_ch,
                kernel: 3,
                pad: 1,
                stride: 1,
                dilation: 1,
                leaky_slope: leaky,
                bottom,
                top,
            }
        };
        // 拓扑：input(3) →conv1(8) →conv2(8) →concat12(16) →conv3(8) →scaled_add(β.3, +conv1)
        //      →up2(8,×2) →conv4(3) ；另测 3 输入 concat：concat(conv1,conv2,conv3 变体)
        let layers = vec![
            conv(3, 8, 0.2, 0, 1),
            conv(3, 8, 0.2, 0, 2),
            ExecLayer::Concat {
                bottoms: vec![1, 2],
                top: 3,
            },
            conv(16, 8, 0.2, 3, 4),
            ExecLayer::ScaledAdd {
                a: 4,
                b: 1,
                beta: 0.3,
                top: 5,
            },
            ExecLayer::NearestUp2 { bottom: 5, top: 6 },
            conv(8, 8, 0.2, 6, 7),
            // 3 输入 concat：conv1 + conv2 + scaled_add 结果（8+8+8=24ch，同空间）
            ExecLayer::Concat {
                bottoms: vec![1, 2, 5],
                top: 8,
            },
            conv(24, 3, 1.0, 8, 9),
        ];
        let model = WaifuModel {
            layers,
            in_ch: 3,
            net_offset: 0,
            inner_scale: 2,
            kind: super::super::model::ModelKind::RealEsrganAnime6B,
            n_slots: 10,
            output_slot: 9,
        };
        let (iw, ih) = (9usize, 7usize); // 非方形奇数尺寸
        let mut inp = Tensor::new(3, ih, iw);
        for (k, v) in inp.data.iter_mut().enumerate() {
            *v = ((k * 37) % 97) as f32 / 97.0;
        }
        let sess = GpuSession::new(&engine, &model, iw, ih).unwrap();
        // 逐层截断对比（截断模型 = 前 n 层 + output_slot=末层 top）
        let mut first_bad = None;
        for n in 1..=model.layers.len() {
            let trunc = WaifuModel {
                layers: model.layers[..n].to_vec(),
                output_slot: model.layers[n - 1].top(),
                ..clone_model(&model)
            };
            let cpu_n = super::super::cpu::forward(&trunc, &inp).unwrap();
            let gpu_n = sess.debug_run_layers(&inp, n).unwrap();
            let mut e = 0.0f32;
            for (g, c) in gpu_n.data.iter().zip(cpu_n.data.iter()) {
                e = e.max((g - c).abs());
            }
            eprintln!(
                "[micro] 层 {n} {:?}: err={:.2e}",
                kind_name(&model.layers[n - 1]),
                e
            );
            if e > 1e-4 && first_bad.is_none() {
                // 打印错误分布模式（前 6 个错误像素的位置与值）
                let mut shown = 0;
                for (idx, (g, c)) in gpu_n.data.iter().zip(cpu_n.data.iter()).enumerate() {
                    if (g - c).abs() > 1e-4 && shown < 6 {
                        let ch = idx / (gpu_n.h * gpu_n.w);
                        let rem = idx % (gpu_n.h * gpu_n.w);
                        let (y, x) = (rem / gpu_n.w, rem % gpu_n.w);
                        eprintln!("[micro]   ch{ch} ({x},{y}): gpu={g:.4} cpu={c:.4}");
                        shown += 1;
                    }
                }
            }
            if e > 1e-4 && first_bad.is_none() {
                first_bad = Some((n, e));
            }
        }
        match first_bad {
            Some((n, e)) => panic!("微型网首个分歧层 {}（err={:.2e}）", n, e),
            None => eprintln!("[test] 微型 RRDB 算子网全部层一致"),
        }
    }

    fn clone_model(m: &WaifuModel) -> WaifuModel {
        WaifuModel {
            layers: Vec::new(),
            in_ch: m.in_ch,
            net_offset: m.net_offset,
            inner_scale: m.inner_scale,
            kind: m.kind,
            n_slots: m.n_slots,
            output_slot: m.output_slot,
        }
    }

    fn kind_name(l: &ExecLayer) -> &'static str {
        match l {
            ExecLayer::Conv { .. } => "Conv",
            ExecLayer::Deconv { .. } => "Deconv",
            ExecLayer::GlobalAvgPool { .. } => "GAP",
            ExecLayer::Sigmoid { .. } => "Sigmoid",
            ExecLayer::Scale { .. } => "Scale",
            ExecLayer::Axpy { .. } => "Axpy",
            ExecLayer::CropCenter { .. } => "CropCenter",
            ExecLayer::Crop { .. } => "Crop",
            ExecLayer::EltwiseAdd { .. } => "Add",
            ExecLayer::Concat { .. } => "Concat",
            ExecLayer::NearestUp2 { .. } => "NearestUp2",
            ExecLayer::ScaledAdd { .. } => "ScaledAdd",
        }
    }

    #[allow(dead_code)]
    fn micro_unused() {}
}
