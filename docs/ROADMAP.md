# ego-lite-bridge 路线图

本路线图以 [`PRD.md`](PRD.md) 为产品行为基准。每阶段保持可构建和可测试，不通过fallback隐藏未完成能力。

## 已完成

### M0 — 分发安全

- installer只接受本产品manifest和可信release URL；
- release不可用或checksum失败时不覆盖已安装binary。

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
- 私有control socket、版本化协议、peer eUID验证和stale socket安全清理；
- daemon单写者配置store、schema version、fsync和原子rename；
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

边界：ownership只fence Linux新请求入口；不承诺瞬时终止网络分区另一侧已运行的Mac child。

### M6 — Remote CRUD与RemoteWorker接入（已实现，待真实E2E验收）

状态：代码与本地自动化已实现；真实Mac/Linux RemoteWorker E2E验收尚未完成，因此本阶段不标记为已完成。

目标：以M4持久状态和M5 claim协议交付完整可用的多remote产品。

实现：

- `remote add/list/status/remove/retry`；
- name和SSH target严格语法；
- add采用pending-first，成功ready后提交active；
- remove先持久化removing tombstone再清理；
- daemon启动时reconcile pending和tombstone；
- 将当前`run_serve`封装为daemon RemoteWorker；
- SSH 255统一按临时session失败重试，协议不匹配、127和owner conflict进入error；
- endpoint alias重复、name冲突和owner conflict返回明确错误；
- M6完成后删除公开`serve`入口或改为明确内部命令。

验收：一台Mac同时服务至少两个Linux；重复endpoint不产生第二配置；失败add无残留claim；pending/removing记录不因崩溃变为active；一个remote故障不影响其他remote；stop强制清理和daemon全局process/payload上限通过真实RemoteWorker E2E。

### M7 — Status与Doctor

- daemon及remote desired/observed状态；
- end-to-end probe；
- Mac检查LaunchAgent、绝对`ego-browser`、SSH；
- Linux检查binary、endpoint identity、运行目录和socket权限；
- exit code：0健康、1环境或连接异常、2用法错误；
- 不自动修复、不自动安装。

### M8 — 真实SSH自动化门禁

- 专用Mac runner与可重置Linux VM/host；
- deterministic fake `ego-browser`；
- 覆盖binary streams、exit/signal、8路并发、cancel、backpressure、daemon重启、Remote CRUD、endpoint去重、owner冲突、断网、claimant竞态和socket安全；
- 增加一条真实`ego-browser`网页smoke；
- master/nightly和release tag环境不可用时失败，不skip。

### M9 — Release 0.1

- 只保留CI、SSH integration和release工作流；
- tag与Cargo version一致；
- 发布经过验证的Linux/macOS架构；
- workflow生成SHA-256和`latest.json`；
- 干净Mac/Linux运行installer smoke；
- 完成至少一次24小时daemon、多remote、断网soak；
- README和installer只展示daemon产品入口；
- 发布前`distribution/latest.json`保持`available: false`。

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

涉及daemon、remote或ownership的阶段还必须执行：

- add/remove每个状态转换点的SIGKILL恢复测试；
- 两个Linux endpoint和两个Mac claimant的真实E2E；
- 100轮并发、claim和断线竞态测试。

24小时soak是release gate，不作为单个实现阶段的完成条件。

## Post-0.1

仅由真实需求驱动：

- remote update/rename；
- 可配置并发额度和跨remote公平调度；
- Homebrew等分发渠道；
- Sigstore、attestation与SBOM；
- 严格跨主机execution fencing；
- Windows或非SSH transport。

继续排除任意命令执行、Linux本地浏览器fallback和静默协议降级。