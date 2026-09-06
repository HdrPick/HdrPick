//! 录制 GPU 预处理管线（v3 设计：GPU 裁剪 + scRGB/Bgra8→PQ16 转换 + 降采样哈希）
//!
//! 建立在 DDA 设备的 immediate context 上（与 DesktopCapturer 同设备）。
//! 双输入格式支持（DDA 帧格式取决于显示器 HDR 开关）：
//! - HDR 桌面 `R16G16B16A16_FLOAT`（scRGB 线性光）→ 直进色域矩阵
//! - SDR 桌面 `B8G8R8A8_UNORM`（sRGB gamma）→ 先 sRGB EOTF 解码到线性光
//! 两种输入汇入同一转换数学（BT.709→BT.2020 + PQ OETF）输出 PQ16。
//!
//! D3D11 immediate context 非线程安全：线程 A（GPU 命令提交）与线程 B
//! （延迟 Map 读回）的所有 context 调用经 `Mutex<ID3D11DeviceContext>`
//! 串行化；Map 与像素拷贝之间释放锁（拷贝不阻塞 GPU 提交）。
//!
//! 每帧 GPU 命令链（线程 A，持锁，~百微秒级 API 提交）：
//!   CopyResource(DDA帧 → 同格式全屏中转纹理)
//!   → ClearUAV(哈希清零)
//!   → Dispatch(转换 shader：裁剪采样 + sRGB 解码(按 cbuf 标志) + 色域 + PQ + 哈希 XOR)
//!   → CopyResource(PQ16 输出纹理 → staging ×2)
//! 线程 B 延迟 Map（池深 4 轮转，Map 时拷贝早已完成，零停顿）：
//!   先 Map 哈希 staging（8 字节）→ 与上一帧相同则跳过大回拷（GPU 去重）
//!   → 不同则 Map PQ16 staging → 去 A 通道压成紧密 RGB 6B/px
//!
//! 坑位（沿用项目经验）：CSSetUnorderedAccessViews 签名 (slot, count, ptr, None)
//! 且 dispatch 后必须解绑 UAV；structured buffer SRV 用 FirstElement/NumElements。

use std::sync::Mutex;

use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_SRV, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC,
    D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R16G16B16A16_UINT, DXGI_FORMAT_R32_UINT, DXGI_SAMPLE_DESC,
};

use crate::color::matrix::bt709_to_bt2020;

/// staging 池深度（= 票据队列容量；池深 N 保证线程 B Map 时该槽位拷贝
/// 已在 ≥N-1 帧前提交完成，Map 零停顿）
pub const STAGING_POOL_DEPTH: usize = 4;

/// 转换 shader：DDA 帧 → PQ16 uint16（BT.709→BT.2020 + PQ OETF）+ 帧哈希
///
/// 两种输入（p2.w 标志位区分，对应双常量缓冲）：
/// - 0 = scRGB f16（HDR 桌面）：采样值已是线性光，直接进矩阵
/// - 1 = Bgra8 sRGB（SDR 桌面）：先 sRGB EOTF 解码到线性光（sRGB 白 = scRGB 1.0
///   = 80 nits 同一基准），再走完全相同的矩阵 + PQ 管线
///
/// 数学与 CPU 版 `encode::jxl::to_pq16_rgb` 逐项对应：
/// - 色域矩阵：Rust 侧 `bt709_to_bt2020()` 计算后经 cbuffer 传入（常量同源）
/// - 亮度换算：scRGB 1.0 = 80 nits → PQ 线性域（1.0 = 10000 nits）= ×(80/10000)
/// - 量化：saturate(pq) × 65535 + 0.5 → uint16
const CONVERT_CS: &str = r#"
Texture2D<float4> srcTex : register(t0);
RWTexture2D<uint4> dstTex : register(u0);
RWTexture2D<uint>  hashTex : register(u1);

cbuffer Params : register(b0)
{
    float4 p0 : packoffset(c0); // BT.709→BT.2020 第 0 行 + cropX
    float4 p1 : packoffset(c1); // 第 1 行 + cropY
    float4 p2 : packoffset(c2); // 第 2 行 + inputMode（0=scRGB / 1=sRGB）
    float4 p3 : packoffset(c3); // dstW, dstH, hashStrideX, hashStrideY
};

// PQ ST.2084 OETF（线性 0..1 = 0..10000 nits → 编码 0..1）
// 常量与 color/transfer.rs 一致；先除再 pow（防 f32 溢出）
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

// sRGB EOTF（gamma 编码 0..1 → 线性光 0..1；分段函数）
// SDR 桌面 Bgra8 输入专用；sRGB 白（1.0）与 scRGB 1.0 同为 80 nits 基准
float srgb_eotf(float e)
{
    if (e <= 0.04045) return e / 12.92;
    return pow((e + 0.055) / 1.055, 2.4);
}

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint dstW = (uint)p3.x;
    uint dstH = (uint)p3.y;
    if (tid.x >= dstW || tid.y >= dstH) return;

    // 区域录制的裁剪偏移采样（全屏录制 offset=0）
    uint2 src = uint2(tid.x + (uint)p0.w, tid.y + (uint)p1.w);
    float3 sc = srcTex[src].rgb;
    // SDR 桌面：sRGB gamma 解码到线性光（HDR 桌面 scRGB 已是线性光）
    if (p2.w > 0.5)
    {
        sc = float3(srgb_eotf(sc.r), srgb_eotf(sc.g), srgb_eotf(sc.b));
    }

    // BT.709 → BT.2020 色域矩阵（行主序；scRGB 允许负分量，转换后再钳）
    float3x3 m = float3x3(
        p0.x, p0.y, p0.z,
        p1.x, p1.y, p1.z,
        p2.x, p2.y, p2.z);
    float3 lin = mul(m, sc);

    // scRGB 1.0 = 80 nits → PQ 线性域（1.0 = 10000 nits）
    lin *= (80.0 / 10000.0);

    float3 pq = float3(pq_oetf(lin.x), pq_oetf(lin.y), pq_oetf(lin.z));
    uint3 q = (uint3)(saturate(pq) * 65535.0 + 0.5);
    dstTex[tid.xy] = uint4(q, 0u);

    // 降采样帧哈希：等距网格采样（~64×64 点）。
    // InterlockedXor 顺序无关 → 结果确定；两路独立混合做成 64bit 指纹。
    uint sx = max(1u, (uint)p3.z);
    uint sy = max(1u, (uint)p3.w);
    if (tid.x % sx == 0 && tid.y % sy == 0)
    {
        uint h1 = q.x ^ (q.y * 0x9E3779B9u) ^ (q.z * 0x85EBCA6Bu);
        uint h2 = (q.x * 0xC2B2AE35u) ^ q.y ^ (q.z * 0x27D4EB2Fu);
        InterlockedXor(hashTex[uint2(0, 0)], h1);
        InterlockedXor(hashTex[uint2(1, 0)], h2);
    }
}
"#;

/// cbuffer 布局（4 × float4 = 64 字节）
#[repr(C)]
struct CsParams {
    p0: [f32; 4],
    p1: [f32; 4],
    p2: [f32; 4],
    p3: [f32; 4],
}

/// 全屏 scRGB 中转纹理（DDA 帧 CopyResource 目标 + shader 采样源）
struct InputBundle {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

/// 输入纹理锁守卫（两格式各一把锁；Deref 统一访问，锁存活至帧处理结束）
enum InputGuard<'a> {
    Scrgb(std::sync::MutexGuard<'a, InputBundle>),
    Bgra(std::sync::MutexGuard<'a, Option<InputBundle>>),
}

impl std::ops::Deref for InputGuard<'_> {
    type Target = InputBundle;
    fn deref(&self) -> &InputBundle {
        match self {
            InputGuard::Scrgb(g) => g,
            InputGuard::Bgra(g) => g.as_ref().expect("Bgra8 中转纹理未创建"),
        }
    }
}

/// 每槽位 GPU 资源：PQ16 输出（UAV + staging 回读）+ 帧哈希（UAV + staging 回读）
struct GpuSlot {
    _pq_tex: ID3D11Texture2D,
    pq_uav: ID3D11UnorderedAccessView,
    pq_staging: ID3D11Texture2D,
    _hash_tex: ID3D11Texture2D,
    hash_uav: ID3D11UnorderedAccessView,
    hash_staging: ID3D11Texture2D,
}

/// 录制 GPU 管线（Send + Sync：context 经 Mutex 串行化）
pub struct RecordGpu {
    device: ID3D11Device,
    context: Mutex<ID3D11DeviceContext>,
    convert_cs: ID3D11ComputeShader,
    /// 全屏 scRGB 中转纹理（HDR 桌面；仅线程 A 访问；Mutex 因 process_frame 取 &self）
    input_scrgb: Mutex<InputBundle>,
    /// 全屏 Bgra8 中转纹理（SDR 桌面；懒创建——同一会话通常恒定一种格式）
    input_bgra: Mutex<Option<InputBundle>>,
    /// 双常量缓冲：仅 p2.w 输入模式位不同（0=scRGB / 1=sRGB），随格式选用
    cbuf_scrgb: ID3D11Buffer,
    cbuf_bgra: ID3D11Buffer,
    slots: Vec<GpuSlot>,
    /// 输出尺寸（区域录制 = 区域尺寸；全屏 = 显示器尺寸）
    pub width: u32,
    pub height: u32,
}

// 跨线程共享说明：windows-rs 接口类型已实现 Send + Sync；
// context 的跨线程访问安全性由 Mutex 串行化保证（D3D11 immediate context 非线程安全）。

impl RecordGpu {
    /// 建立 GPU 管线
    ///
    /// - `full_w/full_h`：DDA 全屏纹理尺寸（显示器分辨率）
    /// - `crop`：区域录制裁剪偏移（相对 DDA 纹理左上；全屏传 (0,0)）
    /// - `out_w/out_h`：输出尺寸（= 区域尺寸或全屏尺寸）
    pub fn new(
        device: &ID3D11Device,
        context: ID3D11DeviceContext,
        full_w: u32,
        full_h: u32,
        crop: (u32, u32),
        out_w: u32,
        out_h: u32,
    ) -> anyhow::Result<Self> {
        unsafe {
            // 1. 编译转换 shader
            let convert_cs = compile_cs(device, CONVERT_CS, b"record_convert\0")?;

            // 2. 全屏 scRGB 中转纹理（HDR 桌面主路径）
            let input_scrgb =
                make_input_bundle(device, full_w, full_h, DXGI_FORMAT_R16G16B16A16_FLOAT)?;

            // 3. 双常量缓冲（矩阵/裁剪/尺寸/哈希步长相同；仅 p2.w 输入模式不同）
            let make_cbuf = |input_mode: f32| -> anyhow::Result<ID3D11Buffer> {
                let mat = bt709_to_bt2020().m;
                let hash_stride_x = (out_w / 64).max(1);
                let hash_stride_y = (out_h / 64).max(1);
                let params = CsParams {
                    p0: [mat[0][0], mat[0][1], mat[0][2], crop.0 as f32],
                    p1: [mat[1][0], mat[1][1], mat[1][2], crop.1 as f32],
                    p2: [mat[2][0], mat[2][1], mat[2][2], input_mode],
                    p3: [
                        out_w as f32,
                        out_h as f32,
                        hash_stride_x as f32,
                        hash_stride_y as f32,
                    ],
                };
                make_constant_buffer(device, &params)
            };
            let cbuf_scrgb = make_cbuf(0.0)?;
            let cbuf_bgra = make_cbuf(1.0)?;

            // 4. staging 池
            let mut slots = Vec::with_capacity(STAGING_POOL_DEPTH);
            for _ in 0..STAGING_POOL_DEPTH {
                slots.push(make_slot(device, out_w, out_h)?);
            }

            Ok(RecordGpu {
                device: device.clone(),
                context: Mutex::new(context),
                convert_cs,
                input_scrgb: Mutex::new(input_scrgb),
                input_bgra: Mutex::new(None),
                cbuf_scrgb,
                cbuf_bgra,
                slots,
                width: out_w,
                height: out_h,
            })
        }
    }

    /// 线程 A：处理一帧（DDA 纹理 → GPU 裁剪+转换+哈希 → staging 拷贝提交）
    ///
    /// 返回后即可推票据给线程 B（池深轮转保证延迟 Map 零停顿）。
    /// 调用方随后须立即 `capturer.release_frame()` 归还 DDA 队列。
    pub fn process_frame(&self, desktop_tex: &ID3D11Texture2D, slot: usize) -> anyhow::Result<()> {
        // 首几帧进出日志：定位 GPU 管线阻塞点（context/锁/Dispatch）
        static PF_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let pf_n = PF_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if pf_n < 5 {
            log::info!("[record] process_frame #{} slot={} 进入", pf_n + 1, slot);
        }
        let slot_res = self
            .slots
            .get(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位越界: {}", slot))?;
        let ctx = self.context.lock().unwrap();
        unsafe {
            // 0. 按帧格式选择输入管线（CopyResource 要求源/目标格式一致）
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            desktop_tex.GetDesc(&mut desc);
            let format = desc.Format;
            if format != DXGI_FORMAT_R16G16B16A16_FLOAT && format != DXGI_FORMAT_B8G8R8A8_UNORM {
                anyhow::bail!(
                    "不支持的 DDA 帧格式 {}（仅 scRGB/Bgra8）",
                    format_name(format)
                );
            }
            // 输入纹理尺寸跟随 DDA 帧（正常不变；显示模式切换会触发错误路径）
            // 守卫枚举：两种格式各持一把锁，Deref 统一取 InputBundle（锁存活至帧处理结束）
            let input = if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let mut guard = self.input_scrgb.lock().unwrap();
                let mut in_desc = D3D11_TEXTURE2D_DESC::default();
                guard.tex.GetDesc(&mut in_desc);
                if in_desc.Width != desc.Width || in_desc.Height != desc.Height {
                    *guard = make_input_bundle(
                        &self.device,
                        desc.Width,
                        desc.Height,
                        DXGI_FORMAT_R16G16B16A16_FLOAT,
                    )?;
                }
                InputGuard::Scrgb(guard)
            } else {
                // SDR 桌面：Bgra8 中转纹理懒创建（尺寸不符则重建）
                let mut guard = self.input_bgra.lock().unwrap();
                let need = match guard.as_ref() {
                    Some(b) => {
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        b.tex.GetDesc(&mut d);
                        d.Width != desc.Width || d.Height != desc.Height
                    }
                    None => true,
                };
                if need {
                    *guard = Some(make_input_bundle(
                        &self.device,
                        desc.Width,
                        desc.Height,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                    )?);
                }
                InputGuard::Bgra(guard)
            };
            let input = &*input;
            let cbuf = if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                &self.cbuf_scrgb
            } else {
                &self.cbuf_bgra
            };

            // 1. DDA 帧 → 全屏中转纹理（建立 SRV 采样源）
            ctx.CopyResource(&input.tex, desktop_tex);
            // 2. 哈希清零
            let zeros = [0u32; 4];
            ctx.ClearUnorderedAccessViewUint(&slot_res.hash_uav, &zeros);
            // 3. 转换 dispatch（裁剪 + 色域 + PQ + 哈希；sRGB 输入由 cbuf 标志位分支）
            ctx.CSSetShader(&self.convert_cs, None);
            ctx.CSSetConstantBuffers(0, Some(&[Some(cbuf.clone())]));
            ctx.CSSetShaderResources(0, Some(&[Some(input.srv.clone())]));
            let uavs = [
                Some(slot_res.pq_uav.clone()),
                Some(slot_res.hash_uav.clone()),
            ];
            ctx.CSSetUnorderedAccessViews(0, uavs.len() as u32, Some(uavs.as_ptr()), None);
            ctx.Dispatch((self.width + 7) / 8, (self.height + 7) / 8, 1);
            // 4. 解绑（UAV 复用防护，沿用项目经验）
            let null_uavs = [None::<ID3D11UnorderedAccessView>, None];
            ctx.CSSetUnorderedAccessViews(0, 2, Some(null_uavs.as_ptr()), None);
            ctx.CSSetShaderResources(0, Some(&[None]));
            ctx.CSSetConstantBuffers(0, Some(&[None]));
            ctx.CSSetShader(None, None);
            // 5. staging 拷贝（GPU 队列异步执行；线程 B 延迟 Map 时已完成）
            ctx.CopyResource(&slot_res.pq_staging, &slot_res._pq_tex);
            ctx.CopyResource(&slot_res.hash_staging, &slot_res._hash_tex);
        }
        if pf_n < 5 {
            log::info!("[record] process_frame #{} 完成", pf_n + 1);
        }
        Ok(())
    }

    /// 线程 B：读取槽位帧哈希（64bit 指纹；去重判定先于大回拷）
    pub fn read_hash(&self, slot: usize) -> anyhow::Result<u64> {
        let slot_res = self
            .slots
            .get(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位越界: {}", slot))?;
        let ctx = self.context.lock().unwrap();
        unsafe {
            let mapped = map_read_timeout(&ctx, &slot_res.hash_staging, "hash")?;
            let p = mapped.pData as *const u32;
            // 2×1 R32_UINT：行内相邻两个 texel
            let lo = *p as u64;
            let hi = *p.add(1) as u64;
            ctx.Unmap(&slot_res.hash_staging, 0);
            Ok((hi << 32) | lo)
        }
    }

    /// 线程 B：读取槽位 PQ16 帧 → 紧密 RGB（6 字节/像素）
    ///
    /// GPU 纹理为 RGBA16（8B/px，无 3×uint16 格式），此处去 A 通道。
    /// Map 与解拷贝之间释放 context 锁（几十 MB 内存拷贝不阻塞 GPU 提交）。
    pub fn read_pq16(&self, slot: usize) -> anyhow::Result<Vec<u8>> {
        let slot_res = self
            .slots
            .get(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位越界: {}", slot))?;
        // Map（持锁；超时轮询防 GPU 回读悬挂）
        let (ptr, row_pitch) = {
            let ctx = self.context.lock().unwrap();
            unsafe {
                let mapped = map_read_timeout(&ctx, &slot_res.pq_staging, "pq16")?;
                (mapped.pData as *const u8, mapped.RowPitch as usize)
            }
        };
        let w = self.width as usize;
        let h = self.height as usize;
        let mut out = vec![0u8; w * h * 6];
        // 逐像素 8B→6B 抽通道：2560×1600 = 4M 次小拷贝，单线程 ~10-20ms
        // ——按行分 4 块并行（std::thread::scope，Map 指针期间只读共享安全；
        // chunks_mut 产出不相交可变切片，借用检查器可证明安全）
        let row_bytes_src = w * 8;
        let row_bytes_dst = w * 6;
        let rows_per = h.div_ceil(4.min(h));
        std::thread::scope(|s| {
            let src_all = unsafe { std::slice::from_raw_parts(ptr, row_pitch * h) };
            let mut pending = Vec::new();
            for (c, dst_c) in out.chunks_mut(row_bytes_dst * rows_per).enumerate() {
                let y0 = c * rows_per;
                let y1 = (y0 + rows_per).min(h);
                if y0 >= y1 {
                    continue;
                }
                pending.push(s.spawn(move || {
                    for y in y0..y1 {
                        let row = &src_all[y * row_pitch..y * row_pitch + row_bytes_src];
                        let dst = &mut dst_c[(y - y0) * row_bytes_dst..(y - y0 + 1) * row_bytes_dst];
                        for x in 0..w {
                            dst[x * 6..x * 6 + 6].copy_from_slice(&row[x * 8..x * 8 + 6]);
                        }
                    }
                }));
            }
            for j in pending {
                let _ = j.join();
            }
        });
        // Unmap（重新持锁；槽位由票据独占，映射期间安全）
        {
            let ctx = self.context.lock().unwrap();
            unsafe { ctx.Unmap(&slot_res.pq_staging, 0) };
        }
        Ok(out)
    }
}

/// 带 3s 超时的 staging Map（READ）
///
/// 裸 `Map(D3D11_MAP_READ)` 在本机（RTX 5060）会间歇性永久阻塞（GPU 回读悬挂，
/// 持有 context 锁 → 抓帧线程连锁冻结 → "点击开始无效/计时不动"）。
/// 改用 DO_NOT_WAIT 轮询：GPU 未完成每 1ms 重试，3s 仍不行则报错（响亮失败，
/// inflight_fatal 走正常错误路径），并打印首次等待时长辅助定位。
unsafe fn map_read_timeout(
    ctx: &ID3D11DeviceContext,
    res: &ID3D11Resource,
    tag: &str,
) -> anyhow::Result<D3D11_MAPPED_SUBRESOURCE> {
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
    let t0 = std::time::Instant::now();
    let mut waited_first = false;
    loop {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let hr = ctx.Map(
            res,
            0,
            D3D11_MAP_READ,
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
            Some(&mut mapped),
        );
        match hr {
            Ok(()) => {
                if waited_first {
                    log::info!(
                        "[record] Map({}) GPU 完成用时 {}ms",
                        tag,
                        t0.elapsed().as_millis()
                    );
                }
                return Ok(mapped);
            }
            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                if !waited_first {
                    waited_first = true;
                    log::info!("[record] Map({}) GPU 未完成，开始轮询等待", tag);
                }
                if t0.elapsed() > std::time::Duration::from_secs(3) {
                    anyhow::bail!(
                        "Map({}) GPU 3s 未完成回读（疑似驱动/设备异常）",
                        tag
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(anyhow::anyhow!("Map({}): {}", tag, e)),
        }
    }
}

/// 全屏中转纹理（DDA 帧 CopyResource 目标；DDA 纹理绑定标志不保证
/// 含 SHADER_RESOURCE，须经中转纹理建立 SRV。格式与 DDA 帧一致——
/// CopyResource 要求源/目标格式相同）
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
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    device
        .CreateTexture2D(&tex_desc, None, Some(&mut tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(input): {}", e))?;
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
        .map_err(|e| anyhow::anyhow!("CreateSRV(input): {}", e))?;
    let srv: ID3D11ShaderResourceView = srv.ok_or_else(|| anyhow::anyhow!("SRV 创建失败"))?;

    Ok(InputBundle { tex, srv })
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
        .map_err(|e| anyhow::anyhow!("CreateBuffer(cbuf): {}", e))?;
    cbuf.ok_or_else(|| anyhow::anyhow!("常量缓冲创建失败"))
}

/// 单槽位资源：PQ16 输出纹理（R16G16B16A16_UINT）+ 2×1 R32_UINT 哈希纹理 + 各自 staging
unsafe fn make_slot(device: &ID3D11Device, w: u32, h: u32) -> anyhow::Result<GpuSlot> {
    // ---- PQ16 输出纹理 + UAV ----
    let pq_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16B16A16_UINT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pq_tex = None;
    device
        .CreateTexture2D(&pq_desc, None, Some(&mut pq_tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(pq16): {}", e))?;
    let pq_tex: ID3D11Texture2D = pq_tex.ok_or_else(|| anyhow::anyhow!("PQ16 纹理创建失败"))?;

    let pq_uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R16G16B16A16_UINT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut pq_uav = None;
    device
        .CreateUnorderedAccessView(&pq_tex, Some(&pq_uav_desc), Some(&mut pq_uav))
        .map_err(|e| anyhow::anyhow!("CreateUAV(pq16): {}", e))?;
    let pq_uav: ID3D11UnorderedAccessView =
        pq_uav.ok_or_else(|| anyhow::anyhow!("PQ16 UAV 创建失败"))?;

    // ---- PQ16 staging（CPU 读回）----
    let pq_staging_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16B16A16_UINT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut pq_staging = None;
    device
        .CreateTexture2D(&pq_staging_desc, None, Some(&mut pq_staging))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(pq16 staging): {}", e))?;
    let pq_staging: ID3D11Texture2D =
        pq_staging.ok_or_else(|| anyhow::anyhow!("PQ16 staging 创建失败"))?;

    // ---- 哈希纹理（2×1 R32_UINT）+ UAV ----
    let hash_desc = D3D11_TEXTURE2D_DESC {
        Width: 2,
        Height: 1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R32_UINT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut hash_tex = None;
    device
        .CreateTexture2D(&hash_desc, None, Some(&mut hash_tex))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(hash): {}", e))?;
    let hash_tex: ID3D11Texture2D = hash_tex.ok_or_else(|| anyhow::anyhow!("哈希纹理创建失败"))?;

    let hash_uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut hash_uav = None;
    device
        .CreateUnorderedAccessView(&hash_tex, Some(&hash_uav_desc), Some(&mut hash_uav))
        .map_err(|e| anyhow::anyhow!("CreateUAV(hash): {}", e))?;
    let hash_uav: ID3D11UnorderedAccessView =
        hash_uav.ok_or_else(|| anyhow::anyhow!("哈希 UAV 创建失败"))?;

    // ---- 哈希 staging ----
    let hash_staging_desc = D3D11_TEXTURE2D_DESC {
        Width: 2,
        Height: 1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R32_UINT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut hash_staging = None;
    device
        .CreateTexture2D(&hash_staging_desc, None, Some(&mut hash_staging))
        .map_err(|e| anyhow::anyhow!("CreateTexture2D(hash staging): {}", e))?;
    let hash_staging: ID3D11Texture2D =
        hash_staging.ok_or_else(|| anyhow::anyhow!("哈希 staging 创建失败"))?;

    Ok(GpuSlot {
        _pq_tex: pq_tex,
        pq_uav,
        pq_staging,
        _hash_tex: hash_tex,
        hash_uav,
        hash_staging,
    })
}

/// 编译 compute shader（模式同 upscale/d3d11.rs::compile_cs）
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
        anyhow::bail!("D3DCompile(record): {:?} {}", hr, msg);
    }
    let blob = code.ok_or_else(|| anyhow::anyhow!("编译无产物"))?;
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

/// DXGI 格式名辅助（错误信息用）
#[allow(dead_code)]
fn format_name(f: DXGI_FORMAT) -> &'static str {
    match f {
        DXGI_FORMAT_R16G16B16A16_FLOAT => "R16G16B16A16_FLOAT(scRGB)",
        DXGI_FORMAT_B8G8R8A8_UNORM => "B8G8R8A8_UNORM(sRGB)",
        DXGI_FORMAT_R16G16B16A16_UINT => "R16G16B16A16_UINT",
        DXGI_FORMAT_R32_UINT => "R32_UINT",
        _ => "其它",
    }
}
