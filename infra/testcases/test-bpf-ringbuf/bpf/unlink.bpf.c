// unlink.bpf.c — 程序态 eBPF 试点程序：kprobe do_unlinkat → ringbuf 事件。
//
// 自包含设计：不 include 内核 uapi 头（.bpf.o 在宿主编译，宿主无 linux 头
// 树），只落最小类型/宏/助手声明。助手函数指针的值 = uapi bpf.h 的
// BPF_FUNC_* ABI id（冻结数字），clang BPF 后端把"常量函数指针调用"降为
// call imm。无 CO-RE 重定位（不解引用内核结构体字段），加载端无需
// vmlinux BTF 参与。编译：clang -target bpf -g -O2（-g 产 BTF，供
// SEC(".maps") 的 BTF map 定义被 aya 解析）。
//
// 事件语义：本用例自身 unlink 一个文件触发 kprobe，ringbuf 收到的事件
// pid 必等于本进程 pid——「加载→挂接→触发→回流」全链的最小确定性证明。

typedef unsigned int __u32;
typedef unsigned long long __u64;

#define SEC(NAME) __attribute__((section(NAME), used))
#define __uint(name, val) int (*name)[val]

/* eBPF 助手函数指针（uapi helper ABI id） */
static void *(*bpf_ringbuf_reserve)(void *ringbuf, __u64 size, __u64 flags) = (void *)131;
static void (*bpf_ringbuf_submit)(void *data, __u64 flags) = (void *)132;
static __u64 (*bpf_get_current_pid_tgid)(void) = (void *)14;
static long (*bpf_get_current_comm)(void *buf, __u32 size) = (void *)16;

/* BPF_MAP_TYPE_RINGBUF = 27（uapi bpf.h，数字冻结） */
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
