# ego-lite-bridge

[English](README.md)

`ego-lite-bridge` 让 Linux 主机使用运行在 Mac 上的 `ego-browser`。浏览器进程和登录状态保留在 Mac；Linux 获得一个本地命令形态的 `ego-browser`，其参数、stdin、stdout、stderr、信号和退出状态通过持久 SSH 通道转发。

本项目派生自 [Herdr](https://github.com/herdrdev/herdr)，并继续采用 Apache-2.0 许可证。

## 架构

```text
Linux ego-browser shim -> Linux broker -> SSH 通道 -> Mac executor -> ego-browser
```

- macOS daemon 持有通过 `ego-lite-bridge remote ...` 配置的持久通道。
- Linux 上的私有 broker socket 接收本地 `ego-browser` 调用。
- 可执行文件名为 `ego-browser` 时进入 shim 模式；真正的二进制只在 Mac 上启动。
- bridge 不可用时，Linux 命令明确失败，不会回退到本地执行。

## 发布状态

版本 0.1.1 为 `linux-x86_64` 和 `macos-aarch64` 提供预编译二进制，用户无需安装 Rust 或从源码构建。Linux 发行版是静态 `x86_64-unknown-linux-musl` binary，macOS 发行版是原生 `aarch64-apple-darwin` binary。

## 快速开始

前置条件：

- macOS 的 `PATH` 中已有真正的 `ego-browser`。
- Linux 主机可通过非交互 SSH 认证访问。
- 两台机器的 `PATH` 中都包含 `~/.local/bin`。

在 Mac 和 Linux 主机上分别安装最新版本：

```bash
curl -fsSL https://raw.githubusercontent.com/imleon/ego-lite-bridge/master/distribution/install.sh | sh
```

安装器会使用发行 manifest 中的 SHA-256 校验二进制。在 macOS 上只安装 `ego-lite-bridge`，不询问 skill 安装。在 Linux 上成功提交 binary 和 `ego-browser` shim 后，通过 `/dev/tty` 询问 `[Y/n]`：回车或 `y`/`yes`（不区分大小写）进入可选 skill 安装；`n`/`no` 跳过；非法输入重新询问。EOF、读取失败或无可用终端时跳过 skill 并显示手动安装链接。跳过时不需要 Node.js、npm/npx 或 tar，也不触碰 Agent 目录。

只有同意后，可选步骤才要求 Node.js 22.20.0 或更高版本、`npm`/`npx` 和 `tar`。安装器复用本次 binary 已获取的 manifest，下载同一 release 的 skill，校验 SHA-256、检查归档安全后解包。在任何 skill 下载或 CLI 启动前，与固定 `skills@1.5.24` 版本配套的 guard 会检测 Agent 执行环境；命中时明确要求用户在普通终端重跑，不静默清空环境变量，也不启动可能自动确认的 CLI。

安装器运行 `npx --yes skills@1.5.24 add <extracted-skill> --skill ego-browser --global --copy`，stdin、stdout、stderr 均连接 `/dev/tty`，同样支持 `curl | sh`。外层 `npx --yes` 允许获取 CLI；不传内层 `--yes`、`--agent` 或 `--all`。Agent 选择与确认使用上游原生界面，不自建选择器：universal target 不可取消，单 Agent 环境可能省略选择界面。默认 `[Y/n]` 不代表替用户接受全部上游选项。

可选步骤失败时，安装器非零退出并明确说明 bridge 已安装、skill 步骤未完成；不回滚 bridge，不自动重试或降级。上游取消也可能退出 0，因此零退出只表示交互流程已结束，请以上游 CLI 输出为准，不一概报告 skill 安装成功。CLI 可能只完成部分目标写入，整体 exit 0 不保证每个目标成功，已写入的 skill 不会回滚。

<a id="optional-agent-skill-installation"></a>
### 可选的 Agent skill 手工安装

release 保留 vendored `ego-browser-skill.tgz` 资产，以及 manifest 中对应的 URL 和 SHA-256。除了安装器的原生交互流程，也可以独立选择在 Linux 上为明确的 Agent 手工安装，需要 Node.js 22.20.0 或更高版本、`npm`/`npx` 和 `tar`。将 `VERSION` 设置为已安装 bridge 的同一明确 release 版本，并在 shell 中将 `AGENT_ID` 设置为 skills CLI 支持的一个明确 Agent ID；此手工命令不得省略 `--agent` 或使用 `*`。

```bash
(
  set -eu
  VERSION=0.1.1
  : "${AGENT_ID:?请将 AGENT_ID 设置为一个明确的 skills CLI Agent ID}"
  WORK_DIR="$(mktemp -d)"
  trap 'rm -rf "$WORK_DIR"' EXIT
  cd "$WORK_DIR"
  RELEASE_URL="https://github.com/imleon/ego-lite-bridge/releases/download/v${VERSION}"
  curl -fLO "${RELEASE_URL}/ego-browser-skill.tgz"
  curl -fLO "${RELEASE_URL}/SHA256SUMS"
  grep ' ego-browser-skill.tgz$' SHA256SUMS > ego-browser-skill.sha256
  sha256sum -c ego-browser-skill.sha256
  mkdir skill
  tar -xzf ego-browser-skill.tgz -C skill
  npx --yes skills@1.5.24 add "$WORK_DIR/skill/ego-browser" \
    --skill ego-browser --global --agent "$AGENT_ID" --yes --copy
)
```

skills CLI 可能覆盖该 Agent 已有的 `ego-browser` skill，包括本地修改。重跑 bridge installer 并同意可选流程也可能覆盖已有 skill；跳过则保持不变。这不是 runtime updater，也不会自动清理 skill。vendored skill 仍作为 release asset 和 manifest entry 管理。skill 调用仍经 Linux shim 转发到 Mac 上运行的真实浏览器。

如果希望手工下载和安装 bridge，请在两台机器上分别运行对应命令。以下步骤只安装 bridge binary 和 Linux shim。

macOS arm64：

```bash
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/ego-lite-bridge-macos-aarch64
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/SHA256SUMS
grep ' ego-lite-bridge-macos-aarch64$' SHA256SUMS | shasum -a 256 -c -
mkdir -p ~/.local/bin
install -m755 ego-lite-bridge-macos-aarch64 ~/.local/bin/ego-lite-bridge
```

Linux x86_64：

```bash
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/ego-lite-bridge-linux-x86_64
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/SHA256SUMS
grep ' ego-lite-bridge-linux-x86_64$' SHA256SUMS | sha256sum -c -
mkdir -p ~/.local/bin
install -m755 ego-lite-bridge-linux-x86_64 ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

在 Mac 启动 daemon 并添加 remote：

```bash
ego-lite-bridge start
ego-lite-bridge remote add user@linux-host
```

然后在 Linux 像使用本地命令一样调用：

```bash
ego-browser --help
ego-browser <args...>
```

daemon 会在短暂的 SSH 或网络故障后自动重连。

## 命令参考

以下控制命令均在 macOS 运行：

| 命令 | 用途 | 成功输出 |
| --- | --- | --- |
| `ego-lite-bridge start` | 启动用户级 daemon；重复运行安全。 | `ego-lite-bridge started`，已启动时为 `ego-lite-bridge is running` |
| `ego-lite-bridge status` | 显示 daemon 健康状态，以及每个 remote 的 desired/observed 状态。 | `daemon=running remotes=<数量>`，随后每个 remote 一行 `<配置ID> desired=<状态> observed=<状态>` |
| `ego-lite-bridge doctor [配置ID]` | 检查 Mac 本地环境，以及 daemon 中全部 remote 或指定 remote 的当前快照。 | 下文说明的 `PASS`、`FAIL` 和 `NOT CHECKED` 记录 |
| `ego-lite-bridge remote add <SSH-target>` | 添加 remote，并等待其 broker ready。 | `<配置ID>\t<SSH-target>\tdesired=active observed=connected` |
| `ego-lite-bridge remote list` | 列出全部已配置 remote。 | 每个 remote 一行 `<配置ID>\t<SSH-target>\tdesired=<状态> observed=<状态>`；空列表无输出 |
| `ego-lite-bridge remote status <配置ID>` | 显示右侧所列的 remote 字段。 | 带标签的多行输出：`config-id`、`target`、`desired`、`observed`、`state-changed-unix-ms`、`last-error`、`protocol-version`、`capabilities`、`reconnect-attempt`、`reconnect-at-unix-ms` 和 `active-requests` |
| `ego-lite-bridge remote retry <配置ID>` | 重试当前处于 `active/error` 的 remote。 | 更新后 remote 的 `remote list` 记录 |
| `ego-lite-bridge remote remove <配置ID>` | 删除 remote 并清理其 worker。 | `removed <配置ID>` |
| `ego-lite-bridge stop` | 停止 daemon 及其 worker。 | `ego-lite-bridge stopped`，已停止时为 `ego-lite-bridge is stopped` |

Desired 状态为 `pending`、`active` 和 `removing`；observed 状态为 `connecting`、`connected`、`reconnecting`、`error` 和 `removing`。当前无法获得的详情显示为 `unknown`；`active-requests` 格式为 `<活跃数>/<容量>`。所有 `[配置ID]` 或 `<配置ID>` selector 都必须是 `remote add` 或 `remote list` 输出的完整 32 字符小写十六进制 ID；不支持短前缀、名称、selector alias、迁移或 fallback。

`doctor` 是只读命令。M7 检查 LaunchAgent 是否 loaded、daemon 是否 running，以及配置中的 `ego-browser` 绝对路径是否有效。对每个 remote，它检查持久配置中 endpoint identity 是否存在、desired/observed 状态，以及 daemon **当前 worker 快照**中的 handshake、容量和重连/错误信息；不验证 live endpoint identity 是否与持久值匹配。`PASS` 表示被检查的本地状态或快照健康；`FAIL` 表示环境、daemon、selector 或快照检查失败；`NOT CHECKED` 明确表示 M7 没有新建 SSH 连接，也没有验证 live endpoint identity、Linux socket 权限或端到端执行。这些主动 remote 检查计划在 Post-0.1 hardening 中完成。没有 `FAIL` 时退出状态为 0，存在任一 `FAIL` 时为 1，`doctor` 语法无效时为 2。`doctor` 不修复、不安装，也不修改配置。

控制命令仅支持 macOS；Linux 提供 `ego-browser` shim。

## 从源码开发

需要 Rust 和 `just`。

```bash
git clone https://github.com/imleon/ego-lite-bridge.git
cd ego-lite-bridge
just build
```

安装到 macOS：

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
```

安装到 Linux：

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

以上步骤仅供贡献者从源码构建；普通用户应使用上方的发行版安装器。

## 当前限制

- 仅支持 macOS executor 和 Linux caller。
- 最多可并发执行 8 个 `ego-browser` 调用；达到容量后新增调用会立即被拒绝，阻塞或断开的请求不会阻塞其他请求。
- Linux broker 路径固定为 `~/.local/bin/ego-lite-bridge`。
- bridge 转发命令参数、标准流和请求级 PNG 截图；不映射 Mac 文件系统或环境变量。

## 信任边界

- Mac 与 Linux 之间的信任由 SSH 认证和主机密钥校验决定；启动 bridge 前应完成配置和验证。
- Linux runtime endpoint 为 `/tmp/ego-lite-bridge-<uid>/broker.sock` 和 `/tmp/ego-lite-bridge-<uid>/owner.sock`。目录权限为 `0700`，socket 权限为 `0600`，只有对应 Linux 用户可以连接。
- 以该 Linux 用户运行的任何进程都可以要求 Mac 使用任意参数和 stdin 启动固定的 `ego-browser`。只应面向可信的 Linux 账户运行 bridge。
- 浏览器输出和退出状态来自已连接的 Mac executor。PNG 截图只会从每请求的 `/tmp/ego-lite-bridge-screenshots-*` transfer directory 回传；stdout 和 stderr 中的路径不会被解析为文件。系统不会回退到本地或其他浏览器。

## 故障排查

- **`ego-browser bridge is not connected`**：在 Mac 运行 `ego-lite-bridge start` 和 `ego-lite-bridge remote add user@linux-host`。
- **SSH 反复重连**：确认 `ssh user@linux-host true` 无需密码或确认即可成功；bridge 使用 SSH batch mode。
- **远端二进制缺失**：在 Linux 的 `~/.local/bin/ego-lite-bridge` 安装可执行文件。
- **Linux 找不到 `ego-browser`**：创建上述软链接，并将 `~/.local/bin` 加入 `PATH`。
- **Mac 启动进程失败**：确认真正的 `ego-browser` 位于 `ego-lite-bridge` 继承的 `PATH` 中。
- **Linux runtime endpoint 残留**：停止 Mac bridge，确认没有 broker 运行后再删除 `/tmp/ego-lite-bridge-$(id -u)/`，然后重新启动。

Mac bridge 和 Linux broker 都会将生命周期及请求诊断写入 stderr。

## 开发

```bash
just test             # Rust 测试
just installer-test   # Unix 安装器测试
just check            # 格式、Clippy、Rust 测试和安装器测试

# 可选：真实 Mac -> SSH 可达 Linux smoke（不属于 just check）
EGO_LITE_BRIDGE_BIN=target/release/ego-lite-bridge \
EGO_LITE_BRIDGE_SSH_TARGET=user@linux-host just e2e-manual
```

迭代时运行最小相关测试，提交前运行 `just check`。手动 smoke 需要 `EGO_LITE_BRIDGE_BIN`（当前 macOS binary）和 `EGO_LITE_BRIDGE_SSH_TARGET`（已安装 Linux bridge 的 SSH 目标）；可用 `EGO_LITE_BRIDGE_LINUX_SHIM` 覆盖默认的 `~/.local/bin/ego-browser`。该测试会启停 daemon，不要在 daemon 正服务其他任务时运行。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)。代码库派生自 Herdr；此归属说明不代表 Herdr 项目为本项目背书。
