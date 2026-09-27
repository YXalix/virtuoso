//! bpf-run 的 BTF 迷你解析器与 JSON 解码。
//!
//! aya-obj 的 BTF 内省不对外（成员访问器 pub(crate)），完整 BTF 解析又远超
//! 本工具所需；这里按 Documentation/btf.rst 直读 .BTF 段（小端 v1 格式），
//! 只覆盖事件解码所需形态：Int / Array / Struct / Enum / Enum64 / Ptr /
//! Typedef 与 cv 限定符穿透 / Var / DataSec（ringbuf 地图发现）。
//! 位域成员跳过（其余字段照常解码）；Union 与未支持形态 hex 兜底。

use serde_json::{json, Map, Value};

pub struct Btf {
    types: Vec<Type>,
}

pub enum Type {
    Void,
    Int {
        size: u32,
        bits: u32,
        signed: bool,
        char: bool,
        boolean: bool,
    },
    Ptr(u32),
    Array {
        elem: u32,
        len: u32,
    },
    Comp {
        kind: CompKind,
        name: String,
        size: u32,
        members: Vec<Member>,
    },
    Enum {
        size: u32,
        variants: Vec<(String, i64)>,
    },
    Typedef {
        name: String,
        ty: u32,
    },
    Modifier(u32),
    Var {
        name: String,
        ty: u32,
    },
    DataSec {
        name: String,
        entries: Vec<(u32, u32, u32)>,
    },
    Float,
    Other,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompKind {
    Struct,
    Union,
}

pub struct Member {
    pub name: String,
    pub ty: u32,
    pub byte_offset: u32,
}

const KIND_VOID: u32 = 0;
const KIND_INT: u32 = 1;
const KIND_PTR: u32 = 2;
const KIND_ARRAY: u32 = 3;
const KIND_STRUCT: u32 = 4;
const KIND_UNION: u32 = 5;
const KIND_ENUM: u32 = 6;
const KIND_FWD: u32 = 7;
const KIND_TYPEDEF: u32 = 8;
const KIND_CONST: u32 = 9;
const KIND_VOLATILE: u32 = 10;
const KIND_RESTRICT: u32 = 11;
const KIND_FUNC: u32 = 12;
const KIND_FUNC_PROTO: u32 = 13;
const KIND_VAR: u32 = 14;
const KIND_DATASEC: u32 = 15;
const KIND_FLOAT: u32 = 16;
const KIND_DECL_TAG: u32 = 17;
const KIND_TYPE_TAG: u32 = 18;
const KIND_ENUM64: u32 = 19;

/// BPF_MAP_TYPE_RINGBUF（uapi bpf.h，数字冻结）
const MAP_TYPE_RINGBUF: u32 = 27;

impl Btf {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 24 {
            return Err("BTF: header truncated".into());
        }
        let u32_at = |off: usize| -> Result<u32, String> {
            data.get(off..off + 4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or_else(|| "BTF: truncated".into())
        };
        let magic = u16::from_le_bytes(
            data[0..2].try_into().expect("bounded by header check"),
        );
        if magic != 0xEB9F {
            return Err(format!("BTF: magic 0x{magic:04X} != 0xEB9F"));
        }
        if data[2] != 1 {
            return Err(format!("BTF: unsupported version {}", data[2]));
        }
        let hdr_len = u32_at(4)? as usize;
        let t_start = hdr_len + u32_at(8)? as usize;
        let t_end = t_start + u32_at(12)? as usize;
        let s_start = hdr_len + u32_at(16)? as usize;
        let s_end = s_start + u32_at(20)? as usize;
        if t_end > data.len() || s_end > data.len() {
            return Err("BTF: section bounds exceed data".into());
        }
        let types = &data[t_start..t_end];
        let strings = data[s_start..s_end].to_vec();
        let tu32 = |off: usize| -> Result<u32, String> {
            types
                .get(off..off + 4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or_else(|| "BTF: type data truncated".into())
        };

        // 类型 id 0 = void 占位
        let mut out: Vec<Type> = vec![Type::Void];
        let mut off = 0usize;
        while off < types.len() {
            let name = string_at(&strings, tu32(off)?);
            let info = tu32(off + 4)?;
            let kind = (info >> 24) & 0x1f;
            let vlen = (info & 0xffff) as usize;
            let kflag = info & 0x8000_0000 != 0;
            let t_or_size = tu32(off + 8)?;
            let pos = off + 12;
            // 各 kind 的定长载荷，先整体定界再读，未知 kind 报错防错位
            let payload = match kind {
                KIND_INT => 4,
                KIND_ARRAY => 12,
                KIND_STRUCT | KIND_UNION => vlen * 12,
                KIND_ENUM => vlen * 8,
                KIND_VAR | KIND_DECL_TAG => 4,
                KIND_DATASEC => vlen * 12,
                KIND_ENUM64 => vlen * 12,
                KIND_FUNC_PROTO => vlen * 8,
                KIND_VOID | KIND_PTR | KIND_FWD | KIND_TYPEDEF | KIND_CONST
                | KIND_VOLATILE | KIND_RESTRICT | KIND_FUNC
                | KIND_FLOAT | KIND_TYPE_TAG => 0,
                other => return Err(format!("BTF: unsupported kind {other}")),
            };
            if pos + payload > types.len() {
                return Err("BTF: type payload out of bounds".into());
            }

            let ty = match kind {
                KIND_INT => {
                    let extra = tu32(pos)?;
                    let enc = (extra >> 24) & 0xf;
                    Type::Int {
                        size: t_or_size,
                        bits: (extra >> 8) & 0x00ff_ffff,
                        signed: enc == 1,
                        char: enc == 2,
                        boolean: enc == 4,
                    }
                }
                KIND_PTR => Type::Ptr(t_or_size),
                KIND_ARRAY => Type::Array {
                    elem: tu32(pos)?,
                    len: tu32(pos + 8)?,
                },
                KIND_STRUCT | KIND_UNION => {
                    let mut members = Vec::new();
                    for i in 0..vlen {
                        let m = pos + i * 12;
                        let m_name = string_at(&strings, tu32(m)?);
                        let m_ty = tu32(m + 4)?;
                        let m_bits = tu32(m + 8)?;
                        // 成员 offset 恒按 bit 读：clang 对 kflag=0 也发 bit
                        // 偏移（实测 map struct max_entries raw=0x40=byte 8）；
                        // kflag=1 时低 24 位同义、高 8 位为位域宽。
                        let (bit_off, bf_size) = if kflag {
                            (m_bits & 0x00ff_ffff, m_bits >> 24)
                        } else {
                            (m_bits, 0)
                        };
                        if bf_size != 0 || m_name.is_empty() {
                            continue; // 位域与匿名成员跳过
                        }
                        members.push(Member {
                            name: m_name,
                            ty: m_ty,
                            byte_offset: bit_off / 8,
                        });
                    }
                    Type::Comp {
                        kind: if kind == KIND_STRUCT {
                            CompKind::Struct
                        } else {
                            CompKind::Union
                        },
                        name,
                        size: t_or_size,
                        members,
                    }
                }
                KIND_ENUM | KIND_ENUM64 => {
                    let stride = if kind == KIND_ENUM { 8 } else { 12 };
                    let mut variants = Vec::new();
                    for i in 0..vlen {
                        let v = pos + i * stride;
                        let v_name = string_at(&strings, tu32(v)?);
                        let value = if kind == KIND_ENUM {
                            i64::from(tu32(v + 4)? as i32)
                        } else {
                            let lo = tu32(v + 4)?;
                            let hi = tu32(v + 8)?;
                            (u64::from(lo) | u64::from(hi) << 32) as i64
                        };
                        variants.push((v_name, value));
                    }
                    Type::Enum {
                        size: t_or_size,
                        variants,
                    }
                }
                KIND_TYPEDEF => Type::Typedef {
                    name,
                    ty: t_or_size,
                },
                KIND_CONST | KIND_VOLATILE | KIND_RESTRICT | KIND_TYPE_TAG => {
                    Type::Modifier(t_or_size)
                }
                KIND_VAR => Type::Var {
                    name,
                    ty: t_or_size,
                },
                KIND_DATASEC => {
                    let mut entries = Vec::new();
                    for i in 0..vlen {
                        let e = pos + i * 12;
                        entries.push((tu32(e)?, tu32(e + 4)?, tu32(e + 8)?));
                    }
                    Type::DataSec {
                        name,
                        entries,
                    }
                }
                KIND_FLOAT => Type::Float,
                _ => Type::Other,
            };
            out.push(ty);
            off = pos + payload;
        }
        Ok(Self { types: out })
    }

    fn get(&self, id: u32) -> &Type {
        self.types.get(id as usize).unwrap_or(&Type::Void)
    }

    /// 穿透 typedef 与 cv 限定符（深度上限防循环）。
    pub fn resolve(&self, mut id: u32) -> u32 {
        for _ in 0..16 {
            match self.get(id) {
                Type::Typedef { ty, .. } | Type::Modifier(ty) => id = *ty,
                _ => break,
            }
        }
        id
    }

    pub fn type_size(&self, id: u32) -> Option<u32> {
        match self.get(self.resolve(id)) {
            Type::Int { size, .. } => Some(*size),
            Type::Comp { size, .. } => Some(*size),
            Type::Enum { size, .. } => Some(*size),
            Type::Array { elem, len } => Some(self.type_size(*elem)? * len),
            Type::Ptr(_) => Some(8),
            _ => None,
        }
    }

    /// 全部复合类型概览（name, is_struct, size）——调试与诊断用。
    pub fn comps(&self) -> Vec<(String, bool, u32)> {
        self.types
            .iter()
            .filter_map(|t| match t {
                Type::Comp { kind, name, size, .. } => {
                    Some((name.clone(), *kind == CompKind::Struct, *size))
                }
                _ => None,
            })
            .collect()
    }

    /// 类型表一行摘要（调试用）：id / kind / name / members。
    pub fn dump_types(&self) -> Vec<String> {
        self.types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let kind = match t {
                    Type::Void => "void".into(),
                    Type::Int { size, bits, .. } => format!("Int(size={size},bits={bits})"),
                    Type::Ptr(_) => "Ptr".into(),
                    Type::Array { elem, len } => format!("Array(elem={elem},len={len})"),
                    Type::Comp { kind, name, size, members } => format!(
                        "Comp({kind:?},name={name:?},size={size},members={:?})",
                        members
                            .iter()
                            .map(|m| (m.name.clone(), m.ty, m.byte_offset))
                            .collect::<Vec<_>>()
                    ),
                    Type::Enum { size, variants } => format!(
                        "Enum(size={size},vars={:?})",
                        variants.iter().map(|v| v.0.clone()).collect::<Vec<_>>()
                    ),
                    Type::Typedef { name, ty } => format!("Typedef({name}->{ty})"),
                    Type::Modifier(ty) => format!("Modifier({ty})"),
                    Type::Var { name, ty } => format!("Var({name}->{ty})"),
                    Type::DataSec { name, entries } => format!("DataSec({name},{entries:?})"),
                    Type::Float => "Float".into(),
                    Type::Other => "Other".into(),
                };
                format!("{i}: {kind}")
            })
            .collect()
    }

    /// 全部具名 struct/union（自动解码候选），返回 (类型 id, 字节大小)。
    pub fn named_comps(&self) -> Vec<(u32, usize)> {
        self.types
            .iter()
            .enumerate()
            .filter_map(|(i, t)| match t {
                Type::Comp { name, .. } if !name.is_empty() => {
                    Some((i as u32, self.type_size(i as u32)? as usize))
                }
                _ => None,
            })
            .collect()
    }

    /// 按名字找 struct/union（含 typedef 别名），返回 (类型 id, 字节大小)。
    pub fn comp_by_name(&self, name: &str) -> Option<(u32, u32)> {
        let direct = self.types.iter().enumerate().position(|(_, t)| {
            matches!(t, Type::Comp { name: n, .. } if n == name)
        });
        let id = match direct {
            Some(i) => i as u32,
            None => self.types.iter().position(|t| {
                matches!(t, Type::Typedef { name: n, .. } if n == name)
            })? as u32,
        };
        let size = self.type_size(id)?;
        Some((id, size))
    }

    /// BTF 定义的 RINGBUF 地图名（.maps datasec → Var → 成员 "type" = 27）。
    pub fn ringbuf_maps(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.types {
            let Type::DataSec { name, entries } = t else {
                continue;
            };
            if name != ".maps" {
                continue;
            }
            for (var_id, _, _) in entries {
                let Type::Var { name: map_name, ty } = self.get(*var_id) else {
                    continue;
                };
                let Type::Comp { members, .. } = self.get(self.resolve(*ty)) else {
                    continue;
                };
                for m in members {
                    if m.name != "type" {
                        continue;
                    }
                    // __uint(type, 27) = int (*type)[27]：Ptr → Array，len 即值
                    let kind = match self.get(self.resolve(m.ty)) {
                        Type::Ptr(p) => match self.get(self.resolve(*p)) {
                            Type::Array { len, .. } => Some(*len),
                            _ => None,
                        },
                        Type::Array { len, .. } => Some(*len),
                        _ => None,
                    };
                    if kind == Some(MAP_TYPE_RINGBUF) {
                        out.push(map_name.clone());
                    }
                }
            }
        }
        out
    }

    /// 事件记录 → JSON 值。深度超限或未支持形态由调用方 hex 兜底。
    pub fn decode(&self, id: u32, data: &[u8], depth: usize) -> Value {
        if depth > 4 {
            return hex(data);
        }
        match self.get(self.resolve(id)) {
            Type::Int {
                size,
                bits,
                signed,
                char: _,
                boolean,
            } => int_value(data, *size, *bits, *signed, *boolean),
            Type::Enum { variants, size } => {
                let raw = int_value(data, *size, 0, false, false);
                if let Value::Number(n) = &raw {
                    let v = n.as_i64().unwrap_or(0);
                    if let Some((name, _)) = variants.iter().find(|(_, val)| *val == v) {
                        return json!(name);
                    }
                }
                raw
            }
            Type::Array { elem, len } => {
                let es = self.type_size(*elem).unwrap_or(0) as usize;
                if es == 0 || data.len() < es * *len as usize {
                    return hex(data);
                }
                // 单字节整型数组 → 字符串（NUL 截断）：char[N] 文本字段，
                // 与 bpftrace 对 char[] 的呈现一致；clang 对 plain char
                // 不打 CHAR 编码位，故按 size==1 判定（bool 数组除外）。
                if let Type::Int { size, boolean, .. } = self.get(self.resolve(*elem)) {
                    if *size == 1 && !*boolean {
                        let end = data[..es * *len as usize]
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(es * *len as usize);
                        return json!(String::from_utf8_lossy(&data[..end]));
                    }
                }
                match self.get(self.resolve(*elem)) {
                    Type::Int { .. } | Type::Enum { .. } | Type::Comp { .. } => Value::Array(
                        (0..*len as usize)
                            .map(|i| self.decode(*elem, &data[i * es..], depth + 1))
                            .collect(),
                    ),
                    _ => hex(data),
                }
            }
            Type::Comp {
                kind: CompKind::Struct,
                members,
                ..
            } => {
                let mut obj = Map::new();
                for m in members {
                    let ms = self.type_size(m.ty).unwrap_or(0) as usize;
                    if m.byte_offset as usize + ms > data.len() || m.name.is_empty() {
                        continue;
                    }
                    obj.insert(
                        m.name.clone(),
                        self.decode(m.ty, &data[m.byte_offset as usize..], depth + 1),
                    );
                }
                Value::Object(obj)
            }
            Type::Ptr(_) => {
                if data.len() < 8 {
                    return hex(data);
                }
                let v = u64::from_le_bytes(data[..8].try_into().expect("bounded"));
                json!(format!("0x{v:016x}"))
            }
            _ => hex(data), // Union、Float、未知形态
        }
    }
}

/// 无符号按字节拼装（小端）+ 位宽截断 + 有符号扩展 + bool/char 语义。
fn int_value(data: &[u8], size: u32, bits: u32, signed: bool, boolean: bool) -> Value {
    let size = (size as usize).clamp(1, 8);
    let mut v = 0u64;
    for (i, b) in data.iter().take(size).enumerate() {
        v |= u64::from(*b) << (8 * i);
    }
    let bits = if bits == 0 { size as u32 * 8 } else { bits.min(size as u32 * 8) };
    if bits < 64 {
        v &= (1u64 << bits) - 1;
    }
    if boolean {
        return Value::Bool(v != 0);
    }
    if signed && bits > 0 {
        if bits < 64 && v & (1 << (bits - 1)) != 0 {
            v |= (!0u64) << bits;
        }
        return json!(v as i64);
    }
    json!(v)
}

fn string_at(strings: &[u8], off: u32) -> String {
    let start = off as usize;
    if start >= strings.len() {
        return String::new();
    }
    let end = strings[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| start + p)
        .unwrap_or(strings.len());
    String::from_utf8_lossy(&strings[start..end]).into_owned()
}

/// 兜底：整段记录 hex（解码器无法表达时的可传输形态）。
pub fn hex(data: &[u8]) -> Value {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    json!(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手搭最小 BTF：struct event { u32 pid; char tag[16]; }（kflag=0，
    /// 成员 offset 为 bit）+ 一个 8 字节成员的匿名 struct，锁死三个实测
    /// 回归点：FUNC_PROTO 载荷步进、成员 bit 偏移、单字节数组按字符串解。
    /// 字节构造后先自证 walk 对齐（dump 长度 = 输入长度）。
    fn fixture() -> Btf {
        // 手搭最小 BTF：struct event { u32 pid; char tag[16]; }（kflag=0，
        // 成员 offset 为 bit）+ 匿名 map struct（成员 type = Ptr→Array(27)，
        // 即 `int (*type)[27]` 的真实形态）+ Var + FuncProto + DataSec。
        // 锁死三个实测回归点：FuncProto 载荷步进（vlen*8）、成员 bit 偏移、
        // 单字节数组按字符串解。字符串表先建，偏移回填类型引用。
        let mut strings: Vec<u8> = vec![0]; // offset 0 = ""
        let put = |s: &mut Vec<u8>, name: &str| -> u32 {
            let off = s.len() as u32;
            s.extend_from_slice(name.as_bytes());
            s.push(0);
            off
        };
        let off_event = put(&mut strings, "event");
        let off_pid = put(&mut strings, "pid");
        let off_tag = put(&mut strings, "tag");
        let off_type = put(&mut strings, "type");
        let off_events = put(&mut strings, "events");
        let off_maps = put(&mut strings, ".maps");

        let mut t: Vec<u8> = Vec::new();
        let w = |t: &mut Vec<u8>, v: u32| t.extend(v.to_le_bytes());
        // id1: Int "unsigned int" size4（enc=0, bits=32）
        w(&mut t, 0);
        w(&mut t, 1 << 24);
        w(&mut t, 4);
        w(&mut t, 32 << 8);
        // id2: Int "char" size1（enc=0——clang 实测不打 CHAR 位）
        w(&mut t, 0);
        w(&mut t, 1 << 24);
        w(&mut t, 1);
        w(&mut t, 8 << 8);
        // id3: Array(elem=2, len=16) —— char[16]
        w(&mut t, 0);
        w(&mut t, 3 << 24);
        w(&mut t, 0);
        w(&mut t, 2); // elem
        w(&mut t, 0); // index
        w(&mut t, 16); // len
        // id4: Struct "event" size20 vlen=2 kflag=0（成员 offset 为 bit）
        w(&mut t, off_event);
        w(&mut t, (4 << 24) | 2);
        w(&mut t, 20);
        w(&mut t, off_pid); // pid: ty=1 @bit0
        w(&mut t, 1);
        w(&mut t, 0);
        w(&mut t, off_tag); // tag: ty=3 @bit32 = byte 4（回归点）
        w(&mut t, 3);
        w(&mut t, 32);
        // id5: Array(elem=1, len=27) —— `int (*type)[27]` 的数组，len 即值
        w(&mut t, 0);
        w(&mut t, 3 << 24);
        w(&mut t, 0);
        w(&mut t, 1); // elem
        w(&mut t, 0); // index
        w(&mut t, 27); // len = BPF_MAP_TYPE_RINGBUF（回归点）
        // id6: Ptr(→5)
        w(&mut t, 0);
        w(&mut t, 2 << 24);
        w(&mut t, 5);
        // id7: Struct "" size16 vlen=1（匿名 map struct，成员 type → Ptr）
        w(&mut t, 0);
        w(&mut t, (4 << 24) | 1);
        w(&mut t, 16);
        w(&mut t, off_type);
        w(&mut t, 6);
        w(&mut t, 0);
        // id8: Var "events" ty=7 linkage=0
        w(&mut t, off_events);
        w(&mut t, 14 << 24);
        w(&mut t, 7);
        w(&mut t, 0);
        // id9: FuncProto vlen=1（回归点：载荷 = vlen*8）
        w(&mut t, 0);
        w(&mut t, (13 << 24) | 1);
        w(&mut t, 0);
        w(&mut t, 0); // param name_off
        w(&mut t, 1); // param ty
        // id10: DataSec ".maps" vlen=1
        w(&mut t, off_maps);
        w(&mut t, (15 << 24) | 1);
        w(&mut t, 16);
        w(&mut t, 8); // entry ty=8 (Var)
        w(&mut t, 0); // offset
        w(&mut t, 16); // size

        let mut blob: Vec<u8> = Vec::new();
        blob.extend(0xEB9Fu16.to_le_bytes());
        blob.push(1); // version
        blob.push(0); // flags
        blob.extend(24u32.to_le_bytes()); // hdr_len
        blob.extend(0u32.to_le_bytes()); // type_off
        blob.extend((t.len() as u32).to_le_bytes());
        blob.extend((t.len() as u32).to_le_bytes()); // str_off 紧随 types
        blob.extend((strings.len() as u32).to_le_bytes());
        blob.extend_from_slice(&t);
        blob.extend_from_slice(&strings);
        Btf::parse(&blob).expect("fixture parses")
    }

    #[test]
    fn walk_aligned_and_names_resolved() {
        let btf = fixture();
        // walk 对齐：FuncProto 载荷步进错就会把后续类型读歪（实测回归点）
        assert_eq!(btf.comp_by_name("event"), Some((4, 20)));
        assert_eq!(btf.ringbuf_maps(), vec!["events".to_string()]);
        // 匿名 struct 不进自动解码候选
        assert_eq!(btf.named_comps(), vec![(4, 20)]);
    }

    #[test]
    fn decodes_pid_and_char_array() {
        let btf = fixture();
        let (id, size) = btf.comp_by_name("event").unwrap();
        let mut rec = vec![0x44, 0x33, 0x22, 0x11]; // pid = 0x11223344 LE
        rec.extend_from_slice(b"hello");
        rec.resize(size as usize, 0);
        let v = btf.decode(id, &rec, 0);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["pid"], 287454020);
        assert_eq!(obj["tag"], "hello"); // 单字节数组按字符串（NUL 截断）
    }

    #[test]
    fn hex_fallback_for_unmatched_records() {
        let btf = fixture();
        let (id, _) = btf.comp_by_name("event").unwrap();
        // 长度不足的记录：可解成员逐个跳过 → 空对象，不 panic 不误读
        let v = btf.decode(id, &[0u8; 3], 0);
        assert!(v.as_object().unwrap().is_empty());
    }
}
