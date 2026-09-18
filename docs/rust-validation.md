# Rust v1 验证记录

验证时间：2026-09-17 UTC。

## 构建与本机环境

- 架构：aarch64
- 内核：7.0.14
- Rust：1.85.1
- seccomp notification ABI 尺寸：request 80、response 24、data 64
- `fs-tracker` 与 `fs-tracker-git-receipt` 最高 glibc 符号版本：2.34

以下质量门槛通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

结果为 10 个单元测试、8 个端到端测试和 0 个失败。真实 seccomp E2E 覆盖 listener 安装/传递、净变化消除、目标非零退出码、配额降级、捕获超时、显式子树排除、长期进程 finish 以及 receipt V2。GitHub Actions 在 `ubuntu-24.04` 提供 x86_64 fmt、clippy、test 和真实 `doctor` gate。

## 捕获稳健性

supervisor 使用固定 notification worker 和有界队列。真正的文件读取、hash、对象写入与 fsync 通过一个常驻 helper 进程执行；每次请求有独立 deadline，超时后 helper 被 SIGKILL、同步 waitpid 回收，相关通知被放行并记录 `capture_timeout`，后续捕获自动启动新 helper。helper 不继承 seccomp listener、控制 FD 或其他 supervisor 私有 FD。队列满或 worker 失效仍会记录 `worker_queue_full` 或 `worker_pool_stopped` 并降级为 `partial`。

超时 E2E 将 helper 延迟设为 2 秒、deadline 设为 1 秒，确认目标写入成功、任务在有界时间完成且报告为 `partial`。配额 E2E 确认超限不会伪造完整报告。另在 2 MiB tmpfs 中捕获 4 MiB 对象触发真实 ENOSPC：目标正常退出，报告为 `partial`，diagnostics 记录 `object_write/ENOSPC`。helper 在对象 rename 前被强杀时可能留下未引用的 PID 唯一临时文件，后续清理不依赖这些文件。

排除 E2E 验证 `.venv` 和 `node_modules` 不产生候选或对象，`.venv-old` 仍正常跟踪，excluded→included rename 报新增，included→excluded rename 报删除；报告保持 `finished` 并在 `coverage.configured_exclusions` 中记录实际规则。

`--finish-fd` 接受一行 `finish`，向目标进程组发送 TERM，宽限期后 KILL，并在此期间继续服务 seccomp 通知。CLI 保留观察到的目标退出语义；shell 或运行时自行把 TERM 转换成退出码时，报告可能记录 `exit_code = 143, signal = null`，不能强制解释为直接 signal 退出。

## 无权限与 OpenSandbox

release 二进制通过 `setpriv` 以 UID/GID 65534、空 bounding/inheritable/ambient capabilities 和 `no_new_privs` 执行 `doctor --json`。listener 与 `/proc/<tid>/mem` 探测均为 available。

独立短期 OpenSandbox Pod 使用：

- `runAsUser/runAsGroup/fsGroup = 65534`
- `seccompProfile = RuntimeDefault`
- `capabilities.drop = [ALL]`
- `allowPrivilegeEscalation = false`

`doctor` 和真实文件修改均通过，报告、before/after 对象和 unified diff 正确。验证未修改业务 Pool、节点 sysctl 或共享安全策略，结束后删除 Pod。

## NFS 与性能

`scripts/benchmark.py` 分别在本地 `/tmp` 和真实共享 NFS `/mnt/shared` 运行三轮基准。中位 wall-time 比值如下：

| 场景 | 本地 | NFS |
| --- | ---: | ---: |
| 只读扫描 | 1.80x | 0.94x |
| 200 个小文件修改 | 3.50x | 3.55x |
| 32 MiB 文件修改 | 6.42x | 5.14x |

这些数据是在可重启常驻 helper 实现后重新测得。NFS 单轮波动明显，例如只读 tracked wall time 为 153-3629 ms，因此 0.94x 只表示本次 baseline 波动，不能解释为 tracker 加速。结果用于容量规划，不是稳定延迟承诺。原始数据位于 `docs/validation/benchmark-local.json` 和 `docs/validation/benchmark-nfs.json`。

外部 NFS 客户端在首次受管写入前修改同一文件的反例仍可得到 `finished`，因为外部写入不经过被跟踪进程树。这验证了 `assurance = best_effort` 和 `external_writers = not_observed_or_controlled` 限定不可移除。

## Pi、Gateway 与 receipt V2

真实 Pi 0.85.1 在受限 Pod 内通过命名 FIFO 的 FD 9 接收 finish。Pi stdout 只有合法 RPC `response` JSONL，没有 tracker 输出；tracker 收到 1632 条通知、0 个变化并生成 `finished` 报告。

独立 `fs-tracker-git-receipt` 使用专用 bare repository 构建 baseline/report commit、run 唯一 ref 和严格 receipt V2。默认拒绝 `partial` 报告。Git stdin、stdout 和 stderr 由三个 scoped pump 并发处理；stdout 限制 64 KiB，stderr 持续 drain 并保留 1 MiB 尾部，避免任一 pipe 填满造成父子互锁。回归测试同时传输 256 KiB stdin、stdout 和 stderr，并由 watchdog 验证不会死锁。验证包括：

- adapter 自身 E2E；
- Gateway 侧 receipt V2 严格解码；
- Gateway 侧报告检查器对修改文本和 diff 的识别；
- 受限 Debian Pod 中真实 Pi finish 后生成 receipt，并由 Gateway 严格解码。

Gateway 试点以环境变量显式启用。环境构造层从 run ID 和 workspace 自动派生 tracker output、控制 FIFO 与项目 roots；OpenSandbox transport 包装 Pi，stop 时通过独立 command 写 finish，等待 tracker 收尾后运行 adapter。Gateway transport 支持 `FS_TRACKER_EXCLUDES` JSON 字符串数组，并将每条相对路径展开到本次所有可写项目 root。相关 Python 静态检查与测试通过。未启用 binary 路径时保持原有启动行为。

## 尚未完成的环境矩阵

当前证据不覆盖：

- 原生 x86_64 生产节点实跑；CI gate 不能替代目标节点验收；
- Yama 1/2、SELinux 及其他 RuntimeDefault 实现；
- NFS 断连时 helper timeout/restart 的真实挂载验证，以及失效输出挂载上的最终报告写入；
- supervisor 崩溃、listener 失效和 watchdog 的完整故障注入；
- 高并发生产 workload、恶意路径竞态和正式 Gateway rollout。

这些缺口不影响当前独立工具和 Gateway 试点的可用性，但意味着 M4 的部署环境矩阵不能标记为全部完成，报告保证仍固定为 `best_effort`。
