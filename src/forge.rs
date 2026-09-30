//! forge — 内核锻造：容器化内核源码供给（named volume + 钉死工具链镜像）。
//!
//! 收编 `devkit/docker/kernel.sh`（薄壳退役，CI 发布的 Dockerfile.kernel
//! 保留在原位）：源码权威存 named volume（容器侧 ext4：大小写敏感 + 构建
//! 性能），宿主经平台视图直接读写——macOS = OrbStack 视图、Linux = volume
//! 本体。`virtuoso kernel` 命令组投影到本模块：clone/defconfig/build/
//! shell + list/use（卷管理与 current 切换，活动卷状态落 repo 根
//! `.virtuoso/kernel-current.json`）。源码编辑走 VS Code devcontainer
//! （容器内 clangd 吃 build 产出的 /ksrc 原始形态 compile_commands.json）。
//!
//! 测试主循环在宿主原生跑，内核树自动跟随活动卷（config 解析序：
//! env KERNEL_PATH > current 活动卷 > toml kernel_path），`virtuoso
//! doctor / build / test` 无需手动指路径。
//!
//! 分节：活动卷状态（current）→ volume 宿主视图与引擎守卫 → 工具链镜像
//! 容器调用 → clone 流水线 → devcontainer 渲染。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::util::quote as shell_quote;
use crate::util::Progress;

use crate::Arch;

/// 缺省卷名（每卷自含源码 + .config + 增量产物，切回免重编）。
pub(crate) const DEFAULT_VOLUME: &str = "virtuoso-kernel";
/// 钉死工具链镜像（kernel-builder.yml 发布 ghcr；pull 失败回落本地构建
/// devkit/docker/Dockerfile.kernel）。
pub(crate) const DEFAULT_IMAGE: &str = "ghcr.io/yxalix/virtuoso-kernel:latest";
/// clone 缺省 ref。
pub(crate) const DEFAULT_REF: &str = "master";

// ---------------------------------------------------------------- 活动卷状态（current）

// current 不进 `virtuoso.toml`（唯一配置面留给持久启动配置），用专门的
// 文件表示"当前是哪个卷"——机器本地会话状态，git 忽略；`kernel use/clone`
// 写入，`path/list` 与 doctor 读它。写 current 的唯一入口 [`write_current`]
// 同步渲染 `.devcontainer/devcontainer.json`（devcontainer 跟随活动卷）。
// 状态文件：`.virtuoso/kernel-current.json`。

/// 当前活动卷（volume + 目标架构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Current {
    pub volume: String,
    pub arch: String,
}

/// 状态文件路径（repo 根 `.virtuoso/kernel-current.json`）。
pub(crate) fn state_path(project_root: &Path) -> PathBuf {
    project_root.join(".virtuoso").join("kernel-current.json")
}

/// 缺失 = 无活动卷（raw/preset 模式，doctor 零打扰）；存在但非法 = 报错。
pub(crate) fn read_current(project_root: &Path) -> anyhow::Result<Option<Current>> {
    let p = state_path(project_root);
    if !p.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("read {} failed", p.display()))?;
    serde_json::from_str(&text).map(Some).with_context(|| {
        format!(
            "{} 损坏（期望 {{volume, arch}}）；重新执行 `virtuoso kernel use <volume>` 修复",
            p.display()
        )
    })
}

/// 写入（目录不存在则创建；父目录 `.virtuoso/` 已 git 忽略）。
/// 同步渲染 `.devcontainer/devcontainer.json`——current 变更必须跟随，
/// 放唯一写入口免得未来新增 writer 漏挂。
pub(crate) fn write_current(project_root: &Path, current: &Current) -> anyhow::Result<()> {
    let p = state_path(project_root);
    std::fs::create_dir_all(p.parent().expect("状态文件必有父目录"))?;
    std::fs::write(&p, format!("{}\n", serde_json::to_string_pretty(current)?))?;
    render(project_root, current)?;
    Ok(())
}

// ---------------------------------------------------------------- volume 宿主视图与引擎守卫

// 源码权威存 named volume，宿主经平台视图直接读写：macOS 走 OrbStack 视图
// （Docker Desktop 的 volume 在 VM 虚拟盘里，宿主不可见）；Linux 上 volume
// 本体就在宿主文件系统，取引擎权威 Mountpoint（rootless 落 $HOME 下，免
// root）。两个平台该路径都是纯宿主路径——测试主循环的 kernel_path（QEMU
// 消费内核镜像）指向它；源码编辑走容器内 devcontainer，不经宿主视图。

/// OrbStack 对 named volume 的宿主视图（OrbStack 运行时存在）。
pub(crate) fn orbstack_view(home: &Path, volume: &str) -> PathBuf {
    home.join("OrbStack")
        .join("docker")
        .join("volumes")
        .join(volume)
}

/// volume 的宿主可见路径。
pub(crate) fn host_view(volume: &str) -> anyhow::Result<PathBuf> {
    match std::env::consts::OS {
        "macos" => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set, cannot locate the OrbStack volume view")?;
            Ok(orbstack_view(&home, volume))
        }
        "linux" => {
            let out = Command::new("docker")
                .args(["volume", "inspect", volume, "--format", "{{ .Mountpoint }}"])
                .output()
                .context("failed to run docker volume inspect (is docker present?)")?;
            anyhow::ensure!(
                out.status.success(),
                "volume {volume} 不存在（先 `virtuoso kernel clone <git-url>`）：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            let mp = String::from_utf8_lossy(&out.stdout).trim().to_string();
            anyhow::ensure!(!mp.is_empty(), "volume {volume} has an empty Mountpoint");
            Ok(PathBuf::from(mp))
        }
        os => anyhow::bail!("不支持的宿主平台 {os}（docker 内核供给支持 macOS/Linux）"),
    }
}

/// macOS 引擎守卫：volume 的宿主视图由 OrbStack 提供，docker 端点必须指向
/// OrbStack 引擎，否则 clone/build 会把卷建进别的引擎 VM，视图永远看不到。
/// 非 macOS 恒通过（Linux 的 volume 本体即宿主目录，无视图引擎问题）。
pub(crate) fn engine_guard() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        return Ok(());
    }
    let ep = match std::env::var("DOCKER_HOST") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => docker_context_endpoint()?,
    };
    if endpoint_is_orbstack(&ep) {
        return Ok(());
    }
    anyhow::bail!(
        "macOS 上当前 docker 引擎不是 OrbStack（endpoint: {ep}）。\n  执行 'docker context use orbstack'，或对单条命令 export DOCKER_HOST=unix://$HOME/.orbstack/run/docker.sock。"
    )
}

/// 引擎端点是否指向 OrbStack（unix socket 是 `$HOME/.orbstack/...`；远程
/// context 主机名通常也含 orbstack 字样，宽松匹配宁可放行——误拦的代价是
/// 用户被守卫挡住）。
fn endpoint_is_orbstack(endpoint: &str) -> bool {
    endpoint.contains("orbstack")
}

/// 当前 docker context 的引擎端点（DOCKER_HOST 未设置时的权威来源）。
fn docker_context_endpoint() -> anyhow::Result<String> {
    let out = Command::new("docker")
        .args(["context", "show"])
        .output()
        .map_err(|_| {
            anyhow::anyhow!("docker unavailable (install and start OrbStack, then retry)")
        })?;
    anyhow::ensure!(
        out.status.success(),
        "docker context show 失败：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(
        !name.is_empty(),
        "docker context is empty (install and start OrbStack, then retry)"
    );
    let out = Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{ .Endpoints.docker.Host }}",
            &name,
        ])
        .output()
        .context("failed to run docker context inspect")?;
    anyhow::ensure!(
        out.status.success(),
        "docker context inspect {name} 失败：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 引擎里的全部卷名（`docker volume ls`，引擎即权威注册表，不另建清单）。
pub(crate) fn list() -> anyhow::Result<Vec<String>> {
    let out = Command::new("docker")
        .args(["volume", "ls", "--format", "{{ .Name }}"])
        .output()
        .context("failed to run docker volume ls (is docker present?)")?;
    anyhow::ensure!(
        out.status.success(),
        "docker volume ls 失败：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// volume 是否存在（引擎权威）。
pub(crate) fn exists(volume: &str) -> bool {
    Command::new("docker")
        .args(["volume", "inspect", volume])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 卷内容状态（fs-only 探测宿主视图，不起容器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// 空 / 不可达
    Empty,
    /// 有源码树（Makefile），未配置（无 .config）
    Cloned,
    /// Makefile + .config 齐备，可构建
    Configured,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Status::Empty => "empty",
            Status::Cloned => "cloned",
            Status::Configured => "configured",
        }
    }
}

/// 按宿主视图探测卷内容：Makefile+.config = configured，仅 Makefile = cloned。
pub(crate) fn status(view: &Path) -> Status {
    match (
        view.join("Makefile").is_file(),
        view.join(".config").is_file(),
    ) {
        (true, true) => Status::Configured,
        (true, false) => Status::Cloned,
        _ => Status::Empty,
    }
}

// ---------------------------------------------------------------- 工具链镜像容器调用

// 镜像供给（pull 回落本地构建 Dockerfile.kernel）、通用 docker run 封装、
// 架构 → make 环境 / clangd triple / make 镜像目标映射（照 kernel.sh 原值）。

/// 工具链镜像覆盖 env。注意不能叫 `KERNEL_IMAGE`——那个名字已被 CLI 配置面
/// 占用（启动内核镜像覆盖，config.rs）。
pub(crate) const IMAGE_ENV: &str = "KERNEL_TOOLCHAIN_IMAGE";

/// 工具链镜像解析：env `KERNEL_TOOLCHAIN_IMAGE` > ghcr 发布镜像。devcontainer
/// 渲染与此同源（保证编辑容器与构建容器同镜像）。
pub(crate) fn resolve_image() -> String {
    std::env::var(IMAGE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string())
}

/// 工具链镜像是否在位（引擎权威）。
pub(crate) fn image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 镜像供给：在位即短路；pull 失败回落本地构建 devkit/docker/Dockerfile.kernel
/// （与 kernel-builder.yml 发布 ghcr 的钉死工具链同源）。
pub(crate) fn ensure_image(
    image: &str,
    dockerfile_dir: &Path,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    engine_guard()?;
    if image_present(image) {
        return Ok(());
    }
    progress.line(&format!("Kernel toolchain: pulling {image} ..."));
    let pulled = Command::new("docker")
        .arg("pull")
        .arg(image)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if pulled {
        return Ok(());
    }
    let dockerfile = dockerfile_dir.join("Dockerfile.kernel");
    progress.line(&format!(
        "Kernel toolchain: pull failed, building locally from {} ...",
        dockerfile.display()
    ));
    let st = Command::new("docker")
        .arg("build")
        .arg("-t")
        .arg(image)
        .arg("-f")
        .arg(&dockerfile)
        .arg(dockerfile_dir)
        .status()
        .context("failed to run docker build")?;
    anyhow::ensure!(
        st.success(),
        "local toolchain-image build failed ({dockerfile:?})"
    );
    Ok(())
}

/// 通用容器调用基座：volume 挂 /ksrc（workspace），工作目录 /ksrc。
fn base_cmd(volume: &str, image: &str, tty: bool) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-i"]);
    if tty {
        cmd.arg("-t");
    }
    cmd.arg("--entrypoint=")
        .arg("-v")
        .arg(format!("{volume}:/ksrc"));
    cmd.arg("-w").arg("/ksrc").arg(image);
    cmd
}

/// 流式执行容器内脚本（stdio 继承，内核构建的长输出实时可见）。
pub(crate) fn run_streaming(
    volume: &str,
    image: &str,
    envs: &[(&str, &str)],
    script: &str,
) -> anyhow::Result<()> {
    let mut cmd = base_cmd(volume, image, false);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.arg("sh").arg("-c").arg(script);
    let st = cmd
        .status()
        .with_context(|| format!("failed to run docker (is the engine up? volume {volume})"))?;
    anyhow::ensure!(
        st.success(),
        "容器内命令失败（exit {}）：{script}",
        st.code().unwrap_or(-1)
    );
    Ok(())
}

/// 交互式容器命令（shell：TTY + stdio 继承），返回退出码。
pub(crate) fn run_tty(
    volume: &str,
    image: &str,
    envs: &[(&str, &str)],
    argv: &[&str],
) -> anyhow::Result<i32> {
    let mut cmd = base_cmd(volume, image, true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(argv);
    let st = cmd
        .status()
        .with_context(|| format!("failed to run docker (is the engine up? volume {volume})"))?;
    Ok(st.code().unwrap_or(130))
}

/// 经 stdin 向容器内写文件（平台无关，不依赖宿主视图在位）。
pub(crate) fn write_file(volume: &str, image: &str, dest: &str, content: &str) -> anyhow::Result<()> {
    let mut child = base_cmd(volume, image, false)
        .arg("sh")
        .arg("-c")
        .arg(format!("cat > {dest}"))
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to start docker")?;
    child
        .stdin
        .as_mut()
        .expect("stdin 已声明 piped")
        .write_all(content.as_bytes())
        .with_context(|| format!("write inside container to {dest} failed"))?;
    let st = child.wait()?;
    anyhow::ensure!(
        st.success(),
        "in-container write to {dest} failed (exit {})",
        st.code().unwrap_or(-1)
    );
    Ok(())
}

/// arch → 容器内 make 环境（arm64 在 arm64 容器 = 原生前端，其余交叉；
/// 照 kernel.sh 原值——容器基座是 arm64，x86_64/riscv64 无条件交叉）。
pub(crate) fn make_env(arch: Arch) -> Vec<(&'static str, &'static str)> {
    match arch {
        Arch::Arm64 => vec![("ARCH", "arm64")],
        Arch::X86_64 => vec![("ARCH", "x86_64"), ("CROSS_COMPILE", "x86_64-linux-gnu-")],
        Arch::Riscv64 => vec![("ARCH", "riscv64"), ("CROSS_COMPILE", "riscv64-linux-gnu-")],
    }
}

/// arch → clangd --target triple（.clangd 的 --target 行替换值）。
pub(crate) fn clangd_target(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64-linux-gnu",
        Arch::X86_64 => "x86_64-linux-gnu",
        Arch::Riscv64 => "riscv64-linux-gnu",
    }
}

/// arch → make 的内核镜像目标（x86_64 无 Image 目标，只有 bzImage）。
pub(crate) fn make_image_target(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 | Arch::Riscv64 => "Image",
        Arch::X86_64 => "bzImage",
    }
}

/// arch → 容器内 `uname -m` 的同构形态（供给链脚本的守卫用）。
fn uname_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64",
        Arch::X86_64 => "x86_64",
        Arch::Riscv64 => "riscv64",
    }
}

/// kernel build 附加步：内核树内静态 bpftool（in-tree libbpf 同树链入，
/// 版本与被测内核严格匹配——`btf dump`/`prog`/`map` 检视的控制面）。
/// libelf.a 的 elf_compress 引 zstd，静态链接须显式 `-lzstd`；主线 6.12+
/// 的 bpftool sign.c 引 libcrypto（EVP/PEM），须显式 `-lcrypto`（openEuler
/// 6.6 树没有 sign.c，多链一个 .a 无害）。仅容器原生 arch 与目标同构时可行
/// （静态 libelf/z/zstd/crypto 只按容器原生 arch 装包，交叉缺静态库）；失败不
/// 致命——kernel make 的退出码不受影响，只 WARN 提示。
pub(crate) fn bpftool_step(jobs: usize, arch: Arch) -> String {
    let uname = uname_arch(arch);
    format!(
        "\
if [ \"$(uname -m)\" = \"{uname}\" ]; then
  if env -u ARCH -u CROSS_COMPILE make -C tools/bpf/bpftool -j{jobs} \\
    LDFLAGS=-static 'LIBS=$(LIBBPF) -lelf -lz -lzstd -lcrypto'; then
    echo 'bpftool: static build ready (tools/bpf/bpftool/bpftool)'
  else
    echo 'WARN: bpftool build failed — refresh the toolchain image (libzstd-dev, see devkit/docker/Dockerfile.kernel) or check the tree state'
  fi
else
  echo 'WARN: bpftool skipped — static supply needs a same-arch container (target {uname})'
fi
"
    )
}

/// compile_commands.json 生成（/ksrc 原始形态，容器内 clangd/devcontainer
/// 消费）。脚本由内核树自带（从 .cmd 文件聚合，无需 bear）：mainline/
/// openEuler 均为 `scripts/clang-tools/gen_compile_commands.py`；个别树可能
/// 放在 `scripts/compile_commands.py`，作回落。
pub(crate) fn cdb_generate(volume: &str, image: &str) -> anyhow::Result<()> {
    let script = "if [ -f scripts/clang-tools/gen_compile_commands.py ]; then \
                      python3 scripts/clang-tools/gen_compile_commands.py; \
                  else \
                      python3 scripts/compile_commands.py; \
                  fi";
    run_streaming(volume, image, &[], script)
}

// ---------------------------------------------------------------- clone 流水线

// clone 流水线：建卷 → git clone 进卷 → 渲染 .clangd → 写 current。

/// .clangd 模板源（devkit/docker/.clangd，唯一事实来源；--target 行由
/// render_clangd 按目标架构替换）。
pub(crate) fn clangd_template(project_root: &Path) -> anyhow::Result<String> {
    let path = project_root.join("devkit").join("docker").join(".clangd");
    std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read .clangd template failed (is {} in place?)",
            path.display()
        )
    })
}

/// --target=<triple> 行替换（退役 kernel.sh 的 sed：`s/--target=[a-z0-9_-]*/…/`）。
pub(crate) fn render_clangd(template: &str, target: &str) -> String {
    const NEEDLE: &str = "--target=";
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find(NEEDLE) {
        out.push_str(&rest[..i + NEEDLE.len()]);
        rest = &rest[i + NEEDLE.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        out.push_str(target);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// 完整 clone 任务的输入（一次装配，免长参数表）。
pub(crate) struct CloneJob<'a> {
    pub project_root: &'a Path,
    pub volume_name: &'a str,
    pub arch: Arch,
    pub url: &'a str,
    pub ref_name: &'a str,
    pub image: &'a str,
    pub dockerfile_dir: &'a Path,
    /// 全量历史克隆（缺省 false = `--depth 1` 只取树；学习用主目录开 true，
    /// git log/blame 需要完整历史）。
    pub full: bool,
}

/// 完整 clone：镜像供给 → 幂等建卷（已有内容拒绝）→ git clone（缺省
/// --depth 1，--full 全量历史）→ .clangd 按架构渲染进源码根 → 写 current。
/// 返回宿主可见路径。
pub(crate) fn clone_kernel(job: &CloneJob, progress: &mut Progress) -> anyhow::Result<PathBuf> {
    let CloneJob {
        project_root,
        volume_name,
        arch,
        url,
        ref_name,
        image,
        dockerfile_dir,
        full,
    } = *job;
    ensure_image(image, dockerfile_dir, progress)?;

    let out = std::process::Command::new("docker")
        .args(["volume", "create", volume_name])
        .output()
        .context("failed to run docker volume create (is docker present?)")?;
    anyhow::ensure!(
        out.status.success(),
        "docker volume create {volume_name} 失败：{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );

    let view = host_view(volume_name)?;
    anyhow::ensure!(
        status(&view) == Status::Empty,
        "volume {volume_name} 已有内容（换 `--as <新卷名>`，或 `virtuoso kernel use` 切过去后走增量构建）"
    );

    let depth = if full { "" } else { "--depth 1 " };
    progress.line(&format!(
        "Kernel: cloning {url}@{ref_name} → volume {volume_name}{} ...",
        if full { " (full history)" } else { "" }
    ));
    let script = format!(
        "git clone --progress {depth}--branch {} {} /tmp/k && cp -a /tmp/k/. /ksrc/",
        shell_quote(ref_name),
        shell_quote(url)
    );
    run_streaming(volume_name, image, &[], &script)?;

    progress.line(&format!(
        "Kernel: rendering .clangd (--target={}) ...",
        clangd_target(arch)
    ));
    let template = clangd_template(project_root)?;
    write_file(
        volume_name,
        image,
        "/ksrc/.clangd",
        &render_clangd(&template, clangd_target(arch)),
    )?;

    write_current(
        project_root,
        &Current {
            volume: volume_name.to_string(),
            arch: arch.name().to_string(),
        },
    )?;
    Ok(view)
}

// ---------------------------------------------------------------- devcontainer 渲染

// current 切换 → `.devcontainer/devcontainer.json`（git 忽略）。
//
// devkit/docker/devcontainer.json 是静态模板（workspaceMount 钉死缺省卷
// virtuoso-kernel），对 `--as` 命名的多卷无感知——named volume 的多内核
// 并存与固定卷名矛盾。这里在 [`write_current`]（clone/use 写 current 的
// 唯一入口）同步渲染一份活动卷专属的 devcontainer.json：workspaceMount
// 指向 current 卷，image = 钉死工具链镜像（复用 `KERNEL_TOOLCHAIN_IMAGE`
// 覆盖；首进免本地 build Dockerfile.kernel）。
//
// 落点必须是 repo 根的 `.devcontainer/`：VS Code 的自动发现契约只扫打开
// 工作区下的 `.devcontainer/`（或根级 devcontainer.json）——放 `.virtuoso/`
// 就只剩手动选隐藏目录一条路（macOS 文件夹选择器还默认不显示 dotfile）。
// 打开 repo 根 →「Reopen in Container」即进 current 卷的 /ksrc。

/// 模板（JSONC，devcontainer.json 规范允许注释）：{VOLUME} / {IMAGE} 由
/// render_template 替换；customizations 与静态模板保持同源。
const TEMPLATE: &str = r#"// 由 `virtuoso kernel use/clone` 自动渲染 —— 勿手编，切卷重跑 `virtuoso kernel use`。
// 打开 repo 根：VS Code →「Reopen in Container」（自动发现本文件）即进 /ksrc。
// workspace = /ksrc（current 活动 named volume）；镜像 = 钉死工具链
// （env KERNEL_TOOLCHAIN_IMAGE 渲染时已代入）。
{
  "name": "virtuoso-kernel",
  "image": "{IMAGE}",
  "workspaceMount": "src={VOLUME},dst=/ksrc,type=volume",
  "workspaceFolder": "/ksrc",
  "remoteUser": "root",
  "customizations": {
    "vscode": {
      "extensions": [
        "llvm-vs-code-extensions.vscode-clangd",
        "rust-lang.rust-analyzer",
        "ms-azuretools.vscode-docker"
      ],
      "settings": {
        // clangd 在容器内跑：吃 /ksrc/.clangd + compile_commands.json
        "clangd.arguments": [
          "--background-index",
          "--clang-tidy=false",
          "--header-insertion=never",
          "--query-driver=/usr/bin/*"
        ],
        // Rust 侧索引：容器里 make rust-analyzer 产出 rust-project.json，扩展自动拾取
        // 内核 C 风格：8 空格缩进；Rust 侧走标准 rustfmt 四空格
        "editor.tabSize": 8,
        "[c]": { "editor.insertSpaces": false },
        "[rust]": { "editor.tabSize": 4 },
        "files.exclude": {
          "**/*.o": true,
          "**/*.cmd": true,
          "**/*.ko": true,
          "**/.*.cmd": true
        }
      }
    }
  }
}
"#;

/// 渲染产物路径（repo 根 `.devcontainer/devcontainer.json`，git 忽略；落点
/// 由 VS Code 自动发现契约决定，见上）。
pub(crate) fn devcontainer_path(project_root: &Path) -> PathBuf {
    project_root.join(".devcontainer").join("devcontainer.json")
}

/// 填模板（volume + image → JSONC 文本）。与写盘分离，测试直接消费。
pub(crate) fn render_template(volume: &str, image: &str) -> String {
    TEMPLATE
        .replace("{VOLUME}", volume)
        .replace("{IMAGE}", image)
}

/// 按 current 渲染并写盘（镜像解析 env KERNEL_TOOLCHAIN_IMAGE > ghcr 发布镜像，
/// 与 kernel 命令组构建容器同源）。返回产物路径。旧落点 `.virtuoso/` 的产物
/// 一并清除（自动发现契约扫不到它，留着只会误导）。
pub(crate) fn render(project_root: &Path, current: &Current) -> anyhow::Result<PathBuf> {
    let body = render_template(&current.volume, &resolve_image());
    let p = devcontainer_path(project_root);
    std::fs::create_dir_all(p.parent().expect("渲染路径必有父目录 .devcontainer"))?;
    std::fs::write(&p, body)?;
    let legacy = project_root.join(".virtuoso").join("devcontainer.json");
    if legacy.is_file() {
        let _ = std::fs::remove_file(&legacy);
    }
    Ok(p)
}

// ---------------------------------------------------------------- 测试

/// 剥 JSONC 注释行（模板头 + 内嵌说明），供测试以 serde_json 校验结构。
#[cfg(test)]
fn json_body(jsonc: &str) -> String {
    jsonc
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立 scratch（并行测试共用 pid 名会互相踩踏）。
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("forge-state-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_state_file_reads_as_none() {
        let dir = scratch("missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_current(&dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = scratch("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let cur = Current {
            volume: "ksrc-oe66".into(),
            arch: "arm64".into(),
        };
        write_current(&dir, &cur).unwrap();
        assert_eq!(read_current(&dir).unwrap(), Some(cur));
        assert!(dir.join(".virtuoso").join("kernel-current.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_is_an_error_with_hint() {
        let dir = scratch("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".virtuoso")).unwrap();
        std::fs::write(state_path(&dir), "{ not json").unwrap();
        let err = format!("{}", read_current(&dir).unwrap_err());
        assert!(err.contains("kernel use"), "hint missing: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_clangd_swaps_target_triple() {
        let template =
            "CompileFlags:\n  Add:\n    - --target=aarch64-linux-gnu\n    - -fno-spell-checking\n";
        let rendered = render_clangd(template, "riscv64-linux-gnu");
        assert!(rendered.contains("--target=riscv64-linux-gnu"));
        assert!(!rendered.contains("aarch64-linux-gnu"));
        assert!(rendered.contains("-fno-spell-checking"));
    }

    #[test]
    fn render_clangd_is_idempotent_for_default() {
        let template = "Add:\n    - --target=aarch64-linux-gnu\n    - -fno-spell-checking\n";
        assert_eq!(render_clangd(template, "aarch64-linux-gnu"), template);
    }

    #[test]
    fn make_env_table_matches_kernel_sh() {
        assert_eq!(make_env(Arch::Arm64), vec![("ARCH", "arm64")]);
        assert_eq!(
            make_env(Arch::X86_64),
            vec![("ARCH", "x86_64"), ("CROSS_COMPILE", "x86_64-linux-gnu-")]
        );
        assert_eq!(
            make_env(Arch::Riscv64),
            vec![("ARCH", "riscv64"), ("CROSS_COMPILE", "riscv64-linux-gnu-")]
        );
    }

    #[test]
    fn clangd_target_table() {
        assert_eq!(clangd_target(Arch::Arm64), "aarch64-linux-gnu");
        assert_eq!(clangd_target(Arch::X86_64), "x86_64-linux-gnu");
        assert_eq!(clangd_target(Arch::Riscv64), "riscv64-linux-gnu");
    }

    #[test]
    fn make_image_target_table() {
        assert_eq!(make_image_target(Arch::Arm64), "Image");
        assert_eq!(make_image_target(Arch::X86_64), "bzImage");
        assert_eq!(make_image_target(Arch::Riscv64), "Image");
    }

    #[test]
    fn bpftool_step_pins_recipe_and_arch_guard() {
        let s = bpftool_step(16, Arch::Arm64);
        // 同构守卫 + 静态配方（-lzstd/-lcrypto 追加在 LIBS 尾部，链接顺序敏感）
        assert!(s.contains("[ \"$(uname -m)\" = \"aarch64\" ]"));
        assert!(s.contains("env -u ARCH -u CROSS_COMPILE make -C tools/bpf/bpftool -j16"));
        assert!(s.contains("LDFLAGS=-static"));
        assert!(s.contains("'LIBS=$(LIBBPF) -lelf -lz -lzstd -lcrypto'"));
        for a in [Arch::X86_64, Arch::Riscv64] {
            assert!(bpftool_step(4, a).contains(uname_arch(a)));
        }
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        // 语义钉在 crate::util（shlex.quote：安全字符原样，其余整体包裹）
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("/home/u/ksrc"), "/home/u/ksrc");
    }

    #[test]
    fn orbstack_view_layout() {
        let p = orbstack_view(Path::new("/Users/u"), "ksrc-oe66");
        assert_eq!(
            p,
            PathBuf::from("/Users/u/OrbStack/docker/volumes/ksrc-oe66")
        );
    }

    #[test]
    fn engine_endpoint_matching() {
        assert!(endpoint_is_orbstack(
            "unix:///Users/u/.orbstack/run/docker.sock"
        ));
        assert!(endpoint_is_orbstack("tcp://orbstack.internal:2375"));
        assert!(!endpoint_is_orbstack("unix:///var/run/docker.sock"));
        assert!(!endpoint_is_orbstack("npipe:////./pipe/docker_engine"));
    }

    #[test]
    fn status_probe_by_makefile_and_config() {
        let dir = std::env::temp_dir().join(format!("forge-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(status(&dir), Status::Empty);
        std::fs::write(dir.join("Makefile"), "").unwrap();
        assert_eq!(status(&dir), Status::Cloned);
        std::fs::write(dir.join(".config"), "").unwrap();
        assert_eq!(status(&dir), Status::Configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_targets_current_volume_and_image() {
        let out = render_template("ksrc-oe66", "ghcr.io/yxalix/virtuoso-kernel:latest");
        let v: serde_json::Value =
            serde_json::from_str(&json_body(&out)).expect("渲染产物须为合法 JSON");
        assert_eq!(v["workspaceMount"], "src=ksrc-oe66,dst=/ksrc,type=volume");
        assert_eq!(v["workspaceFolder"], "/ksrc");
        assert_eq!(v["image"], "ghcr.io/yxalix/virtuoso-kernel:latest");
        // 加速首进：image 直用钉死工具链，不再本地 build Dockerfile.kernel
        assert!(v.get("build").is_none());
        assert_eq!(v["remoteUser"], "root");
        let ext = v["customizations"]["vscode"]["extensions"]
            .as_array()
            .unwrap();
        assert_eq!(ext.len(), 3);
        assert!(ext.iter().any(|e| e == "rust-lang.rust-analyzer"));
    }

    #[test]
    fn render_template_is_idempotent() {
        let a = render_template("ksrc-mainline", "img:1");
        let b = render_template("ksrc-mainline", "img:1");
        assert_eq!(a, b);
        assert!(!a.contains("{VOLUME}") && !a.contains("{IMAGE}"));
    }
}
