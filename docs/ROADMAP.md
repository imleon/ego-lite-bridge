# ego-lite-bridge 路线图

本路线图以 [`PRD.md`](PRD.md) 为产品行为基准。每阶段保持可构建和可测试，不通过fallback隐藏未完成能力。

## 已完成

### M0 — 分发安全

- installer只接受本产品manifest和可信release URL；
- release不可用或checksum失败时不覆盖已安装binary；
- macOS installer保持binary-only；上一轮Linux binary/shim-only流程不要求Node.js或npm、不安装skill且不触碰Agent目录，已由用户实机验收通过；本次新增可选交互流程的待验收项见下文，不撤销此前结论；
- release继续包含manifest记录URL和SHA-256的vendored `ego-browser-skill.tgz`；保留独立的可选手工步骤：以同一显式`VERSION`下载归档和`SHA256SUMS`，用`grep`与`sha256sum`校验，在`set -eu`子shell的新工作目录解包，并要求显式单个`AGENT_ID`后固定调用`skills@1.5.24`；此手工命令不得使用`*`或省略agent；skills CLI可能覆盖目标skill。

### M1 — 产品仓库裁剪

- 删除未编译Herdr产品表面和无关平台/工作流；
- 保留bridge核心、installer、测试、双语README与attribution。

### M2 — Protocol v1冻结

- exact version/capability握手；
- binary framing golden fixture；
- malformed、oversized、truncated和request-ID负向测试。

### M3 — 8路并发multiplexing

- Linux broker和Mac executor按request ID并发路由；
- 每请求bounded queue、cancel/backpressure/error隔离；
- channel断开时清理全部请求和Mac process groups；
- 本地自动化和真实Mac/Linux并发、取消、重连测试通过。

过渡限制：两个`serve`进程指向同一endpoint会互相takeover。`serve`在daemon与Remote CRUD整体可用前保留为开发入口，但不作为0.1正式控制面。

## 0.1 必须完成

M4–M6放在同一长期feature分支中实现，按下面的内部提交边界推进；三者整体可用后再合并到`master`，避免出现只能启动空daemon、却无法添加remote的中间产品状态。

### M4 — Mac daemon、控制协议与配置存储

目标：建立单例后台进程、可靠本机控制面和崩溃一致的配置基础。

实现：

- 用户级LaunchAgent及`start/stop/status`；
- 私有control socket、本地control protocol v3、peer eUID验证和stale socket安全清理；
- daemon单写者配置store、config schema v2、fsync和原子rename；旧schema明确失败且不迁移；
- pending/active/removing生命周期及重启reconcile；
- `start`捕获并验证`ego-browser`绝对路径；
- 定义并单测bounded shutdown协调器：5秒grace、10秒总deadline；
- 定义并单测daemon全局8个active process和8MiB queued payload预算器；
- 当前`serve`保留用于开发回归，暂不从公开CLI删除。

验收：daemon单例、配置崩溃恢复、control socket安全，以及不依赖RemoteWorker的shutdown和资源预算原语测试通过。RemoteWorker接入后的stop强制清理与全局资源上限E2E归入M6。

### M5 — Protocol v2、endpoint identity与owner仲裁

目标：先建立Remote CRUD依赖的完整identity和ownership契约。

实现：

- protocol v2与identity/ownership capability，无v1 fallback；
- Linux稳定endpoint ID，私有`0700`运行/状态目录和no-follow原子创建；
- broker socket迁入owner验证的私有目录；
- Mac remote worker稳定owner ID；
- handshake在broker触碰现有socket前交换endpoint/owner identity；
- broker ready/owner conflict显式结果；
- same-owner reconnect、foreign-owner nonce probe/ack；
- 5秒probe timeout、15秒acquisition deadline、250ms retry；
- pending claimant串行化与竞态收敛；
- 新v2 golden fixture；
- 协议错误仅记录kind、request ID和length，禁止payload Debug泄露。

验收：alias identity、live owner拒绝、dead owner接管、网络分区恢复、多claimant、私有socket创建和日志sentinel测试通过。

M5冻结的是identity/ownership wire基线；最终0.1 remote exec protocol在M8升级为v3，不保留v2 fallback。

边界：ownership只fence Linux新请求入口；不承诺瞬时终止网络分区另一侧已运行的Mac child。

### M6 — Remote CRUD与RemoteWorker接入（已完成）

状态：代码、本地自动化及真实双Linux endpoint的Mac RemoteWorker E2E验收均已通过。

目标：以M4持久状态和M5 claim协议交付完整可用的多remote产品。

实现：

- `remote add/list/status/remove/retry`；
- `remote add <ssh-target>`只接受一个target参数并生成完整32字符小写十六进制config ID；
- doctor/status/retry/remove只接受完整config ID，不支持短前缀、名称、selector alias、迁移或fallback；
- list/add/retry输出config ID、target和状态，status detail不输出name；
- add采用pending-first，成功ready后提交active；
- remove先持久化removing tombstone再清理；
- daemon启动时reconcile pending和tombstone；
- 将当前`run_serve`封装为daemon RemoteWorker；
- SSH 255统一按临时session失败重试，协议不匹配、127和owner conflict进入error；
- endpoint alias重复和owner conflict返回明确错误；
- M6完成后删除公开`serve`入口或改为明确内部命令。

验收：一台Mac同时服务至少两个Linux；重复endpoint不产生第二配置；失败add无残留claim；pending/removing记录不因崩溃变为active；一个remote故障不影响其他remote；stop强制清理和daemon全局process/payload上限通过真实RemoteWorker E2E。

### M7 — Status与Doctor（已完成）

状态：代码、本地自动化及真实Mac/Linux手动E2E均已通过。

- daemon及remote config ID、desired/observed状态；
- remote status展示config ID、target、错误、重连、protocol/capabilities和请求容量，不输出name；
- doctor只读检查Mac本地LaunchAgent、绝对`ego-browser`和daemon当前worker快照；
- 未主动检查的SSH、Linux binary、endpoint identity文件、运行目录/socket权限和end-to-end probe明确显示`NOT CHECKED`，归入Post-0.1 hardening；
- exit code：0无失败、1环境、daemon、selector或快照异常、2用法错误；
- 不自动修复、不自动安装、不修改配置。

### M8 — 0.1发布准备

目标：用可审计的维护者手工流程准备首批候选产物，不把尚未建设的SSH自动化当作0.1前置条件。

候选目标仅为：

- `linux-x86_64`；
- `macos-aarch64`。

发布门禁：

- preparation-only `workflow_dispatch`在所选ref/commit的干净checkout运行`just check`等价门禁，不要求tag且不发布release；
- 输入version与Cargo version一致；
- 维护者分别构建并核验两个候选目标：`linux-x86_64`使用静态`x86_64-unknown-linux-musl` binary，并确认无program interpreter、动态依赖或`GLIBC_*`版本要求；`macos-aarch64`使用原生`aarch64-apple-darwin` binary；
- preparation workflow在Ubuntu 20.04、glibc 2.31容器中运行精确的已暂存Linux候选并验证`--version`；
- 候选产物包含`ego-browser-skill.tgz`，manifest提供其`skill_url`和SHA-256；为binary与skill archive生成并复核SHA-256；
- 在干净的Linux x86_64与macOS arm64环境手工验证安装、daemon控制面、Remote CRUD和一次真实`ego-browser`调用；
- README只陈述已验证的候选范围和实际发布状态，不宣称尚未完成的自动化；
- 实际发布前保持`distribution/latest.json`的`available: false`，installer仍不可用。

发布准备同时将remote exec protocol升级为v3：child exit signal使用canonical signal name白名单，unsupported child signal返回request error，未知wire signal视为protocol error；v2/v3 exact-version mismatch明确失败且无fallback。升级时先停止v2 Mac daemon，再更新Linux binary，最后启动v3 Mac daemon并通过status/doctor确认。remote name移除造成独立的本地wire shape变化，因此本地control protocol也升级为v3；config schema保持独立的v2，不迁移旧schema，不得混淆三个版本。

候选产物准备完成不等于已经发布。

### M9 — Release 0.1

仅在候选产物通过人工门禁并获得明确发布授权后：

- 将CHANGELOG中的`0.1.0`从Unreleased改为实际发布日期；
- 创建与Cargo version一致的`v0.1.0` tag；
- 创建GitHub Release并上传已验证的两个binary、`ego-browser-skill.tgz`及包含三者的`SHA256SUMS`；
- 将审核后的binary与skill URL及SHA-256写入`distribution/latest.json`并设置`available: true`；
- 在Linux x86_64与macOS arm64分别执行一次公开installer smoke；Linux覆盖下述可选交互流程，并保留独立手工skill步骤的同一显式版本、checksum校验、单个显式Agent ID及固定`skills@1.5.24`命令验证；macOS保持binary-only且不询问。

### Linux 可选 skill 原生交互（本次新增，待验收）

- binary与shim成功提交后通过`/dev/tty`询问`[Y/n]`；回车或`y`/`yes`（不区分大小写）继续，`n`/`no`跳过，非法输入重询；EOF、读失败或无TTY跳过并显示手动链接，不误判为默认同意；
- 同意前不检查Node/npm/npx、不下载skill、不触碰Agent目录；同意后才要求Node.js ≥22.20.0、npm/npx和tar，复用本次binary已获取的manifest下载同一release skill，校验可信URL、SHA-256、归档安全和解包完整性；
- 下载skill或启动CLI前，用固定`skills@1.5.24`配套guard检测Agent执行环境，命中时要求普通终端运行，不静默清空环境；升级版本须复核guard，不自建Agent探测表或选择器；
- 固定原生命令为`npx --yes skills@1.5.24 add <extracted-skill> --skill ego-browser --global --copy`；无内层`--yes`、`--agent`或`--all`，stdin/stdout/stderr接到TTY，支持直接运行和`curl | sh`；外层`npx --yes`不代替上游安装确认；
- 原生界面受上游选择逻辑约束：universal target不可取消，单Agent可能省略选择；上游取消也可能exit 0，安装器仅提示交互结束、以CLI输出为准，不一概宣称成功；重跑并同意可选流程可能覆盖已有skill及本地修改，跳过则不触碰；
- 验证无TTY/no/EOF且无Node/npx/tar时仍完成bridge安装；PTY覆盖回车/yes、非法输入、EOF、管道stdin和原生stdio/参数；缺依赖、旧Node、下载/校验/解包/完整性或CLI失败均明确“bridge已安装、skill未完成”，非零退出，不回滚bridge、不自动重试或降级；保持macOS、binary/shim校验、提交回滚与PIPE测试通过。

上一轮用户实机验收通过的事实保持有效；以上新流程的自动化和实机验收单独记录，不以本次文档更新宣称通过。

## 推荐分支与提交顺序

```text
feat/daemon-remotes
  feat(daemon): add launchd control plane and state store
  feat(protocol): add endpoint identity and owner arbitration
  feat(remote): manage persistent remote workers
feat/diagnostics
test/ssh-e2e
release/0.1
```

## 每阶段验证

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
python3 -m unittest scripts.test_unix_installer
git diff --check
```

涉及daemon、remote或ownership的实现阶段还必须执行对应的本地自动化和可用的真实环境验收。

## Post-0.1 hardening

- 建设专用Mac runner与可重置Linux VM/host上的SSH自动化；
- 使用deterministic fake `ego-browser`覆盖binary streams、exit/signal、8路并发、cancel、backpressure、daemon重启、Remote CRUD、endpoint去重、owner冲突、断网、claimant竞态和socket安全；
- 增加真实`ego-browser`网页smoke，并在master/nightly和release tag上fail closed；
- 执行至少一次24小时daemon、多remote和断网soak；
- 扩展SIGKILL状态恢复、双endpoint/claimant和重复竞态测试。

其他工作仅由真实需求驱动：

- remote update；
- bridge或skill的runtime updater（现有skill安装仅限用户同意的可选交互或独立手工步骤）；
- 可配置并发额度和跨remote公平调度；
- Homebrew等分发渠道；
- Sigstore、attestation与SBOM；
- 严格跨主机execution fencing；
- Windows或非SSH transport。

继续排除任意命令执行、Linux本地浏览器fallback和静默协议降级。