//! bpf-run 库面：BTF 迷你解析器与 JSON 解码（纯 Rust，宿主可测）。
//!
//! aya-obj 的 BTF 内省不对外（成员访问器 pub(crate)），完整 BTF 解析又远超
//! 本工具所需；这里按 Documentation/btf.rst 直读 .BTF 段（小端 v1 格式），
//! 只覆盖事件解码所需形态：Int / Array / Struct / Enum / Enum64 / Ptr /
//! Typedef 与 cv 限定符穿透 / Var / DataSec（ringbuf 地图发现）。
//! 位域成员跳过（其余字段照常解码）；Union 与未支持形态 hex 兜底。

pub mod btf;
