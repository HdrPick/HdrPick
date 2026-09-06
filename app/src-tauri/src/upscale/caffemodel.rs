//! Caffe 模型文件（.caffemodel, protobuf）极简解析器
//!
//! 只解析推理所需的字段（字段号来自 Caffe 官方 caffe.proto）：
//! - NetParameter.layer (field 100) → LayerParameter
//! - LayerParameter: name(1)/type(2)/bottom(3)/top(4)/blobs(7)/
//!   convolution_param(100)/relu_param(118)/eltwise_param(110)
//! - ConvolutionParameter: num_output(1)/pad(3)/kernel_size(4)/group(5)/
//!   stride(6)/dilation(18)
//! - BlobProto: data(5, packed float)/shape(7 → BlobShape.dim(1, packed int64))
//!
//! 未识别字段一律跳过（protobuf 向前兼容），读到权重与层类型即止。

// ==================== protobuf wire format 通用读取 ====================

/// protobuf 读取器（光标式）
struct PbReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PbReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn eof(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn read_varint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None; // varint 超长，损坏
            }
        }
    }
    /// 读字段头：返回 (field_number, wire_type)
    fn read_tag(&mut self) -> Option<(u32, u8)> {
        let v = self.read_varint()?;
        Some(((v >> 3) as u32, (v & 0x7) as u8))
    }
    fn read_length_delimited(&mut self) -> Option<&'a [u8]> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.buf.len() {
            return None;
        }
        let s = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Some(s)
    }
    fn read_fixed32(&mut self) -> Option<u32> {
        if self.pos + 4 > self.buf.len() {
            return None;
        }
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().ok()?);
        self.pos += 4;
        Some(v)
    }
    fn read_fixed64(&mut self) -> Option<u64> {
        if self.pos + 8 > self.buf.len() {
            return None;
        }
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().ok()?);
        self.pos += 8;
        Some(v)
    }
    /// 跳过未知字段（按 wire type）
    fn skip(&mut self, wire: u8) -> Option<()> {
        match wire {
            0 => {
                self.read_varint()?;
            }
            1 => {
                self.read_fixed64()?;
            }
            2 => {
                self.read_length_delimited()?;
            }
            5 => {
                self.read_fixed32()?;
            }
            _ => return None, // 3/4 组已废弃
        }
        Some(())
    }
}

// ==================== 解析结果结构 ====================

/// 权重张量（float32，CHW 或 NCHW 布局）
#[derive(Clone, Debug)]
pub struct Blob {
    /// 形状（如 [out_ch, in_ch, kh, kw]）
    pub shape: Vec<usize>,
    /// 数据（shape 乘积个 float）
    pub data: Vec<f32>,
}

/// 卷积/反卷积参数
#[derive(Clone, Copy, Debug, Default)]
pub struct ConvParam {
    pub num_output: u32,
    pub kernel_size: u32,
    pub stride: u32,
    pub pad: u32,
    pub dilation: u32,
    pub group: u32,
}

/// 池化参数（仅 global AVE 用于 SE 块）
#[derive(Clone, Copy, Debug, Default)]
pub struct PoolingParam {
    /// 0=MAX 1=AVE 2=STOCHASTIC
    pub pool: u32,
    pub global: bool,
}

/// Crop 参数（cunet：crop bottom0 到 bottom1 形状，axis/offset）
#[derive(Clone, Debug, Default)]
pub struct CropParam {
    pub axis: u32,
    pub offset: u32,
}

/// CropCenter 参数（upresnet10：offset[2]/offset[3] = H/W 每边裁剪量）
#[derive(Clone, Debug, Default)]
pub struct CropCenterParam {
    pub offsets: Vec<u32>,
}

/// 单层定义
#[derive(Clone, Debug)]
pub struct LayerDef {
    pub name: String,
    pub layer_type: String,
    pub bottoms: Vec<String>,
    pub tops: Vec<String>,
    pub conv: Option<ConvParam>,
    pub pooling: Option<PoolingParam>,
    pub crop: Option<CropParam>,
    pub crop_center: Option<CropCenterParam>,
    /// leaky ReLU 斜率
    pub negative_slope: f32,
    /// 权重 blobs（conv: [weight, bias]）
    pub blobs: Vec<Blob>,
}

/// 网络
#[derive(Clone, Debug, Default)]
pub struct NetDef {
    pub name: String,
    pub layers: Vec<LayerDef>,
}

// ==================== BlobProto 解析 ====================

/// BlobShape.dim (field 1, packed int64)
fn parse_blob_shape(buf: &[u8]) -> Vec<usize> {
    let mut dims = Vec::new();
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        if field == 1 && wire == 2 {
            // packed varint
            if let Some(bytes) = r.read_length_delimited() {
                let mut pr = PbReader::new(bytes);
                while !pr.eof() {
                    if let Some(v) = pr.read_varint() {
                        dims.push(v as usize);
                    } else {
                        break;
                    }
                }
            }
        } else if field == 1 && wire == 0 {
            if let Some(v) = r.read_varint() {
                dims.push(v as usize);
            }
        } else {
            if r.skip(wire).is_none() {
                break;
            }
        }
    }
    dims
}

/// BlobProto：data(5, packed float) / shape(7)
fn parse_blob(buf: &[u8]) -> Blob {
    let mut data: Vec<f32> = Vec::new();
    let mut shape: Vec<usize> = Vec::new();
    let mut legacy: Vec<usize> = Vec::new(); // 旧版字段 num(1)/channels(2)/height(3)/width(4)

    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        match (field, wire) {
            (5, 2) => {
                // packed float
                if let Some(bytes) = r.read_length_delimited() {
                    data.reserve(bytes.len() / 4);
                    for chunk in bytes.chunks_exact(4) {
                        data.push(f32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                }
            }
            (5, 5) => {
                // 非打包 float
                if let Some(v) = r.read_fixed32() {
                    data.push(f32::from_bits(v));
                }
            }
            (7, 2) => {
                if let Some(bytes) = r.read_length_delimited() {
                    shape = parse_blob_shape(bytes);
                }
            }
            // 旧版 BlobProto 兼容（无 shape 字段时）
            (1, 0) => {
                if let Some(v) = r.read_varint() {
                    legacy.insert(0, v as usize);
                }
            }
            (2, 0) => {
                if let Some(v) = r.read_varint() {
                    legacy.push(v as usize);
                }
            }
            (3, 0) => {
                if let Some(v) = r.read_varint() {
                    legacy.push(v as usize);
                }
            }
            (4, 0) => {
                if let Some(v) = r.read_varint() {
                    legacy.push(v as usize);
                }
            }
            _ => {
                if r.skip(wire).is_none() {
                    break;
                }
            }
        }
    }
    if shape.is_empty() && !legacy.is_empty() {
        shape = legacy;
    }
    // 校验：data 长度应等于 shape 乘积（形状缺失时反推）
    let prod: usize = shape.iter().product();
    if shape.is_empty() && !data.is_empty() {
        shape = vec![data.len()];
    } else if prod == 0 && !data.is_empty() {
        log::warn!("[caffemodel] blob {} 形状异常 {:?}", data.len(), shape);
    }
    Blob { shape, data }
}

// ==================== ConvolutionParameter 解析 ====================

fn parse_conv_param(buf: &[u8]) -> ConvParam {
    let mut p = ConvParam {
        stride: 1,
        dilation: 1,
        group: 1,
        ..Default::default()
    };
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        let val = if wire == 0 { r.read_varint() } else { None };
        match field {
            1 => p.num_output = val.unwrap_or(0) as u32, // num_output
            3 => p.pad = val.unwrap_or(0) as u32,        // pad
            4 => p.kernel_size = val.unwrap_or(0) as u32, // kernel_size
            5 => p.group = val.unwrap_or(1).max(1) as u32, // group
            6 => p.stride = val.unwrap_or(1).max(1) as u32, // stride
            18 => p.dilation = val.unwrap_or(1).max(1) as u32, // dilation
            _ => {}
        }
        if val.is_none() {
            if r.skip(wire).is_none() {
                break;
            }
        }
    }
    p
}

// ==================== Pooling/Crop 参数解析 ====================

/// PoolingParameter: pool(1)/kernel_size(2)/stride(3)/pad(4)/global_pooling(12)
fn parse_pooling_param(buf: &[u8]) -> PoolingParam {
    let mut p = PoolingParam::default();
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        let val = if wire == 0 { r.read_varint() } else { None };
        match field {
            1 => p.pool = val.unwrap_or(0) as u32,
            12 => p.global = val.unwrap_or(0) != 0,
            _ => {}
        }
        if val.is_none() {
            if r.skip(wire).is_none() {
                break;
            }
        }
    }
    p
}

/// CropParameter: axis(1)/offset(2, repeated uint32——取首个空间维偏移)
fn parse_crop_param(buf: &[u8]) -> CropParam {
    let mut p = CropParam::default();
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        match (field, wire) {
            (1, 0) => {
                if let Some(v) = r.read_varint() {
                    p.axis = v as u32;
                }
            }
            (2, 0) => {
                // repeated：首个值 = 空间维（axis=2 起）的起始偏移
                if let Some(v) = r.read_varint() {
                    if p.offset == 0 {
                        p.offset = v as u32;
                    }
                }
            }
            (2, 2) => {
                // packed varint
                if let Some(bytes) = r.read_length_delimited() {
                    let mut pr = PbReader::new(bytes);
                    while !pr.eof() {
                        if let Some(v) = pr.read_varint() {
                            if p.offset == 0 {
                                p.offset = v as u32;
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
            _ => {
                if r.skip(wire).is_none() {
                    break;
                }
            }
        }
    }
    p
}

/// CropCenterParameter（自定义）: offset(2, repeated) —— [n,c,h,w]，h/w = 每边裁剪量
fn parse_crop_center_param(buf: &[u8]) -> CropCenterParam {
    let mut p = CropCenterParam::default();
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        match (field, wire) {
            (2, 0) => {
                if let Some(v) = r.read_varint() {
                    p.offsets.push(v as u32);
                }
            }
            (2, 2) => {
                if let Some(bytes) = r.read_length_delimited() {
                    let mut pr = PbReader::new(bytes);
                    while !pr.eof() {
                        match pr.read_varint() {
                            Some(v) => p.offsets.push(v as u32),
                            None => break,
                        }
                    }
                }
            }
            _ => {
                if r.skip(wire).is_none() {
                    break;
                }
            }
        }
    }
    p
}

// ==================== LayerParameter 解析 ====================

fn parse_layer(buf: &[u8]) -> LayerDef {
    let mut def = LayerDef {
        name: String::new(),
        layer_type: String::new(),
        bottoms: Vec::new(),
        tops: Vec::new(),
        conv: None,
        pooling: None,
        crop: None,
        crop_center: None,
        negative_slope: 0.0,
        blobs: Vec::new(),
    };
    let mut r = PbReader::new(buf);
    while let Some((field, wire)) = r.read_tag() {
        match (field, wire) {
            (1, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    def.name = String::from_utf8_lossy(b).into_owned();
                }
            }
            (2, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    def.layer_type = String::from_utf8_lossy(b).into_owned();
                }
            }
            (3, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    def.bottoms.push(String::from_utf8_lossy(b).into_owned());
                }
            }
            (4, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    def.tops.push(String::from_utf8_lossy(b).into_owned());
                }
            }
            (7, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    def.blobs.push(parse_blob(b));
                }
            }
            (106, 2) => {
                // ConvolutionParameter（caffe.proto 字段号 106）
                if let Some(b) = r.read_length_delimited() {
                    def.conv = Some(parse_conv_param(b));
                }
            }
            (121, 2) => {
                // PoolingParameter（本 caffe 构建：pool=1(AVE)、kernel=2、stride=3、pad=4、global_pooling=12）
                if let Some(b) = r.read_length_delimited() {
                    def.pooling = Some(parse_pooling_param(b));
                }
            }
            (144, 2) => {
                // CropParameter（axis=1、offset=2 repeated）
                if let Some(b) = r.read_length_delimited() {
                    def.crop = Some(parse_crop_param(b));
                }
            }
            (149, 2) => {
                // CropCenterParameter（自定义层：offset=2 repeated [n,c,h,w]）
                if let Some(b) = r.read_length_delimited() {
                    def.crop_center = Some(parse_crop_center_param(b));
                }
            }
            (123, 2) => {
                // ReLUParameter.negative_slope (field 1, float)
                if let Some(b) = r.read_length_delimited() {
                    let mut rr = PbReader::new(b);
                    while let Some((f, w)) = rr.read_tag() {
                        if f == 1 && w == 5 {
                            if let Some(v) = rr.read_fixed32() {
                                def.negative_slope = f32::from_bits(v);
                            }
                        } else if rr.skip(w).is_none() {
                            break;
                        }
                    }
                }
            }
            _ => {
                if r.skip(wire).is_none() {
                    break;
                }
            }
        }
    }
    def
}

// ==================== NetParameter 解析 ====================

/// 解析 .caffemodel 全量字节 → NetDef
pub fn parse_caffemodel(bytes: &[u8]) -> Result<NetDef, String> {
    let mut net = NetDef::default();
    let mut r = PbReader::new(bytes);
    while let Some((field, wire)) = r.read_tag() {
        match (field, wire) {
            (1, 2) => {
                if let Some(b) = r.read_length_delimited() {
                    net.name = String::from_utf8_lossy(b).into_owned();
                }
            }
            (100, 2) => {
                // 新版 NetParameter.layer
                if let Some(b) = r.read_length_delimited() {
                    net.layers.push(parse_layer(b));
                }
            }
            (2, 2) => {
                // 旧版 NetParameter.layers（V1LayerParameter）——字段布局不同，
                // waifu2x 模型均为新版，遇到记日志跳过
                let _ = r.read_length_delimited();
                log::warn!("[caffemodel] 遇到 V1 layers 字段（旧版模型），暂不支持");
            }
            _ => {
                if r.skip(wire).is_none() {
                    break;
                }
            }
        }
    }
    if net.layers.is_empty() {
        return Err("caffemodel 未解析到任何 layer".into());
    }
    Ok(net)
}

/// 便捷：读文件并解析
pub fn load_caffemodel(path: &std::path::Path) -> Result<NetDef, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败 {}: {}", path.display(), e))?;
    parse_caffemodel(&bytes)
}
