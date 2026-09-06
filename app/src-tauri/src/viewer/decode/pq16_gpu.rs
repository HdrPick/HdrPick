//! native 播放出口 GPU 化：PQ16 u16（libjxl RGBA 直出）→ scRGB f16 RGBA16F
//! 纹理直写（GPU 常驻显存池槽位）。CPU 侧 PQ→nits LUT 查表 + 色域矩阵 +
//! /80 + f32→f16 转换（jxl.rs `pq16_interleaved_to_scrgb`，动画逐帧 8-12ms，
//! 36ms 解码预算的大头）整体卸载到 GPU compute。
//!
//! 数学与 hybrid_gpu.rs pass F 同源（LUT/矩阵/除法/f32tof16 逐位一致链已锤定），
//! 一处差异：CPU 参考实现 `exr::f32_to_f16` 把次正规区（f16 指数域 0，即
//! |x| < 2^-14 的 RTZ 编码区）压成 ±0（exp<=0 早退），而 `f32tof16` 原生展开
//! 次正规位型 → shader 内 `f16_cpu` 显式对齐（指数域 0 → 只留符号位），否则
//! 黑场暗部会出现 ±0 翻转位型分歧（PQ 低位 LSB 在陡峭区的必经地带）。
//!
//! 布局：输入 = `StructuredBuffer<uint2>`（1 元素/像素，lo=R|G<<16、hi=B|A<<16，
//! 与 libjxl RGBA u16 小端行交错逐字节对应，[`super::jxl::Pq16Frame`]）；
//! 输出 = `RWTexture2D<float4>`（RGBA16F UAV 直写，f16 位型一致性链同
//! hybrid_gpu F16_TEX_TAIL：f32tof16 RTZ 截断 → f16tof32 回 float → RGBA16F
//! 存储转换对 f16 可精确表示值恒等，无二次舍入）。
//!
//! 已知坑（同 hybrid_gpu）：FXC 大数组（LUT 走 structured buffer 非常量数组）；
//! cbuffer 跨 pass 残留 → dispatch 后全量解绑；f32tof16 是 RTZ。

use crate::upscale::d3d11::engine;
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_BUFFER;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_BUFFER_SRV, D3D11_BUFFER_SRV_0, D3D11_BUFFER_SRV_1, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC,
    D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_UNKNOWN};

use super::jxl::Pq16Frame;

// PQ16 → scRGB f16 直写 compute shader（native 播放出口）。
// 线程 = 一个输出像素；数学与 jxl.rs pq16_interleaved_to_scrgb 逐位同源：
// u16 PQ → LUT 线性 nits → 色域矩阵（2020→709，解码器 ColorEncoding 捕获）
// → /80 → f32tof16（RTZ 截断 + 次正规压零，f16_cpu）→ RGBA16F UAV 直写。
const PQ16_F16_CS: &str = r#"
StructuredBuffer<uint2> Pq  : register(t0);   // [w*h] RGBA u16 PQ BT.2020（lo=R|G<<16，hi=B|A<<16）
StructuredBuffer<float> Lut : register(t1);   // 65536：u16 PQ → 线性 nits（与 CPU pq16_nits_lut 同表）

cbuffer Pq16Params : register(b0) {
    uint npix;    // w*h
    uint xsize;   // 图像宽
    uint ysize;   // 图像高
    uint pad0; uint pad1; uint pad2; uint pad3; uint pad4;
    float4 m0;    // 色域矩阵（2020→709）行 0
    float4 m1;    // 行 1
    float4 m2;    // 行 2
};

RWTexture2D<float4> Dst : register(u0); // RGBA16F 直写（调用方槽纹理，规格 = xsize×ysize）

// CPU exr::f32_to_f16 语义复刻：RTZ 截断 + 次正规区（f16 指数域 0）压 ±0
//（f32tof16 原生展开次正规位型，会与 CPU 在暗部 ±0 翻转——显式对齐）
uint f16_cpu(float x) {
    uint h = f32tof16(x);
    if ((h & 0x7C00u) == 0u) h &= 0x8000u;
    return h;
}

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= npix) return;
    uint px = p % xsize;
    uint py = p / xsize;
    uint2 v = Pq[p];
    float nr = Lut[v.x & 0xFFFF];
    float ng = Lut[v.x >> 16];
    float nb = Lut[v.y & 0xFFFF];
    // precise：禁止 FXC 把 a*b+c 收缩成单舍入 mad（FMA）——CPU Mat3::apply
    // 是双舍入（mul、add 各一次），融合会引入 ±1 f16 ULP 位型分歧
    precise float cr = m0.x * nr + m0.y * ng + m0.z * nb;
    precise float cg = m1.x * nr + m1.y * ng + m1.z * nb;
    precise float cb = m2.x * nr + m2.y * ng + m2.z * nb;
    precise float fr = cr / 80.0;
    precise float fg = cg / 80.0;
    precise float fb = cb / 80.0;
    // A：u16 → [0,1] → f16（a=65535 → 1.0 → 0x3C00，与 CPU 3 通道文件硬编码
    // 0x3C00 / 4 通道文件 f32_to_f16(a/65535)（a=65535 时 1.0）两分支同值）
    precise float fa = (float)(v.y >> 16) / 65535.0;
    Dst[uint2(px, py)] = float4(
        f16tof32(f16_cpu(fr)),
        f16tof32(f16_cpu(fg)),
        f16tof32(f16_cpu(fb)),
        f16tof32(f16_cpu(fa)));
}
"#;

/// 会话（shader 编译一次；LUT 一次上传恒定；输入缓冲随尺寸重建——动画各帧
/// 同尺寸 → 常态零重建）。所有访问持 SESSION 锁（引擎锁内短临界区）。
struct Pq16Session {
    cs: ID3D11ComputeShader,
    /// Pq16Params（80B，DYNAMIC 每帧 Map/WRITE_DISCARD）
    cbuf: ID3D11Buffer,
    /// PQ16→nits LUT SRV（65536 f32 = 256KB，内容恒定 → 首次上传后不再触碰；
    /// 缓冲本体由 SRV 的 COM 引用保活）
    lut_srv: ID3D11ShaderResourceView,
    /// PQ16 输入（StructuredBuffer<uint2>，npix 元素 = w*h×8B）
    in_buf: ID3D11Buffer,
    in_srv: ID3D11ShaderResourceView,
    /// 输入容量（元素数；尺寸变化整块重建——全量 UpdateSubresource 语义下
    /// "只增不缩"会越界读 CPU 源）
    cap: u32,
}

impl Pq16Session {
    fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<Self, String> {
        let cs = crate::upscale::d3d11::compile_cs_cached(device, PQ16_F16_CS, b"pq16_f16\0")?;
        let cbuf = mk_cbuf(device, 80)?;
        let (lut_buf, lut_srv) = mk_srv_buf(device, 65536, 4)?;
        // PQ16 → 线性 nits 查找表（与 jxl.rs pq16_nits_lut 同式同源——GPU/CPU
        // 共用同一数值保证逐位一致）
        unsafe {
            let lut: Vec<f32> = (0..65536u32)
                .map(|i| crate::color::pq_eotf(i as f32 / 65535.0) * 10000.0)
                .collect();
            ctx.UpdateSubresource(
                &lut_buf,
                0,
                None,
                lut.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
        drop(lut_buf); // SRV 持 COM 引用，资源保活
        // 占位 1 元素（首次 convert 按 npix 重建）
        let (in_buf, in_srv) = mk_srv_buf(device, 1, 8)?;
        Ok(Pq16Session {
            cs,
            cbuf,
            lut_srv,
            in_buf,
            in_srv,
            cap: 1,
        })
    }
}

static SESSION: std::sync::OnceLock<std::sync::Mutex<Result<Pq16Session, String>>> =
    std::sync::OnceLock::new();

/// PQ16 → scRGB f16 直写槽纹理（引擎锁内单次调用：上传 PQ16 → dispatch →
/// 全量解绑；动画帧 ~33MB 上传 + ~1ms 级 dispatch，锁持有与 hybrid 链同纪律）。
///
/// `dst_tex` 规格（w×h，RGBA16F，UAV 可绑）即转换尺寸；`frame.data` 须为
/// w×h×8（RGBA u16 行交错，[`super::jxl::AnimationDecoder::next_frame_pq16`]
/// 直出）；gamut 取 `frame.gamut`（解码器 ColorEncoding 捕获，与 CPU 出口
/// 同一矩阵）。输出位型与 CPU `pq16_interleaved_to_scrgb` **逐位一致**
///（对拍测试 jxl tests `native_gpu_pq16_matches_cpu`）。
pub fn convert_into(frame: &Pq16Frame, dst_tex: &ID3D11Texture2D) -> Result<(), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { convert_impl(&eng, frame, dst_tex) }
}

unsafe fn convert_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    frame: &Pq16Frame,
    dst_tex: &ID3D11Texture2D,
) -> Result<(), String> {
    let (device, ctx) = eng.device_ctx();
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        dst_tex.GetDesc(&mut desc);
    }
    let (w, h) = (desc.Width, desc.Height);
    let npix = w as u64 * h as u64;
    if frame.data.len() as u64 != npix * 8 {
        return Err(format!(
            "PQ16 帧字节 {} 与槽纹理 {}x{} RGBA u16（{}B）不匹配",
            frame.data.len(),
            w,
            h,
            npix * 8
        ));
    }
    if desc.Format.0 != DXGI_FORMAT_R16G16B16A16_FLOAT.0 {
        return Err(format!(
            "槽纹理格式异常（预期 RGBA16F，实为 {:?}）",
            desc.Format.0
        ));
    }

    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(Pq16Session::new(device, ctx)))
        .lock()
        .map_err(|e| format!("pq16 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };

    // 输入缓冲：尺寸变化整块重建（常态零重建；重建后旧 SRV 引用即失效，
    // 上传/绑定都在本次调用内完成）
    if sess.cap != npix as u32 {
        let (b, v) = mk_srv_buf(device, npix as u32, 8)?;
        sess.in_buf = b;
        sess.in_srv = v;
        sess.cap = npix as u32;
    }
    unsafe {
        // PQ16 帧上传（w*h*8B 全量，扁平 memcpy——与 hybrid xyb 55MB 上传同模式）
        ctx.UpdateSubresource(
            &sess.in_buf,
            0,
            None,
            frame.data.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        // 常量（Pq16Params 80B：8 标量 + 3×float4——float4 起始于 cbuf 的
        // 16 字节寄存器边界（偏移 32），Rust 侧 8×u32 显式对齐同布局）
        #[repr(C)]
        struct Pq16Params {
            npix: u32,
            xsize: u32,
            ysize: u32,
            pad: [u32; 5],
            m0: [f32; 4],
            m1: [f32; 4],
            m2: [f32; 4],
        }
        let g = &frame.gamut.m;
        let params = Pq16Params {
            npix: npix as u32,
            xsize: w,
            ysize: h,
            pad: [0; 5],
            m0: [g[0][0], g[0][1], g[0][2], 0.0],
            m1: [g[1][0], g[1][1], g[1][2], 0.0],
            m2: [g[2][0], g[2][1], g[2][2], 0.0],
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(pq16_cbuf): {e}"))?;
        std::ptr::copy_nonoverlapping(
            &params as *const Pq16Params as *const u8,
            mapped.pData as *mut u8,
            80,
        );
        ctx.Unmap(&sess.cbuf, 0);
    }

    // dst UAV（每次现建——纹理由调用方持有，会话不缓存外部资源；绑定无
    // hazard：in_buf/lut 只绑 SRV，dst_tex 只绑 UAV，不同资源）
    let mut uav: Option<ID3D11UnorderedAccessView> = None;
    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    unsafe {
        device
            .CreateUnorderedAccessView(dst_tex, Some(&uav_desc), Some(&mut uav))
            .map_err(|e| format!("CreateUAV(dst_tex): {e}"))?;
    }
    let dst_uav = uav.ok_or("dst_tex UAV 创建失败")?;

    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv2: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
    unsafe {
        ctx.CSSetShader(&sess.cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[Some(sess.in_srv.clone()), Some(sess.lut_srv.clone())]),
        );
        let uavs = [Some(dst_uav)];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((npix as u32).div_ceil(128), 1, 1);
        // 全量解绑（cbuffer 布局独立 → 显式解绑，跨 pass 残留坑）
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv2));
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

// ==================== D3D 辅助（模块本地，同 hybrid_gpu 模式） ====================

/// 结构化缓冲 SRV（只读上传用；stride = 元素字节宽）
fn mk_srv_buf(
    device: &ID3D11Device,
    elems: u32,
    stride: u32,
) -> Result<(ID3D11Buffer, ID3D11ShaderResourceView), String> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: elems * stride,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: stride,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| format!("CreateBuffer(srv): {e}"))?;
        let buf = buf.ok_or("缓冲创建失败")?;
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D_SRV_DIMENSION_BUFFER,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_SRV {
                    Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                    Anonymous2: D3D11_BUFFER_SRV_1 { NumElements: elems },
                },
            },
        };
        let mut srv = None;
        device
            .CreateShaderResourceView(&buf, Some(&srv_desc), Some(&mut srv))
            .map_err(|e| format!("CreateSRV: {e}"))?;
        Ok((buf, srv.ok_or("SRV 创建失败")?))
    }
}

/// DYNAMIC 常量缓冲（Map/WRITE_DISCARD 每帧写）
fn mk_cbuf(device: &ID3D11Device, bytes: u32) -> Result<ID3D11Buffer, String> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| format!("CreateBuffer(cbuf): {e}"))?;
    }
    buf.ok_or_else(|| "cbuf 创建失败".to_string())
}
