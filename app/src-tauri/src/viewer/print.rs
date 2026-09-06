//! 图片 / PDF 打印（参考 2345 看图王的发票打印体验，以标准系统 API 实现）
//!
//! - 打印机 / 纸张枚举：winspool（EnumPrintersW + DeviceCapabilitiesW）
//! - 图片打印：全格式解码（复用 decode::decode_image，HDR 自动色调映射）
//!   → 32bpp BGRA DIB → GDI（CreateDC WINSPOOL + StretchDIBits）
//! - PDF 打印：WinRT Windows.Data.Pdf（系统组件）逐页渲染位图 → 同一 GDI 路径
//! - 批量队列：前端管理列表，print_files 逐个提交（每文件独立打印任务），
//!   进度经 print://progress 事件推送前端
//!
//! GDI/winspool 用 raw FFI（风格对齐 singleinstance.rs），避免 windows-rs 签名差异。

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{BITMAPINFO, BITMAPINFOHEADER};

use super::decode;

// ==================== 数据结构（与前端 interface 对齐） ====================

/// 打印机（前端 interface PrinterInfo）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrinterInfo {
    pub name: String,
    pub port: String,
    pub is_default: bool,
}

/// 纸张规格（前端 interface PaperInfo；尺寸为毫米）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperInfo {
    pub id: u16,
    pub name: String,
    pub width_mm: f32,
    pub height_mm: f32,
}

/// 单文件打印结果（前端 interface PrintFileResult）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintFileResult {
    pub path: String,
    pub ok: bool,
    pub pages: u32,
    pub error: Option<String>,
}

/// 打印进度事件（前端 interface PrintProgress）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintProgress {
    pub index: usize,
    pub total: usize,
    pub path: String,
    /// printing = 开始打印 | done = 成功 | error = 失败
    pub phase: String,
    pub pages: u32,
    pub error: Option<String>,
}

/// 打印配置（需求文档1+2：print_files / print_preview 共用的选项对象，
/// 前端以 camelCase JSON 传入，避免命令参数爆炸）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintOptions {
    /// 0 = 打印机默认纸张
    pub paper_id: u16,
    /// 0 自动（按首页宽高比）/ 1 纵向 / 2 横向
    pub orientation: u8,
    pub copies: u16,
    /// contain 适应 / fill 铺满 / actual 实际大小
    pub fit: String,
    // ---- 页边距（需求文档1：自定义上下左右） ----
    pub margin_top_mm: f32,
    pub margin_bottom_mm: f32,
    pub margin_left_mm: f32,
    pub margin_right_mm: f32,
    /// single 每页一纸 / nup 一张多页 / booklet 小册子 / invoice 发票
    pub mode: String,
    /// 一张多页：列
    pub nup_cols: u32,
    /// 一张多页：行
    pub nup_rows: u32,
    /// ltr-tb / rtl-tb / tb-ltr / tb-rtl 网格遍历顺序（需求文档1 5.1 四种）
    pub page_order: String,
    /// off / long / short 双面翻转
    pub duplex: String,
    /// 打印边框（网格分隔线，实线）
    pub draw_border: bool,
    /// PDF 渲染 DPI（72-600）
    pub pdf_dpi: u32,
    /// 灰度打印（DEVMODE dmColor 单色；需求文档2 一节）
    pub grayscale: bool,
    /// 逐份打印（DEVMODE dmCollate；需求文档2 一节）
    pub collate: bool,
    /// 自动居中（false = 左上角对齐；需求文档1 4.4）
    pub auto_center: bool,
    /// 自动旋转（格与页横竖不匹配时旋转 90°；需求文档1 4.4）
    pub auto_rotate: bool,
    /// 合并打印：多文件页合并成一个打印任务（需求文档1"合并打印"）
    pub merge_print: bool,
    // ---- 打印范围（需求文档2 一节） ----
    /// 页码区间文本（"1-3,5"；空 = 全部；1-based）
    pub page_range: String,
    /// all / odd / even 奇偶页过滤
    pub odd_even: String,
    /// 逆序打印（逻辑页输出顺序反转）
    pub reverse: bool,
    // ---- 小册子（需求文档2 Tab3） ----
    /// both 双面 / odd 仅奇数面 / even 仅偶数面
    pub booklet_subset: String,
    /// left 左装订 / right 右装订（页面左右格互换）
    pub booklet_bind: String,
    // ---- 发票模式（需求文档2 Tab4） ----
    /// single 单张 / two 1纸2张 / four 1纸4张
    pub invoice_layout: String,
    /// 裁剪线（虚线分隔）
    pub cut_line: bool,
    /// 同一逻辑页在一张纸上重复 2 次
    pub dup_on_sheet: bool,
    /// 自由排版元素（mode=free 时有效；其余模式忽略）
    #[serde(default)]
    pub free_items: Vec<FreeItem>,
}

// ==================== Win32 FFI（winspool + GDI） ====================

type Handle = *mut core::ffi::c_void;

/// DEVMODEW（完整 C 布局，220 字节；DocumentPropertiesW 输出含驱动私有数据，按字节数组持有）
#[repr(C)]
struct DevModeW {
    dm_device_name: [u16; 32],
    dm_spec_version: u16,
    dm_driver_version: u16,
    dm_size: u16,
    dm_driver_extra: u16,
    dm_fields: u32,
    // 打印字段区（与显示字段 dmPosition/dmDisplayOrientation 共用偏移，打印场景按打印字段访问）
    dm_orientation: i16,
    dm_paper_size: i16,
    dm_paper_length: i16,
    dm_paper_width: i16,
    dm_scale: i16,
    dm_copies: i16,
    dm_default_source: i16,
    dm_print_quality: i16,
    dm_color: i16,
    dm_duplex: i16,
    dm_y_resolution: i16,
    dm_ttoption: i16,
    dm_collate: i16,
    dm_form_name: [u16; 32],
    dm_log_pixels: u16,
    dm_bits_per_pel: u32,
    dm_pels_width: u32,
    dm_pels_height: u32,
    dm_display_flags: u32,
    dm_display_frequency: u32,
    dm_icm_method: u32,
    dm_icm_intent: u32,
    dm_media_type: u32,
    dm_dither_type: u32,
    dm_reserved1: u32,
    dm_reserved2: u32,
    dm_panning_width: u32,
    dm_panning_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PointL {
    x: i32,
    y: i32,
}

/// PRINTER_INFO_5W（轻量：名称 + 端口）
#[repr(C)]
struct PrinterInfo5W {
    p_printer_name: *const u16,
    p_port_name: *const u16,
    attributes: u32,
    device_not_selected_timeout: u32,
    transmission_retry_timeout: u32,
}

/// DOCINFOW
#[repr(C)]
struct DocInfoW {
    cb_size: u32,
    lpsz_doc_name: *const u16,
    lpsz_output: *const u16,
    lpsz_datatype: *const u16,
    fw_type: u32,
}

extern "system" {
    fn CreateDCW(
        pwszdriver: *const u16,
        pwszdevice: *const u16,
        pszport: *const u16,
        pdm: *const DevModeW,
    ) -> Handle;
    fn DeleteDC(hdc: Handle) -> i32;
    fn GetDeviceCaps(hdc: Handle, index: i32) -> i32;
    fn SetStretchBltMode(hdc: Handle, mode: i32) -> i32;
    fn StretchDIBits(
        hdc: Handle,
        xdest: i32,
        ydest: i32,
        destwidth: i32,
        destheight: i32,
        xsrc: i32,
        ysrc: i32,
        srcwidth: i32,
        srcheight: i32,
        bits: *const core::ffi::c_void,
        bmi: *const BITMAPINFO,
        usage: u32,
        rop: u32,
    ) -> i32;
    fn StartDocW(hdc: Handle, lpdocinfo: *const DocInfoW) -> i32;
    fn StartPage(hdc: Handle) -> i32;
    fn EndPage(hdc: Handle) -> i32;
    fn EndDoc(hdc: Handle) -> i32;
    // 打印边框（网格分隔线）
    fn CreatePen(penstyle: i32, width: i32, color: u32) -> Handle;
    fn SelectObject(hdc: Handle, h: Handle) -> Handle;
    fn DeleteObject(h: Handle) -> i32;
    fn Rectangle(hdc: Handle, left: i32, top: i32, right: i32, bottom: i32) -> i32;
    fn EnumPrintersW(
        flags: u32,
        name: *const u16,
        level: u32,
        pprinter: *mut u8,
        cbbuf: u32,
        pcbneeded: *mut u32,
        pcreturned: *mut u32,
    ) -> i32;
    fn GetDefaultPrinterW(pszbuffer: *mut u16, pcchbuffer: *mut u32) -> i32;
    fn OpenPrinterW(
        pprintername: *const u16,
        phprinter: *mut Handle,
        pdefault: *const core::ffi::c_void,
    ) -> i32;
    fn ClosePrinter(hprinter: Handle) -> i32;
    fn DocumentPropertiesW(
        hwnd: Handle,
        hprinter: Handle,
        pdevicename: *const u16,
        pdevmodeoutput: *mut DevModeW,
        pdevmodeinput: *const DevModeW,
        fmode: u32,
    ) -> i32;
    fn DeviceCapabilitiesW(
        pdevice: *const u16,
        pport: *const u16,
        fwcapability: u32,
        poutput: *mut u16,
        pdm: *const DevModeW,
    ) -> i32;
    // ---- 打印预览：内存位图渲染（与打印机 DC 同一套绘制调用） ----
    fn CreateCompatibleDC(hdc: Handle) -> Handle;
    fn CreateDIBSection(
        hdc: Handle,
        bmi: *const BITMAPINFO,
        usage: u32,
        bits: *mut *mut core::ffi::c_void,
        hsection: Handle,
        offset: u32,
    ) -> Handle;
    fn PatBlt(hdc: Handle, x1: i32, y1: i32, x2: i32, y2: i32, rop: u32) -> i32;
    fn GdiFlush() -> i32;
    fn GetLastError() -> u32;
}

// GetDeviceCaps 索引 / 常量
const HORZRES: i32 = 8;
const VERTRES: i32 = 9;
const LOGPIXELSX: i32 = 88;
const LOGPIXELSY: i32 = 90;
/// 整张纸物理尺寸（含不可打印边距；驱动 HORZRES/VERTRES 异常时的兜底）
const PHYSICALWIDTH: i32 = 110;
const PHYSICALHEIGHT: i32 = 111;
const HALFTONE: i32 = 4;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;
// DEVMODE dmFields 位
const DM_ORIENTATION: u32 = 0x1;
const DM_PAPERSIZE: u32 = 0x2;
const DM_COPIES: u32 = 0x100;
const DM_COLLATE: u32 = 0x8000;
const DM_DUPLEX: u32 = 0x1000;
const DM_COLOR: u32 = 0x800;
const DMORIENT_LANDSCAPE: i16 = 2;
// 双面：SIMPLEX 单面 / VERTICAL 长边翻转 / HORIZONTAL 短边翻转
const DMDUP_SIMPLEX: i16 = 1;
const DMDUP_VERTICAL: i16 = 2;
const DMDUP_HORIZONTAL: i16 = 3;
// 灰度：DMCOLOR_MONOCHROME
const DMCOLOR_MONOCHROME: i16 = 1;
// 逐份：DMCOLLATE_TRUE
const DMCOLLATE_TRUE: i16 = 1;
const DM_OUT_BUFFER: u32 = 2;
// DocumentPropertiesW 弹窗模式：输入缓冲 + 输出缓冲（打开属性对话框）
const DM_IN_BUFFER: u32 = 8;
// DocumentPropertiesW 返回值：IDOK
const IDOK: i32 = 1;
// 边框画笔：PS_SOLID 实线 / PS_DASH 虚线（发票裁剪线）
const PS_SOLID: i32 = 0;
const PS_DASH: i32 = 1;
// PatBlt 光栅操作：白色填充（预览位图底色 = 纸面白）
const WHITENESS: u32 = 0x00FF_0062;
// EnumPrinters 标志
const PRINTER_ENUM_LOCAL: u32 = 2;
const PRINTER_ENUM_CONNECTIONS: u32 = 4;
// DeviceCapabilities 能力
const DC_PAPERS: u32 = 2;
const DC_PAPERSIZE: u32 = 3;
const DC_PAPERNAMES: u32 = 16;

// ==================== 工具 ====================

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 宽字符指针 → String（容忍空指针）
unsafe fn pwsz_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// 是否为可打印文档（图片 + PDF；与前端过滤器一致）
pub fn is_printable(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "webp"
            | "avif"
            | "tif"
            | "tiff"
            | "ico"
            | "psd"
            | "exr"
            | "jxr"
            | "wdp"
            | "jxl"
            | "pdf"
    )
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

// ==================== 布局模型（N-Up / 小册子，需求文档 4/5 节融合实现） ====================

/// 打印模式（前端字符串直传，见 PrintPanel）
/// - single：每逻辑页一张物理纸
/// - nup：一张多页，cols×rows 网格
/// - booklet：小册子（横向纸每面 2 页 + 骑马钉页序重排，对折装订）
/// - invoice：发票（需求文档2 Tab4：1纸1/2/4张 + 同页重复 + 裁剪线）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintMode {
    Single,
    NUp,
    Booklet,
    Invoice,
    /// 自由排版：预览画布上拖动/缩放元素（free_items 驱动，所见即所得）
    Free,
}

impl PrintMode {
    fn parse(s: &str) -> Self {
        match s {
            "nup" => Self::NUp,
            "booklet" => Self::Booklet,
            "invoice" => Self::Invoice,
            "free" => Self::Free,
            _ => Self::Single,
        }
    }
}

/// 自由排版元素：归一化坐标（0-1，相对纸张可打印区）
///
/// 前端画布拖拽生成；打印与预览按同一坐标绘制（contain 保持比例，同前端 object-fit）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeItem {
    /// 源文件路径
    pub path: String,
    /// PDF 页码（0-based；图片恒为 0）
    pub page_index: u32,
    /// 左上角 X（0-1）
    pub x: f32,
    /// 左上角 Y（0-1）
    pub y: f32,
    /// 宽（0-1）
    pub w: f32,
    /// 高（0-1）
    pub h: f32,
}

/// 自由排版模板（%APPDATA%\jietu-hdr\print_templates.json）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintTemplate {
    pub name: String,
    /// 保存时纸张宽高比（w/h；跨纸张加载时前端提示比例差异）
    pub aspect: f32,
    pub items: Vec<FreeItem>,
}

fn templates_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("jietu-hdr")
        .join("print_templates.json")
}

fn load_templates() -> Vec<PrintTemplate> {
    match std::fs::read_to_string(templates_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_templates(list: &[PrintTemplate]) -> Result<(), String> {
    let p = templates_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建模板目录失败: {}", e))?;
    }
    let s = serde_json::to_string_pretty(list).map_err(|e| format!("模板序列化失败: {}", e))?;
    std::fs::write(&p, s).map_err(|e| format!("写模板文件失败: {}", e))?;
    Ok(())
}

/// 发票排版（需求文档2 Tab4）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvoiceLayout {
    /// 单张发票（1纸1页）
    Single,
    /// 1纸2张（2×1 网格）
    Two,
    /// 1纸4张（2×2 网格）
    Four,
}

impl InvoiceLayout {
    fn parse(s: &str) -> Self {
        match s {
            "two" => Self::Two,
            "four" => Self::Four,
            _ => Self::Single,
        }
    }

    /// 网格规格 (cols, rows)
    fn grid(self) -> (u32, u32) {
        match self {
            Self::Single => (1, 1),
            Self::Two => (2, 1),
            Self::Four => (2, 2),
        }
    }
}

/// 页面排布顺序（需求文档 5.1：四种网格遍历顺序）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageOrder {
    /// 行优先从左到右（默认，UI 第一个图标）
    LtrTb,
    /// 行优先从右向左
    RtlTb,
    /// 列优先从上往下，列从左向右
    TbLtr,
    /// 列优先从上往下，列从右向左
    TbRtl,
}

impl PageOrder {
    fn parse(s: &str) -> Self {
        match s {
            "rtl-tb" => Self::RtlTb,
            "tb-ltr" => Self::TbLtr,
            "tb-rtl" => Self::TbRtl,
            _ => Self::LtrTb,
        }
    }
}

/// 双面翻转模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DuplexMode {
    Off,
    /// 长边翻转（纵向纸常用）
    LongEdge,
    /// 短边翻转（横向纸常用）
    ShortEdge,
}

impl DuplexMode {
    fn parse(s: &str) -> Self {
        match s {
            "long" => Self::LongEdge,
            "short" => Self::ShortEdge,
            _ => Self::Off,
        }
    }
}

/// 页索引 → 网格坐标 (col, row)（需求文档 5.1 四种顺序算法）
fn grid_pos(index: usize, cols: u32, rows: u32, order: PageOrder) -> (u32, u32) {
    let cols = cols.max(1) as usize;
    let rows = rows.max(1) as usize;
    let idx = index as u32;
    match order {
        PageOrder::LtrTb => (idx % cols as u32, idx / cols as u32),
        PageOrder::RtlTb => (cols as u32 - 1 - idx % cols as u32, idx / cols as u32),
        PageOrder::TbLtr => (idx / rows as u32, idx % rows as u32),
        PageOrder::TbRtl => (cols as u32 - 1 - idx / rows as u32, idx % rows as u32),
    }
}

/// 小册子骑马钉页序（需求文档 5.5）：补齐 4 的倍数后重排为 (左, 右) 面
///
/// 经典排列（0-based，总页 N）：第 i 张纸正面 [N-2i-1, 2i]、背面 [2i+1, N-2i-2]。
/// 例 N=8：正面 [7,0] [5,2]、背面 [1,6] [3,4] → 对折装订后 1..8 顺序正确。
/// 返回 None 表示补位的空白格。
fn booklet_faces(page_count: usize) -> Vec<(Option<usize>, Option<usize>)> {
    // 补齐到 4 的倍数（补空白页）
    let n = page_count.max(1).div_ceil(4) * 4;
    let mut faces = Vec::with_capacity(n / 2);
    let mut i = 0;
    while i < n / 4 {
        let front_l = n - 2 * i - 1;
        let front_r = 2 * i;
        let back_l = 2 * i + 1;
        let back_r = n - 2 * i - 2;
        faces.push((Some(front_l), Some(front_r)));
        faces.push((Some(back_l), Some(back_r)));
        i += 1;
    }
    // 越界索引（补位页）转 None
    faces
        .into_iter()
        .map(|(l, r)| (l.filter(|v| *v < page_count), r.filter(|v| *v < page_count)))
        .collect()
}

// ==================== 页码范围过滤（需求文档2：打印范围） ====================

/// 解析页码区间文本（1-based）→ 0-based 索引序列
///
/// 支持逗号分隔的混合："1-3,5" → [0,1,2,4]；"-" 分隔闭区间。
/// 越界页码直接跳过（需求文档2 五.1），乱序/非法片段忽略。
/// 空/全非法 → None 表示不过滤（全部页）。
fn parse_page_range(text: &str, total: usize) -> Option<Vec<usize>> {
    let t = text.trim();
    if t.is_empty() || total == 0 {
        return None;
    }
    let mut set = std::collections::BTreeSet::new();
    for seg in t.split(&[',', '，', ' '][..]) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        // 区间 "a-b" / 单页 "n"
        if let Some((a, b)) = seg.split_once(&['-', '–', '—'][..]) {
            if let (Ok(s), Ok(e)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                if s >= 1 && e >= s {
                    for p in s..=e.min(total) {
                        set.insert(p - 1);
                    }
                }
            }
        } else if let Ok(n) = seg.parse::<usize>() {
            if n >= 1 && n <= total {
                set.insert(n - 1);
            }
        }
    }
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}

/// 页码过滤 + 奇偶 + 逆序（需求文档2 一节"打印范围"）
///
/// odd_even：all 全部 / odd 仅奇数页 / even 仅偶数页（页码 1-based）。
/// reverse：逻辑页输出顺序反转（不是旋转，需求文档2 五.2）。
/// 返回 (过滤后页索引, 实际页数)；空结果返回 Err。
fn filter_pages(total: usize, opt: &PrintOptions) -> Result<Vec<usize>, String> {
    let mut idx: Vec<usize> = match parse_page_range(&opt.page_range, total) {
        Some(v) => v,
        None => (0..total).collect(),
    };
    // 奇偶过滤（基于原 1-based 页码）
    match opt.odd_even.as_str() {
        "odd" => idx.retain(|&i| (i + 1) % 2 == 1),
        "even" => idx.retain(|&i| (i + 1) % 2 == 0),
        _ => {}
    }
    // 逆序（输出顺序反转）
    if opt.reverse {
        idx.reverse();
    }
    if idx.is_empty() {
        Err("打印范围为空：页码区间与奇偶筛选后没有可打印的页".to_string())
    } else {
        Ok(idx)
    }
}

/// 页码过滤 + 发票同页重复（需求文档2 五.5：同一逻辑页重复两次在同一张纸，非连续两页）
///
/// 返回过滤/扩展后的页索引序列（供 plan_physical_pages 消费）。
fn effective_page_indices(total: usize, opt: &PrintOptions) -> Result<Vec<usize>, String> {
    let mut idx = filter_pages(total, opt)?;
    if PrintMode::parse(&opt.mode) == PrintMode::Invoice && opt.dup_on_sheet {
        // [0,1,2] → [0,0,1,1,2,2]
        let mut dup = Vec::with_capacity(idx.len() * 2);
        for i in idx.iter() {
            dup.push(*i);
            dup.push(*i);
        }
        idx = dup;
    }
    Ok(idx)
}

// ==================== 命令：枚举打印机 / 纸张 ====================

/// 列出系统打印机（本地 + 连接），标记默认打印机
#[tauri::command]
pub async fn list_printers() -> Result<Vec<PrinterInfo>, String> {
    tokio::task::spawn_blocking(|| unsafe {
        let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;

        // 第一遍取所需大小
        let mut needed = 0u32;
        let mut returned = 0u32;
        let r = EnumPrintersW(
            flags,
            std::ptr::null(),
            5,
            std::ptr::null_mut(),
            0,
            &mut needed,
            &mut returned,
        );
        if r == 0 && needed == 0 {
            return Ok(Vec::new()); // 无打印机
        }

        let mut buf = vec![0u8; needed as usize];
        let r = EnumPrintersW(
            flags,
            std::ptr::null(),
            5,
            buf.as_mut_ptr(),
            needed,
            &mut needed,
            &mut returned,
        );
        if r == 0 {
            return Err("枚举打印机失败".to_string());
        }

        // 默认打印机名（两遍调用：取长度再取内容）
        let mut default = String::new();
        let mut cap = 0u32;
        if GetDefaultPrinterW(std::ptr::null_mut(), &mut cap) == 0 && cap > 0 {
            let mut dbuf = vec![0u16; cap as usize];
            if GetDefaultPrinterW(dbuf.as_mut_ptr(), &mut cap) != 0 {
                default = String::from_utf16_lossy(&dbuf[..(cap as usize).saturating_sub(1)]);
            }
        }

        let infos =
            std::slice::from_raw_parts(buf.as_ptr() as *const PrinterInfo5W, returned as usize);
        let list = infos
            .iter()
            .map(|pi| {
                let name = pwsz_to_string(pi.p_printer_name);
                PrinterInfo {
                    name: name.clone(),
                    port: pwsz_to_string(pi.p_port_name),
                    is_default: !default.is_empty() && name.eq_ignore_ascii_case(&default),
                }
            })
            .collect();
        Ok(list)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 列出指定打印机支持的纸张规格（名称 + ID + 毫米尺寸）
#[tauri::command]
pub async fn printer_papers(printer: String) -> Result<Vec<PaperInfo>, String> {
    tokio::task::spawn_blocking(move || unsafe {
        let dev = to_wide(&printer);
        // 端口名：DeviceCapabilitiesW 需要（枚举该打印机的 PRINTER_INFO_5 取端口）
        let port = find_printer_port(&printer);

        let n = DeviceCapabilitiesW(
            dev.as_ptr(),
            port.as_ptr(),
            DC_PAPERNAMES,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if n <= 0 {
            return Err(format!("打印机「{}」不支持纸张查询", printer));
        }
        let n = n as usize;

        // 名称：n × 64 宽字符
        let mut names = vec![0u16; n * 64];
        DeviceCapabilitiesW(
            dev.as_ptr(),
            port.as_ptr(),
            DC_PAPERNAMES,
            names.as_mut_ptr(),
            std::ptr::null(),
        );
        // ID：n × WORD
        let mut ids = vec![0u16; n];
        DeviceCapabilitiesW(
            dev.as_ptr(),
            port.as_ptr(),
            DC_PAPERS,
            ids.as_mut_ptr(),
            std::ptr::null(),
        );
        // 尺寸：n × POINT（十分之一毫米）
        let mut sizes = vec![PointL::default(); n];
        DeviceCapabilitiesW(
            dev.as_ptr(),
            port.as_ptr(),
            DC_PAPERSIZE,
            sizes.as_mut_ptr() as *mut u16,
            std::ptr::null(),
        );

        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let name_raw = &names[i * 64..(i + 1) * 64];
            let name_len = name_raw.iter().position(|&c| c == 0).unwrap_or(64);
            let name = String::from_utf16_lossy(&name_raw[..name_len]);
            if name.is_empty() {
                continue;
            }
            out.push(PaperInfo {
                id: ids[i],
                name,
                width_mm: sizes[i].x as f32 / 10.0,
                height_mm: sizes[i].y as f32 / 10.0,
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 查找打印机的端口名（EnumPrinters level 5；找不到返回空串 → 传 null 兜底）
unsafe fn find_printer_port(printer: &str) -> Vec<u16> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let mut needed = 0u32;
    let mut returned = 0u32;
    EnumPrintersW(
        flags,
        std::ptr::null(),
        5,
        std::ptr::null_mut(),
        0,
        &mut needed,
        &mut returned,
    );
    if needed == 0 {
        return vec![0];
    }
    let mut buf = vec![0u8; needed as usize];
    let r = EnumPrintersW(
        flags,
        std::ptr::null(),
        5,
        buf.as_mut_ptr(),
        needed,
        &mut needed,
        &mut returned,
    );
    if r == 0 {
        return vec![0];
    }
    let infos = std::slice::from_raw_parts(buf.as_ptr() as *const PrinterInfo5W, returned as usize);
    for pi in infos {
        if pwsz_to_string(pi.p_printer_name).eq_ignore_ascii_case(printer) {
            return to_wide(&pwsz_to_string(pi.p_port_name));
        }
    }
    vec![0]
}

// ==================== 命令：扫描可打印文件 ====================

/// 递归扫描目录下的可打印文件（图片 + PDF），按名称排序（上限 2000）
#[tauri::command]
pub async fn scan_printable_files(dir: String) -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        walk_printable(Path::new(&dir), &mut out, 0);
        out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn walk_printable(dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 8 || out.len() >= 2000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            // 跳过隐藏 / 系统目录（$RECYCLE.BIN 等）
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.starts_with('$') {
                continue;
            }
            walk_printable(&path, out, depth + 1);
        } else if is_printable(&path) {
            if let Some(p) = path.to_str() {
                out.push(p.to_string());
            }
        }
    }
}

// ==================== 打印核心 ====================

/// 待打印位图页（32bpp BGRA，顶-底行序）
struct PageBitmap {
    w: u32,
    h: u32,
    bgra: Vec<u8>,
}

/// 构造 DEVMODE（含驱动私有数据）：DocumentPropertiesW 取驱动默认值 → 改纸张/方向/份数/双面/灰度/逐份
///
/// paper_id = 0 表示打印机默认纸张（不改 dmPaperSize）。
/// orientation：0 自动（按首页宽高比）/ 1 纵向 / 2 横向（需求文档2"方向：自动"）。
/// duplex：Off 单面 / LongEdge 长边翻转 / ShortEdge 短边翻转（需求文档 5.4，
/// 走 DEVMODE dmDuplex 直控驱动，比 PDF 元数据标记可靠）。
/// grayscale：dmColor = DMCOLOR_MONOCHROME（需求文档2 一节"灰度打印"）。
/// collate：dmCollate = DMCOLLATE_TRUE（需求文档2 一节"逐份打印"）。
#[allow(clippy::too_many_arguments)]
fn build_devmode(
    printer: &str,
    paper_id: u16,
    orientation: u8,
    copies: u16,
    duplex: DuplexMode,
    grayscale: bool,
    collate: bool,
) -> Result<Vec<u8>, String> {
    unsafe {
        let name_w = to_wide(printer);
        let mut hprinter: Handle = std::ptr::null_mut();
        if OpenPrinterW(name_w.as_ptr(), &mut hprinter, std::ptr::null()) == 0 {
            return Err(format!("打开打印机「{}」失败", printer));
        }

        // 第一遍取所需字节数（含驱动私有数据）
        let needed = DocumentPropertiesW(
            std::ptr::null_mut(),
            hprinter,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
        );
        if needed <= 0 {
            ClosePrinter(hprinter);
            return Err("查询打印设备配置失败".to_string());
        }

        // 第二遍取默认 DEVMODE
        let mut buf = vec![0u8; needed as usize];
        let r = DocumentPropertiesW(
            std::ptr::null_mut(),
            hprinter,
            name_w.as_ptr(),
            buf.as_mut_ptr() as *mut DevModeW,
            std::ptr::null(),
            DM_OUT_BUFFER,
        );
        ClosePrinter(hprinter);
        if r <= 0 {
            return Err("读取打印设备默认配置失败".to_string());
        }

        // 修改需要的字段（缓冲区头部即 DEVMODEW；驱动私有数据跟随其后原样保留）
        let dm = buf.as_ptr() as *mut DevModeW;
        (*dm).dm_orientation = if orientation == 2 {
            DMORIENT_LANDSCAPE
        } else {
            1
        };
        (*dm).dm_fields |= DM_ORIENTATION;
        if paper_id > 0 {
            (*dm).dm_paper_size = paper_id as i16;
            (*dm).dm_fields |= DM_PAPERSIZE;
        }
        if copies > 1 {
            (*dm).dm_copies = copies as i16;
            (*dm).dm_fields |= DM_COPIES;
            // 逐份打印仅多份时有意义（需求文档2 一节）
            if collate {
                (*dm).dm_collate = DMCOLLATE_TRUE;
                (*dm).dm_fields |= DM_COLLATE;
            }
        }
        let duplex_val = match duplex {
            DuplexMode::Off => DMDUP_SIMPLEX,
            DuplexMode::LongEdge => DMDUP_VERTICAL,
            DuplexMode::ShortEdge => DMDUP_HORIZONTAL,
        };
        (*dm).dm_duplex = duplex_val;
        (*dm).dm_fields |= DM_DUPLEX;
        // 灰度打印（需求文档2 一节；不改页面颜色，由驱动执行单色）
        if grayscale {
            (*dm).dm_color = DMCOLOR_MONOCHROME;
            (*dm).dm_fields |= DM_COLOR;
        }
        Ok(buf)
    }
}

/// 物理页布局描述：本物理页要画的 (位图引用, 网格坐标) 列表 + 网格规格
struct PhysPageLayout {
    cols: u32,
    rows: u32,
    cells: Vec<(usize, u32, u32)>, // (pages 索引, col, row)
}

/// 页索引序列 → 物理页布局序列（按模式分组）
///
/// - Single：每页一张物理纸（1×1 网格）
/// - NUp：每 cols×rows 页一张物理纸，按 page_order 分配格位（需求文档 4 节）
/// - Booklet：每面左右两格，页序按骑马钉重排（需求文档 5.5）
/// - Invoice：按发票排版网格分组（需求文档2 Tab4；页序固定 LtrTb）
///
/// 入参 idx 已经过打印范围过滤（含发票同页重复扩展）。
fn plan_physical_pages(idx: &[usize], opt: &PrintOptions) -> Vec<PhysPageLayout> {
    let mode = PrintMode::parse(&opt.mode);
    let order = PageOrder::parse(&opt.page_order);
    let mut out = Vec::new();
    if idx.is_empty() {
        return out;
    }
    match mode {
        PrintMode::Single => {
            for &i in idx {
                out.push(PhysPageLayout {
                    cols: 1,
                    rows: 1,
                    cells: vec![(i, 0, 0)],
                });
            }
        }
        PrintMode::NUp => {
            // 自动网格（cols/rows 任一为 0）：按总页数取最小方阵，
            // 4 页 → 2×2、6 页 → 3×2、9 页 → 3×3（全部拼进一张纸）
            let (cols, rows) = if opt.nup_cols == 0 || opt.nup_rows == 0 {
                let n = idx.len().max(1);
                let c = (n as f64).sqrt().ceil().max(1.0) as u32;
                let r = ((n as f64) / c as f64).ceil().max(1.0) as u32;
                (c, r)
            } else {
                (opt.nup_cols.max(1), opt.nup_rows.max(1))
            };
            log::info!(
                "N-Up 布局: {} 页 → 网格 {}×{}（每纸 {} 页）",
                idx.len(),
                cols,
                rows,
                cols * rows
            );
            let per = (cols * rows) as usize;
            let mut p = 0;
            while p < idx.len() {
                let mut cells = Vec::with_capacity(per);
                for k in 0..per {
                    if p + k >= idx.len() {
                        break; // 边界：尾页不足一整张纸，剩余格留空（需求文档 9.1）
                    }
                    let (c, r) = grid_pos(k, cols, rows, order);
                    cells.push((idx[p + k], c, r));
                }
                out.push(PhysPageLayout { cols, rows, cells });
                p += per;
            }
        }
        PrintMode::Booklet => {
            // 每面固定 2×1 网格（左格 + 右格），页序由骑马钉重排给出。
            // 骑马钉算法按"逻辑页数"计算，补位 None → 空格。
            // 右装订 = 左右格互换（书本从右翻开；需求文档2 Tab3）。
            let swap = opt.booklet_bind.eq_ignore_ascii_case("right");
            let mut layouts = Vec::new();
            for (l, r) in booklet_faces(idx.len()) {
                let (lc, rc) = if swap { (1, 0) } else { (0, 1) };
                let mut cells = Vec::with_capacity(2);
                if let Some(li) = l {
                    cells.push((idx[li], lc, 0));
                }
                if let Some(ri) = r {
                    cells.push((idx[ri], rc, 0));
                }
                layouts.push(PhysPageLayout {
                    cols: 2,
                    rows: 1,
                    cells,
                });
            }
            // 子集过滤（需求文档2 Tab3：双面 / 仅奇数面 / 仅偶数面）。
            // 面序 1-based：odd = 第 1、3、5… 面；手动双面打印时先打奇数面再翻纸打偶数面。
            out = match opt.booklet_subset.as_str() {
                "odd" => layouts.into_iter().step_by(2).collect(),
                "even" => layouts.into_iter().skip(1).step_by(2).collect(),
                _ => layouts,
            };
        }
        PrintMode::Invoice => {
            let (cols, rows) = InvoiceLayout::parse(&opt.invoice_layout).grid();
            let per = (cols * rows) as usize;
            let mut p = 0;
            while p < idx.len() {
                let mut cells = Vec::with_capacity(per);
                for k in 0..per {
                    if p + k >= idx.len() {
                        break;
                    }
                    let (c, r) = grid_pos(k, cols, rows, PageOrder::LtrTb);
                    cells.push((idx[p + k], c, r));
                }
                out.push(PhysPageLayout { cols, rows, cells });
                p += per;
            }
        }
        // 自由排版不走网格规划（draw_free_page 按 items 坐标直接绘制；此处恒空）
        PrintMode::Free => {}
    }
    out
}

/// 提交自由排版打印任务（一张纸，按 free_items 布局）
fn print_free_pages(
    printer: &str,
    doc_name: &str,
    items: &[FreeItem],
    map: &std::collections::HashMap<String, Vec<PageBitmap>>,
    dm_buf: &[u8],
) -> Result<(), String> {
    unsafe {
        let driver = to_wide("WINSPOOL");
        let device = to_wide(printer);
        let hdc = CreateDCW(
            driver.as_ptr(),
            device.as_ptr(),
            std::ptr::null(),
            dm_buf.as_ptr() as *const DevModeW,
        );
        if hdc.is_null() {
            return Err("创建打印设备上下文失败".to_string());
        }
        let name_w = to_wide(doc_name);
        let di = DocInfoW {
            cb_size: std::mem::size_of::<DocInfoW>() as u32,
            lpsz_doc_name: name_w.as_ptr(),
            lpsz_output: std::ptr::null(),
            lpsz_datatype: std::ptr::null(),
            fw_type: 0,
        };
        if StartDocW(hdc, &di) <= 0 {
            DeleteDC(hdc);
            return Err("提交打印任务失败（打印机脱机或被占用?）".to_string());
        }
        let (pw, ph, _, _) = dc_paper_pixels(hdc);
        if StartPage(hdc) <= 0 {
            DeleteDC(hdc);
            return Err("开始打印页失败".to_string());
        }
        draw_free_page(hdc, items, map, pw, ph);
        if EndPage(hdc) <= 0 {
            DeleteDC(hdc);
            return Err("输出打印页失败".to_string());
        }
        if EndDoc(hdc) <= 0 {
            DeleteDC(hdc);
            return Err("结束打印任务失败".to_string());
        }
        DeleteDC(hdc);
        Ok(())
    }
}

/// 提交一份打印任务（份数由 DEVMODE dmCopies 驱动；布局见 plan_physical_pages）
fn print_pages(
    printer: &str,
    doc_name: &str,
    pages: &[PageBitmap],
    idx: &[usize],
    dm_buf: &[u8],
    opt: &PrintOptions,
) -> Result<(), String> {
    unsafe {
        let driver = to_wide("WINSPOOL");
        let device = to_wide(printer);
        let hdc = CreateDCW(
            driver.as_ptr(),
            device.as_ptr(),
            std::ptr::null(),
            dm_buf.as_ptr() as *const DevModeW,
        );
        if hdc.is_null() {
            return Err("创建打印设备上下文失败".to_string());
        }

        let result = print_pages_inner(hdc, doc_name, pages, idx, opt);
        DeleteDC(hdc);
        result
    }
}

/// 取打印机 DC 纸面像素规格（可打印区 + DPI；驱动异常时兜底物理纸张）
///
/// 某些驱动（如华为 HUAWEI CV81-WDMSE）对 VERTRES 返回 0，导致
/// 预览位图高度为 0（CreateDIBSection 失败）、打印目标高为 0。
/// 此时退回 PHYSICALWIDTH/PHYSICALHEIGHT（整张纸，含不可打印边距）。
/// 返回 (宽px, 高px, dpix, dpiy)，全部保证 ≥ 1。
fn dc_paper_pixels(hdc: Handle) -> (i32, i32, i32, i32) {
    unsafe {
        let mut pw = GetDeviceCaps(hdc, HORZRES);
        let mut ph = GetDeviceCaps(hdc, VERTRES);
        if pw <= 0 {
            pw = GetDeviceCaps(hdc, PHYSICALWIDTH);
        }
        if ph <= 0 {
            ph = GetDeviceCaps(hdc, PHYSICALHEIGHT);
        }
        let dpix = GetDeviceCaps(hdc, LOGPIXELSX).max(1);
        let dpiy = GetDeviceCaps(hdc, LOGPIXELSY).max(1);
        (pw.max(1), ph.max(1), dpix, dpiy)
    }
}

fn print_pages_inner(
    hdc: Handle,
    doc_name: &str,
    pages: &[PageBitmap],
    idx: &[usize],
    opt: &PrintOptions,
) -> Result<(), String> {
    unsafe {
        let name_w = to_wide(doc_name);
        let di = DocInfoW {
            cb_size: std::mem::size_of::<DocInfoW>() as u32,
            lpsz_doc_name: name_w.as_ptr(),
            lpsz_output: std::ptr::null(),
            lpsz_datatype: std::ptr::null(),
            fw_type: 0,
        };
        if StartDocW(hdc, &di) <= 0 {
            return Err("提交打印任务失败（打印机脱机或被占用?）".to_string());
        }

        // 打印路径：从打印机 DC 取可打印区尺寸 + DPI（HORZRES/VERTRES 为驱动可打印像素区；
        // 驱动异常返回 0 时 dc_paper_pixels 兜底物理纸张尺寸）
        let (pw, ph, dpix, dpiy) = dc_paper_pixels(hdc);

        for phys in plan_physical_pages(idx, opt) {
            if StartPage(hdc) <= 0 {
                return Err("开始打印页失败".to_string());
            }
            draw_phys_page(hdc, &phys, pages, opt, pw, ph, dpix, dpiy);
            if EndPage(hdc) <= 0 {
                return Err("输出打印页失败".to_string());
            }
        }
        if EndDoc(hdc) <= 0 {
            return Err("结束打印任务失败".to_string());
        }
        Ok(())
    }
}

/// 绘制一个物理页（打印 DC 与预览内存 DC 共用；尺寸/DPI 显式传入）
///
/// 边框规则：N-Up/小册子 draw_border 画实线分隔（需求文档 9.3）；
/// 发票模式 cut_line 画虚线裁剪线（需求文档2 Tab4）。
/// auto_rotate：格与页横竖不匹配时把页位图旋转 90°（需求文档1 4.4）。
fn draw_phys_page(
    hdc: Handle,
    phys: &PhysPageLayout,
    pages: &[PageBitmap],
    opt: &PrintOptions,
    pw: i32,
    ph: i32,
    dpix: i32,
    dpiy: i32,
) {
    unsafe {
        for (page_idx, c, r) in &phys.cells {
            let cell = cell_rect(pw, ph, dpix, dpiy, phys.cols, phys.rows, *c, *r, opt);
            let page = &pages[*page_idx];
            // 自动旋转：格与页横竖不匹配（一个横一个竖）时转置位图适配，
            // 提高缩放利用率（方形页/格不旋转）
            let rotated;
            let page = if opt.auto_rotate
                && (cell.2 > cell.3) != (page.w > page.h)
                && cell.2 != cell.3
                && page.w != page.h
            {
                rotated = rotate_page_bgra(page);
                &rotated
            } else {
                page
            };
            draw_cell(hdc, page, cell, &opt.fit, dpix, opt.auto_center);
        }
        let border_style = border_style_of(opt);
        if let Some(dashed) = border_style {
            for (_, c, r) in &phys.cells {
                let cell = cell_rect(pw, ph, dpix, dpiy, phys.cols, phys.rows, *c, *r, opt);
                draw_cell_border(hdc, cell, dashed);
            }
        }
    }
}

/// 自由排版绘制：按 free_items 归一化坐标逐元素绘制（contain 保持比例 = 前端 object-fit）
///
/// 打印 DC 与预览内存 DC 共用；pw/ph 为纸面像素尺寸（坐标乘回像素）。
fn draw_free_page(
    hdc: Handle,
    items: &[FreeItem],
    map: &std::collections::HashMap<String, Vec<PageBitmap>>,
    pw: i32,
    ph: i32,
) {
    unsafe {
        for it in items {
            let Some(pages) = map.get(&it.path) else {
                continue;
            };
            let Some(p) = pages.get(it.page_index as usize) else {
                continue;
            };
            let dx = it.x * pw as f32;
            let dy = it.y * ph as f32;
            let dw = it.w * pw as f32;
            let dh = it.h * ph as f32;
            if dw < 1.0 || dh < 1.0 {
                continue;
            }
            // contain：元素框内保持图片比例居中（与前端画布 object-fit 一致）
            let s = (dw / p.w as f32).min(dh / p.h as f32);
            let w2 = p.w as f32 * s;
            let h2 = p.h as f32 * s;
            let ox = dx + (dw - w2) / 2.0;
            let oy = dy + (dh - h2) / 2.0;
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: p.w as i32,
                    biHeight: -(p.h as i32), // 顶-底行序
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            SetStretchBltMode(hdc, HALFTONE);
            StretchDIBits(
                hdc,
                ox.round() as i32,
                oy.round() as i32,
                (w2.round() as i32).max(1),
                (h2.round() as i32).max(1),
                0,
                0,
                p.w as i32,
                p.h as i32,
                p.bgra.as_ptr() as *const core::ffi::c_void,
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
}

/// 自由排版加载：按 items 里的路径去重加载页面位图
///
/// 返回 path → 页面位图组；加载失败的路径记录在 failed（前端列表提示）。
fn load_free_pages(
    items: &[FreeItem],
    pdf_dpi: u32,
) -> (
    std::collections::HashMap<String, Vec<PageBitmap>>,
    Vec<String>,
) {
    let mut map = std::collections::HashMap::new();
    let mut failed = Vec::new();
    for it in items {
        if map.contains_key(&it.path) {
            continue;
        }
        let p = Path::new(&it.path);
        let load = if p.is_file() {
            if is_pdf(p) {
                pages_from_pdf(p, pdf_dpi)
            } else {
                pages_from_image(p)
            }
        } else {
            Err("文件不存在".to_string())
        };
        match load {
            Ok(v) => {
                map.insert(it.path.clone(), v);
            }
            Err(e) => {
                log::error!("自由排版加载失败 [{}]: {}", it.path, e);
                failed.push(format!("{}: {}", it.path, e));
            }
        }
    }
    (map, failed)
}

/// BGRA 位图顺时针旋转 90°（内存转置；auto_rotate 辅助）
fn rotate_page_bgra(p: &PageBitmap) -> PageBitmap {
    let w = p.w as usize;
    let h = p.h as usize;
    let mut bgra = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let src = (y * w + x) * 4;
            // (x,y) → (y, w-1-x)：新宽 = h
            let dst = ((w - 1 - x) * h + y) * 4;
            bgra[dst..dst + 4].copy_from_slice(&p.bgra[src..src + 4]);
        }
    }
    PageBitmap {
        w: h as u32,
        h: w as u32,
        bgra,
    }
}

/// 本配置下的边框绘制样式：Some(虚线?) = 画（发票裁剪线虚线，其余实线）；None = 不画
fn border_style_of(opt: &PrintOptions) -> Option<bool> {
    if PrintMode::parse(&opt.mode) == PrintMode::Invoice && opt.cut_line {
        return Some(true);
    }
    if opt.draw_border {
        return Some(false);
    }
    None
}

/// 网格单元格在物理页上的矩形（x, y, w, h，设备像素；扣除四边页边距均分网格）
///
/// 尺寸/DPI 显式传入：打印路径传打印机 DC caps，预览路径传内存位图规格
/// （同一算法保证预览与打印所见即所得）。
/// 边距四边独立（需求文档1 页面设置 3：自定义上下左右）。
#[allow(clippy::too_many_arguments)]
fn cell_rect(
    pw: i32,
    ph: i32,
    dpix: i32,
    dpiy: i32,
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
    opt: &PrintOptions,
) -> (i32, i32, i32, i32) {
    let mx_left = (opt.margin_left_mm / 25.4 * dpix as f32) as i32;
    let mx_right = (opt.margin_right_mm / 25.4 * dpix as f32) as i32;
    let my_top = (opt.margin_top_mm / 25.4 * dpiy as f32) as i32;
    let my_bottom = (opt.margin_bottom_mm / 25.4 * dpiy as f32) as i32;
    let avail_w = (pw - mx_left - mx_right).max(1);
    let avail_h = (ph - my_top - my_bottom).max(1);
    let cols = cols.max(1);
    let rows = rows.max(1);
    let cw = avail_w / cols as i32;
    let ch = avail_h / rows as i32;
    let x = mx_left + (col as i32).min(cols as i32 - 1) * cw;
    let y = my_top + (row as i32).min(rows as i32 - 1) * ch;
    // 末行/列吃到整数除法余量，避免右侧/底部留缝
    let w = if col + 1 >= cols {
        pw - mx_right - x
    } else {
        cw
    };
    let h = if row + 1 >= rows {
        ph - my_bottom - y
    } else {
        ch
    };
    (x, y, w.max(1), h.max(1))
}

/// 单元格绘制：按缩放模式在格内排版位图（32bpp BGRA DIB，顶-底行序，保持比例）
///
/// dpix 显式传入（打印 = 打印机 DPI；预览 = 缩放后等效 DPI，保证 actual 模式比例一致）。
/// auto_center=false 时左上角对齐（需求文档1 4.4"自动居中"开关）。
fn draw_cell(
    hdc: Handle,
    p: &PageBitmap,
    cell: (i32, i32, i32, i32),
    fit: &str,
    dpix: i32,
    auto_center: bool,
) {
    unsafe {
        let (cx, cy, cw, chh) = cell;

        let iw = p.w as f32;
        let ih = p.h as f32;
        // contain=适应格子（默认）/ fill=铺满（居中裁切）/ actual=实际大小（96dpi 物理尺寸）
        let scale = match fit {
            "fill" => (cw as f32 / iw).max(chh as f32 / ih),
            "actual" => dpix as f32 / 96.0,
            _ => (cw as f32 / iw).min(chh as f32 / ih),
        };

        let dw = iw * scale;
        let dh = ih * scale;
        let x = if auto_center {
            cx as f32 + (cw as f32 - dw) / 2.0
        } else {
            cx as f32
        };
        let y = if auto_center {
            cy as f32 + (chh as f32 - dh) / 2.0
        } else {
            cy as f32
        };

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: p.w as i32,
                biHeight: -(p.h as i32), // 顶-底行序
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        SetStretchBltMode(hdc, HALFTONE);
        StretchDIBits(
            hdc,
            x.round() as i32,
            y.round() as i32,
            (dw.round() as i32).max(1),
            (dh.round() as i32).max(1),
            0,
            0,
            p.w as i32,
            p.h as i32,
            p.bgra.as_ptr() as *const core::ffi::c_void,
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

/// 单元格边框：1px 矩形（dashed=虚线用于发票裁剪线；实线用于 N-Up 分隔），不覆盖内容
fn draw_cell_border(hdc: Handle, cell: (i32, i32, i32, i32), dashed: bool) {
    unsafe {
        let (x, y, w, h) = cell;
        let pen = CreatePen(
            if dashed { PS_DASH } else { PS_SOLID },
            1,
            0x008C_8C_8C, // 0x00BBGGRR 中性灰
        );
        if pen.is_null() {
            return;
        }
        let old = SelectObject(hdc, pen);
        let _ = Rectangle(hdc, x, y, x + w, y + h);
        SelectObject(hdc, old);
        DeleteObject(pen);
    }
}

/// 图片文件 → 待打印页（全格式解码，HDR 自动色调映射）
fn pages_from_image(path: &Path) -> Result<Vec<PageBitmap>, String> {
    let img = decode::decode_image(path)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA → BGRA
    }
    Ok(vec![PageBitmap { w, h, bgra }])
}

/// PDF 文件 → 待打印页（WinRT Windows.Data.Pdf 逐页渲染位图，DPI 可调）
///
/// 注意：注释渲染不可控（WinRT PdfPageRenderOptions 无 IsIgnoringAnnotations，
/// 需求文档1 的"注释内容"开关是 pdfium 才有的能力；注释随页面一体渲染）。
fn pages_from_pdf(path: &Path, pdf_dpi: u32) -> Result<Vec<PageBitmap>, String> {
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Storage::StorageFile;
    use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    // COM STA（本线程内独立初始化；S_FALSE = 已初始化，无需 Uninitialize）
    let co_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let co_ok = co_hr.is_ok();
    let result = (|| -> Result<Vec<PageBitmap>, String> {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
            .map_err(|e| format!("打开 PDF 失败: {}", e))?
            .get()
            .map_err(|e| format!("打开 PDF 失败: {}", e))?;
        let doc = PdfDocument::LoadFromFileAsync(&file)
            .map_err(|e| format!("加载 PDF 失败: {}", e))?
            .get()
            .map_err(|e| format!("加载 PDF 失败: {}", e))?;
        let count = doc
            .PageCount()
            .map_err(|e| format!("读取 PDF 页数失败: {}", e))?;
        if count == 0 {
            return Err("PDF 无页面".to_string());
        }

        // DPI 可调（需求文档"作为图像打印"）：150 快速 / 200 均衡 / 300 高清 / 600 印刷
        let render_dpi = pdf_dpi.clamp(72, 600) as f32;

        let mut pages = Vec::with_capacity(count as usize);
        for i in 0..count {
            let page = doc
                .GetPage(i)
                .map_err(|e| format!("读取 PDF 第 {} 页失败: {}", i + 1, e))?;
            // Size：DIP（1/96 英寸）→ 渲染 DPI 像素
            let size = page
                .Size()
                .map_err(|e| format!("读取页面尺寸失败: {}", e))?;
            let w_px = (size.Width.max(1.0) * render_dpi / 96.0).ceil() as u32;
            let h_px = (size.Height.max(1.0) * render_dpi / 96.0).ceil() as u32;

            let opts =
                PdfPageRenderOptions::new().map_err(|e| format!("初始化渲染参数失败: {}", e))?;
            let _ = opts.SetDestinationWidth(w_px);
            let _ = opts.SetDestinationHeight(h_px);

            let stream =
                InMemoryRandomAccessStream::new().map_err(|e| format!("创建渲染流失败: {}", e))?;
            page.RenderWithOptionsToStreamAsync(&stream, &opts)
                .map_err(|e| format!("渲染 PDF 页面失败: {}", e))?
                .get()
                .map_err(|e| format!("渲染 PDF 页面失败: {}", e))?;

            let size = stream.Size().unwrap_or(0u64) as u32;
            if size == 0 {
                return Err("PDF 渲染结果为空".to_string());
            }
            let input = stream
                .GetInputStreamAt(0)
                .map_err(|e| format!("读取渲染结果失败: {}", e))?;
            let reader = DataReader::CreateDataReader(&input)
                .map_err(|e| format!("读取渲染结果失败: {}", e))?;
            reader
                .LoadAsync(size)
                .map_err(|e| format!("读取渲染结果失败: {}", e))?
                .get()
                .map_err(|e| format!("读取渲染结果失败: {}", e))?;
            let mut png = vec![0u8; size as usize];
            reader
                .ReadBytes(&mut png)
                .map_err(|e| format!("读取渲染结果失败: {}", e))?;

            // PNG 字节 → RGBA → BGRA
            let img =
                image::load_from_memory(&png).map_err(|e| format!("解码渲染结果失败: {}", e))?;
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            let mut bgra = rgba.into_raw();
            for px in bgra.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            pages.push(PageBitmap { w, h, bgra });
        }
        Ok(pages)
    })();

    if co_ok {
        unsafe { CoUninitialize() };
    }
    result
}

// ==================== 命令：批量打印 / 打印预览 ====================

/// 批量打印：逐文件提交（PDF 逐页；每文件独立打印任务），进度经 print://progress 推送
///
/// 配置见 `PrintOptions`（需求文档2 全量字段：模式/布局/范围/发票）。
#[tauri::command]
pub async fn print_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
    printer: String,
    options: PrintOptions,
) -> Result<Vec<PrintFileResult>, String> {
    use tauri::Emitter;

    if paths.is_empty() {
        return Err("打印队列为空".to_string());
    }
    if printer.trim().is_empty() {
        return Err("未选择打印机".to_string());
    }

    // 小册子：横向纸 + 双面短边翻（对折装订的标准组合），非横向/单面时纠正并提示
    let mode_enum = PrintMode::parse(&options.mode);
    let mut orientation = options.orientation;
    let duplex_enum = match mode_enum {
        PrintMode::Booklet => {
            orientation = 2; // 横向纸
            DuplexMode::ShortEdge // 短边翻 = 对折装订
        }
        _ => DuplexMode::parse(&options.duplex),
    };
    let mut opt = options.clone();
    opt.orientation = orientation;

    tokio::task::spawn_blocking(move || {
        let total = paths.len();
        let mut results = Vec::with_capacity(total);

        // 方向"自动"（需求文档2 纸张设置）：按第一个文件首页宽高比选横/纵。
        // 首文件加载结果缓存，正式循环复用（不重复解码）。
        let mut first_cache: Option<Vec<PageBitmap>> = None;
        if orientation == 0 {
            let p = Path::new(&paths[0]);
            let load = if is_pdf(p) {
                pages_from_pdf(p, opt.pdf_dpi)
            } else {
                pages_from_image(p)
            };
            let decided = load
                .as_ref()
                .ok()
                .and_then(|v| v.first())
                .map(|f| if f.w > f.h { 2 } else { 1 })
                .unwrap_or(1); // 判定失败兜底纵向
            orientation = decided;
            opt.orientation = decided;
            if let Ok(v) = load {
                first_cache = Some(v);
            }
        }

        // DEVMODE 一次构造（同一打印机/纸张/方向/份数/双面/灰度/逐份）
        let dm_buf = match build_devmode(
            &printer,
            opt.paper_id,
            orientation,
            opt.copies,
            duplex_enum,
            opt.grayscale,
            opt.collate,
        ) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

        // ==== 自由排版（需求：画布拖拽自定义排版）：items 驱动，单任务一张纸 ====
        if mode_enum == PrintMode::Free {
            if opt.free_items.is_empty() {
                return Err("自由排版画布为空：请先在右侧画布添加图片或 PDF".to_string());
            }
            let (map, failed) = load_free_pages(&opt.free_items, opt.pdf_dpi);
            if map.is_empty() {
                return Err(format!("自由排版所有文件加载失败：{}", failed.join("; ")));
            }
            let doc_name = "自由排版打印";
            match print_free_pages(&printer, doc_name, &opt.free_items, &map, &dm_buf) {
                Ok(()) => {
                    for (index, path) in paths.iter().enumerate() {
                        let _ = app.emit(
                            "print://progress",
                            PrintProgress {
                                index,
                                total,
                                path: path.clone(),
                                phase: "done".to_string(),
                                pages: 1,
                                error: None,
                            },
                        );
                        results.push(PrintFileResult {
                            path: path.clone(),
                            ok: true,
                            pages: 1,
                            error: None,
                        });
                    }
                }
                Err(e) => return Err(format!("自由排版打印失败: {}", e)),
            }
            return Ok(results);
        }

        // ==== 合并打印（需求文档1"合并打印"）：全部文件页合并成一个打印任务 ====
        if opt.merge_print {
            let mut all_pages: Vec<PageBitmap> = Vec::new();
            let mut ok_files = 0usize;
            let mut failed: Vec<String> = Vec::new(); // 加载失败文件（对齐 paths 输出结果）
            for (index, path) in paths.iter().enumerate() {
                let _ = app.emit(
                    "print://progress",
                    PrintProgress {
                        index,
                        total,
                        path: path.clone(),
                        phase: "printing".to_string(),
                        pages: 0,
                        error: None,
                    },
                );
                // 首文件复用方向判定时的缓存
                let load = if index == 0 && first_cache.is_some() {
                    Ok(first_cache.take().unwrap())
                } else {
                    let p = Path::new(path);
                    if is_pdf(p) {
                        pages_from_pdf(p, opt.pdf_dpi)
                    } else {
                        pages_from_image(p)
                    }
                };
                match load {
                    Ok(pages) if !pages.is_empty() => {
                        ok_files += 1;
                        all_pages.extend(pages);
                    }
                    Ok(_) => {
                        let err = "无有效页面".to_string();
                        let _ = app.emit(
                            "print://progress",
                            PrintProgress {
                                index,
                                total,
                                path: path.clone(),
                                phase: "error".to_string(),
                                pages: 0,
                                error: Some(err.clone()),
                            },
                        );
                        failed.push(path.clone());
                        results.push(PrintFileResult {
                            path: path.clone(),
                            ok: false,
                            pages: 0,
                            error: Some(err),
                        });
                    }
                    Err(e) => {
                        let _ = app.emit(
                            "print://progress",
                            PrintProgress {
                                index,
                                total,
                                path: path.clone(),
                                phase: "error".to_string(),
                                pages: 0,
                                error: Some(e.clone()),
                            },
                        );
                        failed.push(path.clone());
                        results.push(PrintFileResult {
                            path: path.clone(),
                            ok: false,
                            pages: 0,
                            error: Some(e),
                        });
                    }
                }
            }
            if all_pages.is_empty() {
                return Ok(results);
            }
            // 合并后整体应用打印范围过滤 + 布局
            let idx = match effective_page_indices(all_pages.len(), &opt) {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
            let doc_name = format!("合并打印（{} 个文件）", ok_files);
            let page_count = idx.len() as u32;
            match print_pages(&printer, &doc_name, &all_pages, &idx, &dm_buf, &opt) {
                Ok(()) => {
                    for (index, path) in paths.iter().enumerate() {
                        if !failed.contains(path) {
                            let _ = app.emit(
                                "print://progress",
                                PrintProgress {
                                    index,
                                    total,
                                    path: path.clone(),
                                    phase: "done".to_string(),
                                    pages: page_count,
                                    error: None,
                                },
                            );
                            results.push(PrintFileResult {
                                path: path.clone(),
                                ok: true,
                                pages: page_count,
                                error: None,
                            });
                        }
                    }
                }
                Err(e) => {
                    return Err(format!("合并打印失败: {}", e));
                }
            }
            return Ok(results);
        }

        // ==== 常规路径：逐文件独立打印任务 ====
        for (index, path) in paths.iter().enumerate() {
            let _ = app.emit(
                "print://progress",
                PrintProgress {
                    index,
                    total,
                    path: path.clone(),
                    phase: "printing".to_string(),
                    pages: 0,
                    error: None,
                },
            );

            let load: Result<Vec<PageBitmap>, String> = if index == 0 && first_cache.is_some() {
                Ok(first_cache.take().unwrap())
            } else {
                let p = Path::new(path);
                if is_pdf(p) {
                    pages_from_pdf(p, opt.pdf_dpi)
                } else {
                    pages_from_image(p)
                }
            };
            let pages = match load {
                Ok(p) if !p.is_empty() => p,
                Ok(_) => {
                    let err = "无有效页面".to_string();
                    let _ = app.emit(
                        "print://progress",
                        PrintProgress {
                            index,
                            total,
                            path: path.clone(),
                            phase: "error".to_string(),
                            pages: 0,
                            error: Some(err.clone()),
                        },
                    );
                    results.push(PrintFileResult {
                        path: path.clone(),
                        ok: false,
                        pages: 0,
                        error: Some(err),
                    });
                    continue;
                }
                Err(e) => {
                    let _ = app.emit(
                        "print://progress",
                        PrintProgress {
                            index,
                            total,
                            path: path.clone(),
                            phase: "error".to_string(),
                            pages: 0,
                            error: Some(e.clone()),
                        },
                    );
                    results.push(PrintFileResult {
                        path: path.clone(),
                        ok: false,
                        pages: 0,
                        error: Some(e),
                    });
                    continue;
                }
            };

            let doc_name = Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "jietu-hdr".to_string());

            // 打印范围过滤 + 发票同页重复（空结果 = 配置错误，整文件失败）
            let idx = match effective_page_indices(pages.len(), &opt) {
                Ok(v) => v,
                Err(e) => {
                    let _ = app.emit(
                        "print://progress",
                        PrintProgress {
                            index,
                            total,
                            path: path.clone(),
                            phase: "error".to_string(),
                            pages: 0,
                            error: Some(e.clone()),
                        },
                    );
                    results.push(PrintFileResult {
                        path: path.clone(),
                        ok: false,
                        pages: 0,
                        error: Some(e),
                    });
                    continue;
                }
            };

            let page_count = idx.len() as u32;
            match print_pages(&printer, &doc_name, &pages, &idx, &dm_buf, &opt) {
                Ok(()) => {
                    let _ = app.emit(
                        "print://progress",
                        PrintProgress {
                            index,
                            total,
                            path: path.clone(),
                            phase: "done".to_string(),
                            pages: page_count,
                            error: None,
                        },
                    );
                    results.push(PrintFileResult {
                        path: path.clone(),
                        ok: true,
                        pages: page_count,
                        error: None,
                    });
                }
                Err(e) => {
                    let _ = app.emit(
                        "print://progress",
                        PrintProgress {
                            index,
                            total,
                            path: path.clone(),
                            phase: "error".to_string(),
                            pages: 0,
                            error: Some(e.clone()),
                        },
                    );
                    results.push(PrintFileResult {
                        path: path.clone(),
                        ok: false,
                        pages: 0,
                        error: Some(e),
                    });
                }
            }
        }
        Ok(results)
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|results| {
        // 文件级失败汇总落日志（前端列表之外留排查线索）
        let fails: Vec<&PrintFileResult> = results.iter().filter(|r| !r.ok).collect();
        if !fails.is_empty() {
            let detail = fails
                .iter()
                .map(|r| format!("[{}] {}", r.path, r.error.as_deref().unwrap_or("未知错误")))
                .collect::<Vec<_>>()
                .join("; ");
            log::error!("打印完成但有 {} 个文件失败: {}", fails.len(), detail);
        }
        results
    })
}

/// 预览单页位图（print_preview 输出；base64 PNG + 纸面规格）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewPage {
    /// data URL 用的裸 base64 PNG（前端拼 `data:image/png;base64,`）
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
}

/// 打印预览上限（物理页数）：超大文档只渲染前若干页，防内存/时延失控
const PREVIEW_MAX_PAGES: usize = 12;
/// 预览位图宽度（像素）：面板 90% 屏宽时右栏显示宽度，等比缩放纸张
const PREVIEW_WIDTH_PX: i32 = 480;

/// 打印预览（需求文档2"校对"）：按当前配置渲染物理页 → PNG base64
///
/// 与打印共用同一套布局算法（plan_physical_pages / draw_phys_page），
/// 预览所见即打印所得；不发送任何数据到打印机。
/// merge_print 时合并渲染全部文件（与 print_files 合并路径一致），否则只渲染首文件。
#[tauri::command]
pub async fn print_preview(
    paths: Vec<String>,
    printer: String,
    options: PrintOptions,
) -> Result<Vec<PreviewPage>, String> {
    if printer.trim().is_empty() {
        return Err("未选择打印机".to_string());
    }
    if paths.is_empty() {
        return Err("未选择文件".to_string());
    }
    let log_path = paths.join(" | ");
    let log_printer = printer.clone();
    let r = tokio::task::spawn_blocking(move || {
        // 自由排版：items 驱动，path→页面位图映射（坐标归一化，绘制走 draw_free_page）
        let free_mode = PrintMode::parse(&options.mode) == PrintMode::Free;
        let free_map: std::collections::HashMap<String, Vec<PageBitmap>> = if free_mode {
            let (map, failed) = load_free_pages(&options.free_items, options.pdf_dpi);
            if map.is_empty() {
                return Err(format!("自由排版文件加载失败: {}", failed.join("; ")));
            }
            map
        } else {
            Default::default()
        };
        // 页面位图加载：合并打印 = 全部文件页串联；普通 = 仅第一个文件
        let load_one = |p: &Path| -> Result<Vec<PageBitmap>, String> {
            if is_pdf(p) {
                pages_from_pdf(p, options.pdf_dpi)
            } else {
                pages_from_image(p)
            }
        };
        let pages = if free_mode {
            Vec::new() // free 模式页面在 free_map 里
        } else if options.merge_print {
            let mut all: Vec<PageBitmap> = Vec::new();
            for path in paths.iter() {
                let p = Path::new(path);
                if !p.is_file() {
                    return Err(format!("文件不存在: {}", path));
                }
                let v = load_one(p).map_err(|e| format!("{}: {}", path, e))?;
                all.extend(v); // 空文件自然跳过（打印路径同样容错）
            }
            all
        } else {
            let p = Path::new(&paths[0]);
            if !p.is_file() {
                return Err(format!("文件不存在: {}", paths[0]));
            }
            load_one(p)?
        };
        if !free_mode && pages.is_empty() {
            return Err("无有效页面".to_string());
        }
        // 范围过滤 + 方向纠正（小册子横向；auto 按首页宽高比；同打印路径）
        let mut opt = options;
        if PrintMode::parse(&opt.mode) == PrintMode::Booklet {
            opt.orientation = 2;
        } else if opt.orientation == 0 {
            // free 模式无"页"概念：auto 一律纵向（画布比例由预览结果回填前端）
            opt.orientation = if free_mode {
                1
            } else {
                pages
                    .first()
                    .map(|f| if f.w > f.h { 2 } else { 1 })
                    .unwrap_or(1)
            };
        }
        let idx = if free_mode {
            Vec::new()
        } else {
            effective_page_indices(pages.len(), &opt)?
        };

        // 打印机 DC 只为取纸张可打印区尺寸/DPI（CreateDC 不 StartDoc，无副作用）
        let dm_buf = build_devmode(
            &printer,
            opt.paper_id,
            opt.orientation,
            1,
            DuplexMode::parse(&opt.duplex),
            opt.grayscale,
            false,
        )?;
        unsafe {
            let driver = to_wide("WINSPOOL");
            let device = to_wide(&printer);
            let hdc = CreateDCW(
                driver.as_ptr(),
                device.as_ptr(),
                std::ptr::null(),
                dm_buf.as_ptr() as *const DevModeW,
            );
            if hdc.is_null() {
                return Err("创建打印设备上下文失败".to_string());
            }
            // 纸面（可打印区）像素规格 → 预览等比缩放（驱动异常时 dc_paper_pixels 兜底物理纸张）
            let (pw, ph, printer_dpix, printer_dpiy) = dc_paper_pixels(hdc);
            DeleteDC(hdc);

            let scale = PREVIEW_WIDTH_PX as f32 / pw as f32;
            let tw = ((pw as f32 * scale).round() as i32).max(1);
            let th = ((ph as f32 * scale).round() as i32).max(1);
            log::info!(
                "预览纸面规格: 打印区 {}x{}px, DPI {}x{}, 缩放 {:.3} → 预览 {}x{}",
                pw,
                ph,
                printer_dpix,
                printer_dpiy,
                scale,
                tw,
                th
            );
            // 预览等效 DPI：cell_rect 的 mm→px 换算按此比例，与打印布局一致
            let v_dpix = (printer_dpix as f32 * scale).round() as i32;
            let v_dpiy = (printer_dpiy as f32 * scale).round() as i32;

            // 每物理页：内存 DIB（白底）→ draw_phys_page / draw_free_page → PNG base64
            let plans = plan_physical_pages(&idx, &opt);
            let page_total = if free_mode { 1 } else { plans.len() };
            let take = if free_mode {
                1
            } else {
                plans.len().min(PREVIEW_MAX_PAGES)
            };
            let mut out = Vec::with_capacity(take);
            for page_i in 0..take {
                let phys = if free_mode {
                    None
                } else {
                    Some(&plans[page_i])
                };
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: tw,
                        biHeight: -th, // top-down：内存行序与显示一致（bottom-up 读出会上下颠倒）
                        biPlanes: 1,
                        biBitCount: 32,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let dib = CreateDIBSection(
                    std::ptr::null_mut(),
                    &bmi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                );
                if dib.is_null() || bits.is_null() {
                    let gle = unsafe { GetLastError() };
                    return Err(format!(
                        "创建预览位图失败 (tw={}, th={}, GLE=0x{:08X})",
                        tw, th, gle
                    ));
                }
                let mem_dc = CreateCompatibleDC(std::ptr::null_mut());
                if mem_dc.is_null() {
                    DeleteObject(dib);
                    return Err("创建预览设备上下文失败".to_string());
                }
                let old = SelectObject(mem_dc, dib);
                // 纸面白底
                let _ = PatBlt(mem_dc, 0, 0, tw, th, WHITENESS);
                match phys {
                    Some(p) => {
                        draw_phys_page(mem_dc, p, &pages, &opt, tw, th, v_dpix, v_dpiy);
                    }
                    None => {
                        // 自由排版：归一化坐标 × 预览尺寸（与打印同一 draw_free_page）
                        draw_free_page(mem_dc, &opt.free_items, &free_map, tw, th);
                    }
                }
                let _ = GdiFlush();

                // DIB 像素（BGRA bottom-up）→ RGBA PNG
                // alpha 强制 255：GDI 不维护 alpha（HALFTONE 混合还会清零），
                // 不置 255 时 PNG 全透明 → 前端显示为空图
                let px = std::slice::from_raw_parts(bits as *const u8, (tw * th * 4) as usize);
                let mut rgba = px.to_vec();
                for c in rgba.chunks_exact_mut(4) {
                    c.swap(0, 2); // BGRA → RGBA
                    c[3] = 255;
                }
                SelectObject(mem_dc, old);
                DeleteDC(mem_dc);
                DeleteObject(dib);

                let img = image::RgbaImage::from_raw(tw as u32, th as u32, rgba)
                    .ok_or("预览位图尺寸异常")?;
                let mut buf = Vec::new();
                image::DynamicImage::ImageRgba8(img)
                    .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                    .map_err(|e| format!("预览编码失败: {}", e))?;
                use base64::Engine;
                out.push(PreviewPage {
                    png_base64: base64::engine::general_purpose::STANDARD.encode(&buf),
                    width: tw as u32,
                    height: th as u32,
                });
            }
            if page_total > take {
                log::info!("打印预览仅渲染前 {} 张物理纸（共 {} 张）", take, page_total);
            }
            Ok(out)
        }
    })
    .await
    .map_err(|e| format!("预览任务异常: {}", e))
    .and_then(|inner| inner);
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            log::error!(
                "打印预览失败 [{}] 打印机「{}」: {}",
                log_path,
                log_printer,
                e
            );
            Err(e)
        }
    }
}

// ==================== 命令：自由排版（缩略图 + 模板） ====================

/// 自由排版画布缩略图（free_thumbs 输出）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeThumb {
    pub width: u32,
    pub height: u32,
    pub png_base64: String,
}

/// 每个文件的缩略图组（PDF = 每页一张；图片 = 单张）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileThumbs {
    pub path: String,
    pub thumbs: Vec<FreeThumb>,
}

/// 缩略图宽（像素）：画布元素显示用，够拖拽定位即可
const FREE_THUMB_W: u32 = 320;

/// 自由排版缩略图：画布元素渲染用（图片/PDF 逐页小图）
///
/// 失败的文件返回空 thumbs（前端画布显示占位框），不整体报错。
#[tauri::command]
pub async fn free_thumbs(paths: Vec<String>) -> Result<Vec<FileThumbs>, String> {
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let p = Path::new(&path);
            let pages = if p.is_file() {
                if is_pdf(p) {
                    pages_from_pdf(p, 150).unwrap_or_default() // 缩略图固定 150dpi（清晰度足够）
                } else {
                    pages_from_image(p).unwrap_or_default()
                }
            } else {
                Vec::new()
            };
            let thumbs = pages
                .iter()
                .map(|pg| {
                    let (tw, th) = if pg.w == 0 || pg.h == 0 {
                        (FREE_THUMB_W, FREE_THUMB_W)
                    } else {
                        let w = FREE_THUMB_W.min(pg.w); // 小图不放大
                        (w, ((pg.h as f64 / pg.w as f64) * w as f64).round() as u32)
                    };
                    // BGRA → RGBA
                    let mut rgba = pg.bgra.clone();
                    for c in rgba.chunks_exact_mut(4) {
                        c.swap(0, 2);
                    }
                    let img =
                        image::RgbaImage::from_raw(pg.w, pg.h, rgba).ok_or("缩略图源数据异常")?;
                    let resized =
                        image::DynamicImage::ImageRgba8(img).thumbnail_exact(tw.max(1), th.max(1));
                    let mut buf = Vec::new();
                    resized
                        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                        .map_err(|e| format!("缩略图编码失败: {}", e))?;
                    use base64::Engine;
                    Ok::<FreeThumb, String>(FreeThumb {
                        width: tw.max(1),
                        height: th.max(1),
                        png_base64: base64::engine::general_purpose::STANDARD.encode(&buf),
                    })
                })
                .filter_map(|r| r.ok())
                .collect();
            out.push(FileThumbs { path, thumbs });
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("缩略图任务异常: {}", e))?
}

/// 模板列表
#[tauri::command]
pub async fn print_templates_list() -> Result<Vec<PrintTemplate>, String> {
    Ok(load_templates())
}

/// 保存（同名覆盖）/ 更新模板
#[tauri::command]
pub async fn print_templates_save(template: PrintTemplate) -> Result<(), String> {
    if template.name.trim().is_empty() {
        return Err("模板名不能为空".to_string());
    }
    let mut list = load_templates();
    list.retain(|t| t.name != template.name);
    list.push(template);
    list.sort_by(|a, b| a.name.cmp(&b.name));
    write_templates(&list)?;
    log::info!("打印模板已保存: {}", list.len());
    Ok(())
}

/// 删除模板
#[tauri::command]
pub async fn print_templates_delete(name: String) -> Result<(), String> {
    let mut list = load_templates();
    let before = list.len();
    list.retain(|t| t.name != name);
    if list.len() == before {
        return Err(format!("模板「{}」不存在", name));
    }
    write_templates(&list)?;
    Ok(())
}

// ==================== 命令：打印机属性对话框（需求文档2 七.4） ====================

/// 【属性】按钮结果：用户在系统对话框修改的字段（回填前端面板）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrinterPropsResult {
    /// 取消 = false（面板不回填）
    pub changed: bool,
    pub paper_id: u16,
    /// 1 纵向 / 2 横向
    pub orientation: u8,
    /// off / long / short（从 dmDuplex 反解）
    pub duplex: String,
}

/// 打开打印机原生属性对话框（DocumentPropertiesW DM_IN_BUFFER|DM_OUT_BUFFER）
///
/// 以当前面板配置为初始值；用户确认后把修改的纸张/方向/双面回填面板
/// （其余驱动专属参数如打印质量由驱动自行记忆，不经过面板）。
#[tauri::command]
pub async fn printer_properties(
    app: tauri::AppHandle,
    printer: String,
    paper_id: u16,
    orientation: u8,
    duplex: String,
) -> Result<PrinterPropsResult, String> {
    if printer.trim().is_empty() {
        return Err("未选择打印机".to_string());
    }
    tokio::task::spawn_blocking(move || {
        // 当前配置 → 输入 DEVMODE（与打印同一构造路径，保证初始态一致）
        let mut dm_buf = build_devmode(
            &printer,
            paper_id,
            orientation,
            1,
            DuplexMode::parse(&duplex),
            false,
            false,
        )?;

        // 模态对话框属主：主窗口（无主窗口句柄时传 null 也可，但会缺省任务栏归属）
        let hwnd: Handle = app
            .get_webview_window("main")
            .or_else(|| app.get_webview_window("viewer"))
            .and_then(|w| w.hwnd().ok())
            .map(|h| h.0 as *mut core::ffi::c_void)
            .unwrap_or(std::ptr::null_mut());

        unsafe {
            let name_w = to_wide(&printer);
            let mut hprinter: Handle = std::ptr::null_mut();
            if OpenPrinterW(name_w.as_ptr(), &mut hprinter, std::ptr::null()) == 0 {
                return Err(format!("打开打印机「{}」失败", printer));
            }
            // DM_IN_BUFFER（输入当前配置）| DM_OUT_BUFFER（用户确认后写回）
            let r = DocumentPropertiesW(
                hwnd,
                hprinter,
                name_w.as_ptr(),
                dm_buf.as_mut_ptr() as *mut DevModeW,
                dm_buf.as_ptr() as *const DevModeW,
                DM_IN_BUFFER | DM_OUT_BUFFER,
            );
            ClosePrinter(hprinter);
            if r != IDOK {
                // IDCANCEL（0）或出错：视为取消，不改面板
                return Ok(PrinterPropsResult {
                    changed: false,
                    paper_id,
                    orientation,
                    duplex,
                });
            }
            let dm = dm_buf.as_ptr() as *const DevModeW;
            let new_paper = (*dm).dm_paper_size as u16;
            let new_orient = if (*dm).dm_orientation == DMORIENT_LANDSCAPE {
                2
            } else {
                1
            };
            let new_duplex = match (*dm).dm_duplex {
                DMDUP_VERTICAL => "long".to_string(),
                DMDUP_HORIZONTAL => "short".to_string(),
                _ => "off".to_string(),
            };
            Ok(PrinterPropsResult {
                changed: true,
                paper_id: new_paper,
                orientation: new_orient,
                duplex: new_duplex,
            })
        }
    })
    .await
    .map_err(|e| e.to_string())?
}
