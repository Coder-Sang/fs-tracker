# Rust 实现计划

## 1. 判断与目标

Rust 适合实现这个 tracker：它能直接使用 Linux 的进程、FD、seccomp 与信号接口；所有权和 RAII 有助于管理 listener、进程句柄和快照文件；可以构建独立的 aarch64/x86_64 程序，部署到 OpenSandbox 镜像中。

Rust 解决的是工程实现和资源管理问题，不会消除 seccomp CONTINUE 的竞态、共享 NFS 的外部写入以及未覆盖的文件修改入口。

首版目标：

1. 启动任意命令，跟踪该进程和后代的文件修改，无需 Agent 主动上报路径。
2. 支持多个显式指定的项目根目录。
3. 对候选路径保留首次修改前和任务结束后的内容，生成新增、修改、删除及 mode 变化清单。
4. 为文本文件输出 unified diff；为二进制、超限或无法解码的文件输出变化信息。
5. 消除修改后恢复、创建后删除等没有最终净变化的记录。
6. 保持目标程序 stdin/stdout/stderr 的协议与字节内容，提供独立的运行状态和产物。
7. 不在启动时扫描整个项目，不修改项目自身的 `.git`、index 或工作树配置。

首版不实现：多任务工作区隔离、可靠归因外部服务/NFS 客户端写入、恶意程序审计、回滚、文件历史浏览服务、跨平台驱动或长期运行进程的多轮 baseline 重置。

## 2. 报告语义和正确性条件

### 时间边界

一个跟踪任务对应一次被包装的命令进程树：

- Begin：子进程 filter 安装成功、supervisor 就绪，尚未 exec 业务命令。
- End：主进程和受管理后代已退出，没有已知写入者继续活动，开始捕获 after。
- 同一路径的 before 在任务内只设置一次，after 在收尾时读取。
- 原始 syscall 通知只代表一次操作尝试；CONTINUE 没有 syscall-exit 回调，不能据此认定操作成功。
- 报告依据 before/after 的真实状态计算，不按通知次数生成修改记录。

例如 A → B → C → A 最终不报告内容变化；原文件删除后重建使用最初的 A 作为 before；新建后删除不报告。

长连接 Agent 可能在同一进程中处理多轮请求。v1 对整次进程生命周期生成一份报告；不能仅收到 `agent_end` 就认定全部写入结束。按轮次跟踪必须另行设计 FD、后台任务和状态切换，不作为首版的隐含能力。

### 何时可解释为 Begin → End

需要满足以下条件：

- 范围内相关文件没有来自进程树之外的并发修改。
- 所有修改入口在受支持的操作集合内，可写 FD 没有从未跟踪的进程传入。
- 路径身份在捕获和放行之间未因未协调操作而变化。
- after 捕获前相关写入者已经停止。

同一 supervisor 内可协调受拦截的操作，但无法冻结外部文件系统或原子绑定内核随后解析的路径。ID_VALID 只校验通知生命周期，不保证路径、内容或参数不变。重复 stat、重试或串行通知也不能提升为全局快照保证。

### 不夸大报告完整性

输出分开表达两件事：

- `state = finished | partial | failed`：本次采集与收尾是否完成，是否发现缺口或失败。
- `assurance = best_effort`：v1 的保证级别，始终写入产物。`finished` 只表示在声明覆盖范围内未发现缺口。

记录 kernel、ABI、采集策略版本、支持操作、未验证假设及发现的 gap。无法检测的外部写入和绕过入口必须在 coverage 中披露，不能宣称“无 gap 即 100% 完整”。

## 3. 程序边界、CLI 与配置

建议先采用一个 Cargo package，提供 library 和 `fs-tracker` binary，避免过早拆分多个 crate。

预期接口：

```bash
fs-tracker doctor --json

fs-tracker run \
  --root main=/projects/main \
  --root docs=/projects/docs \
  --output /run/fs-tracker/run-001 \
  --config /run/fs-tracker/config.toml \
  -- python3 task.py

fs-tracker diff /run/fs-tracker/run-001
```

- `--` 后 argv 原样传递，直接 exec，不拼接 shell 命令。
- 根目录使用稳定 ID；输出相对路径，避免把宿主路径当报告标识。
- v1 拒绝重叠 roots，配置不接受含歧义的根目录映射。
- output 必须是独立的新目录，并且不在 roots 内。输出目录和控制通道设置私有权限。
- 文件名使用原始 Unix 字节处理；JSON 提供无损编码字段和可选显示字符串。
- 排除规则通过可重复的 `--exclude ROOT_ID=RELATIVE_PATH` 显式配置，默认不自动忽略 `.gitignore` 中的文件。规则按路径组件匹配子树，排除范围以无损路径编码写入 coverage。excluded→included rename 报新增，included→excluded rename 报删除。
- 目标程序继承标准输入输出；tracker 的状态、日志和控制消息使用独立文件/FD，防止污染 Pi JSONL。

初始配置项限定为 roots、output、排除规则、文件数和字节配额、文本 diff 上限、采集期限、后代退出期限及失败策略。

普通命令自然退出即可收尾。另提供显式传入的控制 FD，支持 `finish` 请求，以适配长期 RPC 程序。控制协议只控制进程生命周期，不解析 Pi 消息。

退出策略：目标正常运行过时保留其退出码或信号结果；tracker 的报告状态单独保存在 `status.json`。启动前的配置/能力检查失败使用 tracker 自己的非零退出码。调用方必须同时检查目标结果和报告状态，不能用退出码猜测 diff 完整性。

## 4. Linux 运行架构

```text
调用方 / OpenSandbox execd
    └── fs-tracker supervisor
         ├── listener 与事件循环
         ├── 有界通知 worker
         ├── 可超时/重启的捕获 helper 进程
         ├── 私有状态和 before/after 对象
         └── 被包装命令
              ├── shell
              ├── Python
              └── Node / 其他后代
```

不要求 bwrap。初始支持 Linux 5.10+、原生 aarch64 和 x86_64；5.5 是 CONTINUE 的机制下限，但首版不承诺兼容所有早期内核组合。以运行探测作为可用性判断，版本号仅作诊断。

不申请 CAP_SYS_ADMIN/CAP_SYS_PTRACE。安装 filter 前设置 no_new_privs；父子同 UID，读取 `/proc/<tid>/mem` 仍需满足 dumpable、Yama、LSM 和外层容器策略。

### 启动顺序

1. 校验配置、准备私有目录并运行必要的能力探测。
2. supervisor 设置 subreaper，准备 socketpair、控制 FD 和信号处理。
3. 在任何线程池/异步 runtime 启动前 fork。
4. 子进程仅执行经过审查的 fork/exec 安全调用：建立进程组、清理非必要继承 FD、设置 no_new_privs、安装 filter。
5. 经 SCM_RIGHTS 发送 listener FD，关闭子进程持有的 listener 副本。
6. supervisor 接收 listener，启动事件循环，发送就绪确认；子进程 exec 目标 argv。
7. 启动或握手失败必须回收已创建的进程和 FD，避免目标在未跟踪状态下执行。

v1 保留 stdin/stdout/stderr；关闭其他未声明 FD。可写 stdio 以及显式传入 FD 指向跟踪根目录的情况必须识别并处理，否则作为已知缺口拒绝完整采集。不能假设所有可写 FD 都来自后续 open。

### 通知事件循环

使用 poll/epoll、进程退出通知、信号和控制 FD 组成同步事件循环。初版不引入 Tokio；通知解析放入有界 worker，可能阻塞的文件读取、hash、对象写入和 fsync 通过常驻 helper 进程执行。helper 使用有界 JSONL IPC，单次任务有 deadline；超时后 supervisor KILL/reap helper、放行通知并在下次捕获时重启。

- 启动时调用 GET_NOTIF_SIZES，以实际结构大小分配请求/响应缓冲。
- 校验 syscall ABI，禁止把非支持架构的参数按原生 ABI 误读。
- 访问 `/proc/<tid>` 后和回复前做 ID_VALID 检查，正确处理 ENOENT、信号中断、线程退出和通知重试。
- 通知队列和 worker 队列有界。同一原始状态的捕获完成前，不放行与其冲突的受管写入入口。
- CONTINUE 前完成 before 的持久保存/状态登记。只声明进程级恢复；掉电持久性需要单独的 fsync 策略与测试。
- 保持第一个 before 不变，失败 syscall 产生的多余候选最终由 after 比较消除。

常规文件读操作不进入 supervisor；`openat2` 的 flags 在指针参数中，需要用户态检查。BPF 无法按 pathname 直接过滤 roots，范围外的匹配调用仍有通知成本。

## 5. 文件操作覆盖分阶段实施

| 操作 | 首版目标与处理 |
| --- | --- |
| open/openat/creat | 捕获 WRONLY/RDWR/CREAT/TRUNC；检查 O_TMPFILE、O_PATH 等语义，不能简单把所有 open 都当普通文件 |
| openat2 | 读取并校验 open_how 大小/flags/resolve；不支持的解析语义记 gap |
| truncate/ftruncate | 在截断前捕获；FD 路径通过目标 `/proc` 解析，目标已删除或不可定位时记 gap |
| unlink/unlinkat | 删除前保存原文件或链接状态 |
| rename/renameat/renameat2 | 捕获两端原始状态；普通文件替换为必做，EXCHANGE/NOREPLACE 分别验收 |
| chmod/fchmod/fchmodat 家族 | 报告 Git 关心的可执行位变化；支持 syscall 变体需按 ABI 和内核探测 |
| symlink/symlinkat | 保存链接本身的字节值；与通过链接写 referent 分开处理 |
| mkdir/rmdir | 默认不报告空目录；目录操作参与路径映射和异常检测 |
| 整个目录 rename | 初版可以标为 partial；后续枚举受影响子树。不能许诺这种操作仍是纯 O(最终文件数) |
| link/linkat、已有多硬链接文件 | 初版识别并标记不完整；不全盘扫描时无法保证发现所有别名 |
| 非普通文件、特殊 FS ioctl | 不读取设备/FIFO 等内容；相关操作记 gap 或注明不覆盖 |

关键语义：

- 主要状态按 `(root_id, relative_path_bytes)` 建模；`dev/ino` 用于协调和诊断，不能单独代替路径，也不能假设 inode 永不复用。
- 使用目标线程的 cwd、dirfd 和 root 解析路径；对最终链接跟随行为按具体 syscall 区分。不能使用 supervisor 的 cwd 或简单字符串前缀代替解析。
- 范围外到范围内的移动是目标路径新增；范围内到范围外是原路径删除。不无条件读取范围外内容。
- 通过符号链接访问目标时，报告选择已解析的 root 内目标路径；不能保证自动发现所有符号链接别名。复杂路径变化的保证级别保持 best_effort。
- 在项目根目录中创建过的 O_TMPFILE、随后 linkat 等组合必须纳入缺口检测或专门覆盖，不能默默漏报。

### 不靠逐次 write 捕获的前提

普通 write/pwrite/writev、共享 mmap 写入能够复用“获得可写 FD 前已保存 before”的结果。这要求 writable open 已覆盖，且不存在未受管 FD 进入进程树。

v1 默认不改变任意程序的 IO 行为：

- io_uring_setup/相关入口出现时，记录可能绕过普通 syscall 跟踪的 gap；不能仅拦 io_uring_enter 就认为恢复了操作语义。
- 外部进程传入可写 FD、远程服务代写、跨 namespace 执行等写入无法由本进程树可靠归因；明确写入 coverage。
- 需要禁止 io_uring 或限制 FD 传递时，应作为独立、显式的兼容性策略，经测试后加入，不作为隐含默认策略。

## 6. 快照、状态和 diff

### 产物格式

```text
output/
  status.json              # 原子更新的生命周期和最终报告状态
  report.json              # 版本化的最终净变化列表
  journal.jsonl            # 恢复/诊断记录，不是完整 syscall 审计日志
  objects/<digest>         # 按内容寻址的 before/after 原始字节
  changes.patch            # 可展示的受限 unified diff
  diagnostics.jsonl        # 结构化诊断，不含原始环境变量或凭据
```

业务产物本身包含项目源文件内容，继承项目数据的访问控制；诊断日志不复制正文、环境变量或命令中的敏感信息。

建议 `report.json` 最小契约：

```json
{
  "schema_version": 1,
  "state": "finished",
  "assurance": "best_effort",
  "target": {"exit_code": 0, "signal": null},
  "coverage": {
    "policy_version": 1,
    "external_writers": "not_observed_or_controlled",
    "unsupported_features": [],
    "gaps": []
  },
  "changes": [
    {
      "root_id": "main",
      "path_bytes_base64": "YS5weQ==",
      "display_path": "a.py",
      "kind": "modified",
      "before": {"object_id": "sha256:<digest>", "mode": "100644"},
      "after": {"object_id": "sha256:<digest>", "mode": "100644"},
      "diff": {"kind": "unified", "truncated": false}
    }
  ]
}
```

上例字段形状将在 M1 固定；object_id 中的 digest 是占位符。不存在、无法读取和因配额未捕获必须为不同状态，不能都用 null 冒充删除/新增。

### 捕获实现

- 文件内容流式复制并哈希，不一次性加载大文件。
- before 的状态为 unknown → capturing → present/absent/unavailable；已经捕获的状态不被后续修改覆盖。
- 捕获前后校验已打开 FD 的身份、大小和时间等元数据；发现变化有限重试，仍不稳定则 partial。这只降低普通竞态影响，不构成原子快照。
- 输出采用临时文件 + 原子 rename；登记对象、候选路径、失败原因，禁止引用未完整写入的对象。
- 文件数、单文件字节、总捕获字节、diff 行数分别限额，初始数值经基准测试确定，避免未经测量承诺吞吐。
- NFS IO 可能长时间阻塞。捕获 IO 不在 supervisor 线程内执行；deadline 到期后直接终止并回收 helper 进程，采集进入 partial 并放行。对象临时文件可能在 helper 被强杀时残留，但不会被报告引用；输出目录自身的最终报告写入若位于失效挂载上，仍需调用方总 deadline 和容器销毁兜底。

### 文本 diff 与 Git 适配

核心输出通用对象和变化清单，不要求安装 Git。首版使用成熟 Rust diff 库生成 unified diff，默认支持有效 UTF-8 且满足大小限制的文本；二进制、其他编码和超限文本保留对象引用及原因，不做有损解码。

rename 的基本表示是原路径删除 + 新路径新增，保持净变化正确；精确内容相同的移动可以给出提示。相似度重命名属于后续展示/Git 适配，不以 syscall 请求直接认定成功 rename。

独立 Git 适配器消费 report + objects，使用专用裸仓库/临时 index 构建 before/after tree 和 commit。项目现有 Git receipt V2 兼容放在外部 Gateway 接入阶段完成；tracker 核心不携带 Pi 工具名、Gateway 内部数据模型或 run receipt 类型。

## 7. 收尾、取消和故障策略

### 后代退出

- 使用 subreaper 接收孤儿后代；主进程退出不等于任务结束。
- 跟踪进程组并转发终止信号，利用子进程回收和 `/proc` 关系追踪处理独立 session/进程组后代。
- pidfd 可用时使用它避免 PID 复用；不向已经 reap 的裸 PID 发送信号。
- `finish` 后进入 stopping，发送 TERM，宽限期后 KILL；过程中继续消费通知，避免被阻塞的后代无法退出。
- 只有确认管理范围内已无存活后代，才捕获 after；超时或关系无法确认时标记 partial/failed，不能宣称得到稳定最终状态。
- subreaper/进程组不是 cgroup 强隔离。容器销毁是最终兜底，不申请 cgroup 管理权限作为首版前提。

### 故障区分

| 故障 | 默认行为 |
| --- | --- |
| 启动前不支持 seccomp 或无法读子进程内存 | 不启动业务命令，返回可诊断的启动错误 |
| 单个文件无法读、捕获超限、遇到已识别的不支持操作 | 标记 partial，supervisor 继续接收并放行 |
| 普通路径解析失败但通知仍有效 | 记录 gap，按报告失败策略放行，不伪造成功捕获 |
| 目标线程退出/通知失效 | 丢弃该回复，清理请求状态，避免把它当整个任务失败 |
| supervisor 崩溃、event loop 无法继续服务 | 调用方停止整个任务，保留失败状态；不能假设 filter 被解除 |
| 调用方断连或取消 | 执行有界收尾，结果标记 cancelled/interrupted，保留已落盘对象 |
| 生成 diff 失败 | 保留文件清单与对象，报告 diff 失败原因 |

最后一个 listener FD 关闭后，匹配调用会返回 ENOSYS；listener 存活但没人处理也会卡住调用。必须验证父进程退出通知、watchdog/心跳以及调用方清理路径。PDEATHSIG 只能作为局部保护，不能代替整棵进程树清理。

## 8. Rust 模块与依赖选择

建议结构：

```text
src/
  main.rs                   # CLI 与退出结果映射
  lib.rs                    # 小规模公共 API
  config.rs                 # 配置、配额和 roots 校验
  contracts.rs              # 版本化状态与报告类型
  linux/
    seccomp.rs              # filter/listener，狭窄 unsafe 边界
    abi.rs                  # 原生 ABI 和 syscall 操作分类
    process.rs              # fork/exec、FD、subreaper、进程回收
    signals.rs              # 信号和取消
    target_memory.rs        # proc mem、ID_VALID、参数读取
    paths.rs                # cwd/dirfd/root/链接解析
  supervisor/
    event_loop.rs           # 生命周期与通知分发
    pending.rs              # 候选操作与有限并发协调
    shutdown.rs             # 停止、后代回收和收尾
  capture/
    snapshot.rs             # 捕获前后内容与身份校验
    objects.rs              # 流式对象存储
    journal.rs              # 路径原始状态和恢复记录
  report/
    changes.rs              # 净变化计算
    text_diff.rs            # 文本分类、unified diff、限额
    writer.rs               # 原子发布产物
  doctor.rs                 # 在临时目录执行真实能力探测
```

初步依赖：`clap`、`serde/serde_json`、`toml`、`thiserror`、`tracing`、`tempfile`、`sha2`、`base64`、`similar`，以及 `nix`/`libc` 承接 Linux API。避免同时引入两套重叠的系统调用封装。

优先评估成熟 `libseccomp` Rust binding 生成 filter，避免手写两套 syscall 数字和 BPF 跳转。M1 检查 crate 维护状态、通知 API 覆盖、最低 libseccomp 版本和链接方式；通知 ioctl 缺少安全接口时以小型审查过的 FFI 封装补足。不得为了“单文件部署”先行自写完整 seccomp 框架。

`unsafe` 限制在 Linux 边界，逐段解释结构布局、FD 所有权、缓冲区大小和 fork 安全性。使用 OwnedFd/BorrowedFd，fork 后子分支避免 Rust 分配、日志和可能继承锁的库调用。

首版构建 aarch64/x86_64 GNU Linux 产物，glibc 基线与当前 Debian bookworm 运行时兼容；如果采用动态 libseccomp，明确镜像依赖。静态 musl 构建作为后续交付优化，不作为首轮正确性的前置条件。

## 9. 实施顺序与交付门槛

| 阶段 | 具体交付 | 当前状态 |
| --- | --- | --- |
| M0：计划评审 | 本文、实测依据、首版保证范围 | 完成；固定为 `best_effort`，不承诺共享工作区快照 |
| M1：Rust 最小闭环 | Cargo 工程、CLI、doctor、启动握手、真实 listener 和目标进程回收 | 完成；aarch64 原生与受限 OpenSandbox 已验证，x86_64 由 CI gate 覆盖 |
| M2：普通文件变化 | 多 root、open/truncate/unlink/rename、mode、对象存储和净变化报告 | 完成；真实 seccomp E2E 覆盖净变化与目标退出语义 |
| M3：稳健性和 diff | 文本 diff、复杂操作 gap、配额、有界 worker、可终止捕获 helper、信号/取消、后代收尾 | 功能交付完成；更广的 supervisor 崩溃/通知失效故障矩阵仍可扩展 |
| M4：真实环境验证 | OpenSandbox 安全矩阵、独立 NFS 测试目录、故障注入、基准结果 | 部分完成；受限 Pod、NFS、外部写入反例、ENOSPC 和基准已完成，Yama/SELinux、NFS 断连及原生 x86_64 待专用环境 |
| M5：Gateway 试点 | Pi 启动包装、finish 时序、Git receipt V2 适配、可开关的部署配置 | 试点完成；真实 Pi 零变化闭环、stdout JSONL 和 receipt 严格解码通过，生产 rollout 尚未执行 |

每个阶段先提供可独立运行的命令和产物，再扩大覆盖。M2 是首个功能原型，M3+M4 通过后才进入 Gateway 试点。

当前仓库已交付可运行的 Rust v1 和独立 Git adapter。M1-M3 的实现闭环完成；M4 已取得 aarch64、受限 OpenSandbox、真实 NFS、ENOSPC 和性能数据；M5 已在外部 Gateway 工作树完成可开关的 OpenSandbox Pi 包装、out-of-band finish 和 receipt V2 发布，并通过真实 Pi 试点。尚未取得的证据集中在原生 x86_64、Yama/SELinux、NFS 断连和更广故障注入，不应将这些环境验收项写成已完成。详见 `docs/rust-validation.md`。

## 10. 验证设计

### 行为测试

用临时小工作区在 Begin/End 独立全量快照作为测试 oracle；生产路径仍按需捕获。核对文件字节、类型、mode 和差异，不只检查“收到了某个 syscall”。

必测：

- 重复编辑、追加、截断、新建、删除、删除后重建、改回原文、创建后删除。
- 临时文件 rename 覆盖、跨 root 移动、rename 失败、NOREPLACE/EXCHANGE。
- openat dirfd、相对路径、带空格/换行/非 UTF-8 的名字、符号链接和范围逃逸。
- Python、shell、Node，孙进程、并行工具、父进程先退出、后台进程 setsid。
- mmap 写入、ftruncate、可写 FD 继承、外部 FD/硬链接/io_uring 的缺口策略。
- 同一受管文件并发打开时只保存最初 before；外部写入的反例明确展示不能保证 Begin 快照。
- 超限、权限不足、磁盘满、NFS 延迟/断连、通知失效、信号打断、supervisor 故障和重复收尾。
- 目标程序 stdin/stdout/stderr 字节流与退出语义，不使用模型请求代替协议测试。

### 权限与部署矩阵

- 默认 OpenSandbox、capabilities 全空、非 root、RuntimeDefault + 禁止提权。
- Yama=0/1，以及更严格策略下可诊断失败；不为了测试修改共享节点 sysctl，可使用专用 VM。
- aarch64 和 x86_64 原生测试；仿真结果单独注明。
- bwrap 作为可选部署兼容测试，不是 tracker 核心测试前提。
- 普通容器文件系统与实际 NFS 测试挂载分别验收。NFS 只使用独立测试目录，清理可核对。

### 性能基准

比较不包装与包装执行的：只读扫描、少量源码编辑、大文件修改、编译/测试产生临时文件、高并发写打开。记录 wall time、通知数、排队与处理延迟分位数、峰值 RSS、before/after 字节、候选数、最终变化数及 NFS IO。

明确成本按“匹配 syscall 次数 + 捕获字节 + 必要子树枚举”增长，不能宣传为只与最终变化文件数成正比。M4 根据实测设默认配额和发布门槛。

### Rust 质量门槛

工程创建后执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

真实 seccomp 集成测试使用显式标记和环境能力检查；支持环境中失败必须导致 gate 失败，不能一律 skip。依赖锁定并进行维护/许可证审查；解析层可用 fuzz/property tests，Linux syscall 交互以真实进程测试验证。

## 11. 已验证事实与待验证项目

Rust 实现已在 aarch64 本机和非 root、drop ALL capabilities、RuntimeDefault、禁止提权的 OpenSandbox Pod 中通过真实 seccomp listener 测试。共享 NFS 已完成读扫描、小文件修改、大文件修改基准和外部写入反例；对象存储 ENOSPC 会保留目标执行并明确降级为 `partial`。真实 Pi 0.85.1 RPC 在 tracker 下保持 stdout 为合法 JSONL，显式 finish 后生成 `finished` 报告；独立 adapter 生成的 receipt 已通过 Gateway 严格 V2 解码和真实报告检查器。

尚未证明或尚未覆盖完整矩阵：原生 x86_64 机器结果、Yama 1/2、SELinux、NFS 断连/长时间阻塞、supervisor 崩溃和通知失效注入、恶意并发路径竞态以及生产规模 rollout。x86_64 GitHub Actions 提供持续 gate，但不能替代目标生产节点验收。

参考：[OpenSandbox 验证记录](docs/opensandbox-validation.md)、[seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html)、[seccomp_unotify(2)](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html)、[proc_pid_mem(5)](https://man7.org/linux/man-pages/man5/proc_pid_mem.5.html)。
