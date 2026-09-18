# fs-tracker

`fs-tracker` 是一个 Linux 文件变更追踪启动器。它启动任意命令，通过 seccomp user notification 在目标进程及其后代首次尝试修改文件前保存原始状态，并在进程树结束后生成净变化报告和 unified diff。

当前提供可运行的 Rust v1，保证级别固定为 `best_effort`。它适合在没有外部写入者的受控任务工作区中记录 Agent、shell、Python、Node 等命令造成的变化，不是文件系统快照或安全审计边界。

## 构建

要求 Linux 5.10+、Rust 1.85+，支持原生 `aarch64` 和 `x86_64`。

```bash
cargo build --release
cargo test --workspace

target/release/fs-tracker doctor --json
```

运行时不依赖 `libseccomp` 动态库；seccomp filter 和 notification ioctl 由狭窄的 Linux FFI 层实现。

## 使用

```bash
fs-tracker run \
  --root project=/projects/project \
  --exclude project=generated/cache \
  --exclude-recursive .git \
  --exclude-recursive node_modules \
  --output /run/fs-tracker/task-001 \
  -- pi --mode rpc

fs-tracker diff /run/fs-tracker/task-001

# 长期 RPC：调用方预先创建 FIFO 并打开控制 FD
mkfifo /run/fs-tracker/task-002.finish
exec 9<>/run/fs-tracker/task-002.finish
fs-tracker run \
  --root project=/projects/project \
  --output /run/fs-tracker/task-002 \
  --finish-fd 9 \
  --termination-grace-seconds 5 \
  -- pi --mode rpc
# 另一控制进程结束本轮：printf 'finish\n' > /run/fs-tracker/task-002.finish
```

可以重复传入 `--root ID=PATH`。root 必须存在、互不重叠；output 必须是尚不存在的新目录，且不能位于 root 内。`--exclude ROOT_ID=RELATIVE_PATH` 保持原有语义：只排除指定 root 下该相对路径对应的子树。`--exclude-recursive NAME` 按完整目录组件在所有 root 的任意深度排除同名子树，例如 `.git` 不会误匹配 `.github`。两类规则都可重复传入，且 tracker 不会自动读取 `.gitignore`。`--` 后的 argv 直接传给 `execvp`，不会拼成 shell 命令。

root 和排除规则较多时，可改用版本化 JSON policy，避免把配置展开到进程参数中：

```json
{
  "schemaVersion": 1,
  "roots": [
    {"id": "project", "path": "/projects/project"},
    {"id": "shared", "path": "/projects/shared"}
  ],
  "exclusions": [
    {"rootId": "project", "path": "generated/cache"}
  ],
  "recursiveExclusions": [".git", "node_modules", ".venv"],
  "limits": {"maxFiles": 20000}
}
```

```bash
fs-tracker run \
  --config /run/fs-tracker/task-001-policy.json \
  --output /run/fs-tracker/task-001 \
  -- pi --mode rpc
```

`--config` 不能与 `--root`、`--exclude`、`--exclude-recursive` 或四个 `--max-*` 配额参数混用。`limits` 可省略或只提供部分字段，缺失值使用下文列出的当前默认值。policy 最大 16 MiB，拒绝未知字段和不支持的 `schemaVersion`，并执行与 CLI 相同的 root、重叠路径和 exclusion 校验。原有 `--root` 和 `--exclude` 命令行方式继续受支持。

目标命令的 stdin/stdout/stderr 保持原样，额外继承 FD 会在 exec 前关闭。若可写 stdio 指向 tracked root，tracker 会在启动目标前捕获该文件。CLI 保留目标退出码；报告状态需单独读取 `status.json` 或 `report.json`。

可配置配额及并发边界：

```text
--max-files 10000
--max-file-bytes 33554432
--max-total-bytes 268435456
--max-diff-bytes 2097152
--capture-workers 4
--notification-queue 256
--capture-timeout-seconds 30
```

捕获 worker 和队列固定有界。真正的文件读取、hash 和对象写入由常驻 helper 进程隔离；单次捕获超过 deadline 时 helper 会被 KILL/reap，相关 syscall 被放行，记录 `capture_timeout` 并将报告降为 `partial`，后续捕获会启动新 helper。队列满或 worker 停止也会显式降级，不会让通知循环或任务收尾无限等待不可取消的文件 IO。

## 产物

每次运行生成：

```text
output/
  status.json
  report.json
  journal.jsonl
  diagnostics.jsonl
  objects/<sha256>
  changes.patch
```

`report.json` 使用 `schema_version = 1`、`policy_version = 2`。路径始终提供无损的 `path_bytes_base64`；UTF-8 文件名另外提供 `display_path`。文件状态明确区分 `absent`、`present` 和 `unavailable`。`state` 为 `finished`、`partial` 或 `failed`，`assurance` 始终为 `best_effort`。`coverage.configured_exclusions` 记录 root-relative 排除范围，`coverage.configured_recursive_exclusions` 记录递归组件排除范围；排除是声明范围缩小，不会将报告降为 `partial`。`metrics` 记录运行耗时、通知数、队列旁路数、候选路径、捕获字节和最终变化数。

文本文件生成 unified diff。二进制文件、非 UTF-8 内容、超限内容和读取失败仍保留变化信息及对象或原因。修改后恢复原内容、创建后删除等无最终净变化的候选不会出现在报告中。

## Git receipt V2

独立 adapter 将通用 tracker 产物转换为 Gateway receipt，不修改项目自身 Git 元数据：

```bash
fs-tracker-git-receipt \
  --tracker-output /run/fs-tracker/task-001 \
  --repository /run/fs-tracker/report.git \
  --receipt /run/fs-tracker/receipt.json \
  --run-id task-001 \
  --workspace-id "$WORKSPACE_SHA256" \
  --project project=/projects/project \
  --report-ref refs/fs-tracker/reports/task-001
```

adapter 创建专用 bare repository、确定性的 baseline/report commit 和 run 唯一 ref。默认拒绝 `partial` 报告；只有明确接受降级语义时才使用 `--allow-partial`。当 tracker 使用 policy 文件时，可用 `--config /run/fs-tracker/task-001-policy.json` 代替所有 `--project ID=PATH`；原有可重复 `--project` 参数继续受支持，两种方式不能混用。

Gateway 侧 OpenSandbox 试点通过 provider env 显式启用：

```text
FS_TRACKER_BINARY=/opt/fs-tracker/bin/fs-tracker
FS_TRACKER_GIT_RECEIPT_BINARY=/opt/fs-tracker/bin/fs-tracker-git-receipt
FS_TRACKER_TERMINATION_GRACE_SECONDS=5
FS_TRACKER_EXCLUDES='[".venv","node_modules"]'
```

Gateway 根据 run ID、workspace 和 agent Git metadata projection 自动派生 output、FIFO、项目 roots 及 receipt 参数。只配置 tracker binary 时仅生成通用报告；同时配置 adapter binary 且请求启用 agent Git report 时，stop 会在 tracker 完成后发布 receipt。二进制必须预装在 OpenSandbox 镜像的绝对路径中。

## 覆盖范围

v1 处理：

- `open/openat/openat2` 的写入、创建与截断形式
- `truncate/ftruncate`
- `unlink/unlinkat`
- `rename/renameat/renameat2`，包括普通文件覆盖与 exchange 的两端状态
- `chmod/fchmod/fchmodat/fchmodat2`
- `symlink/symlinkat`
- 进程树继承、subreaper 收尾、TERM/KILL 超时处理和 INT/TERM/HUP 转发
- shell、Python、Node 后代以及 mmap 前的可写 open

检测到目录 rename、硬链接、`io_uring_setup`、特殊文件、配额超限、路径读取失败或后代超时会记录 coverage gap，并将结果降级为 `partial`。

无法可靠控制或归因的范围包括外部/NFS 客户端写入、运行中传入的外部可写 FD、硬链接别名的全盘发现、特殊文件系统 ioctl 和恶意进程利用路径竞态。`finished` 仅表示声明的覆盖范围内没有发现 gap。

## 验证

质量门槛：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

集成测试使用真实 seccomp listener，不使用 mock。本机 aarch64、共享 NFS、独立 OpenSandbox 受限 Pod、真实 Pi RPC 和 Gateway receipt V2 的验证记录见 [Rust 验证记录](docs/rust-validation.md)。基准脚本为 `scripts/benchmark.py`，原始结果保存在 `docs/validation/`。设计、正确性条件和剩余环境验收见 [实现计划](IMPLEMENTATION_PLAN.md)。
