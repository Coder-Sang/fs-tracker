# OpenSandbox 可行性验证依据

验证时间：2026-09-17 UTC。测试先于 Rust 项目规划完成，使用 Python ctypes 直接调用 Linux API。它证明基础机制可行，不能替代 Rust 实现的验收。

## 环境

| 项目 | 实测值 |
| --- | --- |
| Kubernetes context | <local-cluster> |
| 节点内核、架构 | 7.0.14，aarch64 |
| Kubernetes / 容器运行时 | v1.35.6 / Docker 29.4.0 |
| OpenSandbox Server / execd / Python SDK | v0.2.3 / v1.0.22 / 0.1.16 |
| 运行时镜像 | <internal-registry>/opensandbox-pi@sha256:883bf8e77b92ca63b6253249052203cae6431c753bf6307ade3ac012609415e9 |
| Yama ptrace_scope | 0 |
| 默认进程外层 seccomp | Seccomp=0 |
| 独立受限 Pool | RuntimeDefault、drop ALL、allowPrivilegeEscalation=false |
| 受限 Pool 进程状态 | CapEff/CapBnd 等全部为 0；NoNewPrivs=1；Seccomp=2；Seccomp_filters=1 |

通过真实 OpenSandbox API 创建两个短期 Sandbox，并使用 SDK 上传和执行探针。受限 Pool 单独创建，没有修改既有业务 Pool 或节点策略。

## 功能验证

子进程设置 no_new_privs、安装 NEW_LISTENER、通过 SCM_RIGHTS 传递 listener FD 后 exec Python 工作负载；supervisor 读取通知中的 TID，通过 `/proc/<tid>/mem` 获取路径、校验 ID_VALID、读取 before，再发送 CONTINUE。

每个成功用例核对了最终文件内容，并覆盖：

- openat 截断覆盖前保留原文。
- 临时文件 rename 原子替换前保留目标原文。
- 删除前保留原文，新建前记录不存在。
- O_RDONLY | O_CREAT 被捕获，纯只读打开不进入通知集合。
- 相对路径解析。
- shell 和 Node 后代继承 filter；不同 PID 的文件修改被捕获。
- supervisor 无 CAP_SYS_ADMIN、CAP_SYS_PTRACE。

## 结果矩阵

| 环境与启动方式 | 结果 |
| --- | --- |
| 默认 Sandbox，原有 root 权限 | 通过 |
| 默认 Sandbox，清空 capabilities + no_new_privs | 通过 |
| 默认 Sandbox，UID/GID 65534 | 通过 |
| 独立受限 Pool，直接运行 tracker | 通过 |
| 默认 Sandbox，bwrap 新 PID namespace + 新 proc | 挂载 proc 被拒绝 |
| 默认 Sandbox，bwrap 复用既有 PID namespace/proc | 通过 |
| 默认 Sandbox，capabilities 全空 + bwrap 新 PID namespace | 设置 uid map 被拒绝 |
| 默认 Sandbox，UID 65534 + bwrap 新 PID namespace/proc | 挂载 proc 被拒绝 |
| 默认 Sandbox，UID 65534 + bwrap 复用 PID namespace/proc | 通过 |
| 受限 Pool，bwrap 新 PID namespace | 创建 namespace 被拒绝 |
| 受限 Pool，bwrap 复用 PID namespace | 创建 namespace 被拒绝 |

共 11 个用例，6 个完整功能通过，5 个在 bwrap 启动阶段失败。当前镜像没有 bwrap，嵌套测试临时上传了 Debian bookworm arm64 bubblewrap 0.8.0-2+deb12u1；未修改基础镜像。

结论：tracker 不需要 bwrap 才能工作。安装 seccomp 与创建 namespace/挂载 proc 是不同权限问题。当前 RuntimeDefault 下 seccomp 路径通过，不代表其他运行时的默认策略也一定允许。

## 验证边界

文件操作全部在私有临时目录内，before 保存在 supervisor 内存。未挂载业务目录，未测试 NFS、多写入者、完整 syscall 集合、外部可写 FD、io_uring、Yama=1/2、其他 LSM 配置、Rust 实现、性能或真实 Pi 模型调用。

探针使用受控路径和工作负载，路径解析逻辑不是通用生产实现。不能将这次通过解释为完整文件审计保证。

两个测试 Sandbox 删除后 API 查询均为 404；临时 Pool、端口转发和连接凭据已清理；Kubernetes 中未残留本次标签的资源，既有 Pool 的 available/total 保持 1/1。

本目录保存[结果快照](validation/opensandbox-results.json)，包含原型的通知事件、before/after、断言和权限状态，没有连接凭据。原型脚本和更完整操作记录位于本机 `/tmp/<probe-dir>/`；该临时目录不是后续 Rust 工程的构建依赖。
