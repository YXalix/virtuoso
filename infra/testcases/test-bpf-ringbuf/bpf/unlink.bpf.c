// unlink.bpf.c — programmatic eBPF pilot program: kprobe do_unlinkat, then
// ringbuf events.
//
// Self-contained design: no kernel uapi headers are included (the .bpf.o is
// built on the host, which has no linux header tree); only minimal type/
// macro/helper declarations are laid down. The helper function pointer
// values are the BPF_FUNC_* ABI ids from uapi bpf.h (frozen numbers); the
// clang BPF backend lowers "calls through constant function pointers" to
// call imm. No CO-RE relocations (no dereferencing kernel struct fields),
// so the loading side needs no vmlinux BTF. Build: clang -target bpf -g -O2
// (-g emits BTF so the SEC(".maps") BTF map definitions can be parsed by
// aya).
//
// Event semantics: the test case itself unlinks a file to fire the kprobe;
// the event received over the ringbuf must have pid equal to this process's
// pid — the minimal deterministic proof of the full load, attach, trigger,
// return chain.

typedef unsigned int __u32;
typedef unsigned long long __u64;

#define SEC(NAME) __attribute__((section(NAME), used))
#define __uint(name, val) int (*name)[val]

/* eBPF helper function pointers (uapi helper ABI ids) */
static void *(*bpf_ringbuf_reserve)(void *ringbuf, __u64 size, __u64 flags) = (void *)131;
static void (*bpf_ringbuf_submit)(void *data, __u64 flags) = (void *)132;
static __u64 (*bpf_get_current_pid_tgid)(void) = (void *)14;
static long (*bpf_get_current_comm)(void *buf, __u32 size) = (void *)16;

/* BPF_MAP_TYPE_RINGBUF = 27 (uapi bpf.h, frozen number) */
struct {
    __uint(type, 27);
    __uint(max_entries, 1 << 16);
} events SEC(".maps");

struct event {
    __u32 pid;
    char comm[16];
};

SEC("kprobe/do_unlinkat")
int on_unlink(void *ctx)
{
    struct event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e)
        return 0;
    e->pid = (__u32)(bpf_get_current_pid_tgid() >> 32);
    bpf_get_current_comm(e->comm, sizeof(e->comm));
    bpf_ringbuf_submit(e, 0);
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
