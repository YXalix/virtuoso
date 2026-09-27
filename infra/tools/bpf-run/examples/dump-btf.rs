use object::{Object, ObjectSection};

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let bytes = std::fs::read(&path).unwrap();
    let elf = object::File::parse(&bytes[..]).unwrap();
    let sec = elf.section_by_name(".BTF").expect("no .BTF");
    let btf = bpf_run::btf::Btf::parse(sec.data().unwrap()).unwrap();
    let (id, size) = btf.comp_by_name("event").unwrap();
    let mut rec = vec![0x44u8, 0x33, 0x22, 0x11];
    rec.extend_from_slice(b"hello");
    rec.resize(size as usize, 0);
    println!("event id={id} decoded = {}", btf.decode(id, &rec, 0));
}
