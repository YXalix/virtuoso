//! bpf-run library surface: a miniature BTF parser and JSON decoding (pure
//! Rust, testable on the host).
//!
//! aya-obj's BTF introspection is not public (member accessors are
//! pub(crate)), and full BTF parsing far exceeds this tool's needs; here the
//! .BTF section is read directly per Documentation/btf.rst (little-endian v1
//! format), covering only the shapes event decoding needs: Int / Array /
//! Struct / Enum / Enum64 / Ptr / Typedef with cv-qualifier pass-through /
//! Var / DataSec (ringbuf map discovery).
//! Bitfield members are skipped (other fields decode as usual); unions and
//! unsupported shapes fall back to hex.

pub mod btf;
