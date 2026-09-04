# ego-lite-bridge 产品需求文档

- 状态：Approved for implementation
- 目标版本：尚未发布的 0.1.0
- 产品范围：由 macOS 单例 daemon 管理一个或多个 Linux remote，并让 Linux 透明调用 Mac 上的 `ego-browser`

## 1. 背景

`ego-browser` 的浏览器状态和登录会话位于 Mac，但 AI Agent、开发工具和自动化任务经常运行在 Linux 主机。`ego-lite-bridge` 让 Linux 用户像调用本地 CLI 一样使用 `ego-browser`，实际进程仍在 Mac 上运行。

当前开发阶段的 `serve <linux-host>` 由每个进程维护一条连接，已经验证执行、并发和自动重连，但多个进程指向同一 Linux endpoint 时会竞争 broker ownership。正式产品改为一个 Mac daemon 统一管理 remote，并通过稳定的 Linux endpoint identity 和 broker owner 仲裁解决 alias 去重及跨 Mac 竞争。

Cargo 中的 `0.1.0` 是未发布开发版本；`distribution/latest.json` 在正式发布前必须保持 `available: false`。只有本 PRD 的 0.1 验收项完成后才能创建 `v0.1.0` tag。

## 2. 产品目标

Mac 用户只管理一个后台服务，并通过 CLI 管理 remote：

```bash
ego-lite-bridge start
ego-lite-bridge stop
ego-lite-bridge status
ego-lite-bridge remote add dev-linux gaolei.veew@linux-a
ego-lite-bridge remote list
ego-lite-bridge remote status dev-linux
ego-lite-bridge remote retry dev-linux
ego-lite-bridge remote remove dev-linux
```

Linux 用户继续按需运行：

```bash
ego-browser <args...>
```

系统通过 daemon 主动建立的 SSH channel，将请求交给 Mac 上真实的 `ego-browser`，并传递：

- argv，包括 Unix 非 UTF-8 参数；
- binary-safe stdin、stdout、stderr；
- stdin EOF；
- exit code，以及 Mac child 因 signal 终止时的 signal；
- spawn、协议和连接错误；
- request 级取消。

Linux shim 收到 SIGINT/SIGTERM 或断开时取消所属请求；取消会终止 Mac child process group，但不承诺保留原始取消 signal 的类型。

## 3. 产品原则

1. **Mac 主动连接**：Linux 不反向连接 Mac，也不持有 Mac 登录凭据。
2. **Mac 单例管理**：每个 Mac 用户只有一个 daemon，统一管理 remote、重连、状态和资源上限。
3. **固定执行目标**：只允许调用 Mac 上的 `ego-browser`，不提供任意 shell 或 executable 选择。
4. **失败透明**：daemon 不可用、remote 未连接、协议不匹配、容量已满或 owner 冲突时明确失败。
5. **禁止 fallback**：不在 Linux 本地运行浏览器，不切换浏览器、transport 或旧协议。
6. **请求隔离**：一个请求的取消、慢消费、异常或退出不能破坏其他请求或 remote。
7. **连接可恢复**：网络变化和 SSH 中断由 daemon 自动恢复，不同 owner 不得循环抢占。

## 4. 产品组成

```text
Linux ego-browser shim
        → Linux broker
        ⇄ SSH channel（由 Mac RemoteWorker 主动建立）
        ⇄ Mac executor
        → real ego-browser process

Mac CLI → private control socket → Mac daemon → RemoteWorker A/B/...
```

### 4.1 Mac daemon

- 每个登录用户单实例运行；
- 由 macOS `launchd` 管理，不自行 double-fork；
- 是配置文件的唯一 writer；
- 通过当前用户私有 Unix socket接受本机 CLI控制；
- 启动时恢复所有已配置 remote，并继续清理未完成的pending add或remove tombstone；
- 每个 remote worker独立维护 SSH、broker ownership和重连状态。

### 4.2 Mac CLI

CLI 只通过本机 control socket操作 daemon，不直接修改配置或持有 remote worker：

```text
start
stop
status
remote add <name> <ssh-target>
remote list
remote status <name-or-id>
remote remove <name-or-id>
remote retry <name-or-id>
```

0.1 不提供交互式 TUI或 GUI管理面。

### 4.3 Linux broker 与 shim

- 每个 Linux UID对应一个私有 broker endpoint；
- `ego-browser` 是 one-shot shim，只控制自己的 request；
- broker在一条 SSH channel上路由最多8个并发请求；
- Linux不运行真实浏览器，也不选择其他执行路径。

## 5. Daemon 生命周期

### 5.1 单实例与控制 socket

- 使用用户级 LaunchAgent identity保证正常启动路径只有一个daemon实例；
- 手工重复启动必须明确失败，不得替换运行中的daemon；
- socket path存在不代表daemon存活，只有成功完成版本化control handshake才算running；
- control socket目录必须为当前用户所有且`0700`，socket不得向其他用户开放；
- daemon通过`getpeereid`验证调用方eUID，尤其是`stop`、`remove`等破坏性命令；
- 清理stale socket前验证owner UID、file type、mode和inode/device identity；
- 崩溃或SIGKILL后不得留下阻止恢复的永久PID锁。

### 5.2 `ego-browser` 路径

LaunchAgent不得依赖交互式shell的`PATH`。`start`应解析并保存`ego-browser`的canonical absolute path：

- 必须是regular executable；
- owner必须是当前用户或root；
- daemon只执行该绝对路径；
- 路径变化需要显式重新`start`或后续配置命令；
- 配置中不保存浏览器凭据。

### 5.3 启停语义

`start`：

- 安装或加载当前用户LaunchAgent；
- 等待control handshake成功；
- 已运行时幂等返回当前状态；
- 单个remote失败不等于daemon启动失败。

`stop`：

1. 持久化daemon停止意图并停止接收配置修改和新请求；
2. 向active requests发出取消，等待最多5秒；
3. 强制终止仍存活的Mac process groups和SSH children；
4. 在总计10秒内完成本地worker和control socket清理；
5. 卸载LaunchAgent。

若本地资源全部清理但远端清理无法确认，`stop`返回非零并显示警告；不得因此保留活跃本地worker。

## 6. Remote 管理与持久化

### 6.1 Remote 名称与 target

Remote name：

- 1–64个ASCII字母、数字、`-`、`_`或`.`；
- 不允许以`.`开头；
- 不允许`all`、`default`等保留名；
- 在同一daemon配置中唯一。

SSH target：

- 是单个UTF-8 OpenSSH destination operand，例如`host`或`user@host`；
- 不接受额外SSH option，不允许以`-`开头；
- 使用`BatchMode=yes`和有界connect/handshake timeout；
- 保留用户/系统SSH config和host-key验证；
- 不允许交互式密码提示。

### 6.2 持久模型

每个remote记录：

- `config_id`：Mac本地稳定记录ID；
- `name`：用户指定名称；
- `target`：原始OpenSSH target；
- `endpoint_id`：成功识别后记录的Linux endpoint identity；
- `lifecycle`：`pending`、`active`或`removing`；
- `observed_state`：`connecting`、`connected`、`reconnecting`、`error`或`removing`；
- 最近状态变化时间和不含敏感数据的错误摘要。

每个remote只使用一条持久记录，不建立独立operation journal。`pending → active`和`active → removing`通过整份配置原子替换完成。只有`active`记录表示daemon应长期保持连接；`pending`表示一次有deadline的初始add；`removing`是禁止重连并等待清理的tombstone。daemon持续reconcile持久配置与runtime state，跨Mac/Linux状态不假装为单一事务。

### 6.3 添加 remote

```bash
ego-lite-bridge remote add <name> <ssh-target>
```

流程：

1. 验证name和target；
2. 原子写入一条`lifecycle=pending`的remote记录；
3. 在30秒总deadline内建立SSH并验证remote bridge、protocol/capabilities；
4. 读取Linux endpoint identity并检查重复；
5. 获取broker ownership并等待ready；
6. 在同一条记录上原子转换为`lifecycle=active`、`observed_state=connected`。

命令成功必须表示端到端ready。30秒内允许按统一backoff重试SSH临时失败；deadline到达后回滚已获取的运行态并删除pending记录，命令明确失败。CLI连接在命令执行期间断开时，daemon继续该操作直到成功或deadline，不因客户端消失留下未定义状态；结果可通过`remote status`查询。若daemon崩溃，重启后先清理pending记录可能遗留的ownership，再删除记录，不自动转为active。

### 6.4 Host alias 与 endpoint 去重

不得用target字符串、DNS结果、IP或host key单独判断是否为同一endpoint。

Linux bridge为当前UID持久保存随机endpoint ID：

```text
~/.local/state/ego-lite-bridge/endpoint-id
```

要求：

- 至少128-bit系统随机值；
- 父目录`0700`、文件`0600`；
- no-follow、原子创建并稳定复用；
- 仅用于identity，不作为认证凭据。

判定：

- 相同endpoint、相同target：返回already exists；
- 相同endpoint、不同target：报告现有remote及两个target，不静默替换；
- 不同endpoint、相同name：拒绝名称冲突；
- target变更需要未来显式`remote update`，0.1不提供隐式替换。

### 6.5 Remove 与 retry

`remote remove`：

1. 先原子持久化`lifecycle=removing` tombstone；
2. 停止新请求，取消active requests并关闭SSH；
3. 等待5秒后强制终止本地child和SSH process group；
4. 本地清理总deadline为10秒；
5. 清理成功后删除record；远端清理无法确认时保留tombstone和错误，重启后继续清理，绝不重连。

`remote retry`：

- 仅用于重新尝试`error`状态的正式remote；
- 不改变target、identity或ownership规则；
- owner conflict在原owner停止后可由用户显式retry。

强制清理发生或远端清理无法确认时命令返回非零，但`removing`状态必须已经持久化。0.1不提供暂停remote；不再需要某个remote时使用`remove`，之后可重新`add`。

### 6.6 SSH失败分类

OpenSSH的exit 255不能可靠区分认证、DNS、路由、host-key和传输故障，产品不得解析stderr猜测永久类别。

- active remote的SSH launch/session失败保留最后诊断并按250ms、1s、2s、5s封顶的backoff持续重试；初始add只在其30秒总deadline内重试；
- protocol/capability mismatch、明确的remote binary missing（127）和owner conflict进入`error`，不自动重试；
- 用户通过`remote retry`显式恢复永久错误。

## 7. Endpoint ownership

### 7.1 保证范围

Linux broker保证：

- 任一时刻只有当前owner可以通过该endpoint接收**新的Linux请求**；
- 请求不会迁移到另一个owner，也不会在断线后自动重试。

Linux broker无法瞬时fence网络分区另一侧已经运行的Mac进程。former owner上的既有请求可能继续，直到其SSH keepalive或其他channel-loss检测触发本地清理；检测到后必须终止相关process groups。严格的跨主机执行fencing需要外部一致性服务，不属于0.1范围。

### 7.2 Owner identity

- daemon每次启动生成新的daemon instance ID；
- 每个remote worker拥有在其所有reconnect中稳定的随机owner ID；
- owner ID不持久化、不进入文件名、不作为认证凭据。

### 7.3 仲裁规则

| 当前状态 | claimant | 行为 |
|---|---|---|
| 无broker或stale socket | 任意owner | 获取ownership |
| broker存活 | 同owner reconnect | 立即替换自己的旧channel |
| broker存活且当前owner可响应 | 不同owner | 拒绝claimant，当前owner不受影响 |
| broker存活但当前owner不可响应 | 不同owner | probe timeout后允许接管 |
| 正在验证另一个claimant | 其他owner | 返回retry，不覆盖当前仲裁 |

仲裁参数固定为：

- owner probe timeout：5秒；
- claimant总acquisition deadline：15秒；
- retry间隔：250ms；
- 每endpoint同时只允许一个pending foreign claimant；
- probe timeout从probe frame成功写入当前owner channel之后开始；
- owner control frames由中央router直接处理，不进入request mailbox，也不受request backpressure影响。

existing broker通过当前SSH channel发送随机nonce probe；只有匹配ack证明owner仍活跃。错误或过期nonce不能建立活性。

### 7.4 网络分区

若旧owner失联而新owner可连接Linux：

- 新owner在probe timeout后取得新请求admission ownership；
- 旧owner上已经运行的请求可能持续到其检测到channel loss；
- 网络恢复后，旧worker发现endpoint已有可响应owner，进入永久`error`，不得夺回；
- 新请求始终只由Linux当前owner接收。

## 8. 请求执行与资源边界

### 8.1 并发

- 每个remote最多8个active requests；
- daemon全局最多8个active `ego-browser` processes；
- 满载立即返回capacity error，不无限排队；
- 0.1不提供动态配置或复杂公平调度。

### 8.2 消息与内存边界

- framing最大payload保持2MiB，用于整体协议防护；
- `Stdin`、`Stdout`、`Stderr`单消息data最大64KiB；
- 每方向每请求最多排队8个data frames，即512KiB payload；
- daemon全部request mailbox payload总预算为8MiB；
- 超过单消息、request queue或daemon预算时只取消所属请求并返回明确错误；
- argv整体编码仍受2MiB frame上限约束。

### 8.3 隔离与清理

- 每请求使用独立bounded queue；
- 慢客户端超过buffer或write timeout后只取消自身；
- duplicate active request ID只拒绝冲突请求；
- request ID在远端终态和本地worker均结束前不得复用；
- remote channel断开时，其请求全部失败并被唤醒；
- Mac child使用独立process group，取消和断线必须终止并回收整个组；
- 一个remote故障不得影响其他remote。

## 9. Linux broker socket安全

broker socket路径改为：

```text
/tmp/ego-lite-bridge-<uid>/broker.sock
```

要求：

- 使用限制性umask创建父目录并设置`0700`；
- 已存在目录必须通过`lstat`验证：是目录、不是symlink、owner为当前eUID、mode为`0700`；
- 验证失败时fail closed，不删除或修复未知对象；
- socket只在私有目录中bind，并保持`0600`；
- stale socket删除与最终cleanup继续校验file type、owner和inode/device identity；
- 测试必须覆盖创建期间其他UID无权连接，而不只检查最终mode。

## 10. Mac配置与控制协议

建议路径：

```text
~/Library/Application Support/ego-lite-bridge/config.json
~/Library/Application Support/ego-lite-bridge/control.sock
```

要求：

- 目录`0700`，配置和socket仅当前用户可访问；
- daemon是配置唯一writer；
- 配置写入使用同目录临时文件、flush、fsync和原子rename，并包含schema version；
- 未知或损坏schema明确失败，不猜测修复；
- CLI与daemon使用独立、版本化、长度限制的本机控制协议；
- 控制协议不复用远程exec协议，也不能传递任意shell命令。

## 11. 日志与敏感数据

状态和错误应清晰表达：

- daemon running/stopped；
- remote connecting/connected/reconnecting/error/removing；
- protocol/capability mismatch；
- endpoint duplicate与owner conflict；
- capacity/backpressure rejection；
- channel disconnect与下次重连时间。

禁止输出：

- owner ID、endpoint ID和nonce原值；
- SSH凭据；
- argv内容；
- stdin/stdout/stderr payload；
- 浏览器页面内容。

协议错误只能记录message kind、request ID和payload length；不得对request-scoped frame整体使用`Debug`格式化。测试使用sentinel secret断言所有错误和日志均不包含payload。

## 12. 协议要求

正式0.1采用包含endpoint identity与ownership语义的protocol v2：

- exact-version和exact-capability匹配；
- 不支持v1 fallback或静默降级；
- Mac owner identity必须在candidate broker触碰现有socket之前交换；
- ownership确定后broker返回明确ready或owner-conflict；
- takeover、liveness probe和ack使用framed protocol，不解析日志；
- v2 golden fixture覆盖全部消息和仲裁状态；
- wire shape变化必须显式升级版本和fixture。

## 13. Status 与 Doctor

```bash
ego-lite-bridge status
ego-lite-bridge remote list
ego-lite-bridge remote status <name-or-id>
ego-lite-bridge doctor [name-or-id]
```

- status展示daemon和持久remote的desired/observed state；
- remote status展示最近错误、重连状态、protocol/capabilities和请求容量；
- doctor执行Mac环境、SSH、remote bridge、endpoint identity、socket权限和端到端probe检查；
- exit code：0健康、1环境或连接异常、2用法错误；
- doctor不自动修复、不安装软件、不修改配置。

## 14. 非目标

0.1不实现：

- 任意命令远程执行；
- Linux本地浏览器fallback；
- 非SSH transport；
- Windows；
- daemon GUI或TUI；
- 自动更新；
- dashboard或metrics框架；
- owner手工强制抢占；
- hostname/IP/SSH alias规范化；
- 跨UID broker共享；
- 运行中请求迁移或自动重试；
- 严格跨主机execution fencing；
- 动态并发配额和复杂公平调度。

## 15. 验收标准

### 15.1 Daemon与remote管理

- macOS登录用户只能运行一个daemon；重复start幂等或明确失败；
- control socket path残留不会被误判为running；其他eUID不能执行控制命令；
- add成功代表端到端ready；失败或崩溃后的pending add最终回滚；
- daemon重启后恢复全部正式remote，并继续清理pending add和removing tombstone；
- remove在deadline内清理本地资源，超时保留tombstone并明确失败；
- 一台Mac可同时服务至少两个Linux endpoint。

### 15.2 Identity与ownership

- IP、domain和SSH alias连接到同一Linux UID时识别为同一endpoint；
- 同endpoint第二个live owner被拒绝，第一个持续接收新请求；
- 同owner断网恢复后立即收回自己的stale broker；
- 原owner死亡后，新owner在15秒acquisition deadline内成功或明确失败；
- 网络分区恢复后旧owner不夺回新请求admission；
- 多claimant竞争最终只有一个owner；
- SIGKILL注入每个add/remove转换点后，重启不得激活pending或removing remote，并最终释放孤立claim。

### 15.3 执行与可靠性

- argv、非UTF-8 argv、binary streams、EOF、exit、child exit signal和Cancel语义符合第2节；
- 8路并发不串流，单请求失败不影响其他请求；
- daemon全局active process不超过8，queued payload不超过8MiB；
- takeover、断线、remove和stop后无残留Mac child、Linux shim或无限等待；
- broker运行目录和socket从创建开始不可被其他UID访问；
- 日志sentinel测试证明不泄露request payload；
- 并发和ownership核心测试连续100轮无偶发失败；
- Linux/macOS tests、Clippy `-D warnings`和release build全部通过。

## 16. 过渡与升级

当前`serve <linux-host>`是开发入口，不是0.1最终控制面。在daemon与Remote CRUD整体可用前保留该入口，避免中间版本不可用；最终切换时删除公开入口或改为明确内部命令，不保留静默兼容别名。

protocol v2升级顺序：

1. 停止现有v1 Mac supervisor；
2. 更新Linux binary；
3. 更新并启动v2 Mac daemon；
4. 通过`remote add`建立配置。

不得在v1 Linux broker仍运行时直接用v2 Mac尝试接管。正式0.1只承诺daemon架构和protocol v2，不承诺开发阶段v1兼容。