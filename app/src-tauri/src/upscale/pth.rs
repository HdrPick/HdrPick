//! PyTorch checkpoint（.pth）极简解析器：Real-ESRGAN 权重加载
//!
//! 格式：zip 容器（archive/data.pkl = pickle 序列化的 state_dict +
//! archive/data/<storage_key> = 原始张量字节 + archive/version）。
//!
//! 本解析器只实现本用例所需的 pickle opcode 子集（协议 2-5）：
//! dict/unicode/int/tuple/memo/REDUCE(张量重建)/BINPERSID(storage 引用)。
//! 目标产物：`{参数名: (shape, dtype, storage_key, storage_offset)}`，
//! 张量字节按需从 zip 惰性读取。

use std::collections::HashMap;
use std::io::Read;

/// 张量元数据（数据留在 zip 内按需读）
#[derive(Clone, Debug)]
pub struct PthTensor {
    pub shape: Vec<usize>,
    pub dtype: String, // "f32" 等
    pub storage_key: String,
    pub storage_offset: usize, // 元素单位
}

/// storage 持久引用（pickle BINPERSID 的 pid 元组解包）
#[derive(Clone, Debug)]
struct StorageRef {
    type_name: String, // torch.FloatStorage / torch.HalfStorage / ...
    key: String,
    numel: i64,
}

/// pickle VM 值
#[derive(Clone, Debug)]
enum PVal {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<PVal>),
    List(Vec<PVal>),
    Dict(Vec<(PVal, PVal)>),
    Storage(StorageRef),
    Tensor(PthTensor),
    Global(String, String),
    Mark,
}

impl PVal {
    fn type_name(&self) -> &'static str {
        match self {
            PVal::None => "None",
            PVal::Bool(_) => "bool",
            PVal::Int(_) => "int",
            PVal::Float(_) => "float",
            PVal::Str(_) => "str",
            PVal::Bytes(_) => "bytes",
            PVal::Tuple(_) => "tuple",
            PVal::List(_) => "list",
            PVal::Dict(_) => "dict",
            PVal::Storage(_) => "storage",
            PVal::Tensor(_) => "tensor",
            PVal::Global(..) => "global",
            PVal::Mark => "mark",
        }
    }
}

/// 已加载的 .pth（持有 zip 归档；张量数据惰性读取）
pub struct PthFile {
    archive: zip::ZipArchive<std::fs::File>,
    tensors: HashMap<String, PthTensor>,
}

impl PthFile {
    /// 打开并解析 state_dict
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("打开 pth 失败: {}", e))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|e| format!("pth 非 zip 容器: {}", e))?;

        let pkl = read_entry(&mut archive, "archive/data.pkl")?;
        let top = unpickle(&pkl)?;

        let tensors = match top {
            PVal::Dict(items) => {
                let mut map = HashMap::new();
                for (k, v) in items {
                    let name = match k {
                        PVal::Str(s) => s,
                        other => {
                            return Err(format!("state_dict 键非字符串: {}", other.type_name()))
                        }
                    };
                    // torch.save 携带的顶层版本元数据（{"version": n}）——跳过
                    if name == "_metadata" {
                        continue;
                    }
                    match v {
                        // 平铺布局：{name: tensor}
                        PVal::Tensor(t) => {
                            map.insert(name, t);
                        }
                        // BasicSR 布局：{"params_ema": OrderedDict({name: tensor})}
                        PVal::Dict(pairs) => {
                            for (n2, v2) in pairs {
                                // torch state_dict 携带的 _metadata（版本信息 dict）——跳过
                                if matches!(&n2, PVal::Str(s) if s == "_metadata") {
                                    continue;
                                }
                                match (n2, v2) {
                                    (PVal::Str(n), PVal::Tensor(t)) => {
                                        map.insert(n, t);
                                    }
                                    (nk, nv) => {
                                        let kd = match nk {
                                            PVal::Str(s) => s.clone(),
                                            other => format!("<{}>", other.type_name()),
                                        };
                                        let n_dict = match nv {
                                            PVal::Dict(d) => format!(
                                                "dict[{}] 首键 {:?}",
                                                d.len(),
                                                d.first().map(|(k, _)| match k {
                                                    PVal::Str(s) => s.clone(),
                                                    o => format!("<{}>", o.type_name()),
                                                })
                                            ),
                                            other => format!("<{}>", other.type_name()),
                                        };
                                        return Err(format!(
                                            "参数组 {} 条目 {} = {}",
                                            name, kd, n_dict
                                        ));
                                    }
                                }
                            }
                        }
                        other => {
                            return Err(format!(
                                "参数 {} 非张量（{}）——pth 可能是完整模型而非 state_dict",
                                name,
                                other.type_name()
                            ))
                        }
                    }
                }
                map
            }
            other => {
                return Err(format!(
                    "pth 顶层非 dict（{}）——不支持该 checkpoint 结构",
                    other.type_name()
                ))
            }
        };
        Ok(PthFile { archive, tensors })
    }

    /// 参数名列表（排序）
    pub fn names(&self) -> Vec<&String> {
        let mut v: Vec<&String> = self.tensors.keys().collect();
        v.sort();
        v
    }

    pub fn tensor(&self, name: &str) -> Option<&PthTensor> {
        self.tensors.get(name)
    }

    /// 读取 f32 张量数据（CHW 连续；storage_offset 已应用）
    pub fn get_f32(&mut self, name: &str) -> Result<Vec<f32>, String> {
        let t = self
            .tensors
            .get(name)
            .ok_or_else(|| format!("pth 缺少参数 {}", name))?;
        if t.dtype != "f32" {
            return Err(format!("参数 {} 非f32（{}）", name, t.dtype));
        }
        let total: usize = t.shape.iter().product();
        let raw = read_entry(
            &mut self.archive,
            &format!("archive/data/{}", t.storage_key),
        )?;
        let avail = raw.len() / 4;
        if t.storage_offset + total > avail {
            return Err(format!(
                "参数 {} 数据越界: offset {} + {} > {}",
                name, t.storage_offset, total, avail
            ));
        }
        let mut out = Vec::with_capacity(total);
        for i in 0..total {
            let b: [u8; 4] = raw[(t.storage_offset + i) * 4..(t.storage_offset + i) * 4 + 4]
                .try_into()
                .unwrap();
            out.push(f32::from_le_bytes(b));
        }
        Ok(out)
    }
}

fn read_entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<Vec<u8>, String> {
    let mut f = archive
        .by_name(name)
        .map_err(|e| format!("zip 条目 {} 读取失败: {}", name, e))?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)
        .map_err(|e| format!("zip 条目 {} 解压失败: {}", name, e))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pth 解析冒烟：顶层结构 + 前几个参数名
    #[test]
    fn test_pth_open() {
        let root = std::path::Path::new(r"e:\jietu\图片放大器\models");
        let f = root.join("RealESRGAN_x4plus_anime_6B.pth");
        if !f.exists() {
            eprintln!("[pth] 模型缺失，跳过");
            return;
        }
        let mut p = PthFile::open(&f).unwrap();
        let names = p.names();
        eprintln!(
            "[pth] 参数数 {}，前 5：{:?}",
            names.len(),
            &names[..names.len().min(5)]
        );
        assert!(names.len() > 100, "参数过少: {}", names.len());
        assert!(
            names.iter().any(|n| n.as_str() == "conv_first.weight"),
            "缺 conv_first.weight，全部键样例：{:?}",
            &names[..names.len().min(8)]
        );
        let w = p.get_f32("conv_first.weight").unwrap();
        assert_eq!(w.len(), 64 * 3 * 3 * 3);
        eprintln!("[pth] conv_first.weight[0..4] = {:?}", &w[..4]);
    }
}

fn unpickle(data: &[u8]) -> Result<PVal, String> {
    let mut stack: Vec<PVal> = Vec::new();
    let mut memo: Vec<PVal> = Vec::new();
    let mut i = 0usize;

    macro_rules! pop {
        () => {
            stack.pop().ok_or_else(|| format!("pickle 栈下溢 @{}", i))?
        };
    }
    macro_rules! pop_mark {
        () => {{
            let mut args = Vec::new();
            loop {
                match stack.pop() {
                    Some(PVal::Mark) => break,
                    Some(v) => args.push(v),
                    None => return Err(format!("pickle MARK 缺失 @{}", i)),
                }
            }
            args.reverse();
            args
        }};
    }
    fn read_line(data: &[u8], i: &mut usize) -> Result<String, String> {
        let start = *i;
        while *i < data.len() && data[*i] != b'\n' {
            *i += 1;
        }
        if *i >= data.len() {
            return Err("pickle 文本参数截断".into());
        }
        let s = String::from_utf8_lossy(&data[start..*i]).into_owned();
        *i += 1; // 跳过 \n
        Ok(s)
    }

    while i < data.len() {
        let op = data[i];
        i += 1;
        match op {
            0x80 => {
                i += 1; // PROTO 版本号
            }
            0x95 => {
                i += 8; // FRAME 长度（不需校验）
            }
            b'.' => {
                // STOP
                return stack.pop().ok_or_else(|| "pickle 栈空 STOP".to_string());
            }
            b'N' => stack.push(PVal::None),
            0x88 => stack.push(PVal::Bool(true)),
            0x89 => stack.push(PVal::Bool(false)),
            b'(' => stack.push(PVal::Mark),
            b')' => stack.push(PVal::Tuple(Vec::new())),
            b']' => stack.push(PVal::List(Vec::new())),
            b'}' => stack.push(PVal::Dict(Vec::new())),
            b'2' => {
                let v = pop!();
                stack.push(v.clone());
                stack.push(v);
            }
            b'0' => {
                let _ = pop!();
            }
            b'1' => {
                let _ = pop_mark!();
            }
            b'h' => {
                // BINGET：push memo[idx]（重复对象复用）
                let idx = data[i] as usize;
                i += 1;
                let v = memo.get(idx).cloned().filter(|v| !matches!(v, PVal::None));
                match v {
                    Some(v) => stack.push(v),
                    None => return Err(format!("BINGET memo[{}] 未写入 @{}", idx, i)),
                }
            }
            b'j' => {
                // LONG_BINGET
                let idx = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                let v = memo.get(idx).cloned().filter(|v| !matches!(v, PVal::None));
                match v {
                    Some(v) => stack.push(v),
                    None => return Err(format!("LONG_BINGET memo[{}] 未写入 @{}", idx, i)),
                }
            }
            b'q' => {
                let idx = data[i] as usize;
                i += 1;
                if let Some(v) = stack.last() {
                    while memo.len() <= idx {
                        memo.push(PVal::None);
                    }
                    memo[idx] = v.clone();
                }
            }
            b'r' => {
                let idx = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                if let Some(v) = stack.last() {
                    while memo.len() <= idx {
                        memo.push(PVal::None);
                    }
                    memo[idx] = v.clone();
                }
            }
            0x94 => {
                // MEMOIZE
                if let Some(v) = stack.last() {
                    memo.push(v.clone());
                }
            }
            b'K' => {
                stack.push(PVal::Int(data[i] as i64));
                i += 1;
            }
            _ if op == b'M' => {
                let v = u16::from_le_bytes(data[i..i + 2].try_into().unwrap());
                stack.push(PVal::Int(v as i64));
                i += 2;
            }
            b'J' => {
                let v = i32::from_le_bytes(data[i..i + 4].try_into().unwrap());
                stack.push(PVal::Int(v as i64));
                i += 4;
            }
            b'I' | b'L' => {
                let line = read_line(data, &mut i)?;
                let v: i64 = line
                    .trim_end_matches('L')
                    .parse()
                    .map_err(|e| format!("pickle 整数 {:?} 解析失败: {}", line, e))?;
                stack.push(PVal::Int(v));
            }
            0x8a => {
                // LONG1
                let n = data[i] as usize;
                i += 1;
                let v = decode_long_bytes(&data[i..i + n]);
                i += n;
                stack.push(PVal::Int(v));
            }
            0x8b => {
                // LONG4
                let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                let v = decode_long_bytes(&data[i..i + n]);
                i += n;
                stack.push(PVal::Int(v));
            }
            b'G' => {
                let v = f64::from_be_bytes(data[i..i + 8].try_into().unwrap());
                stack.push(PVal::Float(v));
                i += 8;
            }
            b'X' => {
                let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                stack.push(PVal::Str(
                    String::from_utf8_lossy(&data[i..i + n]).into_owned(),
                ));
                i += n;
            }
            0x8c => {
                let n = data[i] as usize;
                i += 1;
                stack.push(PVal::Str(
                    String::from_utf8_lossy(&data[i..i + n]).into_owned(),
                ));
                i += n;
            }
            0x8d => {
                let n = data[i] as usize;
                i += 1;
                stack.push(PVal::Str(
                    String::from_utf8_lossy(&data[i..i + n]).into_owned(),
                ));
                i += n;
            }
            0x8e => {
                let n = u16::from_le_bytes(data[i..i + 2].try_into().unwrap()) as usize;
                i += 2;
                stack.push(PVal::Str(
                    String::from_utf8_lossy(&data[i..i + n]).into_owned(),
                ));
                i += n;
            }
            b'C' => {
                let n = data[i] as usize;
                i += 1;
                stack.push(PVal::Bytes(data[i..i + n].to_vec()));
                i += n;
            }
            b'B' => {
                let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                stack.push(PVal::Bytes(data[i..i + n].to_vec()));
                i += n;
            }
            b't' => {
                let args = pop_mark!();
                stack.push(PVal::Tuple(args));
            }
            0x85 => {
                let a = pop!();
                stack.push(PVal::Tuple(vec![a]));
            }
            0x86 => {
                let b = pop!();
                let a = pop!();
                stack.push(PVal::Tuple(vec![a, b]));
            }
            0x87 => {
                let c = pop!();
                let b = pop!();
                let a = pop!();
                stack.push(PVal::Tuple(vec![a, b, c]));
            }
            b'l' => {
                let args = pop_mark!();
                stack.push(PVal::List(args));
            }
            b'a' => {
                let v = pop!();
                if let Some(PVal::List(l)) = stack.last_mut() {
                    l.push(v);
                }
            }
            b'e' => {
                let args = pop_mark!();
                if let Some(PVal::List(l)) = stack.last_mut() {
                    l.extend(args);
                }
            }
            b's' => {
                let v = pop!();
                let k = pop!();
                if let Some(PVal::Dict(d)) = stack.last_mut() {
                    d.push((k, v));
                }
            }
            b'u' => {
                // SETITEMS：MARK 区为平铺的 key1, v1, key2, v2, ...
                let items = pop_mark!();
                if let Some(PVal::Dict(d)) = stack.last_mut() {
                    let mut it = items.into_iter();
                    while let Some(k) = it.next() {
                        let v = it.next().unwrap_or(PVal::None);
                        d.push((k, v));
                    }
                }
            }
            b'c' => {
                let module = read_line(data, &mut i)?;
                let name = read_line(data, &mut i)?;
                stack.push(PVal::Global(module, name));
            }
            0x93 => {
                // STACK_GLOBAL
                let name = pop!();
                let module = pop!();
                match (module, name) {
                    (PVal::Str(m), PVal::Str(n)) => stack.push(PVal::Global(m, n)),
                    _ => return Err("STACK_GLOBAL 参数非字符串".into()),
                }
            }
            b'Q' => {
                // BINPERSID：pid 元组 ('storage', storage_type, key, location, numel)
                let pid = pop!();
                let pid = match pid {
                    PVal::Tuple(t) => t,
                    other => {
                        return Err(format!("persistent id 非 tuple（{}）", other.type_name()))
                    }
                };
                if pid.len() != 5 {
                    return Err(format!("persistent id 元组长度 {} ≠ 5", pid.len()));
                }
                // pid[1] = storage 类型：torch 以类对象序列化（STACK_GLOBAL → Global），
                // 旧版也有直接字符串的写法
                let type_name = match &pid[1] {
                    PVal::Global(m, n) => format!("{}.{}", m, n),
                    PVal::Str(s) => s.clone(),
                    other => {
                        return Err(format!(
                            "storage 类型非 str/global（{}）",
                            other.type_name()
                        ))
                    }
                };
                let key = match &pid[2] {
                    PVal::Str(s) => s.clone(),
                    other => return Err(format!("storage key 非 str（{}）", other.type_name())),
                };
                let numel = match &pid[4] {
                    PVal::Int(v) => *v,
                    other => return Err(format!("storage numel 非 int（{}）", other.type_name())),
                };
                stack.push(PVal::Storage(StorageRef {
                    type_name,
                    key,
                    numel,
                }));
            }
            b'P' => {
                // PERSID（文本）——旧格式，不支持
                let _line = read_line(data, &mut i)?;
                return Err("旧式文本 PERSID 不支持".into());
            }
            b'R' => {
                // REDUCE：栈顶 = args tuple，其下 = callable（args 是单个 tuple 对象）
                let args = match pop!() {
                    PVal::Tuple(t) => t,
                    other => return Err(format!("REDUCE args 非 tuple（{}）", other.type_name())),
                };
                let global = pop!();
                let global = match global {
                    PVal::Global(m, n) => (m, n),
                    other => return Err(format!("REDUCE 目标非 global（{}）", other.type_name())),
                };
                let val = do_reduce(&global.0, &global.1, args)?;
                stack.push(val);
            }
            b'b' => {
                // BUILD：Tensor 忽略；Dict(setstate) 合并
                let state = pop!();
                if let Some(PVal::Dict(target)) = stack.last_mut() {
                    if let PVal::Dict(new_items) = state {
                        target.extend(new_items);
                    }
                }
                // 其它对象（无状态）忽略
            }
            other => {
                return Err(format!(
                    "不支持的 pickle opcode 0x{:02x} @{}（data.pkl 可能用了新协议特性）",
                    other, i
                ))
            }
        }
    }
    Err("pickle 数据提前结束（无 STOP）".into())
}

fn decode_long_bytes(b: &[u8]) -> i64 {
    if b.is_empty() {
        return 0;
    }
    let mut v: u64 = 0;
    for (k, byte) in b.iter().enumerate() {
        v |= (*byte as u64) << (8 * k);
    }
    // 补码：最高字节的最高位
    let bits = b.len() * 8;
    if bits < 64 && (v >> (bits - 1)) & 1 == 1 {
        v |= !0u64 << bits;
    }
    v as i64
}

/// REDUCE 分派：本用例只关心张量重建链
fn do_reduce(module: &str, name: &str, args: Vec<PVal>) -> Result<PVal, String> {
    match (module, name) {
        ("torch._utils", "_rebuild_tensor_v2") => {
            // args: (storage, storage_offset, size, stride, requires_grad, backward_hooks)
            if args.len() < 4 {
                return Err("_rebuild_tensor_v2 参数不足".into());
            }
            let storage = match &args[0] {
                PVal::Storage(s) => s,
                other => return Err(format!("storage 参数非 Storage（{}）", other.type_name())),
            };
            let offset = match args[1] {
                PVal::Int(v) => v as usize,
                _ => return Err("storage_offset 非整数".into()),
            };
            let shape = match &args[2] {
                PVal::Tuple(t) => t
                    .iter()
                    .map(|v| match v {
                        PVal::Int(n) => Ok(*n as usize),
                        other => Err(format!("尺寸元素非整数（{}）", other.type_name())),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                other => return Err(format!("size 非 tuple（{}）", other.type_name())),
            };
            // stride 忽略（state_dict 张量恒为连续布局）
            let dtype = storage.type_name.clone();
            let dtype = match dtype.rsplit('.').next().unwrap_or(&dtype) {
                "FloatStorage" | "UntypedStorage" => "f32", // UntypedStorage 时类型在 BUILD/persid 之外——Real-ESRGAN 均为 FloatStorage
                "HalfStorage" => "f16",
                "DoubleStorage" => "f64",
                "BFloat16Storage" => "bf16",
                other => return Err(format!("不支持的数据类型 {}", other)),
            };
            Ok(PVal::Tensor(PthTensor {
                shape,
                dtype: dtype.to_string(),
                storage_key: storage.key.clone(),
                storage_offset: offset,
            }))
        }
        ("torch._utils", "_rebuild_parameter") | ("torch.nn.parameter", "Parameter") => {
            // Parameter 包装：透传内部张量
            match args.into_iter().next() {
                Some(PVal::Tensor(t)) => Ok(PVal::Tensor(t)),
                _ => Err("_rebuild_parameter 内部非张量".into()),
            }
        }
        // OrderedDict（dict 子类）：REDUCE 空实例 → MARK+SETITEMS 平铺填充
        ("collections", "OrderedDict") => Ok(PVal::Dict(Vec::new())),
        (m, n) => Err(format!(
            "不支持的 REDUCE 目标 {}.{}（pth 可能是完整模型而非 state_dict）",
            m, n
        )),
    }
}
