//! 混合解码 GPU 路径 v2（路线 B Increment B6：块层驻留 GPU）
//!
//! D3D11 compute 管线：pass A 反量化+DC → pass B 1D 行变换（读转置）→
//! pass C 1D 列变换**直写全帧布局** → [pass G Gaborish（gab=true）] →
//! [pass E EPF1（epf_iters≥1 且 sigma 有效）] → [pass D XYB→线性 RGB 直接
//! 消费 GPU 全帧]。
//! 每帧先 UpdateSubresource 把 libjxl 块层 dump（idct_ref：非 DCT8 槽位有效、
//! DCT8 槽位为 0）灌入 full_out 作底图，pass C 覆盖 DCT8 槽位 → full_out =
//! 完整帧块层（XYB 域）全帧驻留 GPU：无 CPU scatter、无 packed 读回、
//! XYB pass 免 44.8MB 再上传（hybrid_reconstruct_linear 全链仅一次读回）。
//! 数学与 [`super::hybrid::golden_reconstruct`] 逐元素一致（差异仅 f32 舍入级）。
//!
//! # 已知坑（勿踩）
//! - FXC 大局部数组动态索引 → 每线程一个输出元素
//! - cbuffer 跨 pass 残留绑定 → 布局不同的 pass 后显式解绑
//! - D3D11 无 CopyBufferRegion → 用 CopyResource（staging 与源同容量创建）
//!
//! # 分工
//! - CPU：ac_strategy/cmap/dc 解析（块遍历 + per-block 标量 s/fx/fb/dc 预计算）
//! - GPU：纯数学（dequant bias 校正 × 量化矩阵 + CfL + IDCT 两维分离变换）
//! - 非 DCT8 块不进 GPU 列表（后续增量走 CPU 回退）
//!
//! # IDCT 语义（B2 踩坑记录，勿改）
//! `dct_for_test.h` IDCT1D 的基在使用时参数交换：`out[u][x] = Σ_y I(u,y)·in[y][x]`，
//! 且整体语义 = `IDCTSlow(Cᵀ)`（第一次 1D 读转置输入）。C++ 内部对拍已锤定（3.5e-7）。
//! GPU 实现等价式：
//! - pass B（f 作用于转置输入）：`Mid[u][x] = Σ_y B[u][y]·Dq[x][y]`（读转置）
//! - pass C：`Out[a][b] = Σ_p B[a][p]·Mid[b][p]`（= f(T(f(Cᵀ))) = IDCTSlow(Cᵀ)）

use super::hybrid::{compare_against_ref, CoeffSnapshot, CompareResult};
use crate::upscale::d3d11::engine;
use windows::Win32::Graphics::Direct3D::{
    D3D_SRV_DIMENSION_BUFFER,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_BUFFER_SRV, D3D11_BUFFER_SRV_0, D3D11_BUFFER_SRV_1,
    D3D11_BUFFER_UAV, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_UAV,
    D3D11_UAV_DIMENSION_BUFFER, D3D11_UAV_DIMENSION_TEXTURE2D,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_UNKNOWN,
};

/// varblock 覆盖块数 LUT（ac_strategy.h covered_blocks_x/y，按枚举序）
const COVERED_X: [u8; 27] = [
    1, 1, 1, 1, 2, 4, 1, 2, 1, 4, 2, 4, 1, 1, 1, 1, 1, 1, 8, 4, 8, 16, 8, 16, 32, 16, 32,
];
const COVERED_Y: [u8; 27] = [
    1, 1, 1, 1, 2, 4, 2, 1, 4, 1, 4, 2, 1, 1, 1, 1, 1, 1, 8, 8, 4, 16, 16, 8, 32, 32, 16,
];
const DCT8: usize = 0;
const BLOCK: usize = 64;

// ==================== HLSL ====================

const DEQUANT_CS: &str = r#"
// pass A：反量化 + DC 覆盖。线程 = 块内一个系数（tid = blk*192 + c*64 + k）
struct BlockInfo {
    float dc0; float dc1; float dc2;   // DC 图值（三通道，块 k=0 覆盖用）
    float s;                           // inv_global_scale / quant
    float fx;                          // CfL X 系数
    float fb;                          // CfL B 系数
    float pad0; float pad1;
};

StructuredBuffer<int>       Coeffs : register(t0); // [block][3*64] 通道序 x,y,b
StructuredBuffer<float>     Dm     : register(t1); // 3*64 反量化矩阵
StructuredBuffer<BlockInfo> Info   : register(t2); // 每块标量
RWStructuredBuffer<float>   Dq     : register(u0); // [block][3*64] 反量化后

cbuffer Params : register(b0) {
    uint  nblocks;
    float xdm;      // x 通道量化矩阵倍率
    float bdm;      // b 通道量化矩阵倍率
    float pad;
    float4 biases;  // x,y,b,common
};

// AdjustQuantBias（quantizer-inl.h）
float adb(int c, int q) {
    if (q == 0) return 0.0;
    if (abs(q) == 1) return q < 0 ? -biases[c] : biases[c];
    return q - biases[3] / q;
}

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint idx = tid.x;
    if (idx >= nblocks * 192) return;
    uint blk = idx / 192;
    uint rem = idx % 192;
    uint c = rem / 64;   // 0=x 1=y 2=b
    uint k = rem % 64;
    BlockInfo bi = Info[blk];
    uint base = blk * 192;

    float dq;
    if (c == 1) {
        dq = adb(1, Coeffs[idx]) * bi.s * Dm[64 + k];
    } else if (c == 0) {
        float dy = adb(1, Coeffs[base + 64 + k]) * bi.s * Dm[64 + k];
        dq = adb(0, Coeffs[idx]) * bi.s * xdm * Dm[k] + bi.fx * dy;
    } else {
        float dy = adb(1, Coeffs[base + 64 + k]) * bi.s * Dm[64 + k];
        dq = adb(2, Coeffs[idx]) * bi.s * bdm * Dm[128 + k] + bi.fb * dy;
    }
    // DC 覆盖（DCT8 块 k=0 ← DC 图值）
    if (k == 0) {
        dq = (c == 0) ? bi.dc0 : (c == 1 ? bi.dc1 : bi.dc2);
    }
    Dq[idx] = dq;
}
"#;

// pass A2：反量化 + DC 覆盖（快照 coeffs **直读版**——免 CPU gather 紧凑化）。
// 线程 = 块内一个系数（tid = blk*192 + c*64 + k）。系数读自导出快照布局
// [c*nggc + SrcBase[blk] + k]（每通道平面内 64 连续，与 DCT8 块的 src_base
// 寻址一致），输出仍写 packed [block][3*64]（pass B/C 不变）。
// 剖析依据：CPU gather（散读 55MB + 写 49MB）~7ms/帧为 DRAM 延迟瓶颈，
// 直读 + 直传（55MB UpdateSubresource）把这段完全消掉。
const DEQUANT_DIRECT_CS: &str = r#"
struct BlockInfo {
    float dc0; float dc1; float dc2;
    float s;
    float fx;
    float fb;
    float pad0; float pad1;
};

StructuredBuffer<int>       Coeffs : register(t0); // [3*nggc] 快照 coeffs（平面 x,y,b）
StructuredBuffer<float>     Dm     : register(t1); // 3*64 反量化矩阵
StructuredBuffer<uint>      SrcBase: register(t2); // 每块快照平面内起始槽位
StructuredBuffer<BlockInfo> Info   : register(t3); // 每块标量
RWStructuredBuffer<float>   Dq     : register(u0); // [block][3*64] 反量化后

cbuffer Params : register(b0) {
    uint  nblocks;
    float xdm;      // x 通道量化矩阵倍率
    float bdm;      // b 通道量化矩阵倍率
    uint  nggc;     // 每平面元素数（num_groups × group_dim²）
    float4 biases;  // x,y,b,common
};

// AdjustQuantBias（quantizer-inl.h）
float adb(int c, int q) {
    if (q == 0) return 0.0;
    if (abs(q) == 1) return q < 0 ? -biases[c] : biases[c];
    return q - biases[3] / q;
}

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint idx = tid.x;
    if (idx >= nblocks * 192) return;
    uint blk = idx / 192;
    uint rem = idx % 192;
    uint c = rem / 64;
    uint k = rem % 64;
    uint src = SrcBase[blk];
    BlockInfo bi = Info[blk];

    float dq;
    if (c == 1) {
        dq = adb(1, Coeffs[nggc + src + k]) * bi.s * Dm[64 + k];
    } else if (c == 0) {
        float dy = adb(1, Coeffs[nggc + src + k]) * bi.s * Dm[64 + k];
        dq = adb(0, Coeffs[src + k]) * bi.s * xdm * Dm[k] + bi.fx * dy;
    } else {
        float dy = adb(1, Coeffs[nggc + src + k]) * bi.s * Dm[64 + k];
        dq = adb(2, Coeffs[2 * nggc + src + k]) * bi.s * bdm * Dm[128 + k] + bi.fb * dy;
    }
    // DC 覆盖（DCT8 块 k=0 ← DC 图值）
    if (k == 0) {
        dq = (c == 0) ? bi.dc0 : (c == 1 ? bi.dc1 : bi.dc2);
    }
    Dq[idx] = dq;
}
"#;

const IDCT_HEAD: &str = r#"
// pass B/C：两维分离 IDCT。线程 = 块内一个输出元素（tid = blk*192 + c*64 + e）
StructuredBuffer<float> In    : register(t0); // pass B 输入 = Dq；pass C 输入 = Mid
StructuredBuffer<float> Basis: register(t1); // 64：B[u*8+y] = I(u,y)
RWStructuredBuffer<float> Out : register(u0);

cbuffer Params : register(b0) {
    uint  nblocks;
    float xdm;
    float bdm;
    float pad;
    float4 biases;
};
"#;

// pass B：Mid[u][x] = Σ_y B(u,y)·Dq[x][y]（Dq 转置读——139.6dB 实证语义）
const IDCT_B_TAIL: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint idx = tid.x;
    if (idx >= nblocks * 192) return;
    uint blk = idx / 192;
    uint rem = idx % 192;
    uint c = rem / 64;
    uint u = (rem % 64) / 8;
    uint x = rem % 8;
    uint base = blk * 192 + c * 64;
    float s = 0;
    [unroll] for (int y = 0; y < 8; y++) {
        s += Basis[u * 8 + y] * In[base + x * 8 + y];
    }
    Out[idx] = s;
}
"#;

// pass C：Out[a][b] = Σ_p B(a,p)·Mid[b][p]（Mid 转置读——139.6dB 实证语义）
const IDCT_C_TAIL: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint idx = tid.x;
    if (idx >= nblocks * 192) return;
    uint blk = idx / 192;
    uint rem = idx % 192;
    uint c = rem / 64;
    uint a = (rem % 64) / 8;
    uint b = rem % 8;
    uint base = blk * 192 + c * 64;
    float s = 0;
    [unroll] for (int p = 0; p < 8; p++) {
        s += Basis[a * 8 + p] * In[base + b * 8 + p];
    }
    Out[idx] = s;
}
"#;

// pass C 全帧直写版（B6 块层驻留 GPU）：与 packed 版并存。cbuffer 布局独立
// （FullParams 16B），不与 packed 版共用 IDCT_HEAD，避免加 nggc 字段后错位。
// 输出直写全帧布局：FullOut[c*nggc + SrcBase[blk] + 块内偏移]；
// 尾转置：packed 块内 k=x*8+y → 全帧块内 (k%8)*8 + (k/8)。
const IDCT_C_FULL_HEAD: &str = r#"
StructuredBuffer<float> In        : register(t0); // Mid（packed [block][3*64]）
StructuredBuffer<float> Basis     : register(t1); // 64：B[u*8+y] = I(u,y)
StructuredBuffer<uint>  SrcBase   : register(t2); // 每块全帧平面内起始槽位
RWStructuredBuffer<float> FullOut : register(u0); // [3*nggc] 全帧块层 x,y,b

cbuffer FullParams : register(b0) {
    uint nblocks;
    uint nggc;  // num_groups * group_dim²（每平面元素数）
    uint pad0;  uint pad1;
};
"#;

const IDCT_C_FULL_TAIL: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint idx = tid.x;
    if (idx >= nblocks * 192) return;
    uint blk = idx / 192;
    uint rem = idx % 192;
    uint c = rem / 64;
    uint k = rem % 64;
    uint base = blk * 192 + c * 64;
    uint a = k / 8;
    uint b = k % 8;
    float s = 0;
    [unroll] for (int p = 0; p < 8; p++) {
        s += Basis[a * 8 + p] * In[base + b * 8 + p];
    }
    FullOut[c * nggc + SrcBase[blk] + (k % 8) * 8 + (k / 8)] = s;
}
"#;

// pass D：XYB→线性 RGB（dec_xyb-inl.h XybToRgb 逐元素同数学）。线程 = 一个像素。
// 输入 = 块层（3 平面 x,y,b 各 n 元素），输出同布局（线性 RGB，0-255 标度）。
const XYB_HEAD: &str = r#"
StructuredBuffer<float> In : register(t0); // [3*n] 平面序 x,y,b

cbuffer XYBParams : register(b0) {
    float4 biases;       // opsin_biases x,y,b,unused
    float4 biases_cbrt;  // opsin_biases_cbrt x,y,b,unused
    float4 m0;           // 逆 opsin 矩阵行 0（3 有效）
    float4 m1;           // 行 1
    float4 m2;           // 行 2
    uint  npix;          // 每平面元素数 n
    uint  pad0; uint pad1; uint pad2;
};
"#;

const XYB_TAIL: &str = r#"
RWStructuredBuffer<float> Out : register(u0); // [3*n] 线性 RGB

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= npix) return;
    float x = In[p];
    float y = In[npix + p];
    float b = In[2 * npix + p];
    // gamma 域（undo 颜色空间混合）+ 去偏置立方（undo simple gamma）
    float gr = y + x - biases_cbrt.x;
    float gg = y - x - biases_cbrt.y;
    float gb = b     - biases_cbrt.z;
    float mr = gr * gr * gr + biases.x;
    float mg = gg * gg * gg + biases.y;
    float mb = gb * gb * gb + biases.z;
    // 3×3 逆 opsin 吸收矩阵解混
    Out[p]           = m0.x * mr + m0.y * mg + m0.z * mb;
    Out[npix + p]    = m1.x * mr + m1.y * mg + m1.z * mb;
    Out[2 * npix + p] = m2.x * mr + m2.y * mg + m2.z * mb;
}
"#;

// pass G：Gaborish（帧内去卷积锐化，render pipeline stage_gaborish.cc 同数学）。
// 本 vendor 解码只走 render pipeline（dec_cache.cc PreparePipeline，EPF stage
// 之前）→ 单 pass 3×3 核（非旧 separable5），无中间 Tmp：In→Out ping-pong。
// 线程 = 一个像素（三通道一起算）。镜像 = image_ops.h Mirror 边缘复制式
// （-1→0、n→n-1）；采样点距有效域至多 1（仅 valid 像素采样）→ 单步镜像即可。
const GABORISH_HEAD: &str = r#"
StructuredBuffer<float> In : register(t0); // [3*n] 平面序 x,y,b，块主序（每块 64 像素连续）
RWStructuredBuffer<float> Out : register(u0);

cbuffer GabParams : register(b0) {
    uint n;      // 每平面元素数（num_groups × group_dim²）
    uint gd;     // group_dim（256）
    uint xg;     // xsize_groups
    uint xsize;  // 图像宽（镜像边界 = 真实像素尺寸）
    uint ysize;  // 图像高
    uint pad0; uint pad1; uint pad2;
    float4 wx;   // x 通道归一化权重 w0,w1,w2,0
    float4 wy;   // y 通道
    float4 wb;   // b 通道
};
"#;

const GABORISH_TAIL: &str = r#"
// image_ops.h Mirror 单步版（x 距 [0,size) 至多 1：-1→0，size→size-1）
int mir1(int x, int size) {
    return (x < 0) ? (-x - 1) : ((x >= size) ? (2 * size - 1 - x) : x);
}

// 像素→块层槽位采样（全局坐标 → (组, 组内偏移)；边界镜像，跨组正常寻址）。
// 块层为块主序：每 8×8 块的 64 像素按行主序连续存放于该块的系数偏移处
// （与 golden_reconstruct / libjxl idct_ref 导出布局一致）——B8 端到端对拍
// 发现按行主序误读导致整帧块级打乱，此处按块主序解码。
float samp(int px, int py, uint c, uint gdc) {
    int mx = mir1(px, (int)xsize);
    int my = mir1(py, (int)ysize);
    uint g = (uint)(my / (int)gd) * xg + (uint)(mx / (int)gd);
    uint lx = (uint)(mx % (int)gd);
    uint ly = (uint)(my % (int)gd);
    uint idx = ((ly / 8) * (gd / 8) + lx / 8) * 64 + (ly % 8) * 8 + lx % 8;
    return In[c * n + g * gdc + idx];
}

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= n) return;
    uint gdc = gd * gd;
    uint g = p / gdc;
    uint rem = p % gdc;
    int px = (int)((g % xg) * gd + rem % gd);
    int py = (int)((g / xg) * gd + rem / gd);
    bool valid = px < (int)xsize && py < (int)ysize;
    [unroll] for (uint c = 0; c < 3; c++) {
        float4 w = (c == 0) ? wx : ((c == 1) ? wy : wb);
        float v;
        if (valid) {
            float sum0 = samp(px, py, c, gdc);
            float sum1 = samp(px - 1, py, c, gdc) + samp(px + 1, py, c, gdc)
                       + samp(px, py - 1, c, gdc) + samp(px, py + 1, c, gdc);
            float sum2 = samp(px - 1, py - 1, c, gdc) + samp(px + 1, py - 1, c, gdc)
                       + samp(px - 1, py + 1, c, gdc) + samp(px + 1, py + 1, c, gdc);
            // stage_gaborish.cc：MulAdd(sum2, w2, MulAdd(sum1, w1, Mul(sum0, w0)))
            v = sum2 * w.z + (sum1 * w.y + sum0 * w.x);
        } else {
            v = In[c * n + p]; // 填充槽位（图像外）原样透传
        }
        Out[c * n + p] = v;
    }
}
"#;

// pass E：EPF1（render pipeline stage_epf.cc EpfStage::One 同数学；Gaborish 后 /
// XYB 前）。线程 = 一个像素（三通道齐算，13 采样点 × 3）。5×5 去角支撑（13 像素），
// 4 邻居各一个 plus-SAD（5 像素对）；sigma 为 StructuredBuffer 负值（1/sigma），
// sad_mul 只乘进 inv_sigma 一次。镜像 = image_ops.h Mirror（-1→0、-2→1、n→n-1）。
const EPF1_HEAD: &str = r#"
StructuredBuffer<float> In    : register(t0); // [3*n] XYB 块层（Gaborish 输出）
StructuredBuffer<float> Sigma : register(t1); // [(xb+4)*(yb+4)] 负值 1/sigma（紧凑行距 sbx）
RWStructuredBuffer<float> Out : register(u0);

cbuffer EpfParams : register(b0) {
    uint n;      // 每平面元素数（num_groups × group_dim²）
    uint gd;     // group_dim（256）
    uint xg;     // xsize_groups
    uint xsize;  // 图像宽（镜像边界 = 真实像素尺寸）
    uint ysize;  // 图像高
    uint sbx;    // sigma_xsize = xsize_blocks + 4（含 kSigmaPadding=2 环）
    uint pad0; uint pad1;
    float4 cs;   // epf_channel_scale x,y,b,0
    float bsm;   // 1.65 * epf_border_sad_mul
    float min_sig; // kMinSigma
    float2 pad2;
};
"#;

const EPF1_TAIL: &str = r#"
// image_ops.h Mirror 单步版（x 距 [0,size) 至多 2：-1→0、-2→1、size→size-1）
int mir1(int x, int size) {
    return (x < 0) ? (-x - 1) : ((x >= size) ? (2 * size - 1 - x) : x);
}

// 像素→块层槽位采样（全局坐标 → (组, 组内偏移)；边界镜像，跨组正常寻址）
float samp(int px, int py, uint c, uint gdc) {
    int mx = mir1(px, (int)xsize);
    int my = mir1(py, (int)ysize);
    uint g = (uint)(my / (int)gd) * xg + (uint)(mx / (int)gd);
    uint idx = (uint)(my % (int)gd) * gd + (uint)(mx % (int)gd);
    return In[c * n + g * gdc + idx];
}

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= n) return;
    uint gdc = gd * gd;
    uint g = p / gdc;
    uint rem = p % gdc;
    int px = (int)((g % xg) * gd + rem % gd);
    int py = (int)((g / xg) * gd + rem / gd);
    bool valid = px < (int)xsize && py < (int)ysize;
    float sig = 0.0;
    if (valid) {
        // sigma 索引 = (py/8+2)*sbx + (px/8+2)（导出图含 +2 padding 环）
        sig = Sigma[(uint)(py / 8 + 2) * sbx + (uint)(px / 8 + 2)];
    }
    if (!valid || sig < min_sig) {
        [unroll] for (uint c = 0; c < 3; c++) {
            Out[c * n + p] = In[c * n + p]; // 填充槽位 / 低 sigma 块直拷
        }
        return;
    }
    int ix = px % 8;
    int iy = py % 8;
    // 块内边界缩放（stage_epf.cc 240-255 8 元素模式）；只乘进 inv_sigma 一次
    float sad_mul = (iy == 0 || iy == 7 || ix == 0 || ix == 7) ? bsm : 1.65;
    float inv_sigma = sig * sad_mul;

    [unroll] for (uint c = 0; c < 3; c++) {
        float pc  = samp(px,     py,     c, gdc); // 中心 p22
        float pu  = samp(px,     py - 1, c, gdc); // 上 p21
        float pl  = samp(px - 1, py,     c, gdc); // 左 p12
        float pr  = samp(px + 1, py,     c, gdc); // 右 p32
        float pd  = samp(px,     py + 1, c, gdc); // 下 p23
        float pu2 = samp(px,     py - 2, c, gdc); // 上² p20
        float pul = samp(px - 1, py - 1, c, gdc); // 左上 p11
        float pur = samp(px + 1, py - 1, c, gdc); // 右上 p31
        float pl2 = samp(px - 2, py,     c, gdc); // 左² p02
        float pr2 = samp(px + 2, py,     c, gdc); // 右² p42
        float pdl = samp(px - 1, py + 1, c, gdc); // 左下 p13
        float pdr = samp(px + 1, py + 1, c, gdc); // 右下 p33
        float pd2 = samp(px,     py + 2, c, gdc); // 下² p24
        float sc = cs[c];
        // 4 邻居 plus-SAD（5 像素对；源码 277-336 展开式）
        float sad0 = sc * (abs(pu2 - pu) + abs(pul - pl) + abs(pc - pu) + abs(pur - pr) + abs(pd - pc));  // 上
        float sad1 = sc * (abs(pu - pul) + abs(pl - pl2) + abs(pc - pl) + abs(pr - pc) + abs(pd - pdl));  // 左
        float sad2 = sc * (abs(pc - pr) + abs(pu - pur) + abs(pl - pc) + abs(pr - pr2) + abs(pd - pdr));  // 右
        float sad3 = sc * (abs(pc - pd) + abs(pu - pc) + abs(pl - pdl) + abs(pr - pdr) + abs(pd - pd2));  // 下
        float w0 = max(0.0, 1.0 + sad0 * inv_sigma); // Weight = ZeroIfNegative(1+sad·inv_sigma)
        float w1 = max(0.0, 1.0 + sad1 * inv_sigma);
        float w2 = max(0.0, 1.0 + sad2 * inv_sigma);
        float w3 = max(0.0, 1.0 + sad3 * inv_sigma);
        float wsum = 1.0 + w0 + w1 + w2 + w3;
        float acc = pc + w0 * pu + w1 * pl + w2 * pr + w3 * pd;
        Out[c * n + p] = acc * (1.0 / wsum); // JXL_HIGH_PRECISION：精确除法求倒数再乘
    }
}
"#;

// pass O：PQ16 BT.2020（u16）输出——线性 RGB →（可选 709→2020 原色）→ PQ 编码 →
// u16。数学与 [`super::hybrid::pq16_out_ref`] 逐元素同数学（B7，源码依据见该函数
// 文档：nits = linear × intensity_target，1.0 = it nits；PQ 域 1.0 = 10000 nits）。
// 线程 = 一个像素（三通道一起算）；输入 = linear 平面 SRV，输出 = uint 槽位
// （Rust 侧按 u16 读回）。色域矩阵由 CPU 反解后传入（2020 已烘焙时传恒等）。
const PQ16_HEAD: &str = r#"
StructuredBuffer<float> In : register(t0); // [3*n] 线性 RGB（1.0 = intensity_target nits）
RWStructuredBuffer<uint> Out : register(u0); // [3*n] u16 PQ BT.2020（uint 槽位，≤65535）

cbuffer PQParams : register(b0) {
    uint  npix;    // 每平面元素数 n
    uint  gd;      // group_dim（256）
    uint  xg;      // xsize_groups
    uint  xsize;   // 图像宽（有效像素判定）
    uint  ysize;   // 图像高
    float it_nits; // intensity_target（nits）：nits = linear × it
    uint  pad0; uint pad1;
    float4 m0;     // 色域矩阵行 0（3 有效；2020 已烘焙时 = 恒等）
    float4 m1;     // 行 1
    float4 m2;     // 行 2
};

// PQ ST 2084 OETF：L（nits/10000）→ 编码值 [0,1]（clamp 与 Native FloatToU32 等价）
float pq_oetf(float l) {
    l = clamp(l, 0.0, 1.0);
    const float m1 = 0.1593017578125; // 2610/16384
    const float m2 = 78.84375;        // 2523/4096*128
    const float c1 = 0.8359375;       // 3424/4096
    const float c2 = 18.8515625;      // 2413/4096*32
    const float c3 = 18.6875;         // 2392/4096*32
    float lm = pow(l, m1);
    float np = (c1 + c2 * lm) / (1.0 + c3 * lm);
    return pow(np, m2);
}
"#;

const PQ16_TAIL: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= npix) return;
    uint gdc = gd * gd;
    uint g = p / gdc;
    uint rem = p % gdc;
    int px = (int)((g % xg) * gd + rem % gd);
    int py = (int)((g / xg) * gd + rem / gd);
    if (px >= (int)xsize || py >= (int)ysize) {
        Out[p] = 0; Out[npix + p] = 0; Out[2 * npix + p] = 0; // 无效槽位填 0
        return;
    }
    float r = In[p];
    float gg = In[npix + p];
    float b = In[2 * npix + p];
    // 原色转换（线性域；恒等时精确透传）
    float r2 = m0.x * r + m0.y * gg + m0.z * b;
    float g2 = m1.x * r + m1.y * gg + m1.z * b;
    float b2 = m2.x * r + m2.y * gg + m2.z * b;
    // linear（1.0 = it nits）→ PQ 域（1.0 = 10000 nits）→ u16（round，饱和）
    float k = it_nits / 10000.0;
    Out[p]            = (uint)min(pq_oetf(r2 * k) * 65535.0 + 0.5, 65535.0);
    Out[npix + p]     = (uint)min(pq_oetf(g2 * k) * 65535.0 + 0.5, 65535.0);
    Out[2 * npix + p] = (uint)min(pq_oetf(b2 * k) * 65535.0 + 0.5, 65535.0);
}
"#;

// pass F：PQ16 三平面 u16 → scRGB f16 RGBA **行交错直写**（播放出口 GPU 化：
// CPU 重排 + PQ 解码/色域/半精度转换一步消除）。数学与 jxl.rs
// pq16_interleaved_to_scrgb 同源：u16 PQ → LUT 线性 nits → 色域矩阵（2020→709，
// 解码器 ColorEncoding 捕获）→ /80 → f32tof16（截断舍入，与 exr::f32_to_f16 的
// round-to-zero 一致）→ RGBA 打包（A=1.0 = 0x3C00）。线程 = 一个槽位（n 含组
// 填充，无效槽位跳过；有效像素恰好覆盖 w×h 输出一次）。
const F16_HEAD: &str = r#"
StructuredBuffer<uint>  Pq  : register(t0);   // [3*n] u16 PQ BT.2020（uint 槽位，pass O 输出）
StructuredBuffer<float> Lut : register(t1);   // 65536：u16 PQ → 线性 nits（与 CPU 同表）
// 输出（u0）由各 tail 自行声明：F16_TAIL = 行交错 structured buffer；
// F16_TEX_TAIL = RGBA16F 纹理直写（常驻显存播放）

cbuffer F16Params : register(b0) {
    uint  npix;    // 每平面元素数 n
    uint  gd;      // group_dim
    uint  xg;      // xsize_groups
    uint  xsize;   // 图像宽（= 输出行宽）
    uint  ysize;   // 图像高
    uint  pad0; uint pad1; uint pad2;
    float4 m0;     // 色域矩阵（2020→709）行 0
    float4 m1;     // 行 1
    float4 m2;     // 行 2
};
"#;

const F16_TAIL: &str = r#"
RWStructuredBuffer<uint2> Out : register(u0); // [w*h] RGBA f16 行交错（2×u32/px）

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= npix) return;
    uint gdc = gd * gd;
    uint g = p / gdc;
    uint rem = p % gdc;
    int px = (int)((g % xg) * gd + rem % gd);
    int py = (int)((g / xg) * gd + rem / gd);
    if (px >= (int)xsize || py >= (int)ysize) return; // 组填充槽位：不写输出
    float nr = Lut[Pq[p] & 0xFFFF];
    float ng = Lut[Pq[npix + p] & 0xFFFF];
    float nb = Lut[Pq[2 * npix + p] & 0xFFFF];
    float cr = m0.x * nr + m0.y * ng + m0.z * nb;
    float cg = m1.x * nr + m1.y * ng + m1.z * nb;
    float cb = m2.x * nr + m2.y * ng + m2.z * nb;
    uint2 o;
    o.x = f32tof16(cr / 80.0) | (f32tof16(cg / 80.0) << 16);
    o.y = f32tof16(cb / 80.0) | (0x3C00u << 16); // A = 1.0
    Out[py * xsize + px] = o;
}
"#;

// 纹理直写版 tail：与 F16_TAIL 唯一差异 = 输出改绑 RWTexture2D<float4>（RGBA16F
// UAV）。保持 1D 线程 + 同一槽位→(px,py) 寻址（最小 diff；行交错 idx = py*xsize+px
// ≡ 纹理坐标 uint2(px,py)，采样与数学逐行复用）。
//
// f16 位型一致性链（必须与 buffer 版逐位一致，勿改成 half 直接赋值——编译器
// round-to-nearest 会产生 0.5 ULP 级差异）：
//   f32tof16(x)   → RTZ 截断半精度位型（与 buffer 版 / CPU exr::f32_to_f16 同源）
//   f16tof32(u)   → 位型回 float（半精度值在 f32 精确可表示，含次正规/±0/Inf）
//   写 RGBA16F UAV → 硬件 float→f16 存储转换 = round-to-nearest-even，但对已
//   可精确表示的值恒等（无二次舍入）→ 存储位型 == f32tof16 截断位型。
const F16_TEX_TAIL: &str = r#"
RWTexture2D<float4> Dst : register(u0); // RGBA16F 直写（调用方创建，规格 = xsize×ysize）

[numthreads(128, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint p = tid.x;
    if (p >= npix) return;
    uint gdc = gd * gd;
    uint g = p / gdc;
    uint rem = p % gdc;
    int px = (int)((g % xg) * gd + rem % gd);
    int py = (int)((g / xg) * gd + rem / gd);
    if (px >= (int)xsize || py >= (int)ysize) return; // 组填充槽位：不写输出
    float nr = Lut[Pq[p] & 0xFFFF];
    float ng = Lut[Pq[npix + p] & 0xFFFF];
    float nb = Lut[Pq[2 * npix + p] & 0xFFFF];
    float cr = m0.x * nr + m0.y * ng + m0.z * nb;
    float cg = m1.x * nr + m1.y * ng + m1.z * nb;
    float cb = m2.x * nr + m2.y * ng + m2.z * nb;
    Dst[uint2(px, py)] = float4(
        f16tof32(f32tof16(cr / 80.0)),
        f16tof32(f32tof16(cg / 80.0)),
        f16tof32(f32tof16(cb / 80.0)),
        f16tof32(0x3C00u)); // A = 1.0（位型 0x3C00 直通）
}
"#;

// ==================== CPU 预处理 ====================

/// 每块 GPU 入参（HLSL BlockInfo，32 字节）
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BlockInfo {
    dc0: f32,
    dc1: f32,
    dc2: f32,
    s: f32,
    fx: f32,
    fb: f32,
    pad: [f32; 2],
}

/// 组 g 的块网格跨度（abs 块坐标基址 + 边缘组实际块数），prep_frame 两遍共用
fn group_block_span(s: &CoeffSnapshot, g: usize) -> (usize, usize, usize, usize) {
    let group_dim_blocks = s.group_dim as usize / 8;
    let xb = s.xsize_blocks as usize;
    let yb = s.ysize_blocks as usize;
    let xg = s.xsize_groups as usize;
    let gx_blocks = (g % xg) * group_dim_blocks;
    let gy_blocks = (g / xg) * group_dim_blocks;
    let gw = group_dim_blocks.min(xb - gx_blocks.min(xb));
    let gh = group_dim_blocks.min(yb - gy_blocks.min(yb));
    (gx_blocks, gy_blocks, gw, gh)
}

// ==================== GPU 会话 ====================

/// spec 基矩阵（与 Rust idct_1d 修后语义一致）：B[u*8+y] = basis(u, y)
fn basis_matrix() -> Vec<f32> {
    let mut b = vec![0.0f32; 64];
    for u in 0..8 {
        for y in 0..8 {
            b[u * 8 + y] = if y == 0 {
                1.0
            } else {
                (std::f64::consts::SQRT_2
                    * (((u as f64 + 0.5) * y as f64) * std::f64::consts::PI / 8.0)
                        .cos()) as f32
            };
        }
    }
    b
}

/// GPU 黄金重建：与 [`super::hybrid::golden_reconstruct`] 同输入同输出布局。
pub fn gpu_reconstruct(
    s: &CoeffSnapshot,
) -> Result<(Vec<f32>, super::hybrid::GoldenStats, CompareResult), String> {
    gpu_reconstruct_dbg(s, 0)
}

/// 带 debug 阶段直出 / verify 开关的版本。
/// debug bit0 = 1 走旧 packed 路径（pass C packed + CPU scatter，对照/回退）。
pub fn gpu_reconstruct_dbg(
    s: &CoeffSnapshot,
    debug: u32,
) -> Result<(Vec<f32>, super::hybrid::GoldenStats, CompareResult), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    let (full, stats, cmp, _bases) = unsafe { gpu_reconstruct_core(&eng, s, debug, true, true)? };
    Ok((full.ok_or("verify 路径 full 读回缺失")?, stats, cmp))
}

/// 仅输出（无对拍开销）——混合管线生产路径用。返回 (完整帧块层, DCT8 块的 coeff base 列表)。
/// 注意：块层驻留 GPU 后此路径仍读回 44.8MB（签名兼容；零读回路径见
/// [`hybrid_reconstruct_linear`]）。
pub fn gpu_reconstruct_raw(s: &CoeffSnapshot) -> Result<(Vec<f32>, Vec<usize>), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    let (full, _gs, _cmp, bases) = unsafe { gpu_reconstruct_core(&eng, s, 0, true, false)? };
    Ok((full.ok_or("full 读回缺失")?, bases))
}

// ==================== 会话缓存（B4 性能：shader/缓冲跨帧复用） ====================
//
// v0 每次调用 D3DCompile×3 + 重建 ~180MB 缓冲（2K 帧 elems≈9M）——实测为此路径
// 耗时大头（远超 GPU 计算）。会话化后：shader 编译一次；缓冲按容量复用（只增
// 不缩）；DM/基矩阵按内容 tag 跳过重传。动画场景同文件各帧规格一致 → 全命中。

struct SrvBuf {
    buf: ID3D11Buffer,
    srv: ID3D11ShaderResourceView,
    /// 元素容量
    cap: u32,
}

struct RwBuf {
    buf: ID3D11Buffer,
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
    cap: u32,
}

struct HybridSession {
    dequant_cs: ID3D11ComputeShader,
    /// pass A2：快照 coeffs 直读版反量化（生产路径，免 CPU gather）
    a2_cs: ID3D11ComputeShader,
    /// A2 Params（32B：nblocks/xdm/bdm/nggc/biases，独立布局）
    a2_cbuf: ID3D11Buffer,
    idct_b_cs: ID3D11ComputeShader,
    idct_c_cs: ID3D11ComputeShader,
    /// pass C 全帧直写版（B6 块层驻留）
    idct_c_full_cs: ID3D11ComputeShader,
    cbuf: ID3D11Buffer,
    /// pass C 全帧版常量（FullParams 16B，独立布局不与 packed 共用）
    full_cbuf: ID3D11Buffer,
    /// spec 基矩阵（恒定，初始化时一次上传）
    basis: SrvBuf,
    /// 反量化矩阵（随文件；tag 一致跳过重传）
    dm: SrvBuf,
    dm_tag: u64,
    /// 系数 / 块标量（每帧重传）
    coeff: SrvBuf,
    info: SrvBuf,
    dq: RwBuf,
    mid: RwBuf,
    out: RwBuf,
    staging: ID3D11Buffer,
    /// 会话容量（nblocks）
    cap: u32,
    // --- B5b：XYB→线性 RGB pass ---
    xyb_cs: ID3D11ComputeShader,
    xyb_cbuf: ID3D11Buffer,
    xyb_in: RwBuf,
    xyb_out: RwBuf,
    xyb_staging: ID3D11Buffer,
    /// XYB 会话容量（每平面像素数 n）
    xyb_cap: u32,
    // --- B6：块层驻留 GPU ---
    /// 全帧块层（3*nggc 元素，XYB 域；idct_ref 底图 + pass C 直写覆盖 DCT8）
    full_out: RwBuf,
    full_staging: ID3D11Buffer,
    /// 全帧缓冲容量（3*nggc 元素）
    full_cap: u32,
    /// DCT8 块全帧槽位 base 表（uint；内容 tag 一致跳过重传）
    bases: SrvBuf,
    bases_tag: u64,
    // --- B6c：Gaborish（pass G，IDCT 后 / EPF 位置；gab=true 才 dispatch）---
    gab_cs: ID3D11ComputeShader,
    /// GabParams（80B，独立布局）
    gab_cbuf: ID3D11Buffer,
    /// pass G 输出（3n 元素，与 full_out 同布局 ping-pong）
    gab_out: RwBuf,
    gab_staging: ID3D11Buffer,
    /// Gaborish 会话容量（每平面像素数 n）
    gab_cap: u32,
    // --- B6d：EPF1（pass E，Gaborish 后 / XYB 前；epf_iters≥1 且 sigma 有效才 dispatch）---
    epf_cs: ID3D11ComputeShader,
    /// EpfParams（64B，独立布局）
    epf_cbuf: ID3D11Buffer,
    /// pass E 输出（3n 元素，与 full_out/gab_out 同布局 ping-pong）
    epf_out: RwBuf,
    epf_staging: ID3D11Buffer,
    /// EPF 会话容量（每平面像素数 n）
    epf_cap: u32,
    /// sigma 图（动画各帧不同 → 每帧无条件重传，无 tag 跳过；~261KB @ 2K 帧）
    sigma: SrvBuf,
    sigma_cap: u32,
    // --- B7：PQ16 输出（pass O，线性 RGB → PQ16 BT.2020 u16）---
    pq_cs: ID3D11ComputeShader,
    /// PqParams（80B，独立布局）
    pq_cbuf: ID3D11Buffer,
    /// pass O 输出（3n uint 槽位，每槽一个 u16 值）
    pq_out: RwBuf,
    pq_staging: ID3D11Buffer,
    /// PQ pass 会话容量（每平面像素数 n）
    pq_cap: u32,
    // --- B9：pass F（PQ16 → scRGB f16 行交错直写，播放出口 GPU 化）---
    f16_cs: ID3D11ComputeShader,
    /// pass F 直写纹理版（F16_HEAD + F16_TEX_TAIL；输出 = RWTexture2D<float4>
    /// RGBA16F UAV，常驻显存播放底层准备）
    f16_tex_cs: ID3D11ComputeShader,
    /// F16Params（80B，独立布局）
    f16_cbuf: ID3D11Buffer,
    /// PQ16→nits LUT（65536 f32，内容恒定 → 首次上传后 tag 跳过）
    f16_lut: SrvBuf,
    /// pass F 输出（[w*h] uint2 行交错 f16 RGBA，stride 8）
    f16_out: RwBuf,
    f16_staging: ID3D11Buffer,
    /// pass F 会话容量（输出像素数 w*h）
    f16_cap: u32,
    // --- 剖析 fence（JXL_PROF_SYNC=1 时逐 pass 同步用，16B 哑元拷贝）---
    sync_buf: ID3D11Buffer,
    sync_staging: ID3D11Buffer,
}

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

fn mk_rw_buf(
    device: &ID3D11Device,
    elems: u32,
) -> Result<(ID3D11Buffer, ID3D11ShaderResourceView, ID3D11UnorderedAccessView), String> {
    mk_rw_buf_stride(device, elems, 4)
}

/// 任意 stride 的 RW 结构化缓冲（pass F 输出 uint2 = stride 8）
fn mk_rw_buf_stride(
    device: &ID3D11Device,
    elems: u32,
    stride: u32,
) -> Result<(ID3D11Buffer, ID3D11ShaderResourceView, ID3D11UnorderedAccessView), String> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: elems * stride,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: stride,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| format!("CreateBuffer(rw): {e}"))?;
        let buf: ID3D11Buffer = buf.ok_or("RW 缓冲创建失败")?;
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
            .map_err(|e| format!("CreateSRV(rw): {e}"))?;
        let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: elems,
                    Flags: 0,
                },
            },
        };
        let mut uav = None;
        device
            .CreateUnorderedAccessView(&buf, Some(&uav_desc), Some(&mut uav))
            .map_err(|e| format!("CreateUAV: {e}"))?;
        Ok((
            buf,
            srv.ok_or("SRV 创建失败")?,
            uav.ok_or("UAV 创建失败")?,
        ))
    }
}

fn mk_staging(device: &ID3D11Device, elems: u32) -> Result<ID3D11Buffer, String> {
    mk_staging_stride(device, elems, 4)
}

/// 任意 stride 的 staging 缓冲（与源同容量创建 → CopyResource 恒合法）
fn mk_staging_stride(device: &ID3D11Device, elems: u32, stride: u32) -> Result<ID3D11Buffer, String> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: elems * stride,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: stride,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| format!("CreateBuffer(staging): {e}"))?;
    }
    buf.ok_or_else(|| "staging 创建失败".to_string())
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

/// FNV-1a 内容 tag（DM/基矩阵重传跳过判断）
fn data_tag(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl HybridSession {
    fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<HybridSession, String> {
        let basis = basis_matrix();
        let (basis_buf, basis_srv) = mk_srv_buf(device, basis.len() as u32, 4)?;
        unsafe {
            ctx.UpdateSubresource(
                &basis_buf,
                0,
                None,
                basis.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
        let (dm_buf, dm_srv) = mk_srv_buf(device, 1, 4)?; // 占位，容量随首帧扩
        let (coeff_buf, coeff_srv) = mk_srv_buf(device, 1, 4)?;
        let (info_buf, info_srv) = mk_srv_buf(device, 1, 32)?;
        let (dq_buf, dq_srv, dq_uav) = mk_rw_buf(device, 1)?;
        let (mid_buf, mid_srv, mid_uav) = mk_rw_buf(device, 1)?;
        let (out_buf, _out_srv, out_uav) = mk_rw_buf(device, 1)?;
        let cbuf_desc = D3D11_BUFFER_DESC {
            ByteWidth: 32, // 16 对齐
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut cbuf = None;
        unsafe {
            device
                .CreateBuffer(&cbuf_desc, None, Some(&mut cbuf))
                .map_err(|e| format!("CreateBuffer(cbuf): {e}"))?;
        }
        Ok(HybridSession {
            dequant_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                DEQUANT_CS,
                b"hybrid_dequant\0",
            )?,
            a2_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                DEQUANT_DIRECT_CS,
                b"hybrid_dequant_direct\0",
            )?,
            a2_cbuf: mk_cbuf(device, 32)?,
            idct_b_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", IDCT_HEAD, IDCT_B_TAIL),
                b"hybrid_idct_b\0",
            )?,
            idct_c_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", IDCT_HEAD, IDCT_C_TAIL),
                b"hybrid_idct_c\0",
            )?,
            idct_c_full_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", IDCT_C_FULL_HEAD, IDCT_C_FULL_TAIL),
                b"hybrid_idct_c_full\0",
            )?,
            cbuf: cbuf.ok_or("cbuf 创建失败")?,
            full_cbuf: mk_cbuf(device, 16)?,
            basis: SrvBuf {
                buf: basis_buf,
                srv: basis_srv,
                cap: basis.len() as u32,
            },
            dm: SrvBuf {
                buf: dm_buf,
                srv: dm_srv,
                cap: 1,
            },
            dm_tag: 0,
            coeff: SrvBuf {
                buf: coeff_buf,
                srv: coeff_srv,
                cap: 1,
            },
            info: SrvBuf {
                buf: info_buf,
                srv: info_srv,
                cap: 1,
            },
            dq: RwBuf {
                buf: dq_buf,
                srv: dq_srv,
                uav: dq_uav,
                cap: 1,
            },
            mid: RwBuf {
                buf: mid_buf,
                srv: mid_srv,
                uav: mid_uav,
                cap: 1,
            },
            out: RwBuf {
                buf: out_buf,
                srv: _out_srv,
                uav: out_uav,
                cap: 1,
            },
            staging: mk_staging(device, 1)?,
            cap: 0,
            xyb_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", XYB_HEAD, XYB_TAIL),
                b"hybrid_xyb\0",
            )?,
            xyb_cbuf: mk_cbuf(device, 96)?,
            xyb_in: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            xyb_out: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            xyb_staging: mk_staging(device, 1)?,
            xyb_cap: 0,
            full_out: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            full_staging: mk_staging(device, 1)?,
            full_cap: 0,
            bases: {
                let (b, v) = mk_srv_buf(device, 1, 4)?;
                SrvBuf {
                    buf: b,
                    srv: v,
                    cap: 1,
                }
            },
            bases_tag: 0,
            gab_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", GABORISH_HEAD, GABORISH_TAIL),
                b"hybrid_gab\0",
            )?,
            gab_cbuf: mk_cbuf(device, 80)?,
            gab_out: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            gab_staging: mk_staging(device, 1)?,
            gab_cap: 0,
            epf_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", EPF1_HEAD, EPF1_TAIL),
                b"hybrid_epf1\0",
            )?,
            epf_cbuf: mk_cbuf(device, 64)?,
            epf_out: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            epf_staging: mk_staging(device, 1)?,
            epf_cap: 0,
            sigma: {
                let (b, v) = mk_srv_buf(device, 1, 4)?;
                SrvBuf {
                    buf: b,
                    srv: v,
                    cap: 1,
                }
            },
            sigma_cap: 0,
            pq_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", PQ16_HEAD, PQ16_TAIL),
                b"hybrid_pq16\0",
            )?,
            pq_cbuf: mk_cbuf(device, 80)?,
            pq_out: {
                let (b, v, u) = mk_rw_buf(device, 1)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            pq_staging: mk_staging(device, 1)?,
            pq_cap: 0,
            f16_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", F16_HEAD, F16_TAIL),
                b"hybrid_f16\0",
            )?,
            f16_tex_cs: crate::upscale::d3d11::compile_cs_cached(
                device,
                &format!("{}\n{}", F16_HEAD, F16_TEX_TAIL),
                b"hybrid_f16_tex\0",
            )?,
            f16_cbuf: mk_cbuf(device, 80)?,
            f16_lut: {
                let (b, v) = mk_srv_buf(device, 1, 4)?;
                SrvBuf {
                    buf: b,
                    srv: v,
                    cap: 1,
                }
            },
            f16_out: {
                let (b, v, u) = mk_rw_buf_stride(device, 1, 8)?;
                RwBuf {
                    buf: b,
                    srv: v,
                    uav: u,
                    cap: 1,
                }
            },
            f16_staging: mk_staging_stride(device, 1, 8)?,
            f16_cap: 0,
            sync_buf: {
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: 0,
                    CPUAccessFlags: 0,
                    MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                    StructureByteStride: 4,
                };
                let mut b = None;
                unsafe {
                    device
                        .CreateBuffer(&desc, None, Some(&mut b))
                        .map_err(|e| format!("CreateBuffer(sync): {e}"))?;
                }
                b.ok_or("sync_buf 创建失败")?
            },
            sync_staging: {
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                    StructureByteStride: 4,
                };
                let mut b = None;
                unsafe {
                    device
                        .CreateBuffer(&desc, None, Some(&mut b))
                        .map_err(|e| format!("CreateBuffer(sync_staging): {e}"))?;
                }
                b.ok_or("sync_staging 创建失败")?
            },
        })
    }

    /// 容量不足时重建动态缓冲（只增不缩）。coeff 缓冲独立按元素容量扩
    ///（pass A2 直读版 = 3*nggc 快照布局；旧 packed 路径 = nblocks*192）
    fn ensure_capacity(
        &mut self,
        device: &ID3D11Device,
        nblocks: u32,
        coeff_elems: u32,
    ) -> Result<(), String> {
        if self.cap < nblocks {
            let elems = nblocks * 192;
            let (b, v) = mk_srv_buf(device, nblocks, 32)?;
            self.info = SrvBuf {
                buf: b,
                srv: v,
                cap: nblocks,
            };
            let (b, v, u) = mk_rw_buf(device, elems)?;
            self.dq = RwBuf {
                buf: b,
                srv: v,
                uav: u,
                cap: elems,
            };
            let (b, v, u) = mk_rw_buf(device, elems)?;
            self.mid = RwBuf {
                buf: b,
                srv: v,
                uav: u,
                cap: elems,
            };
            let (b, v, u) = mk_rw_buf(device, elems)?;
            self.out = RwBuf {
                buf: b,
                srv: v,
                uav: u,
                cap: elems,
            };
            self.staging = mk_staging(device, elems)?;
            self.cap = nblocks;
        }
        if self.coeff.cap < coeff_elems {
            let (b, v) = mk_srv_buf(device, coeff_elems, 4)?;
            self.coeff = SrvBuf {
                buf: b,
                srv: v,
                cap: coeff_elems,
            };
        }
        Ok(())
    }

    /// DM 内容变化时重传（同文件各帧 tag 一致 → 跳过）
    fn ensure_dm(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        dm: &[f32],
    ) -> Result<(), String> {
        let tag = data_tag(bytemuck_bytes(dm));
        if self.dm.cap >= dm.len() as u32 && self.dm_tag == tag {
            return Ok(());
        }
        if self.dm.cap < dm.len() as u32 {
            let (b, v) = mk_srv_buf(device, dm.len() as u32, 4)?;
            self.dm = SrvBuf {
                buf: b,
                srv: v,
                cap: dm.len() as u32,
            };
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.dm.buf,
                0,
                None,
                dm.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
        self.dm_tag = tag;
        Ok(())
    }

    /// XYB pass 容量不足时重建（n = 每平面像素数）
    fn ensure_xyb(&mut self, device: &ID3D11Device, n: u32) -> Result<(), String> {
        if self.xyb_cap >= n {
            return Ok(());
        }
        let elems = n * 3;
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.xyb_in = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.xyb_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.xyb_staging = mk_staging(device, elems)?;
        self.xyb_cap = n;
        Ok(())
    }

    /// 全帧块层缓冲容量不足时重建（elems = 3*nggc；staging 与源同容量创建 →
    /// CopyResource 恒合法；只增不缩）
    fn ensure_full(&mut self, device: &ID3D11Device, elems: u32) -> Result<(), String> {
        if self.full_cap >= elems {
            return Ok(());
        }
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.full_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.full_staging = mk_staging(device, elems)?;
        self.full_cap = elems;
        Ok(())
    }

    /// Gaborish pass 容量不足时重建（n = 每平面像素数；缓冲 = 3n；只增不缩）
    fn ensure_gab(&mut self, device: &ID3D11Device, n: u32) -> Result<(), String> {
        if self.gab_cap >= n {
            return Ok(());
        }
        let elems = n * 3;
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.gab_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.gab_staging = mk_staging(device, elems)?;
        self.gab_cap = n;
        Ok(())
    }

    /// EPF pass 容量不足时重建（n = 每平面像素数；缓冲 = 3n；只增不缩）
    fn ensure_epf(&mut self, device: &ID3D11Device, n: u32) -> Result<(), String> {
        if self.epf_cap >= n {
            return Ok(());
        }
        let elems = n * 3;
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.epf_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.epf_staging = mk_staging(device, elems)?;
        self.epf_cap = n;
        Ok(())
    }

    /// sigma 图容量不足时重建（内容每帧不同 → 无 tag，由派发函数无条件重传）
    fn ensure_sigma(&mut self, device: &ID3D11Device, len: u32) -> Result<(), String> {
        if self.sigma_cap >= len {
            return Ok(());
        }
        let (b, v) = mk_srv_buf(device, len, 4)?;
        self.sigma = SrvBuf {
            buf: b,
            srv: v,
            cap: len,
        };
        self.sigma_cap = len;
        Ok(())
    }

    /// PQ pass 容量不足时重建（n = 每平面像素数；缓冲 = 3n uint 槽位；只增不缩）
    fn ensure_pq(&mut self, device: &ID3D11Device, n: u32) -> Result<(), String> {
        if self.pq_cap >= n {
            return Ok(());
        }
        let elems = n * 3;
        let (b, v, u) = mk_rw_buf(device, elems)?;
        self.pq_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.pq_staging = mk_staging(device, elems)?;
        self.pq_cap = n;
        Ok(())
    }

    /// pass F 容量不足时重建（elems = w*h 输出像素，uint2/8B；只增不缩）。
    /// LUT 内容恒定 → 首次调用上传一次（256KB），此后跳过。
    fn ensure_f16(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, elems: u32) -> Result<(), String> {
        if self.f16_cap >= elems {
            return self.ensure_f16_lut(device, ctx);
        }
        let (b, v, u) = mk_rw_buf_stride(device, elems, 8)?;
        self.f16_out = RwBuf {
            buf: b,
            srv: v,
            uav: u,
            cap: elems,
        };
        self.f16_staging = mk_staging_stride(device, elems, 8)?;
        self.f16_cap = elems;
        self.ensure_f16_lut(device, ctx)
    }

    /// PQ16→nits LUT（65536 f32 = 256KB，内容恒定 → 首次上传后跳过；buffer /
    /// 纹理两版 pass F 共用，直写纹理路径只触碰它，不建 f16_out/staging）
    fn ensure_f16_lut(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<(), String> {
        if self.f16_lut.cap >= 65536 {
            return Ok(());
        }
        // PQ16 → 线性 nits 查找表（与 jxl.rs pq16_nits_lut 同式同源）
        let lut: Vec<f32> = (0..65536u32)
            .map(|i| crate::color::pq_eotf(i as f32 / 65535.0) * 10000.0)
            .collect();
        let (b, v) = mk_srv_buf(device, 65536, 4)?;
        unsafe {
            ctx.UpdateSubresource(
                &b,
                0,
                None,
                lut.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
        self.f16_lut = SrvBuf {
            buf: b,
            srv: v,
            cap: 65536,
        };
        Ok(())
    }

    /// 系数槽位 base 表内容变化时重传（同文件动画各帧相同 → FNV tag 跳过）
    fn ensure_bases(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        bases: &[u32],
    ) -> Result<(), String> {
        let tag = data_tag(bytemuck_bytes(bases));
        if self.bases.cap >= bases.len() as u32 && self.bases_tag == tag {
            return Ok(());
        }
        if self.bases.cap < bases.len() as u32 {
            let (b, v) = mk_srv_buf(device, bases.len() as u32, 4)?;
            self.bases = SrvBuf {
                buf: b,
                srv: v,
                cap: bases.len() as u32,
            };
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.bases.buf,
                0,
                None,
                bases.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
        self.bases_tag = tag;
        Ok(())
    }
}

/// POD 切片按字节视图（无 bytemuck 依赖）
fn bytemuck_bytes<T>(data: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of::<T>() * data.len())
    }
}

static SESSION: std::sync::OnceLock<std::sync::Mutex<Result<HybridSession, String>>> =
    std::sync::OnceLock::new();

/// 空对拍结果（verify=false 占位）
fn cmp_none() -> CompareResult {
    CompareResult {
        psnr_db: f64::INFINITY,
        max_abs_diff: 0.0,
        mse: 0.0,
        peak: 0.0,
        samples: 0,
    }
}

/// CPU 预处理（每帧，全帧驻留路径）：块标量收集 + 全帧槽位 base 表。
/// 剖析后重构：不再做系数紧凑化 gather（旧版散读 55MB + 写 49MB，DRAM 延迟
/// 瓶颈 ~7ms/帧）——pass A2 经 SrcBase 表直读快照 coeffs（GPU 侧上传 55MB
/// 连续拷贝代替），本函数只产出 infos + bases（~512KB）。
/// 两遍结构：Pass1 顺序数各组 DCT8 块数做前缀偏移（组序 × 组内行主序，与旧
/// 单线程扫描产出完全一致的块序），Pass2 组级并行填充 infos/bases。
fn prep_frame(s: &CoeffSnapshot) -> Result<(Vec<BlockInfo>, Vec<u32>), String> {
    let prof = super::prof_enabled();
    let t0 = std::time::Instant::now();
    let ng = s.num_groups as usize;
    let gc = s.group_dim as usize * s.group_dim as usize;
    let xb = s.xsize_blocks as usize;

    // --- Pass 1：各组 DCT8 块计数 → 前缀偏移 ---
    let mut counts = Vec::with_capacity(ng);
    let mut total = 0usize;
    for g in 0..ng {
        let (gx_blocks, gy_blocks, gw, gh) = group_block_span(s, g);
        let mut c = 0usize;
        for by in 0..gh {
            for bx in 0..gw {
                let acs_byte = s.ac_strategy[(gy_blocks + by) * xb + gx_blocks + bx];
                if acs_byte & 1 != 0 && (acs_byte >> 1) as usize == DCT8 {
                    c += 1;
                }
            }
        }
        total += c;
        counts.push(c);
    }
    if total == 0 {
        return Err("无 DCT8 块".to_string());
    }
    let mut group_out_base = Vec::with_capacity(ng + 1);
    let mut acc = 0usize;
    for &c in &counts {
        group_out_base.push(acc);
        acc += c;
    }
    group_out_base.push(total); // 哨兵：group_out_base[ng] = 总块数（分段切分用）
    let nblocks = total;
    if prof {
        println!(
            "[prof] prep Pass1(计数): {:.1}ms（{} DCT8 块）",
            t0.elapsed().as_secs_f64() * 1000.0,
            nblocks
        );
    }
    let t1 = std::time::Instant::now();

    let mut infos = vec![BlockInfo::default(); nblocks];
    let mut bases = vec![0u32; nblocks];

    // --- Pass 2：组级并行填充（各组写互不相交的输出槽位区间） ---
    let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
        .max(1);
    let chunk = (ng + nthreads - 1) / nthreads;
    let tiles_x = s.cmap_tiles_x as usize;
    let dc_x = s.dc_xsize as usize;
    let dc_y = s.dc_ysize as usize;
    std::thread::scope(|scope| {
        // 组区间 → 输出槽位区间逐段 split（各线程持有互不相交的可变子切片）
        let gob = &group_out_base; // 共享只读借用进线程（move 只搬引用）
        let err_ref = &err;
        let mut infos_rest = &mut infos[..];
        let mut bases_rest = &mut bases[..];
        let mut prev = 0usize;
        for g0 in (0..ng).step_by(chunk) {
            let g1 = (g0 + chunk).min(ng);
            let take = group_out_base[g1] - prev;
            let (i_mine, i_next) = infos_rest.split_at_mut(take);
            let (b_mine, b_next) = bases_rest.split_at_mut(take);
            infos_rest = i_next;
            bases_rest = b_next;
            let base = prev;
            scope.spawn(move || {
                for g in g0..g1 {
                    let (gx_blocks, gy_blocks, gw, gh) = group_block_span(s, g);
                    let mut out_idx = gob[g] - base;
                    let mut offset = 0usize;
                    for by in 0..gh {
                        for bx in 0..gw {
                            let abs_bx = gx_blocks + bx;
                            let abs_by = gy_blocks + by;
                            let acs_byte =
                                s.ac_strategy[(gy_blocks + by) * xb + gx_blocks + bx];
                            if acs_byte & 1 == 0 {
                                continue;
                            }
                            let kind = (acs_byte >> 1) as usize;
                            let size = COVERED_X[kind] as usize * COVERED_Y[kind] as usize * BLOCK;
                            if kind != DCT8 {
                                offset += size;
                                continue;
                            }
                            b_mine[out_idx] = (g * gc + offset) as u32;
                            let quant = s.raw_quant_field[abs_by * xb + abs_bx];
                            let scaled_s = s.inv_global_scale / quant as f32;
                            let tx = abs_bx / 8;
                            let ty = abs_by / 8;
                            let fx = s.cfl_base_x
                                + s.cmap_ytox[ty * tiles_x + tx] as f32 / s.cfl_color_factor;
                            let fb = s.cfl_base_b
                                + s.cmap_ytob[ty * tiles_x + tx] as f32 / s.cfl_color_factor;
                            if abs_by >= dc_y || abs_bx >= dc_x {
                                let mut e = err_ref.lock().unwrap();
                                if e.is_none() {
                                    *e = Some(format!(
                                        "DC 坐标越界: abs ({abs_bx},{abs_by}) dc {dc_x}x{dc_y}"
                                    ));
                                }
                            } else {
                                i_mine[out_idx] = BlockInfo {
                                    dc0: s.dc[(0 * dc_y + abs_by) * dc_x + abs_bx],
                                    dc1: s.dc[(1 * dc_y + abs_by) * dc_x + abs_bx],
                                    dc2: s.dc[(2 * dc_y + abs_by) * dc_x + abs_bx],
                                    s: scaled_s,
                                    fx,
                                    fb,
                                    pad: [0.0; 2],
                                };
                            }
                            out_idx += 1;
                            offset += size;
                        }
                    }
                }
            });
            prev = group_out_base[g1];
        }
    });
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }
    if prof {
        println!(
            "[prof] prep Pass2(并行 infos+bases): {:.1}ms",
            t1.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok((infos, bases))
}

/// 旧 packed 路径（debug=1 对照/回退保留）专用预处理：块标量 + 系数紧凑化
///（[block][3][64] i32，单线程散拷贝——生产路径已由 pass A2 直读取代）。
fn prep_frame_packed(s: &CoeffSnapshot) -> Result<(Vec<BlockInfo>, Vec<i32>, Vec<usize>), String> {
    let (infos, _bases) = prep_frame(s)?;
    let nblocks = infos.len();
    let group_coeffs = s.group_dim as usize * s.group_dim as usize;
    let ng = s.num_groups as usize;

    // gather：[block][3][64] int32（DCT8 块系数紧凑化）
    let mut coeffs_packed = vec![0i32; nblocks * 192];
    let mut coeff_bases = Vec::with_capacity(nblocks);
    let mut i = 0usize;
    for g in 0..s.num_groups as usize {
        let gx_blocks = (g % s.xsize_groups as usize) * (s.group_dim as usize / 8);
        let gy_blocks = (g / s.xsize_groups as usize) * (s.group_dim as usize / 8);
        let gw = (s.group_dim as usize / 8).min(s.xsize_blocks as usize - gx_blocks.min(s.xsize_blocks as usize));
        let gh = (s.group_dim as usize / 8).min(s.ysize_blocks as usize - gy_blocks.min(s.ysize_blocks as usize));
        let mut offset = 0usize;
        for by in 0..gh {
            for bx in 0..gw {
                let acs_byte = s.ac_strategy[(gy_blocks + by) * s.xsize_blocks as usize + gx_blocks + bx];
                if acs_byte & 1 == 0 {
                    continue;
                }
                let kind = (acs_byte >> 1) as usize;
                let size = COVERED_X[kind] as usize * COVERED_Y[kind] as usize * BLOCK;
                if kind == DCT8 {
                    let src_base = g * group_coeffs + offset;
                    for c in 0..3 {
                        let from = c * ng * group_coeffs + src_base;
                        coeffs_packed[i * 192 + c * 64..i * 192 + c * 64 + 64]
                            .copy_from_slice(&s.coeffs[from..from + 64]);
                    }
                    coeff_bases.push(src_base);
                    i += 1;
                }
                offset += size;
            }
        }
    }
    debug_assert_eq!(i, nblocks);
    Ok((infos, coeffs_packed, coeff_bases))
}

/// 剖析 fence（JXL_PROF_SYNC=1）：哑元 16B CopyResource + Map(READ)——
/// Map 隐式等待此前全部 GPU 命令完成（Flush 只提交不等待，量不准）
unsafe fn prof_fence(ctx: &ID3D11DeviceContext, sess: &HybridSession) -> Result<(), String> {
    unsafe {
        ctx.CopyResource(&sess.sync_staging, &sess.sync_buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.sync_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(sync): {e}"))?;
        ctx.Unmap(&sess.sync_staging, 0);
    }
    Ok(())
}

/// 每帧输入上传 + Params cbuf（32B）+ pass A（反量化+DC）+ pass B（1D 行变换）。
/// cbuf 保持绑定（pass A/B/C-packed 同布局共用）。
unsafe fn run_dequant_row_passes(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    nblocks: usize,
    coeffs_packed: &[i32],
    infos: &[BlockInfo],
) -> Result<(), String> {
    unsafe {
        ctx.UpdateSubresource(
            &sess.coeff.buf,
            0,
            None,
            coeffs_packed.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        ctx.UpdateSubresource(
            &sess.info.buf,
            0,
            None,
            infos.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
    }

    // 常量缓冲（DYNAMIC，每帧 DISCARD 写）
    #[repr(C)]
    struct Params {
        nblocks: u32,
        xdm: f32,
        bdm: f32,
        pad: f32,
        biases: [f32; 4],
    }
    let params = Params {
        nblocks: nblocks as u32,
        xdm: s.x_dm_multiplier,
        bdm: s.b_dm_multiplier,
        pad: 0.0,
        biases: s.quant_biases,
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const Params as *const u8,
        mapped.pData as *mut u8,
        32,
    );
    ctx.Unmap(&sess.cbuf, 0);

    let groups = ((nblocks * 192) as u32).div_ceil(128);
    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv1: [Option<ID3D11ShaderResourceView>; 1] = [None];
    let null_srv3: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];

    // --- pass A：反量化 + DC ---
    ctx.CSSetShader(&sess.dequant_cs, None);
    ctx.CSSetConstantBuffers(0, Some(&[Some(sess.cbuf.clone())]));
    ctx.CSSetShaderResources(
        0,
        Some(&[
            Some(sess.coeff.srv.clone()),
            Some(sess.dm.srv.clone()),
            Some(sess.info.srv.clone()),
        ]),
    );
    let uavs_a = [Some(sess.dq.uav.clone())];
    ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_a.as_ptr()), None);
    ctx.Dispatch(groups, 1, 1);
    ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
    ctx.CSSetShaderResources(0, Some(&null_srv3));

    // --- pass B：1D 行变换（读转置）---
    ctx.CSSetShader(&sess.idct_b_cs, None);
    ctx.CSSetShaderResources(
        0,
        Some(&[Some(sess.dq.srv.clone()), Some(sess.basis.srv.clone())]),
    );
    let uavs_b = [Some(sess.mid.uav.clone())];
    ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_b.as_ptr()), None);
    ctx.Dispatch(groups, 1, 1);
    ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
    ctx.CSSetShaderResources(0, Some(&null_srv1));
    Ok(())
}

/// A2 版每帧输入上传（快照 coeffs 全量直传 55MB，免 CPU gather）+ A2 Params
/// cbuf（32B）+ pass A2（直读反量化）+ pass B（1D 行变换，读旧 cbuf）。
/// bases 表的上传（内容 tag 跳过）由 run_full_passes 的 ensure_bases 负责。
unsafe fn run_dequant_row_passes_a2(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    nblocks: usize,
    nggc: usize,
    infos: &[BlockInfo],
) -> Result<(), String> {
    unsafe {
        // 快照 coeffs 全量直传（3*nggc i32 = 55MB 连续拷贝；同文件各帧都要传）
        ctx.UpdateSubresource(
            &sess.coeff.buf,
            0,
            None,
            s.coeffs.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        ctx.UpdateSubresource(
            &sess.info.buf,
            0,
            None,
            infos.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
    }

    // A2 Params（32B：nblocks/xdm/bdm/nggc/biases）
    #[repr(C)]
    struct ParamsA2 {
        nblocks: u32,
        xdm: f32,
        bdm: f32,
        nggc: u32,
        biases: [f32; 4],
    }
    let params = ParamsA2 {
        nblocks: nblocks as u32,
        xdm: s.x_dm_multiplier,
        bdm: s.b_dm_multiplier,
        nggc: nggc as u32,
        biases: s.quant_biases,
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.a2_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(a2_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const ParamsA2 as *const u8,
        mapped.pData as *mut u8,
        32,
    );
    ctx.Unmap(&sess.a2_cbuf, 0);

    // pass B 的 cbuf（旧布局：nblocks/xdm/bdm/pad/biases；B 只读 nblocks）
    #[repr(C)]
    struct Params {
        nblocks: u32,
        xdm: f32,
        bdm: f32,
        pad: f32,
        biases: [f32; 4],
    }
    let params_b = Params {
        nblocks: nblocks as u32,
        xdm: s.x_dm_multiplier,
        bdm: s.b_dm_multiplier,
        pad: 0.0,
        biases: s.quant_biases,
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params_b as *const Params as *const u8,
        mapped.pData as *mut u8,
        32,
    );
    ctx.Unmap(&sess.cbuf, 0);

    let groups = ((nblocks * 192) as u32).div_ceil(128);
    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv1: [Option<ID3D11ShaderResourceView>; 1] = [None];
    let null_srv4: [Option<ID3D11ShaderResourceView>; 4] = [None, None, None, None];

    unsafe {
        // --- pass A2：直读快照 coeffs 反量化 + DC ---
        ctx.CSSetShader(&sess.a2_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.a2_cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[
                Some(sess.coeff.srv.clone()),
                Some(sess.dm.srv.clone()),
                Some(sess.bases.srv.clone()),
                Some(sess.info.srv.clone()),
            ]),
        );
        let uavs_a = [Some(sess.dq.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_a.as_ptr()), None);
        ctx.Dispatch(groups, 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv4));
        ctx.CSSetConstantBuffers(0, Some(&[None]));

        // --- pass B：1D 行变换（读转置；旧 cbuf 布局）---
        ctx.CSSetShader(&sess.idct_b_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[Some(sess.dq.srv.clone()), Some(sess.basis.srv.clone())]),
        );
        let uavs_b = [Some(sess.mid.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_b.as_ptr()), None);
        ctx.Dispatch(groups, 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv1));
        ctx.CSSetConstantBuffers(0, Some(&[None]));
    }
    Ok(())
}

/// 全帧驻留路径（B6 + A2 直读）：容量/内容准备 → 每帧上传（快照 coeffs 直传/
/// 标量/bases/[idct_ref 底图]）→ pass A2（直读反量化）→ pass B → pass C 全帧直写。
/// 结果：sess.full_out = 完整帧块层（XYB 域）。
/// upload_base=false（生产播放路径）时按文件类型自适应：**全 DCT8** 文件（录制
/// 产物，idct_ref 槽位恒 0，55MB/帧纯浪费）跳过底图上传——pass C 直写覆盖全部
/// 有效块槽位；组填充槽位（图像外）的透传脏值被 pass O 对无效槽位强制写 0、
/// pass F 跳过，不影响有效像素输出。**混合文件**（含 varblock）非 DCT8 槽位
/// 是有效像素数据，必须上传。read_full/verify 路径（gpu_reconstruct_core）传
/// true 保持对拍语义（填充槽位 = idct_ref = 0 参与 PSNR）。
#[allow(clippy::too_many_arguments)]
unsafe fn run_full_passes(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    nblocks: usize,
    infos: &[BlockInfo],
    bases_u32: &[u32],
    upload_base: bool,
) -> Result<(), String> {
    let nggc = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    let full_elems = (3 * nggc) as u32;
    if s.idct_ref.len() < full_elems as usize {
        return Err(format!(
            "idct_ref 尺寸异常: {} < {}",
            s.idct_ref.len(),
            full_elems
        ));
    }
    sess.ensure_capacity(device, nblocks as u32, full_elems)?;
    sess.ensure_dm(device, ctx, &s.dequant_matrices_dct8)?;
    sess.ensure_full(device, full_elems)?;
    sess.ensure_bases(device, ctx, bases_u32)?;

    unsafe { run_dequant_row_passes_a2(ctx, sess, s, nblocks, nggc, infos)? };

    // 全帧底图：非 DCT8 槽位 = libjxl 块层（有效值），DCT8 槽位随后被 pass C 覆盖。
    // 全 DCT8 文件（录制产物）且调用方不要求底图 → 跳过 55MB/帧 的零值上传；
    // 混合文件（含 varblock）非 DCT8 槽位是有效像素数据 → 必须上传。
    let all_dct8 = nblocks == s.xsize_blocks as usize * s.ysize_blocks as usize;
    if upload_base || !all_dct8 {
        unsafe {
            ctx.UpdateSubresource(
                &sess.full_out.buf,
                0,
                None,
                s.idct_ref.as_ptr() as *const core::ffi::c_void,
                0,
                0,
            );
        }
    }

    // FullParams cbuf（16B；独立布局，不与 packed Params 共用避免字段错位）
    #[repr(C)]
    struct FullParams {
        nblocks: u32,
        nggc: u32,
        pad: [u32; 2],
    }
    let fp = FullParams {
        nblocks: nblocks as u32,
        nggc: nggc as u32,
        pad: [0; 2],
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.full_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(full_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &fp as *const FullParams as *const u8,
        mapped.pData as *mut u8,
        16,
    );
    ctx.Unmap(&sess.full_cbuf, 0);

    let groups = ((nblocks * 192) as u32).div_ceil(128);
    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv3: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];

    // --- pass C-full：Mid 转置读 → 直写全帧布局（块层驻留 GPU）---
    ctx.CSSetShader(&sess.idct_c_full_cs, None);
    ctx.CSSetConstantBuffers(0, Some(&[Some(sess.full_cbuf.clone())]));
    ctx.CSSetShaderResources(
        0,
        Some(&[
            Some(sess.mid.srv.clone()),
            Some(sess.basis.srv.clone()),
            Some(sess.bases.srv.clone()),
        ]),
    );
    let uavs_c = [Some(sess.full_out.uav.clone())];
    ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_c.as_ptr()), None);
    ctx.Dispatch(groups, 1, 1);
    ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
    ctx.CSSetShaderResources(0, Some(&null_srv3));
    // cbuffer 布局不同 → 显式解绑（跨 pass 残留坑）
    ctx.CSSetConstantBuffers(0, Some(&[None]));
    ctx.CSSetShader(None, None);
    Ok(())
}

/// 旧 packed 路径（debug=1 对照/回退保留）：pass A/B + pass C packed → sess.out（packed）
#[allow(dead_code)]
unsafe fn run_packed_passes(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    nblocks: usize,
    coeffs_packed: &[i32],
    infos: &[BlockInfo],
) -> Result<(), String> {
    sess.ensure_capacity(device, nblocks as u32, (nblocks * 192) as u32)?;
    sess.ensure_dm(device, ctx, &s.dequant_matrices_dct8)?;
    unsafe { run_dequant_row_passes(ctx, sess, s, nblocks, coeffs_packed, infos)? };

    let groups = ((nblocks * 192) as u32).div_ceil(128);
    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv1: [Option<ID3D11ShaderResourceView>; 1] = [None];

    // --- pass C：1D 列变换（packed 输出）---
    ctx.CSSetShader(&sess.idct_c_cs, None);
    ctx.CSSetShaderResources(
        0,
        Some(&[Some(sess.mid.srv.clone()), Some(sess.basis.srv.clone())]),
    );
    let uavs_c = [Some(sess.out.uav.clone())];
    ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs_c.as_ptr()), None);
    ctx.Dispatch(groups, 1, 1);
    ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
    ctx.CSSetShaderResources(0, Some(&null_srv1));
    ctx.CSSetConstantBuffers(0, Some(&[None]));
    ctx.CSSetShader(None, None);
    Ok(())
}

/// 核心管线：默认全帧驻留（pass C 直写 full_out，无 CPU scatter）；
/// debug bit0=1 走旧 packed 路径。read_full=true 读回 CPU full（签名兼容，
/// gpu_reconstruct/_dbg/_raw 均需返回 Vec）；verify=true 附加 golden 对拍。
unsafe fn gpu_reconstruct_core(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    debug: u32,
    read_full: bool,
    verify: bool,
) -> Result<(Option<Vec<f32>>, super::hybrid::GoldenStats, CompareResult, Vec<usize>), String> {
    let (device, ctx) = eng.device_ctx();
    let prof_sync = super::prof_sync();

    // --- 会话：shader/缓冲跨帧复用（容量不足才重建；DM/bases 按内容 tag 跳过重传） ---
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };

    let ng = s.num_groups as usize;
    let gc = s.group_dim as usize * s.group_dim as usize;
    let full_elems = 3 * ng * gc;

    let full: Option<Vec<f32>>;
    let coeff_bases: Vec<usize>;
    if debug & 1 != 0 {
        // ===== 旧 packed 路径（对照/回退）：读回 + CPU scatter =====
        let (infos, coeffs_packed, bases) = prep_frame_packed(s)?;
        let nblocks = infos.len();
        if nblocks == 0 {
            return Err("无 DCT8 块".to_string());
        }
        coeff_bases = bases;
        unsafe { run_packed_passes(device, ctx, sess, s, nblocks, &coeffs_packed, &infos)? };
        if prof_sync { prof_fence(ctx, sess)?; }
        unsafe {
            // 读回（staging 复用；与 out 同容量创建 → CopyResource 恒合法）
            ctx.CopyResource(&sess.staging, &sess.out.buf);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&sess.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map(staging): {e}"))?;
            let gpu_out: Vec<f32> =
                std::slice::from_raw_parts(mapped.pData as *const f32, nblocks * 192).to_vec();
            ctx.Unmap(&sess.staging, 0);

            // scatter 回全帧布局（尾转置：packed 块内 k=x*8+y → 全帧 (k%8)*8 + k/8）
            let mut f = vec![0.0f32; full_elems];
            for (i, src_base) in coeff_bases.iter().enumerate() {
                for c in 0..3 {
                    let from = i * 192 + c * 64;
                    let to = c * ng * gc + src_base;
                    for y in 0..8 {
                        for x in 0..8 {
                            f[to + y * 8 + x] = gpu_out[from + x * 8 + y];
                        }
                    }
                }
            }
            full = Some(f);
        }
    } else {
        // ===== 全帧驻留路径：full_out = idct_ref 底图 ∪ pass C 直写 DCT8 =====
        let (infos, bases_u32) = prep_frame(s)?;
        let nblocks = infos.len();
        if nblocks == 0 {
            return Err("无 DCT8 块".to_string());
        }
        coeff_bases = bases_u32.iter().map(|&b| b as usize).collect();
        // read_full/verify 路径：保留 idct_ref 底图上传（对拍含填充槽位语义）
        unsafe { run_full_passes(device, ctx, sess, s, nblocks, &infos, &bases_u32, true)? };
        if prof_sync { prof_fence(ctx, sess)?; }
        if read_full {
            unsafe {
                ctx.CopyResource(&sess.full_staging, &sess.full_out.buf);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&sess.full_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|e| format!("Map(full_staging): {e}"))?;
                let f = std::slice::from_raw_parts(mapped.pData as *const f32, full_elems).to_vec();
                ctx.Unmap(&sess.full_staging, 0);
                full = Some(f);
            }
        } else {
            full = None;
        }
    }

    eng.flush();

    // --- 对拍参照：B4 混合模式下 libjxl 跳过 DCT8（idct_ref 的 DCT8 位置为 0），
    // DCT8 位置的参照改用 CPU 黄金重建（从系数独立计算），非 DCT8 用 libjxl block_ref。
    // golden 输出为全帧布局（c*NG*GC + src_base）——与 expected 同布局直拷。
    // verify=false 时跳过（生产路径免 CPU golden 开销，cmp 全零占位）。
    let cmp = if verify {
        let full = full.as_ref().ok_or("verify 需要 full 读回")?;
        let mut e = s.idct_ref.clone();
        let (golden, _) = crate::viewer::decode::hybrid::golden_reconstruct(s)?;
        for src_base in coeff_bases.iter() {
            for c in 0..3 {
                let to = c * ng * gc + src_base;
                e[to..to + 64].copy_from_slice(&golden[to..to + 64]);
            }
        }
        compare_against_ref(full, &e)?
    } else {
        cmp_none()
    };
    Ok((full, super::hybrid::GoldenStats::default(), cmp, coeff_bases))
}

/// B3：混合重建集成入口——GPU 处理全部 DCT8 块（83% 像素），非 DCT8/varblock
/// 块复用 libjxl 已算好的块层（B6 起 = full_out 的 idct_ref 底图，熵解码顺带
/// 产出、零额外成本）。输出 = 完整帧块层（3×num_groups×group_dim²，XYB 域），
/// 可直接接 XYB→RGB + EPF 后续管线。
/// verify=true 时用 CPU golden 交叉对拍（B4 混合模式下 libjxl 不产 DCT8 块层）。
pub fn hybrid_reconstruct(
    s: &CoeffSnapshot,
    verify: bool,
) -> Result<(Vec<f32>, CompareResult), String> {
    // B6 全帧驻留：raw 返回的 full = idct_ref 底图（非 DCT8 槽位已是 libjxl 块层）
    // ∪ pass C 直写 DCT8 —— CPU scatter 与非 DCT8 回填均已消除
    let (full, coeff_bases) = gpu_reconstruct_raw(s)?;
    let ng = s.num_groups as usize;
    let group_coeffs = s.group_dim as usize * s.group_dim as usize;

    // 对拍：B4 混合模式下 libjxl 不产 DCT8 块层——基准 = golden(DCT8) ∪ block_ref(其余)
    let cmp = if verify {
        let (golden, _) = crate::viewer::decode::hybrid::golden_reconstruct(s)?;
        let mut expected = s.idct_ref.clone();
        // golden 输出为全帧布局（c*NG*GC + src_base），与 packed 无关
        for src_base in coeff_bases.iter() {
            for c in 0..3 {
                let from = c * ng * group_coeffs + src_base;
                expected[from..from + 64].copy_from_slice(&golden[from..from + 64]);
            }
        }
        compare_against_ref(&full, &expected).map(|mut c| {
            let mut found = 0;
            for (i, (f, e)) in full.iter().zip(expected.iter()).enumerate() {
                let d = (f - e).abs();
                if d > 1e-3 && found < 3 {
                    let blk = i / 64;
                    let g = blk / (s.group_dim as usize * s.group_dim as usize / 64);
                    println!(
                        "verify差异[{}]: elem={} blk组内#{} gpu={:.5} exp={:.5}",
                        found,
                        i,
                        blk % (s.group_dim as usize * s.group_dim as usize / 64),
                        f,
                        e
                    );
                    let _ = g;
                    found += 1;
                    if found >= 3 {
                        break;
                    }
                }
            }
            c
        })?
    } else {
        cmp_none()
    };
    Ok((full, cmp))
}

/// B5b：GPU XYB→线性 RGB（pass D，dec_xyb-inl.h XybToRgb 同数学）。
/// xyb = 块层（3 平面 x,y,b 各 n 元素）；输出 = 线性 RGB 同布局（0-255 标度）。
pub fn gpu_xyb_to_linear(s: &CoeffSnapshot, xyb: &[f32]) -> Result<Vec<f32>, String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { gpu_xyb_to_linear_impl(&eng, s, xyb) }
}

/// 写 XYB pass 常量（opsin biases + 逆 opsin 矩阵，SIMD 布局抽 3×3）
unsafe fn write_xyb_cbuf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    n: usize,
) -> Result<(), String> {
    #[repr(C)]
    struct XYBParams {
        biases: [f32; 4],
        cbrt: [f32; 4],
        m0: [f32; 4],
        m1: [f32; 4],
        m2: [f32; 4],
        npix: u32,
        pad: [u32; 3],
    }
    let mut m = [[0.0f32; 3]; 3];
    for j in 0..3 {
        for i in 0..3 {
            m[j][i] = s.inverse_opsin_matrix[(j * 3 + i) * 4];
        }
    }
    let params = XYBParams {
        biases: [s.opsin_biases[0], s.opsin_biases[1], s.opsin_biases[2], 0.0],
        cbrt: [
            s.opsin_biases_cbrt[0],
            s.opsin_biases_cbrt[1],
            s.opsin_biases_cbrt[2],
            0.0,
        ],
        m0: [m[0][0], m[0][1], m[0][2], 0.0],
        m1: [m[1][0], m[1][1], m[1][2], 0.0],
        m2: [m[2][0], m[2][1], m[2][2], 0.0],
        npix: n as u32,
        pad: [0; 3],
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.xyb_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(xyb_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const XYBParams as *const u8,
        mapped.pData as *mut u8,
        96,
    );
    ctx.Unmap(&sess.xyb_cbuf, 0);
    Ok(())
}

/// pass D 派发（不读回）。in_srv = XYB 块层来源（xyb_in 上传版 / full_out 驻留版）。
/// 输出驻留 sess.xyb_out（管线链可由 pass O 直读，免中间 44.8MB 读回+上传）。
unsafe fn run_xyb_pass_dispatch(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    in_srv: &ID3D11ShaderResourceView,
) -> Result<(), String> {
    sess.ensure_xyb(device, n as u32)?;
    unsafe { write_xyb_cbuf(ctx, sess, s, n)? };

    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
    unsafe {
        ctx.CSSetShader(&sess.xyb_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.xyb_cbuf.clone())]));
        ctx.CSSetShaderResources(0, Some(&[Some(in_srv.clone())]));
        let uavs = [Some(sess.xyb_out.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv));
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// 读回 xyb_out（staging 与源同容量创建 → CopyResource 恒合法；Map(READ) 隐式同步）
unsafe fn readback_xyb(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    n: usize,
) -> Result<Vec<f32>, String> {
    unsafe {
        ctx.CopyResource(&sess.xyb_staging, &sess.xyb_out.buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.xyb_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(xyb_staging): {e}"))?;
        let out: Vec<f32> =
            std::slice::from_raw_parts(mapped.pData as *const f32, 3 * n).to_vec();
        ctx.Unmap(&sess.xyb_staging, 0);
        Ok(out)
    }
}

/// pass D 派发 + 读回。in_srv = XYB 块层来源（xyb_in 上传版 / full_out 驻留版）。
unsafe fn run_xyb_pass_readback(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    in_srv: &ID3D11ShaderResourceView,
) -> Result<Vec<f32>, String> {
    unsafe { run_xyb_pass_dispatch(device, ctx, sess, s, n, in_srv)? };
    unsafe { readback_xyb(ctx, sess, n) }
}

unsafe fn gpu_xyb_to_linear_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    xyb: &[f32],
) -> Result<Vec<f32>, String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    if xyb.len() != 3 * n {
        return Err(format!("XYB 层尺寸异常: {} ≠ 3×{}", xyb.len(), n));
    }
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    // 独立入口：XYB 块层来自 CPU → 上传（管线内路径绑 full_out 直读，免此 44.8MB）。
    // 注意 ensure_xyb 必须先于上传（重建缓冲后旧 SRV/上传内容即失效）
    unsafe {
        sess.ensure_xyb(device, n as u32)?;
        ctx.UpdateSubresource(
            &sess.xyb_in.buf,
            0,
            None,
            xyb.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        let in_srv = sess.xyb_in.srv.clone();
        run_xyb_pass_readback(device, ctx, sess, s, n, &in_srv)
    }
}

// ==================== B6c：Gaborish（pass G） ====================

/// 每通道归一化 Gaborish 权重 [w0, w1, w2]（stage_gaborish.cc Normalize 同式：
/// div = 1 + 4·(w1+w2)，全部乘 1/div）
fn gab_weights(w1: f32, w2: f32) -> [f32; 4] {
    let div = 1.0 + 4.0 * (w1 + w2);
    [1.0 / div, w1 / div, w2 / div, 0.0]
}

/// 写 pass G 常量（GabParams 80B；xsize/ysize = 真实图像尺寸 = 镜像边界）
unsafe fn write_gab_cbuf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    n: usize,
) -> Result<(), String> {
    #[repr(C)]
    struct GabParams {
        n: u32,
        gd: u32,
        xg: u32,
        xsize: u32,
        ysize: u32,
        pad: [u32; 3],
        wx: [f32; 4],
        wy: [f32; 4],
        wb: [f32; 4],
    }
    let params = GabParams {
        n: n as u32,
        gd: s.group_dim,
        xg: s.xsize_groups,
        xsize: s.width,
        ysize: s.height,
        pad: [0; 3],
        // gab=false → 恒等权重（v = 中心采样）：pass G 仍运行，兼作块主序→
        // 行主序的布局转换（块层为块主序，下游 EPF/XYB/pass O 按行主序消费）
        wx: if s.gab { gab_weights(s.gab_xweight1, s.gab_xweight2) } else { [1.0, 0.0, 0.0, 0.0] },
        wy: if s.gab { gab_weights(s.gab_yweight1, s.gab_yweight2) } else { [1.0, 0.0, 0.0, 0.0] },
        // b 通道权重未随快照导出（JxlCoefficientExportInfo 无 gab_b 字段）；
        // libjxl 编码器从不设 gab_custom（三通道恒用同组默认权重），复用 y 对
        wb: if s.gab { gab_weights(s.gab_yweight1, s.gab_yweight2) } else { [1.0, 0.0, 0.0, 0.0] },
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.gab_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(gab_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const GabParams as *const u8,
        mapped.pData as *mut u8,
        80,
    );
    ctx.Unmap(&sess.gab_cbuf, 0);
    Ok(())
}

/// pass G 派发：in_srv（全帧块层 SRV，[c][ng][pix]）→ sess.gab_out。
/// gab=false 时由调用方直接跳过 dispatch（libjxl 同语义）。
unsafe fn run_gaborish_pass(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    in_srv: &ID3D11ShaderResourceView,
) -> Result<(), String> {
    sess.ensure_gab(device, n as u32)?;
    unsafe { write_gab_cbuf(ctx, sess, s, n)? };

    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
    unsafe {
        ctx.CSSetShader(&sess.gab_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.gab_cbuf.clone())]));
        ctx.CSSetShaderResources(0, Some(&[Some(in_srv.clone())]));
        let uavs = [Some(sess.gab_out.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv));
        // cbuffer 布局独立 → 显式解绑（跨 pass 残留坑）
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// 读回 gab_out（staging 与源同容量创建 → CopyResource 恒合法；
/// Map(READ) 隐式同步）
unsafe fn readback_gab(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    n: usize,
) -> Result<Vec<f32>, String> {
    unsafe {
        ctx.CopyResource(&sess.gab_staging, &sess.gab_out.buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.gab_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(gab_staging): {e}"))?;
        let out: Vec<f32> =
            std::slice::from_raw_parts(mapped.pData as *const f32, 3 * n).to_vec();
        ctx.Unmap(&sess.gab_staging, 0);
        Ok(out)
    }
}

/// B6c：GPU Gaborish（pass G，独立验证入口：上传 → dispatch → 读回）。
/// xyb = 全帧块层（[c][num_groups][group_dim²]，XYB 域）；输出同布局。
/// gab=false 时直接返回输入克隆（libjxl 无 Gaborish stage）。
pub fn gpu_gaborish(s: &CoeffSnapshot, xyb: &[f32]) -> Result<Vec<f32>, String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { gpu_gaborish_impl(&eng, s, xyb) }
}

unsafe fn gpu_gaborish_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    xyb: &[f32],
) -> Result<Vec<f32>, String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    if xyb.len() != 3 * n {
        return Err(format!("XYB 层尺寸异常: {} ≠ 3×{}", xyb.len(), 3 * n));
    }
    if !s.gab {
        return Ok(xyb.to_vec());
    }
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    unsafe {
        // 独立入口：块层来自 CPU → 复用 xyb_in 上传（容量同 3n；
        // ensure 必须先于上传，重建缓冲后旧内容失效）
        sess.ensure_xyb(device, n as u32)?;
        ctx.UpdateSubresource(
            &sess.xyb_in.buf,
            0,
            None,
            xyb.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        let in_srv = sess.xyb_in.srv.clone();
        run_gaborish_pass(device, ctx, sess, s, n, &in_srv)?;
        readback_gab(ctx, sess, n)
    }
}

/// B6c：管线链用 no-upload 变体——full_out 已驻留当前帧块层（run_full_passes
/// 之后）→ pass G（full_out → gab_out）→ 读回最终输出。gab=false 时无 stage，
/// 直接读回 full_out（即结果）。
pub fn gpu_gaborish_from_full(s: &CoeffSnapshot) -> Result<Vec<f32>, String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe {
        let (device, ctx) = eng.device_ctx();
        let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
        let mut sess_guard = SESSION
            .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
            .lock()
            .map_err(|e| format!("hybrid 会话锁: {e}"))?;
        let sess = match sess_guard.as_mut() {
            Ok(x) => x,
            Err(e) => return Err(e.clone()),
        };
        if !s.gab {
            // 无 Gaborish stage：full_out 即结果（读回全帧缓冲）
            ctx.CopyResource(&sess.full_staging, &sess.full_out.buf);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&sess.full_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map(full_staging): {e}"))?;
            let out: Vec<f32> =
                std::slice::from_raw_parts(mapped.pData as *const f32, 3 * n).to_vec();
            ctx.Unmap(&sess.full_staging, 0);
            return Ok(out);
        }
        let in_srv = sess.full_out.srv.clone();
        run_gaborish_pass(device, ctx, sess, s, n, &in_srv)?;
        readback_gab(ctx, sess, n)
    }
}

/// ==================== B6d：EPF1（pass E） ====================

/// 写 pass E 常量（EpfParams 64B）。vsm = 1.65（EPF1 固定，非 pass0_sigma_scale）；
/// bsm = 1.65 × epf_border_sad_mul；kMinSigma 同 epf.h。
unsafe fn write_epf_cbuf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    n: usize,
) -> Result<(), String> {
    #[repr(C)]
    struct EpfParams {
        n: u32,
        gd: u32,
        xg: u32,
        xsize: u32,
        ysize: u32,
        sbx: u32,
        pad0: u32,
        pad1: u32,
        cs: [f32; 4],
        bsm: f32,
        min_sig: f32,
        pad2: [f32; 2],
    }
    let params = EpfParams {
        n: n as u32,
        gd: s.group_dim,
        xg: s.xsize_groups,
        xsize: s.width,
        ysize: s.height,
        sbx: s.sigma_xsize,
        pad0: 0,
        pad1: 0,
        cs: [
            s.epf_channel_scale[0],
            s.epf_channel_scale[1],
            s.epf_channel_scale[2],
            0.0,
        ],
        bsm: 1.65 * s.epf_border_sad_mul,
        min_sig: super::hybrid::K_MIN_SIGMA,
        pad2: [0.0; 2],
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.epf_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(epf_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const EpfParams as *const u8,
        mapped.pData as *mut u8,
        64,
    );
    ctx.Unmap(&sess.epf_cbuf, 0);
    Ok(())
}

/// pass E 派发：in_srv（全帧块层 SRV）→ sess.epf_out。
/// sigma 图每帧无条件重传（libjxl 每帧 DecodeAcMetadata 重算 sigma，动画各帧
/// 不同 → 不可 tag 跳过；~261KB @ 2K 帧，开销可忽略）。
/// 调用方保证 epf_iters≥1 且 sigma 非空（否则 libjxl 无 EPF stage，不应 dispatch）。
unsafe fn run_epf1_pass(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    in_srv: &ID3D11ShaderResourceView,
) -> Result<(), String> {
    sess.ensure_epf(device, n as u32)?;
    sess.ensure_sigma(device, s.sigma.len() as u32)?;
    unsafe {
        ctx.UpdateSubresource(
            &sess.sigma.buf,
            0,
            None,
            s.sigma.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        write_epf_cbuf(ctx, sess, s, n)?;

        let null_uav: Option<ID3D11UnorderedAccessView> = None;
        let null_srv2: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
        ctx.CSSetShader(&sess.epf_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.epf_cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[Some(in_srv.clone()), Some(sess.sigma.srv.clone())]),
        );
        let uavs = [Some(sess.epf_out.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv2));
        // cbuffer 布局独立 → 显式解绑（跨 pass 残留坑）
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// 读回 epf_out（staging 与源同容量创建 → CopyResource 恒合法；Map(READ) 隐式同步）
unsafe fn readback_epf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    n: usize,
) -> Result<Vec<f32>, String> {
    unsafe {
        ctx.CopyResource(&sess.epf_staging, &sess.epf_out.buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.epf_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(epf_staging): {e}"))?;
        let out: Vec<f32> =
            std::slice::from_raw_parts(mapped.pData as *const f32, 3 * n).to_vec();
        ctx.Unmap(&sess.epf_staging, 0);
        Ok(out)
    }
}

/// B6d：GPU EPF1（pass E，独立验证入口：上传 → dispatch → 读回）。
/// xyb = 全帧块层（[c][num_groups][group_dim²]，XYB 域，= Gaborish 输出）；
/// 输出同布局。epf_iters=0 或 sigma 空 → 无 EPF stage，返回输入克隆。
pub fn gpu_epf1(s: &CoeffSnapshot, xyb: &[f32]) -> Result<Vec<f32>, String> {
    if s.epf_iters == 0 || s.sigma.is_empty() {
        return Ok(xyb.to_vec());
    }
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { gpu_epf1_impl(&eng, s, xyb) }
}

unsafe fn gpu_epf1_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    xyb: &[f32],
) -> Result<Vec<f32>, String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    if xyb.len() != 3 * n {
        return Err(format!("XYB 层尺寸异常: {} ≠ 3×{}", xyb.len(), 3 * n));
    }
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    unsafe {
        // 独立入口：块层来自 CPU → 复用 xyb_in 上传（容量同 3n；
        // ensure 必须先于上传，重建缓冲后旧内容失效）
        sess.ensure_xyb(device, n as u32)?;
        ctx.UpdateSubresource(
            &sess.xyb_in.buf,
            0,
            None,
            xyb.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        let in_srv = sess.xyb_in.srv.clone();
        run_epf1_pass(device, ctx, sess, s, n, &in_srv)?;
        readback_epf(ctx, sess, n)
    }
}


/// B5b/B6：混合重建完整前置管线——块层（GPU DCT8 + libjxl 非 DCT8 复用）
/// → [pass G Gaborish（gab=true；libjxl 序：IDCT → Gab → EPF → XYB）]
/// → [pass E EPF1（epf_iters≥1 且 sigma 有效；B6d 接入）]
/// → XYB→线性 RGB，**全 GPU 链**：块层驻留 full_out，pass G ping-pong 到
/// gab_out，pass E ping-pong 到 epf_out，pass D 直读 GPU 全帧，仅线性 RGB
/// 一次读回。
/// 输出 = 线性 RGB（sRGB 原色域，0-255 标度，255 = intensity_target）。
pub fn hybrid_reconstruct_linear(s: &CoeffSnapshot) -> Result<(Vec<f32>, CompareResult), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { hybrid_reconstruct_linear_impl(&eng, s) }
}

unsafe fn hybrid_reconstruct_linear_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
) -> Result<(Vec<f32>, CompareResult), String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;

    let (infos, bases_u32) = prep_frame(s)?;
    if infos.is_empty() {
        return Err("无 DCT8 块".to_string());
    }
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };

    unsafe {
        // pass A2/B/C-full → full_out 驻留（无读回、无 CPU scatter；生产路径免
        // idct_ref 底图上传）
        run_full_passes(device, ctx, sess, s, infos.len(), &infos, &bases_u32, false)?;
        // pass G：Gaborish（IDCT 后）——**恒运行**：gab=false 时以恒等权重
        // dispatch，兼作块主序→行主序的布局转换（块层为块主序，块主序写 /
        // 行主序读的错位由 B8 端到端对拍发现，见 gaborish_ref 同步修复）
        let full_srv = sess.full_out.srv.clone();
        run_gaborish_pass(device, ctx, sess, s, n, &full_srv)?;
        let mut in_srv = sess.gab_out.srv.clone();
        // pass E：EPF1（Gaborish 后 / XYB 前；epf_iters≥1 且 sigma 有效才 dispatch，
        // 输出 epf_out；epf_iters=0 时 pass D 输入是 gab_out（行主序块层））
        if s.epf_iters >= 1 && !s.sigma.is_empty() {
            run_epf1_pass(device, ctx, sess, s, n, &in_srv)?;
            in_srv = sess.epf_out.srv.clone();
        }
        // pass D：直接消费 GPU 全帧块层（免 44.8MB 上传）
        let linear = run_xyb_pass_readback(device, ctx, sess, s, n, &in_srv)?;
        eng.flush();
        Ok((linear, cmp_none()))
    }
}

/// ==================== B7：PQ16 BT.2020 输出（pass O） ====================

/// 写 pass O 常量（PqParams 80B）。it/色域矩阵来自 CPU 反解
/// （[`super::hybrid::pq16_out_params`]，GPU/CPU 共用同一数值保证 LSB 级对拍）。
unsafe fn write_pq_cbuf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    it: f32,
    gamut: &crate::color::Mat3,
) -> Result<(), String> {
    #[repr(C)]
    struct PqParams {
        npix: u32,
        gd: u32,
        xg: u32,
        xsize: u32,
        ysize: u32,
        it_nits: f32,
        pad: [u32; 2],
        m0: [f32; 4],
        m1: [f32; 4],
        m2: [f32; 4],
    }
    let params = PqParams {
        npix: n as u32,
        gd: s.group_dim,
        xg: s.xsize_groups,
        xsize: s.width,
        ysize: s.height,
        it_nits: it,
        pad: [0; 2],
        m0: [gamut.m[0][0], gamut.m[0][1], gamut.m[0][2], 0.0],
        m1: [gamut.m[1][0], gamut.m[1][1], gamut.m[1][2], 0.0],
        m2: [gamut.m[2][0], gamut.m[2][1], gamut.m[2][2], 0.0],
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.pq_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(pq_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const PqParams as *const u8,
        mapped.pData as *mut u8,
        80,
    );
    ctx.Unmap(&sess.pq_cbuf, 0);
    Ok(())
}

/// pass O 派发：in_srv（linear 三平面 SRV，[c][n]）→ sess.pq_out（3n uint 槽位）。
unsafe fn run_pq16_pass(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    in_srv: &ID3D11ShaderResourceView,
    it: f32,
    gamut: &crate::color::Mat3,
) -> Result<(), String> {
    sess.ensure_pq(device, n as u32)?;
    unsafe { write_pq_cbuf(ctx, sess, s, n, it, gamut)? };

    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
    unsafe {
        ctx.CSSetShader(&sess.pq_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.pq_cbuf.clone())]));
        ctx.CSSetShaderResources(0, Some(&[Some(in_srv.clone())]));
        let uavs = [Some(sess.pq_out.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv));
        // cbuffer 布局独立 → 显式解绑（跨 pass 残留坑）
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// 读回 pq_out（uint 槽位 → u16；shader 已 min 饱和 ≤65535，as u16 精确）
unsafe fn readback_pq16(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    n: usize,
) -> Result<Vec<u16>, String> {
    unsafe {
        ctx.CopyResource(&sess.pq_staging, &sess.pq_out.buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.pq_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(pq_staging): {e}"))?;
        let out: Vec<u16> = std::slice::from_raw_parts(mapped.pData as *const u32, 3 * n)
            .iter()
            .map(|&v| v as u16)
            .collect();
        ctx.Unmap(&sess.pq_staging, 0);
        Ok(out)
    }
}

/// B7：GPU PQ16 BT.2020 输出（pass O，独立验证/调用入口：上传 linear → dispatch
/// → 读回 u16）。linear = 线性 RGB 三平面（[c][n]，pass D / hybrid_reconstruct_linear
/// 输出，1.0 = intensity_target nits）；输出 = 同布局 u16（PQ BT.2020，与
/// [`super::hybrid::pq16_out_ref`] 同语义同数学）。
pub fn gpu_pq16_out(s: &CoeffSnapshot, linear: &[f32]) -> Result<Vec<u16>, String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { gpu_pq16_out_impl(&eng, s, linear) }
}

unsafe fn gpu_pq16_out_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    linear: &[f32],
) -> Result<Vec<u16>, String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    if linear.len() != 3 * n {
        return Err(format!("linear 层尺寸异常: {} ≠ 3×{}", linear.len(), 3 * n));
    }
    let (it, gamut) = super::hybrid::pq16_out_params(s)?;
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    unsafe {
        // 独立入口：linear 来自 CPU → 复用 xyb_in 上传（容量同 3n；
        // ensure 必须先于上传，重建缓冲后旧内容失效）
        sess.ensure_xyb(device, n as u32)?;
        ctx.UpdateSubresource(
            &sess.xyb_in.buf,
            0,
            None,
            linear.as_ptr() as *const core::ffi::c_void,
            0,
            0,
        );
        let in_srv = sess.xyb_in.srv.clone();
        run_pq16_pass(device, ctx, sess, s, n, &in_srv, it, &gamut)?;
        readback_pq16(ctx, sess, n)
    }
}

/// B7：混合管线完整出图——块层 GPU 驻留 → [pass G Gaborish] → [pass E EPF1]
/// → pass D XYB→线性 RGB（GPU 驻留，无中间读回）→ pass O PQ16 u16 读回。
/// **混合后端出可显示像素的最后一块拼图**：全链仅一次 u16 读回（3n×2 字节）。
/// 返回 (u16 PQ BT.2020 三平面 [c][n]，反解 intensity_target nits)。
pub fn hybrid_reconstruct_pq16(s: &CoeffSnapshot) -> Result<(Vec<u16>, f32), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { hybrid_reconstruct_pq16_impl(&eng, s) }
}

unsafe fn hybrid_reconstruct_pq16_impl(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
) -> Result<(Vec<u16>, f32), String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    let prof = super::prof_enabled();
    let prof_sync = super::prof_sync();

    let mut t = std::time::Instant::now();
    let (infos, bases_u32) = prep_frame(s)?;
    if infos.is_empty() {
        return Err("无 DCT8 块".to_string());
    }
    if prof {
        println!("[prof] pq16 准备(prep): {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    t = std::time::Instant::now();
    let (it, gamut) = super::hybrid::pq16_out_params(s)?;
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    if prof {
        println!("[prof] pq16 参数反解+会话锁: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    unsafe {
        let mut t = std::time::Instant::now();
        // pass A2/B/C-full → full_out 驻留（同 hybrid_reconstruct_linear_impl）
        run_full_passes(device, ctx, sess, s, infos.len(), &infos, &bases_u32, false)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_abc = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        // pass G：Gaborish（IDCT 后）——**恒运行**：gab=false 时以恒等权重
        // dispatch，兼作块主序→行主序的布局转换（同 hybrid_reconstruct_linear，
        // B8 端到端对拍发现块主序写 / 行主序读错位，与 gaborish_ref 同步修复）
        let full_srv = sess.full_out.srv.clone();
        run_gaborish_pass(device, ctx, sess, s, n, &full_srv)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_g = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        let mut in_srv = sess.gab_out.srv.clone();
        let t_e = if s.epf_iters >= 1 && !s.sigma.is_empty() {
            run_epf1_pass(device, ctx, sess, s, n, &in_srv)?;
            if prof_sync { prof_fence(ctx, sess)?; }
            let e = t.elapsed().as_secs_f64() * 1000.0;
            t = std::time::Instant::now();
            in_srv = sess.epf_out.srv.clone();
            e
        } else {
            0.0
        };
        // pass D：linear 驻留 xyb_out（不读回）
        run_xyb_pass_dispatch(device, ctx, sess, s, n, &in_srv)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_d = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        // pass O：直读 xyb_out → pq_out（u16）→ 读回
        let xyb_out_srv = sess.xyb_out.srv.clone();
        run_pq16_pass(device, ctx, sess, s, n, &xyb_out_srv, it, &gamut)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_o = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        let pq = readback_pq16(ctx, sess, n)?;
        eng.flush();
        let t_rb = t.elapsed().as_secs_f64() * 1000.0;
        if prof {
            println!(
                "[prof] pq16 GPU 链: 上传+ABC {:.1}ms | G {:.1}ms | E {:.1}ms | D {:.1}ms | O {:.1}ms | 读回(u32→u16) {:.1}ms",
                t_abc, t_g, t_e, t_d, t_o, t_rb
            );
        }
        Ok((pq, it))
    }
}

// ==================== B9：播放出口 GPU 化（pass F：f16 行交错直出） ====================

/// 写 pass F 常量（F16Params 80B）。gamut = 2020→709 矩阵（解码器 ColorEncoding
/// 捕获，与 CPU 出口转换 pq16_interleaved_to_scrgb 同一矩阵）。
unsafe fn write_f16_cbuf(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    gamut: &crate::color::Mat3,
) -> Result<(), String> {
    #[repr(C)]
    struct F16Params {
        npix: u32,
        gd: u32,
        xg: u32,
        xsize: u32,
        ysize: u32,
        pad: [u32; 3],
        m0: [f32; 4],
        m1: [f32; 4],
        m2: [f32; 4],
    }
    let params = F16Params {
        npix: n as u32,
        gd: s.group_dim,
        xg: s.xsize_groups,
        xsize: s.width,
        ysize: s.height,
        pad: [0; 3],
        m0: [gamut.m[0][0], gamut.m[0][1], gamut.m[0][2], 0.0],
        m1: [gamut.m[1][0], gamut.m[1][1], gamut.m[1][2], 0.0],
        m2: [gamut.m[2][0], gamut.m[2][1], gamut.m[2][2], 0.0],
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&sess.f16_cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map(f16_cbuf): {e}"))?;
    std::ptr::copy_nonoverlapping(
        &params as *const F16Params as *const u8,
        mapped.pData as *mut u8,
        80,
    );
    ctx.Unmap(&sess.f16_cbuf, 0);
    Ok(())
}

/// pass F 派发：pq_out（pass O 输出，GPU 驻留）→ f16_out（w×h uint2 行交错）。
unsafe fn run_f16_pass(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    gamut: &crate::color::Mat3,
) -> Result<(), String> {
    let out_px = s.width as usize * s.height as usize;
    sess.ensure_f16(device, ctx, out_px as u32)?;
    unsafe { write_f16_cbuf(ctx, sess, s, n, gamut)? };

    let null_uav: Option<ID3D11UnorderedAccessView> = None;
    let null_srv2: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
    unsafe {
        ctx.CSSetShader(&sess.f16_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.f16_cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[Some(sess.pq_out.srv.clone()), Some(sess.f16_lut.srv.clone())]),
        );
        let uavs = [Some(sess.f16_out.uav.clone())];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv2));
        // cbuffer 布局独立 → 显式解绑（跨 pass 残留坑）
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// pass F 直写纹理派发：pq_out（pass O 输出，GPU 驻留）→ dst_tex（RGBA16F
/// UAV，调用方创建；UAV 每次调用现建——纹理由调用方持有，会话不缓存外部资源）。
/// 只触碰共用 LUT（ensure_f16_lut），不建 f16_out/f16_staging。绑定无 hazard：
/// pq_out 只绑 SRV，dst_tex 只绑 UAV（不同资源）。f16 位型与 buffer 版逐位一致，
/// 见 F16_TEX_TAIL 注释。
unsafe fn run_f16_pass_into(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    sess: &mut HybridSession,
    s: &CoeffSnapshot,
    n: usize,
    gamut: &crate::color::Mat3,
    dst_tex: &ID3D11Texture2D,
) -> Result<(), String> {
    sess.ensure_f16_lut(device, ctx)?;
    unsafe { write_f16_cbuf(ctx, sess, s, n, gamut)? };

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
        ctx.CSSetShader(&sess.f16_tex_cs, None);
        ctx.CSSetConstantBuffers(0, Some(&[Some(sess.f16_cbuf.clone())]));
        ctx.CSSetShaderResources(
            0,
            Some(&[Some(sess.pq_out.srv.clone()), Some(sess.f16_lut.srv.clone())]),
        );
        let uavs = [Some(dst_uav)];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch((n as u32).div_ceil(128), 1, 1);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&null_uav as *const _), None);
        ctx.CSSetShaderResources(0, Some(&null_srv2));
        // cbuffer 布局独立 → 显式解绑（跨 pass 残留坑）
        ctx.CSSetConstantBuffers(0, Some(&[None]));
        ctx.CSSetShader(None, None);
    }
    Ok(())
}

/// 读回 f16_out（行交错 f16 RGBA → AnimFrame.data 直用；staging 与源同容量创建
/// → CopyResource 恒合法；Map(READ) 隐式同步。纯 memcpy，免 u32→u16 映射）
unsafe fn readback_f16(
    ctx: &ID3D11DeviceContext,
    sess: &HybridSession,
    out_px: usize,
) -> Result<Vec<u8>, String> {
    unsafe {
        ctx.CopyResource(&sess.f16_staging, &sess.f16_out.buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&sess.f16_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(f16_staging): {e}"))?;
        let out: Vec<u8> = std::slice::from_raw_parts(mapped.pData as *const u8, out_px * 8)
            .to_vec();
        ctx.Unmap(&sess.f16_staging, 0);
        Ok(out)
    }
}

/// B9：混合管线播放出口（生产路径）——与 [`hybrid_reconstruct_pq16`] 同链
///（块层 GPU 驻留 → [pass G] → [pass E] → pass D → pass O PQ16 u16 驻留），
/// 再加 **pass F**：直读 pq_out → LUT PQ 解码 + 2020→709 色域 + /80 → f16 RGBA
/// **行交错直写**，读回即 AnimFrame.data（w×h×8B）。CPU 侧重排与 PQ→scRGB
/// 转换（par_rows_mut ×2 + u32→u16 映射）全部消除。
/// gamut = 2020→709 色域矩阵（解码器捕获，与 CPU 出口转换同一矩阵）。
/// 返回 (f16 RGBA 行交错字节, 反解 intensity_target nits)。
pub fn hybrid_reconstruct_pq16_f16(
    s: &CoeffSnapshot,
    gamut: &crate::color::Mat3,
) -> Result<(Vec<u8>, f32), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    let (data, it) = unsafe { hybrid_reconstruct_pq16_f16_chain(&eng, s, gamut, None)? };
    Ok((data.ok_or("buffer 路径读回缺失")?, it))
}

/// B9 直写纹理变体（常驻显存播放/纹理池底层准备）：与 [`hybrid_reconstruct_pq16_f16`]
/// 同链（gather/prep/pass A/B/C′/G/E/D/O 逐 pass 一致），唯一差异 = pass F 输出
/// 改绑 `dst_tex`（RGBA16F，规格 = s.width×s.height，BindFlags =
/// UNORDERED_ACCESS|SHADER_RESOURCE，**调用方创建**）的 UAV 直写，**不读回**
/// ——调用方持纹理直接用（同设备后续 GPU 消费由 D3D11 命令序保证；会话容量
/// 复用/引擎锁语义与 buffer 版一致）。f16 位型与 buffer 版逐位一致（f32tof16
/// RTZ 截断 → f16tof32 回 float → RGBA16F 存储转换对 f16 可精确表示值恒等，
/// 见 F16_TEX_TAIL 注释）。
/// gamut = 2020→709 矩阵（解码器 ColorEncoding 捕获；[`super::hybrid::pq16_out_params`]
/// 返回的是 pass O 编码侧矩阵，语义不同不可替代，故保留入参）。
/// duration 由调用方持有（snapshot 流程自带计时）→ 返回 `Result<(), String>`。
pub fn hybrid_reconstruct_pq16_f16_into(
    s: &CoeffSnapshot,
    gamut: &crate::color::Mat3,
    dst_tex: &ID3D11Texture2D,
) -> Result<(), String> {
    let engine = engine().as_ref().map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let eng = engine.0.lock().map_err(|e| format!("GPU 锁: {e}"))?;
    unsafe { hybrid_reconstruct_pq16_f16_chain(&eng, s, gamut, Some(dst_tex))? };
    Ok(())
}

/// 链实现（buffer / 直写纹理两版共用）：dst_tex = None → pass F 写 f16_out +
/// staging 读回（生产出口）；Some(tex) → pass F 直写 tex 的 RGBA16F UAV，无读回。
/// 返回 (buffer 版读回数据（纹理版为 None）, 反解 intensity_target nits)。
unsafe fn hybrid_reconstruct_pq16_f16_chain(
    eng: &crate::upscale::d3d11::GpuEngine,
    s: &CoeffSnapshot,
    gamut: &crate::color::Mat3,
    dst_tex: Option<&ID3D11Texture2D>,
) -> Result<(Option<Vec<u8>>, f32), String> {
    let (device, ctx) = eng.device_ctx();
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    let prof = super::prof_enabled();
    let prof_sync = super::prof_sync();

    let mut t = std::time::Instant::now();
    let (infos, bases_u32) = prep_frame(s)?;
    if infos.is_empty() {
        return Err("无 DCT8 块".to_string());
    }
    if prof {
        println!("[prof] f16 准备(prep): {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    t = std::time::Instant::now();
    let (it, _gamut_o) = super::hybrid::pq16_out_params(s)?;
    let mut sess_guard = SESSION
        .get_or_init(|| std::sync::Mutex::new(HybridSession::new(device, ctx)))
        .lock()
        .map_err(|e| format!("hybrid 会话锁: {e}"))?;
    let sess = match sess_guard.as_mut() {
        Ok(x) => x,
        Err(e) => return Err(e.clone()),
    };
    if prof {
        println!("[prof] f16 参数反解+会话锁: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    unsafe {
        let mut t = std::time::Instant::now();
        run_full_passes(device, ctx, sess, s, infos.len(), &infos, &bases_u32, false)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_abc = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        let full_srv = sess.full_out.srv.clone();
        run_gaborish_pass(device, ctx, sess, s, n, &full_srv)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_g = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        let mut in_srv = sess.gab_out.srv.clone();
        let t_e = if s.epf_iters >= 1 && !s.sigma.is_empty() {
            run_epf1_pass(device, ctx, sess, s, n, &in_srv)?;
            if prof_sync { prof_fence(ctx, sess)?; }
            let e = t.elapsed().as_secs_f64() * 1000.0;
            t = std::time::Instant::now();
            in_srv = sess.epf_out.srv.clone();
            e
        } else {
            0.0
        };
        run_xyb_pass_dispatch(device, ctx, sess, s, n, &in_srv)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_d = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        let xyb_out_srv = sess.xyb_out.srv.clone();
        run_pq16_pass(device, ctx, sess, s, n, &xyb_out_srv, it, &_gamut_o)?;
        if prof_sync { prof_fence(ctx, sess)?; }
        let t_o = t.elapsed().as_secs_f64() * 1000.0;
        t = std::time::Instant::now();
        // pass F：pq_out（GPU 驻留）→ f16 行交错（buffer 版读回 = 播放出口唯一
        // 读回）或直写 dst_tex RGBA16F UAV（纹理版，无读回）
        let (t_f, tail_label, data) = match dst_tex {
            Some(tex) => {
                run_f16_pass_into(device, ctx, sess, s, n, gamut, tex)?;
                if prof_sync { prof_fence(ctx, sess)?; }
                let tf = t.elapsed().as_secs_f64() * 1000.0;
                t = std::time::Instant::now();
                (tf, "直写纹理", None)
            }
            None => {
                run_f16_pass(device, ctx, sess, s, n, gamut)?;
                if prof_sync { prof_fence(ctx, sess)?; }
                let tf = t.elapsed().as_secs_f64() * 1000.0;
                t = std::time::Instant::now();
                let out_px = s.width as usize * s.height as usize;
                let d = readback_f16(ctx, sess, out_px)?;
                eng.flush();
                (tf, "读回(f16)", Some(d))
            }
        };
        let t_rb = t.elapsed().as_secs_f64() * 1000.0;
        if prof {
            println!(
                "[prof] f16 GPU 链: 上传+ABC {:.1}ms | G {:.1}ms | E {:.1}ms | D {:.1}ms | O {:.1}ms | F {:.1}ms | {} {:.1}ms",
                t_abc, t_g, t_e, t_d, t_o, t_f, tail_label, t_rb
            );
        }
        Ok((data, it))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B5b：GPU XYB→线性 RGB（pass D）vs CPU 黄金参考（≥50dB）
    #[test]
    fn xyb_to_linear_gpu_matches_cpu() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        let (xyb, _) = hybrid_reconstruct(&snap, false).expect("混合重建失败");
        let t0 = std::time::Instant::now();
        let gpu = gpu_xyb_to_linear(&snap, &xyb).expect("GPU XYB pass 失败");
        let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let cpu = crate::viewer::decode::hybrid::xyb_to_linear_ref(&snap, &xyb);
        let cmp = compare_against_ref(&gpu, &cpu).expect("对拍失败");
        println!(
            "XYB→linear GPU vs CPU: PSNR = {:.2} dB, max_abs_diff = {:.3e}, 耗时 {:.1} ms（含 44.8MB 上传+读回）",
            cmp.psnr_db, cmp.max_abs_diff, gpu_ms
        );
        assert!(
            cmp.psnr_db >= 50.0,
            "XYB pass 对拍 PSNR {:.2} dB < 50 dB",
            cmp.psnr_db
        );
    }

    /// 路线 B 验证样本（2K VarDCT d=0.1，含 varblock 混合）
    const VAR_DCT_SAMPLE: &str =
        "C:/Users/Administrator/Pictures/jietu-hdr/jietu_20260827_025539.jxl";

    /// B9 直写纹理对拍：pass F 纹理版（RGBA16F UAV 直写 + staging **纹理**读回）
    /// vs buffer 版（structured buffer + 读回）——同一 CoeffSnapshot、同一 gamut，
    /// **逐字节（f16 位型）一致**。staging 纹理 Map 的 RowPitch ≠ w*8 → 按行裁剪
    /// 有效区后比对；失败打印首个差异 (px, py, 通道)。
    #[test]
    fn pq16_f16_into_matches_buffer() {
        use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        // pass F 色域矩阵：生产 = 解码器 ColorEncoding 捕获（2020→709）。两变体
        // 同输入 → 位型对比与矩阵具体取值无关，此处取标准 2020→709。
        let gamut = crate::color::bt2020_to_bt709();
        let (w, h) = (snap.width, snap.height);

        // 旧：buffer 版（行交错 f16 RGBA 字节流 w*h*8）。内部自带引擎锁。
        let t0 = std::time::Instant::now();
        let (buf, _) = hybrid_reconstruct_pq16_f16(&snap, &gamut).expect("buffer 版失败");
        let buf_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            buf.len(),
            w as usize * h as usize * 8,
            "buffer 版长度异常: {} ≠ w*h*8",
            buf.len()
        );

        // 新：纹理版。dst 纹理（RGBA16F，UNORDERED_ACCESS|SHADER_RESOURCE）+
        // 同规格 staging（CPU 读）。注意 _into 内部自带引擎锁 → 纹理创建/读回
        // 用短锁段，调用链时不得持锁（std Mutex 不可重入）。
        let (tex, stg) = {
            let engine = engine().as_ref().expect("GPU 引擎不可用");
            let eng = engine.0.lock().expect("GPU 锁");
            let (device, _ctx) = eng.device_ctx();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let tex = unsafe {
                let mut tex: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .expect("CreateTexture2D(dst) 失败");
                tex.expect("dst 纹理创建失败")
            };
            let mut sdesc = desc;
            sdesc.Usage = D3D11_USAGE_STAGING;
            sdesc.BindFlags = 0;
            sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let stg = unsafe {
                let mut stg: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&sdesc, None, Some(&mut stg))
                    .expect("CreateTexture2D(staging) 失败");
                stg.expect("staging 纹理创建失败")
            };
            (tex, stg)
        };

        let t0 = std::time::Instant::now();
        hybrid_reconstruct_pq16_f16_into(&snap, &gamut, &tex).expect("纹理版失败");
        let tex_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 读回 + 对拍（短锁段）
        {
            let engine = engine().as_ref().expect("GPU 引擎不可用");
            let eng = engine.0.lock().expect("GPU 锁");
            let (_device, ctx) = eng.device_ctx();
            unsafe {
                // CopyResource（同规格纹理恒合法）+ Map(READ) 隐式同步
                ctx.CopyResource(&stg, &tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("Map(staging) 失败");
                let row_pitch = mapped.RowPitch as usize;
                let base = mapped.pData as *const u8;
                let row_bytes = w as usize * 8;
                let mut first_diff: Option<(u32, u32, usize, u16, u16)> = None;
                'outer: for py in 0..h {
                    for px in 0..w {
                        for ch in 0..4usize {
                            let toff = py as usize * row_pitch + px as usize * 8 + ch * 2;
                            let boff = py as usize * row_bytes + px as usize * 8 + ch * 2;
                            let tv = u16::from_le_bytes([*base.add(toff), *base.add(toff + 1)]);
                            let bv = u16::from_le_bytes([buf[boff], buf[boff + 1]]);
                            if tv != bv {
                                first_diff = Some((px, py, ch, bv, tv));
                                break 'outer;
                            }
                        }
                    }
                }
                ctx.Unmap(&stg, 0);
                if let Some((px, py, ch, bv, tv)) = first_diff {
                    panic!(
                        "pass F 纹理版与 buffer 版位型不一致 @ (px={px}, py={py}, 通道{ch}): buffer={bv:#06x} tex={tv:#06x}"
                    );
                }
                println!(
                    "pass F 直写纹理 vs buffer: {w}×{h} 逐位一致（{} 字节，RowPitch={row_pitch} ≠ 行有效区 {row_bytes}）| buffer 链 {buf_ms:.1}ms vs 纹理链 {tex_ms:.1}ms",
                    buf.len()
                );
            }
        }
    }

    /// B2 增量：GPU 反量化+IDCT vs libjxl IDCT 参照层（≥50dB）
    #[test]
    fn gpu_reconstruct_matches_idct_ref() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        {
            let eng = engine().as_ref().unwrap().0.lock().unwrap();
            println!(
                "GPU 设备: {}",
                if eng.is_warp { "WARP 软渲染（慢）" } else { "硬件" }
            );
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        let t0 = std::time::Instant::now();
        let (gpu_out, _stats, _cmp) = gpu_reconstruct(&snap).expect("GPU 重建失败");
        // B6 全帧驻留：GPU 输出 = idct_ref 底图（非 DCT8 槽位）∪ pass C 直写 DCT8。
        // 对拍参照同构造：idct_ref ∪ golden(DCT8 槽位)——golden 非 DCT8 槽位为 0，
        // 不代表块层真值；DCT8 槽位两者独立计算（golden=Rust 标量，GPU=HLSL，
        // 应逐元素一致），≥50dB 仍只考核 GPU DCT8 数学。
        let (golden, _) =
            crate::viewer::decode::hybrid::golden_reconstruct(&snap).expect("CPU 黄金重建失败");
        let nggc = snap.num_groups as usize * snap.group_dim as usize * snap.group_dim as usize;
        let group_coeffs = snap.group_dim as usize * snap.group_dim as usize;
        let mut expected = snap.idct_ref.clone();
        {
            let gdb = snap.group_dim as usize / 8;
            let xb = snap.xsize_blocks as usize;
            let yb = snap.ysize_blocks as usize;
            let xg = snap.xsize_groups as usize;
            for g in 0..snap.num_groups as usize {
                let gx_blocks = (g % xg) * gdb;
                let gy_blocks = (g / xg) * gdb;
                let gw = gdb.min(xb - gx_blocks.min(xb));
                let gh = gdb.min(yb - gy_blocks.min(yb));
                let mut offset = 0usize;
                for by in 0..gh {
                    for bx in 0..gw {
                        let acs = snap.ac_strategy[(gy_blocks + by) * xb + gx_blocks + bx];
                        if acs & 1 == 0 {
                            continue;
                        }
                        let kind = (acs >> 1) as usize;
                        let size = COVERED_X[kind] as usize * COVERED_Y[kind] as usize * BLOCK;
                        if kind == DCT8 {
                            let src_base = g * group_coeffs + offset;
                            for c in 0..3 {
                                let to = c * nggc + src_base;
                                expected[to..to + 64].copy_from_slice(&golden[to..to + 64]);
                            }
                        }
                        offset += size;
                    }
                }
            }
        }
        let cmp = compare_against_ref(&gpu_out, &expected).expect("交叉对拍失败");
        println!(
            "GPU vs CPU golden: PSNR = {:.2} dB, max_abs_diff = {:.3e}, peak = {:.3}, 耗时 {:.1} ms",
            cmp.psnr_db,
            cmp.max_abs_diff,
            cmp.peak,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        assert!(
            cmp.psnr_db >= 50.0,
            "GPU 重建 PSNR {:.2} dB < 50 dB 阈值",
            cmp.psnr_db
        );
    }

    /// B3：混合重建（GPU DCT8 + libjxl 块层复用）全帧对拍（≥50dB）
    #[test]
    fn hybrid_reconstruct_full_frame() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        let t0 = std::time::Instant::now();
        let (_full, cmp) = hybrid_reconstruct(&snap, true).expect("混合重建失败");
        // 差异定位：首个 >1e-3 的元素位置与所在块类型
        let ng_total = snap.num_groups as usize * snap.group_dim as usize * snap.group_dim as usize;
        let (golden, _) =
            crate::viewer::decode::hybrid::golden_reconstruct(&snap).expect("CPU 黄金重建失败");
        {
            let gd = snap.group_dim as usize;
            let gcoeffs = gd * gd;
            let gdb = gd / 8;
            let xb = snap.xsize_blocks as usize;
            let xg = snap.xsize_groups as usize;
            let cx = [1u8, 1, 1, 1, 2, 4, 1, 2, 1, 4, 2, 4, 1, 1, 1, 1, 1, 1, 8, 4, 8, 16, 8, 16, 32, 16, 32];
            let cy = [1u8, 1, 1, 1, 2, 4, 2, 1, 4, 1, 4, 2, 1, 1, 1, 1, 1, 1, 8, 8, 4, 16, 16, 8, 32, 32, 16];
            let full = hybrid_reconstruct(&snap, false).expect("重建失败").0;
            let mut found = 0;
            'find: for c in 0..3 {
                for i in 0..ng_total {
                    let d = (full[c * ng_total + i] - golden[c * ng_total + i]).abs();
                    if d > 1e-3 && found < 3 {
                        let blk_global = i / 64;
                        let g = blk_global / (gcoeffs / 64);
                        let off_in_group = blk_global % (gcoeffs / 64);
                        // 找该块的策略
                        let gb_x = (g % xg) * gdb;
                        let gb_y = (g / xg) * gdb;
                        let bx_in = off_in_group % gdb;
                        let by_in = off_in_group / gdb;
                        let acs = snap.ac_strategy[(gb_y + by_in) * xb + gb_x + bx_in];
                        println!(
                            "差异[{}]: c={} g={} 块组内#{} kind={} first={} gpu={:.5} golden={:.5}",
                            found, c, g, off_in_group, acs >> 1, acs & 1,
                            full[c * ng_total + i], golden[c * ng_total + i]
                        );
                        found += 1;
                        if found >= 3 {
                            break 'find;
                        }
                    }
                }
            }
        }
        println!(
            "混合重建全帧: PSNR = {:.2} dB, max_abs_diff = {:.3e}, 耗时 {:.1} ms",
            cmp.psnr_db,
            cmp.max_abs_diff,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        assert!(
            cmp.psnr_db >= 50.0,
            "混合重建 PSNR {:.2} dB < 50 dB 阈值",
            cmp.psnr_db
        );
    }

    /// B6c：GPU Gaborish（pass G）vs CPU 黄金参考（≥40dB，理想 f32 舍入级 ≥100dB）。
    /// 输入 = 现有混合重建产出的 XYB 块层（idct_ref 底图 ∪ GPU DCT8 直写）。
    #[test]
    fn gaborish_gpu_matches_cpu() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        let (xyb, _) = hybrid_reconstruct(&snap, false).expect("混合重建失败");
        println!(
            "样本 gab={} gab_xw=({:.7}, {:.7}) gab_yw=({:.7}, {:.7}) 尺寸 {}x{}（libjxl 默认 w1=0.1151695 w2=0.0612486）",
            snap.gab,
            snap.gab_xweight1,
            snap.gab_xweight2,
            snap.gab_yweight1,
            snap.gab_yweight2,
            snap.width,
            snap.height
        );
        assert!(snap.gab, "录制样本应启用 Gaborish（gab=true）");
        let t0 = std::time::Instant::now();
        let gpu = gpu_gaborish(&snap, &xyb).expect("GPU Gaborish 失败");
        let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let cpu = crate::viewer::decode::hybrid::gaborish_ref(&snap, &xyb).expect("CPU 参考失败");
        let cmp = compare_against_ref(&gpu, &cpu).expect("对拍失败");
        println!(
            "Gaborish GPU vs CPU: PSNR = {:.2} dB, max_abs_diff = {:.3e}, 耗时 {:.1} ms（含 ~47MB 上传+读回）",
            cmp.psnr_db, cmp.max_abs_diff, gpu_ms
        );
        assert!(
            cmp.psnr_db >= 40.0,
            "Gaborish 对拍 PSNR {:.2} dB < 40 dB 阈值",
            cmp.psnr_db
        );
    }

    /// B6d：GPU EPF1（pass E）vs CPU 黄金参考。
    /// 纯环节：同一 CPU Gaborish 输出分别喂 CPU/GPU EPF1（隔离 EPF pass，理想
    /// f32 舍入级）；链式：CPU 链 gaborish_ref→epf1_ref vs GPU 链
    /// gaborish→epf1（对拍 ≥40dB）。
    #[test]
    fn epf1_gpu_matches_cpu() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        println!(
            "样本 epf_iters={} sigma={}x{}（{} 项，紧凑行距）channel_scale=({:.1},{:.1},{:.1}) border_sad_mul={:.4} 尺寸 {}x{}",
            snap.epf_iters,
            snap.sigma_xsize,
            snap.sigma_ysize,
            snap.sigma.len(),
            snap.epf_channel_scale[0],
            snap.epf_channel_scale[1],
            snap.epf_channel_scale[2],
            snap.epf_border_sad_mul,
            snap.width,
            snap.height
        );
        assert!(snap.epf_iters >= 1, "录制样本应 epf_iters≥1（=1 → 仅 EPF1）");
        assert!(!snap.sigma.is_empty(), "epf_iters≥1 时 sigma 图应非空");
        let (xyb, _) = hybrid_reconstruct(&snap, false).expect("混合重建失败");
        let gab_cpu =
            crate::viewer::decode::hybrid::gaborish_ref(&snap, &xyb).expect("CPU Gaborish 失败");

        // --- 纯 EPF 环节对拍（同一输入，隔离 EPF pass）---
        let t0 = std::time::Instant::now();
        let cpu_epf =
            crate::viewer::decode::hybrid::epf1_ref(&snap, &gab_cpu).expect("CPU EPF1 失败");
        let cpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let gpu_epf_pure = gpu_epf1(&snap, &gab_cpu).expect("GPU EPF1 失败");
        let pure = compare_against_ref(&gpu_epf_pure, &cpu_epf).expect("纯 EPF 对拍失败");
        println!(
            "EPF1 纯环节 GPU vs CPU: PSNR = {:.2} dB, max_abs_diff = {:.3e}",
            pure.psnr_db, pure.max_abs_diff
        );
        assert!(
            pure.psnr_db >= 40.0,
            "纯 EPF1 对拍 PSNR {:.2} dB < 40 dB 阈值",
            pure.psnr_db
        );

        // --- 链式对拍（CPU 链 gaborish→epf1 vs GPU 链 gaborish→epf1）---
        let t1 = std::time::Instant::now();
        let gab_gpu = gpu_gaborish(&snap, &xyb).expect("GPU Gaborish 失败");
        let gpu_chain = gpu_epf1(&snap, &gab_gpu).expect("GPU EPF1 失败");
        let chain_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let chain = compare_against_ref(&gpu_chain, &cpu_epf).expect("链式对拍失败");
        println!(
            "EPF1 链式（Gaborish→EPF1）GPU vs CPU: PSNR = {:.2} dB, max_abs_diff = {:.3e}, GPU 链耗时 {:.1} ms, CPU 参考耗时 {:.1} ms（含 ~47MB×2 上传+读回）",
            chain.psnr_db, chain.max_abs_diff, chain_ms, cpu_ms
        );
        assert!(
            chain.psnr_db >= 40.0,
            "链式 EPF1 对拍 PSNR {:.2} dB < 40 dB 阈值",
            chain.psnr_db
        );
    }

    /// B7：GPU PQ16 输出（pass O）vs CPU 黄金参考——u16 编码域 LSB 统计断言
    /// （≥99% 像素 |a-b|≤1 且 max ≤2；PQ 编码浮点舍入在暗部可能 ±1 LSB，
    /// PSNR 不适合 u16 编码域故用 LSB 统计）。
    /// 链路：XYB 块层 →（现有 GPU 链 hybrid_reconstruct_linear）linear →
    /// CPU pq16_out_ref vs GPU pass O（独立入口）；另跑全链
    /// hybrid_reconstruct_pq16（linear GPU 驻留 → pass O 直读 xyb_out）二次对拍。
    #[test]
    fn pq16_out_gpu_matches_cpu() {
        let path = std::path::Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let snap =
            crate::viewer::decode::jxl::coeff_export_snapshot(path).expect("系数快照失败");
        // 现有 GPU 链 → linear（读回；GPU/CPU 消费同一输入，只考核 PQ 输出环节）
        let (linear, _) = hybrid_reconstruct_linear(&snap).expect("混合重建线性失败");
        let cpu = crate::viewer::decode::hybrid::pq16_out_ref(&snap, &linear)
            .expect("CPU PQ16 参考失败");

        let check = |gpu: &[u16], tag: &str| {
            assert_eq!(gpu.len(), cpu.len(), "{} 槽位数不一致", tag);
            let mut within1 = 0usize;
            let mut max_diff = 0u16;
            let mut sum_diff = 0u64;
            for (a, b) in gpu.iter().zip(cpu.iter()) {
                let d = a.abs_diff(*b);
                sum_diff += d as u64;
                if d <= 1 {
                    within1 += 1;
                }
                max_diff = max_diff.max(d);
            }
            let total = gpu.len();
            let ratio = within1 as f64 / total as f64;
            let mean = sum_diff as f64 / total as f64;
            println!(
                "{}: 总槽位 {}，|a-b|≤1 占比 {:.4}%（阈值 99%），max = {} LSB，mean = {:.5} LSB",
                tag,
                total,
                ratio * 100.0,
                max_diff,
                mean
            );
            assert!(
                ratio >= 0.99,
                "{} |a-b|≤1 占比 {:.4}% < 99%",
                tag,
                ratio * 100.0
            );
            assert!(max_diff <= 2, "{} max 差 {} LSB > 2", tag, max_diff);
        };

        // --- 纯 pass O 环节对拍（同一 linear 输入）---
        let t0 = std::time::Instant::now();
        let gpu = gpu_pq16_out(&snap, &linear).expect("GPU PQ16 pass 失败");
        let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        check(&gpu, "PQ16 纯环节 GPU vs CPU");
        println!("（独立入口耗时 {:.1} ms，含 ~45MB 上传+读回）", gpu_ms);

        // --- 全链出图（生产路径形状：linear GPU 驻留 → pass O 直读）二次对拍 ---
        let (gpu_chain, it) = hybrid_reconstruct_pq16(&snap).expect("全链 PQ16 出图失败");
        check(&gpu_chain, "PQ16 全链 GPU vs CPU");
        println!("全链反解 intensity_target = {:.2} nits", it);
    }
}
