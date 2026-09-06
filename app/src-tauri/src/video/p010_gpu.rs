//! HDR10 P010 GPU 转换器（视频录制专用）
//!
//! 管线：DDA 帧（scRGB f16 / Bgra8）→ 线性 RGB BT.2020 → PQ OETF per 通道
//! → BT.2100 NCL YCbCr（矩阵作用于 PQ 编码域，标准 HDR10 流程）→ 10bit
//! limited 量化 → P010 双平面（Y: R16_UINT w×h + UV: R16G16_UINT w/2×h/2，
//! 16bit 样点高 10 位 = 值<<6）。
//!
//! 与 record/gpu.rs（动图 PQ16 RGB）的关系：模式借用（中转纹理/cbuf/
//! map_read_timeout/解绑序列），实现独立——动图输出 RGB 紧凑 PQ16 给 libjxl，
//! 视频输出 YUV 双平面给 nvenc main10，二者数学前段相同后段分叉。
//!
//! 单线程同步模型（对齐 video/session.rs）：process_frame 提交 GPU →
//! map_yuv 等待回读 → 调用方读切片 → unmap。无池深轮转（无跨线程延迟 Map）。

use std::sync::Mutex;

use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BUFFER_DESC,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAP_READ_WRITE,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_TEXTURE2D,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R16G16_UINT, DXGI_FORMAT_R16_UINT, DXGI_SAMPLE_DESC,
};

use crate::color::matrix::bt709_to_bt2020;

/// P010 转换 shader：DDA 帧 → BT.2100 NCL YCbCr（PQ 域）→ P010 双平面
///
/// 两种输入（p2.w 标志位）：0 = scRGB f16（HDR 桌面）/ 1 = Bgra8 sRGB（SDR 桌面
/// 兜底——HDR 录制中显示器切回 SDR 时 DDA 交付 BGRA8，走 sRGB 解码分支保持录制连续）。
///
/// 数学（BT.2100 HDR10 标准序）：
/// 1. 线性 RGB（BT.709→BT.2020 矩阵，scRGB 1.0=80nits）
/// 2. ×(80/10000) → PQ 线性域 → PQ OETF per 通道（非线性编码域）
/// 3. NCL 矩阵作用于 **PQ 编码域**：Y=0.2627R'+0.6780G'+0.0593B'；
///    Cb=(B'-Y')/1.8814；Cr=(R'-Y')/1.4746（BT.2100 Table 5）
/// 4. 10bit limited 量化：Y10=64+876·Y（[64,940]）；C10=512+896·C（[64,960]）
/// 5. P010 样点 = 值 << 6（16bit 高 10 位）
/// 6. UV 4:2:0：2×2 像素 PQ 域平均 RGB → 平均色算 YCbCr（box 下采样）
const CONVERT_P010_CS: &str = r#"
Texture2D<float4> srcTex : register(t0);
RWTexture2D<uint>  yTex  : register(u0);
RWTexture2D<uint2> uvTex : register(u1);

cbuffer Params : register(b0)
{
    float4 p0 : packoffset(c0); // BT.709→BT.2020 第 0 行 + cropX
    float4 p1 : packoffset(c1); // 第 1 行 + cropY
    float4 p2 : packoffset(c2); // 第 2 行 + inputMode（0=scRGB / 1=sRGB）
    float4 p3 : packoffset(c3); // dstW, dstH, 0, 0
};

float pq_oetf(float l)
{
    if (l <= 0.0) return 0.0;
    l = min(l, 1.0);
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float lm = pow(l, m1);
    float np = (c1 + c2 * lm) / (1.0 + c3 * lm);
    return pow(np, m2);
}

float srgb_eotf(float e)
{
    if (e <= 0.04045) return e / 12.92;
    return pow((e + 0.055) / 1.055, 2.4);
}

// PQ 编码域 YCbCr → 10bit limited 量化
uint quant_y(float y) { return (uint)clamp(64.0 + 876.0 * y + 0.5, 64.0, 940.0); }
uint quant_c(float c) { return (uint)clamp(512.0 + 896.0 * c + 0.5, 64.0, 960.0); }

// 单像素：采样 → sRGB 解码（可选）→ BT.2020 矩阵 → PQ 编码
float3 to_pq(uint2 s)
{
    float3 sc = srcTex[s].rgb;
    if (p2.w > 0.5)
    {
        sc = float3(srgb_eotf(sc.r), srgb_eotf(sc.g), srgb_eotf(sc.b));
    }
    float3x3 m = float3x3(
        p0.x, p0.y, p0.z,
        p1.x, p1.y, p1.z,
        p2.x, p2.y, p2.z);
    float3 lin = mul(m, sc);
    lin *= (80.0 / 10000.0);
    return float3(pq_oetf(lin.x), pq_oetf(lin.y), pq_oetf(lin.z));
}

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint dstW = (uint)p3.x;
    uint dstH = (uint)p3.y;
    if (tid.x >= dstW || tid.y >= dstH) return;

    // 输出坐标 → 源采样坐标（crop 偏移；全屏录制 offset=0）
    uint2 src = uint2(tid.x + (uint)p0.w, tid.y + (uint)p1.w);
    // 源纹理边界（UV 下采样 clamp 用）
    uint srcW = dstW + (uint)p0.w;
    uint srcH = dstH + (uint)p1.w;

    float3 pq = to_pq(src);

    // Y（PQ 域 NCL 矩阵行 0；<<6 = P010 高 10 位）
    float yy = 0.2627 * pq.r + 0.6780 * pq.g + 0.0593 * pq.b;
    yTex[tid.xy] = quant_y(yy) << 6;

    // UV 4:2:0：偶数行列线程计算 2×2 邻域 PQ 域平均色度（box 下采样；
    // 奇数尺寸边界 clamp 采样）
    if (tid.x % 2 == 0 && tid.y % 2 == 0)
    {
        uint x1 = min(src.x + 1, srcW - 1);
        uint y1 = min(src.y + 1, srcH - 1);
        float3 p10 = to_pq(uint2(x1, src.y));
        float3 p01 = to_pq(uint2(src.x, y1));
        float3 p11 = to_pq(uint2(x1, y1));
        float3 avg = (pq + p10 + p01 + p11) * 0.25;
        float ay = 0.2627 * avg.r + 0.6780 * avg.g + 0.0593 * avg.b;
        float cb = (avg.b - ay) / 1.8814;
        float cr = (avg.r - ay) / 1.4746;
        uvTex[uint2(tid.x / 2, tid.y / 2)] = uint2(quant_c(cb) << 6, quant_c(cr) << 6);
    }
}
"#;

/// cbuffer 布局（4 × float4 = 64 字节；对齐 record/gpu.rs CsParams）
#[repr(C)]
struct CsParams {
    p0: [f32; 4],
    p1: [f32; 4],
    p2: [f32; 4],
    p3: [f32; 4],
}

/// 输入中转纹理（DDA 帧 CopyResource 目标 + SRV 采样源；DDA 纹理不保证
/// SHADER_RESOURCE 绑定，必须经中转——CopyResource 要求源/目标同格式）
struct InputBundle {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

/// P010 双平面输出（UAV 纹理 + staging 读回）
struct P010Targets {
    y_tex: ID3D11Texture2D,
    y_uav: ID3D11UnorderedAccessView,
    y_staging: ID3D11Texture2D,
    uv_tex: ID3D11Texture2D,
    uv_uav: ID3D11UnorderedAccessView,
    uv_staging: ID3D11Texture2D,
}

/// HDR10 P010 GPU 转换器（单线程使用：video 录制线程独占）
pub struct P010Gpu {
    device: ID3D11Device,
    context: Mutex<ID3D11DeviceContext>,
    convert_cs: ID3D11ComputeShader,
    /// 输入中转纹理（格式跟随 DDA 帧：HDR 桌面 scRGB / SDR 桌面 Bgra8；
    /// 格式切换时重建——录制中显示模式切换罕见）
    input: Mutex<InputBundle>,
    /// 双常量缓冲（仅 p2.w 输入模式位不同，0=scRGB / 1=sRGB）
    cbuf_scrgb: ID3D11Buffer,
    cbuf_bgra: ID3D11Buffer,
    targets: P010Targets,
    pub width: u32,
    pub height: u32,
}

impl P010Gpu {
    /// 创建（设备/上下文来自 DDA capturer——GPU 管线与 DDA 帧同设备才能 CopyResource）
    pub fn new(
        device: &ID3D11Device,
        context: ID3D11DeviceContext,
        w: u32,
        h: u32,
    ) -> anyhow::Result<Self> {
        unsafe {
            let convert_cs = compile_cs(device, CONVERT_P010_CS, b"video_p010\0")?;
            let input = make_input_bundle(device, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT)?;
            let make_cbuf = |input_mode: f32| -> anyhow::Result<ID3D11Buffer> {
                let mat = bt709_to_bt2020().m;
                let params = CsParams {
                    p0: [mat[0][0], mat[0][1], mat[0][2], 0.0],
                    p1: [mat[1][0], mat[1][1], mat[1][2], 0.0],
                    p2: [mat[2][0], mat[2][1], mat[2][2], input_mode],
                    p3: [w as f32, h as f32, 0.0, 0.0],
                };
                make_constant_buffer(device, &params)
            };
            let cbuf_scrgb = make_cbuf(0.0)?;
            let cbuf_bgra = make_cbuf(1.0)?;
            let targets = make_targets(device, w, h)?;
            Ok(P010Gpu {
                device: device.clone(),
                context: Mutex::new(context),
                convert_cs,
                input: Mutex::new(input),
                cbuf_scrgb,
                cbuf_bgra,
                targets,
                width: w,
                height: h,
            })
        }
    }

    /// 提交一帧 GPU 转换（DDA 帧 → P010 双平面 → staging 拷贝入队）
    ///
    /// 调用后须先 `capturer.release_frame()` 归还 DDA 队列，再 map_yuv 读回。
    /// Bgra8 帧（录制中显示器切回 SDR）自动切换中转纹理与 cbuf（sRGB 解码分支）。
    pub fn process_frame(&self, desktop_tex: &ID3D11Texture2D) -> anyhow::Result<()> {
        let ctx = self.context.lock().unwrap();
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            desktop_tex.GetDesc(&mut desc);
            let format = desc.Format;
            if format != DXGI_FORMAT_R16G16B16A16_FLOAT && format != DXGI_FORMAT_B8G8R8A8_UNORM {
                anyhow::bail!("不支持的 DDA 帧格式 {:?}（仅 scRGB/Bgra8）", format);
            }
            // 输入中转纹理与帧同格式同尺寸（懒切换；格式稳定时零开销）
            let input_guard = self.input.lock().unwrap();
            let mut in_desc = D3D11_TEXTURE2D_DESC::default();
            input_guard.tex.GetDesc(&mut in_desc);
            if in_desc.Format != format
                || in_desc.Width != desc.Width
                || in_desc.Height != desc.Height
            {
                drop(input_guard);
                let mut guard = self.input.lock().unwrap();
                // 双重检查（drop 后重新 lock 之间的窗口；单线程下无实际竞争）
                let mut d = D3D11_TEXTURE2D_DESC::default();
                guard.tex.GetDesc(&mut d);
                if d.Format != format || d.Width != desc.Width || d.Height != desc.Height {
                    *guard = make_input_bundle(&self.device, desc.Width, desc.Height, format)?;
                }
                process_dispatch(
                    &ctx,
                    self,
                    &guard,
                    format,
                    desktop_tex,
                )
            } else {
                process_dispatch(&ctx, self, &input_guard, format, desktop_tex)
            }
        }
    }

    /// Map 双平面 staging（等待 GPU 完成；DO_NOT_WAIT 轮询防驱动回读悬挂）
    ///
    /// 返回 (y, uv) 映射；调用方读切片后必须 unmap()。
    /// y 切片 = RowPitch × height 字节；uv 切片 = RowPitch × ceil(h/2) 字节。
    pub fn map_yuv(
        &self,
    ) -> anyhow::Result<(D3D11_MAPPED_SUBRESOURCE, D3D11_MAPPED_SUBRESOURCE)> {
        let ctx = self.context.lock().unwrap();
        unsafe {
            let y = map_read_timeout(&ctx, &self.targets.y_staging, "p010_y")?;
            let uv = map_read_timeout(&ctx, &self.targets.uv_staging, "p010_uv")?;
            Ok((y, uv))
        }
    }

    /// Unmap 双平面（Map 读完后调用）
    pub fn unmap(&self) {
        let ctx = self.context.lock().unwrap();
        unsafe {
            ctx.Unmap(&self.targets.y_staging, 0);
            ctx.Unmap(&self.targets.uv_staging, 0);
        }
    }
}

/// dispatch 序列（CopyResource → CSSet → Dispatch → 解绑 → staging 拷贝）
/// 独立函数避免 process_frame 中锁守卫生命周期的分支重复
unsafe fn process_dispatch(
    ctx: &ID3D11DeviceContext,
    gpu: &P010Gpu,
    input: &InputBundle,
    format: DXGI_FORMAT,
    desktop_tex: &ID3D11Texture2D,
) -> anyhow::Result<()> {
    let cbuf = if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        &gpu.cbuf_scrgb
    } else {
        &gpu.cbuf_bgra
    };
    ctx.CopyResource(&input.tex, desktop_tex);
    ctx.CSSetShader(&gpu.convert_cs, None);
    ctx.CSSetConstantBuffers(0, Some(&[Some(cbuf.clone())]));
    ctx.CSSetShaderResources(0, Some(&[Some(input.srv.clone())]));
    let uavs = [
        Some(gpu.targets.y_uav.clone()),
        Some(gpu.targets.uv_uav.clone()),
    ];
    ctx.CSSetUnorderedAccessViews(0, uavs.len() as u32, Some(uavs.as_ptr()), None);
    ctx.Dispatch((gpu.width + 7) / 8, (gpu.height + 7) / 8, 1);
    // 解绑（UAV 复用防护，沿用项目经验）
    let null_uavs = [None::<ID3D11UnorderedAccessView>, None];
    ctx.CSSetUnorderedAccessViews(0, 2, Some(null_uavs.as_ptr()), None);
    ctx.CSSetShaderResources(0, Some(&[None]));
    ctx.CSSetConstantBuffers(0, Some(&[None]));
    ctx.CSSetShader(None, None);
    // staging 拷贝（GPU 队列异步执行）
    ctx.CopyResource(&gpu.targets.y_staging, &gpu.targets.y_tex);
    ctx.CopyResource(&gpu.targets.uv_staging, &gpu.targets.uv_tex);
    Ok(())
}

/// 中转纹理创建（格式可指定）
unsafe fn make_input_bundle(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> anyhow::Result<InputBundle> {
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    device
        .CreateTexture2D(&tex_desc, None, Some(&mut tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(p010 input): {}", e))?;
    let tex: ID3D11Texture2D = tex.ok_or_else(|| anyhow::anyhow!("中转纹理创建失败"))?;

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut srv = None;
    device
        .CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv))
        .map_err(|e| anyhow::anyhow!("CreateSRV(p010 input): {}", e))?;
    let srv: ID3D11ShaderResourceView = srv.ok_or_else(|| anyhow::anyhow!("SRV 创建失败"))?;

    Ok(InputBundle { tex, srv })
}

/// P010 双平面资源（Y: R16_UINT w×h；UV: R16G16_UINT ceil(w/2)×ceil(h/2)）
unsafe fn make_targets(device: &ID3D11Device, w: u32, h: u32) -> anyhow::Result<P010Targets> {
    let uv_w = w.div_ceil(2);
    let uv_h = h.div_ceil(2);

    let (y_tex, y_uav) = make_uav_texture(device, w, h, DXGI_FORMAT_R16_UINT)?;
    let y_staging = make_staging(device, w, h, DXGI_FORMAT_R16_UINT)?;
    let (uv_tex, uv_uav) = make_uav_texture(device, uv_w, uv_h, DXGI_FORMAT_R16G16_UINT)?;
    let uv_staging = make_staging(device, uv_w, uv_h, DXGI_FORMAT_R16G16_UINT)?;

    Ok(P010Targets {
        y_tex,
        y_uav,
        y_staging,
        uv_tex,
        uv_uav,
        uv_staging,
    })
}

unsafe fn make_uav_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> anyhow::Result<(ID3D11Texture2D, ID3D11UnorderedAccessView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(p010 uav): {}", e))?;
    let tex: ID3D11Texture2D = tex.ok_or_else(|| anyhow::anyhow!("P010 纹理创建失败"))?;

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav = None;
    device
        .CreateUnorderedAccessView(&tex, Some(&uav_desc), Some(&mut uav))
        .map_err(|e| anyhow::anyhow!("CreateUAV(p010): {}", e))?;
    let uav: ID3D11UnorderedAccessView = uav.ok_or_else(|| anyhow::anyhow!("UAV 创建失败"))?;
    Ok((tex, uav))
}

unsafe fn make_staging(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        // READ|WRITE：读回 + 鼠标光标/点击效果 CPU 叠加原地写（Map READ_WRITE；
        // 写入仅影响 CPU 侧副本——数据随后由 push_p010 读走，无需回传 GPU）
        CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(p010 staging): {}", e))?;
    tex.ok_or_else(|| anyhow::anyhow!("staging 创建失败"))
}

/// 常量缓冲创建（DEFAULT + 初始数据一次性上传）
unsafe fn make_constant_buffer(
    device: &ID3D11Device,
    params: &CsParams,
) -> anyhow::Result<ID3D11Buffer> {
    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<CsParams>() as u32, // 64
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: params as *const CsParams as *const core::ffi::c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut cbuf = None;
    device
        .CreateBuffer(&cb_desc, Some(&init), Some(&mut cbuf))
        .map_err(|e| anyhow::anyhow!("CreateBuffer(p010 cbuf): {}", e))?;
    cbuf.ok_or_else(|| anyhow::anyhow!("常量缓冲创建失败"))
}

/// 带 3s 超时的 staging Map（READ_WRITE：光标叠加原地写；DO_NOT_WAIT 轮询防
/// 驱动回读永久悬挂——RTX 5060 实测坑，沿用 record/gpu.rs 模式）
unsafe fn map_read_timeout(
    ctx: &ID3D11DeviceContext,
    res: &ID3D11Texture2D,
    tag: &str,
) -> anyhow::Result<D3D11_MAPPED_SUBRESOURCE> {
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
    let t0 = std::time::Instant::now();
    loop {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let hr = ctx.Map(
            res,
            0,
            D3D11_MAP_READ_WRITE,
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
            Some(&mut mapped),
        );
        match hr {
            Ok(()) => return Ok(mapped),
            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                if t0.elapsed() > std::time::Duration::from_secs(3) {
                    anyhow::bail!("Map({}) GPU 3s 未完成回读（疑似驱动/设备异常）", tag);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(anyhow::anyhow!("Map({}): {}", tag, e)),
        }
    }
}

/// 编译 compute shader（cs_5_0）
unsafe fn compile_cs(
    device: &ID3D11Device,
    src: &str,
    name: &[u8],
) -> anyhow::Result<ID3D11ComputeShader> {
    use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
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
        anyhow::bail!("D3DCompile(video p010): {:?} {}", hr, msg);
    }
    let blob: ID3DBlob = code.ok_or_else(|| anyhow::anyhow!("编译输出缺失"))?;
    let mut cs = None;
    let bytecode = {
        let p = blob.GetBufferPointer() as *const u8;
        std::slice::from_raw_parts(p, blob.GetBufferSize())
    };
    device
        .CreateComputeShader(bytecode, None, Some(&mut cs))
        .map_err(|e| anyhow::anyhow!("CreateComputeShader: {}", e))?;
    cs.ok_or_else(|| anyhow::anyhow!("CS 创建失败"))
}
