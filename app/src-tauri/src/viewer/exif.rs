//! EXIF 解析：kamadak-exif 解析 JPEG/TIFF EXIF，输出中文分组
//!
//! 分组：设备与拍摄 / 曝光 / 镜头 / 图像属性 / GPS / 时间。
//! 无 EXIF（或格式不含 EXIF）返回空 groups；GPS 转十进制度 + 度分秒字符串。

use std::io::BufReader;
use std::path::Path;

use exif::{Exif, Field, Reader, Tag, Value};

use super::{ExifData, ExifGroup, ExifItem};

/// 快速判定文件是否含 EXIF（ImageInfo.has_exif 用；仅 JPEG/TIFF 可能有）
pub fn has_exif(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "tif" | "tiff") {
        return false;
    }
    matches!(read_exif(path), Ok(Some(exif)) if exif.fields().next().is_some())
}

/// 读取并解析 EXIF：无 EXIF / 解析失败返回 None（文件打不开返回 Err）
fn read_exif(path: &Path) -> Result<Option<Exif>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut buf = BufReader::new(file);
    // 非 JPEG/TIFF 容器或无 EXIF 段 → None（视为无 EXIF 而非错误）
    match Reader::new().read_from_container(&mut buf) {
        Ok(exif) => Ok(Some(exif)),
        Err(_) => Ok(None),
    }
}

/// 解析 EXIF 并输出分组数据（无 EXIF 时 groups 为空）
pub fn parse(path: &Path) -> Result<ExifData, String> {
    let Some(exif) = read_exif(path)? else {
        return Ok(ExifData { groups: vec![] });
    };
    if exif.fields().next().is_none() {
        return Ok(ExifData { groups: vec![] });
    }

    Ok(ExifData {
        groups: build_groups(&exif),
    })
}

/// 组装全部分组（空分组不输出）
fn build_groups(exif: &Exif) -> Vec<ExifGroup> {
    let mut groups = Vec::new();

    // === 设备与拍摄 ===
    let mut g = Vec::new();
    push_field(exif, &mut g, Tag::Make, "制造商");
    push_field(exif, &mut g, Tag::Model, "型号");
    push_field(exif, &mut g, Tag::Software, "软件");
    push_orientation(exif, &mut g);
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "设备与拍摄".into(),
            items: g,
        });
    }

    // === 曝光 ===
    let mut g = Vec::new();
    push_field(exif, &mut g, Tag::ExposureTime, "曝光时间");
    push_field(exif, &mut g, Tag::FNumber, "光圈值");
    push_field(exif, &mut g, Tag::ISOSpeed, "ISO 感光度");
    push_enum(
        exif,
        &mut g,
        Tag::ExposureProgram,
        "曝光程序",
        exposure_program_cn,
    );
    push_field(exif, &mut g, Tag::ExposureBiasValue, "曝光补偿");
    push_enum(
        exif,
        &mut g,
        Tag::MeteringMode,
        "测光模式",
        metering_mode_cn,
    );
    push_enum(exif, &mut g, Tag::Flash, "闪光灯", flash_cn);
    push_enum(
        exif,
        &mut g,
        Tag::ExposureMode,
        "曝光模式",
        exposure_mode_cn,
    );
    push_enum(exif, &mut g, Tag::WhiteBalance, "白平衡", white_balance_cn);
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "曝光".into(),
            items: g,
        });
    }

    // === 镜头 ===
    let mut g = Vec::new();
    push_field(exif, &mut g, Tag::LensMake, "镜头制造商");
    push_field(exif, &mut g, Tag::LensModel, "镜头型号");
    push_field(exif, &mut g, Tag::FocalLength, "焦距");
    push_field(exif, &mut g, Tag::FocalLengthIn35mmFilm, "等效 35mm 焦距");
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "镜头".into(),
            items: g,
        });
    }

    // === 图像属性 ===
    let mut g = Vec::new();
    push_field(exif, &mut g, Tag::ImageWidth, "图像宽度");
    push_field(exif, &mut g, Tag::ImageLength, "图像高度");
    push_field(exif, &mut g, Tag::XResolution, "水平分辨率");
    push_field(exif, &mut g, Tag::YResolution, "垂直分辨率");
    push_enum(
        exif,
        &mut g,
        Tag::ResolutionUnit,
        "分辨率单位",
        resolution_unit_cn,
    );
    push_field(exif, &mut g, Tag::ColorSpace, "色彩空间");
    push_field(exif, &mut g, Tag::PixelXDimension, "Exif 像素宽度");
    push_field(exif, &mut g, Tag::PixelYDimension, "Exif 像素高度");
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "图像属性".into(),
            items: g,
        });
    }

    // === GPS ===
    let mut g = Vec::new();
    push_gps_latitude(exif, &mut g);
    push_gps_longitude(exif, &mut g);
    push_field(exif, &mut g, Tag::GPSAltitude, "海拔");
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "GPS".into(),
            items: g,
        });
    }

    // === 时间 ===
    let mut g = Vec::new();
    push_field(exif, &mut g, Tag::DateTime, "修改时间");
    push_field(exif, &mut g, Tag::DateTimeOriginal, "拍摄时间");
    push_field(exif, &mut g, Tag::DateTimeDigitized, "数字化时间");
    if !g.is_empty() {
        groups.push(ExifGroup {
            title: "时间".into(),
            items: g,
        });
    }

    groups
}

/// 追加普通字段（display_value 带单位转字符串）
fn push_field(exif: &Exif, items: &mut Vec<ExifItem>, tag: Tag, label: &str) {
    if let Some(f) = field_of(exif, tag) {
        let value = f.display_value().with_unit(exif).to_string();
        let value = value.trim().to_string();
        if !value.is_empty() && value != "\"\"" {
            items.push(ExifItem {
                label: label.into(),
                value,
            });
        }
    }
}

/// 追加枚举型字段（值先过中文映射，未映射的回退原始数字）
fn push_enum(
    exif: &Exif,
    items: &mut Vec<ExifItem>,
    tag: Tag,
    label: &str,
    map: fn(u32) -> Option<&'static str>,
) {
    let Some(f) = field_of(exif, tag) else {
        return;
    };
    let v = f.value.get_uint(0).map(map).flatten().map(str::to_string);
    if let Some(v) = v {
        items.push(ExifItem {
            label: label.into(),
            value: v,
        });
    }
}

/// 方向（Orientation）：1-8 中文映射（EXIF 方向标准值）
fn push_orientation(exif: &Exif, items: &mut Vec<ExifItem>) {
    let Some(f) = field_of(exif, Tag::Orientation) else {
        return;
    };
    let cn = f.value.get_uint(0).and_then(|v| match v {
        1 => Some("正常（水平）"),
        2 => Some("水平镜像"),
        3 => Some("旋转 180°"),
        4 => Some("垂直镜像"),
        5 => Some("旋转 90° + 水平镜像"),
        6 => Some("顺时针旋转 90°"),
        7 => Some("旋转 270° + 水平镜像"),
        8 => Some("顺时针旋转 270°"),
        _ => None,
    });
    if let Some(v) = cn {
        items.push(ExifItem {
            label: "方向".into(),
            value: v.into(),
        });
    }
}

/// GPS 纬度：十进制度 + 度分秒（南纬取负）
fn push_gps_latitude(exif: &Exif, items: &mut Vec<ExifItem>) {
    if let Some(v) = gps_decimal(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef) {
        let dir = if v.is_sign_negative() {
            "南纬"
        } else {
            "北纬"
        };
        items.push(ExifItem {
            label: "纬度".into(),
            value: format!(
                "{} {}°（{}）",
                dir,
                format!("{:.6}", v.abs()),
                dms_string(exif, Tag::GPSLatitude)
            ),
        });
    }
}

/// GPS 经度：十进制度 + 度分秒（西经取负）
fn push_gps_longitude(exif: &Exif, items: &mut Vec<ExifItem>) {
    if let Some(v) = gps_decimal(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef) {
        let dir = if v.is_sign_negative() {
            "西经"
        } else {
            "东经"
        };
        items.push(ExifItem {
            label: "经度".into(),
            value: format!(
                "{} {}°（{}）",
                dir,
                format!("{:.6}", v.abs()),
                dms_string(exif, Tag::GPSLongitude)
            ),
        });
    }
}

/// GPS 坐标 → 带符号十进制度（Ref 为 S/W 时取负；无 Ref 按正值）
fn gps_decimal(exif: &Exif, tag: Tag, ref_tag: Tag) -> Option<f64> {
    let f = field_of(exif, tag)?;
    let Value::Rational(v) = &f.value else {
        return None;
    };
    if v.len() < 3 {
        return None;
    }
    let deg = v[0].to_f64();
    let min = v[1].to_f64();
    let sec = v[2].to_f64();
    let mut dec = deg + min / 60.0 + sec / 3600.0;
    // Ref："S" / "W" 取负（值为 ASCII，如 "N\0"）
    if let Some(r) = field_of(exif, ref_tag) {
        let s = r.display_value().to_string();
        if s.contains('S') || s.contains('W') {
            dec = -dec;
        }
    }
    Some(dec)
}

/// GPS 坐标 → 度分秒字符串（如 39° 54′ 30.60″）
fn dms_string(exif: &Exif, tag: Tag) -> String {
    let empty = String::new();
    let f = match field_of(exif, tag) {
        Some(f) => f,
        None => return empty,
    };
    let Value::Rational(v) = &f.value else {
        return empty;
    };
    if v.len() < 3 {
        return empty;
    }
    format!(
        "{}° {}′ {}″",
        v[0].to_f64(),
        v[1].to_f64(),
        format!("{:.2}", v[2].to_f64())
    )
}

// ==================== 枚举值中文映射 ====================

/// 曝光程序（ExposureProgram 0-8）
fn exposure_program_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        0 => "未定义",
        1 => "手动",
        2 => "程序自动",
        3 => "光圈优先",
        4 => "快门优先",
        5 => "创意自动",
        6 => "动作模式",
        7 => "人像模式",
        8 => "风景模式",
        _ => return None,
    })
}

/// 测光模式（MeteringMode 0-6）
fn metering_mode_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        0 => "未知",
        1 => "平均测光",
        2 => "中央重点测光",
        3 => "点测光",
        4 => "多区测光",
        5 => "评价测光",
        6 => "局部测光",
        _ => return None,
    })
}

/// 闪光灯（Flash 常见组合值）
fn flash_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        0x00 => "未闪光",
        0x01 => "已闪光",
        0x05 => "已闪光（返回光未检测）",
        0x07 => "已闪光（返回光检测到）",
        0x09 => "已闪光（补光模式）",
        0x10 => "闪光灯关闭",
        0x18 => "自动模式，未闪光",
        0x19 => "自动模式，已闪光",
        0x41 => "已闪光（红眼消除）",
        0x65 => "已闪光（补光模式，红眼消除）",
        _ => return None,
    })
}

/// 曝光模式（ExposureMode 0-2）
fn exposure_mode_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        0 => "自动曝光",
        1 => "手动曝光",
        2 => "包围曝光",
        _ => return None,
    })
}

/// 白平衡（WhiteBalance 0/1）
fn white_balance_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        0 => "自动白平衡",
        1 => "手动白平衡",
        _ => return None,
    })
}

/// 分辨率单位（ResolutionUnit 1-3）
fn resolution_unit_cn(v: u32) -> Option<&'static str> {
    Some(match v {
        1 => "无单位",
        2 => "英寸",
        3 => "厘米",
        _ => return None,
    })
}

/// 跨 IFD 查找字段（EXIF 字段分布：Make/Model 等在 IFD0，曝光参数在 Exif SubIFD，
/// GPS 在 GPS IFD；遍历全部字段按 tag 匹配，不依赖字段的标准存放位置）
fn field_of<'a>(exif: &'a Exif, tag: Tag) -> Option<&'a Field> {
    exif.fields().find(|f| f.tag == tag)
}
