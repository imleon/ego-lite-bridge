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

## 快速开始

前置条件：

- macOS 的 `PATH` 中已有真正的 `ego-browser`。
- Linux 主机可通过非交互 SSH 认证访问。
- Linux 的 `~/.local/bin/ego-lite-bridge` 已安装。
- Mac 已安装 `ego-lite-bridge`。

在 Linux 安装二进制并创建 shim：

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

在 Mac 启动 daemon 并添加 remote：

```bash
ego-lite-bridge start
ego-lite-bridge remote add dev-linux user@linux-host
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
| `ego-lite-bridge status` | 检查 daemon 健康状态和 remote 数量。 | `running (<数量> remotes)` |
| `ego-lite-bridge remote add <名称> <SSH-target>` | 添加 remote，并等待其 broker ready。 | 以 `Active/Connected` 结尾的制表符分隔记录 |
| `ego-lite-bridge remote list` | 列出全部已配置 remote。 | 每个 remote 一行；空列表无输出 |
| `ego-lite-bridge remote status <名称或配置ID>` | 显示一个 remote 的生命周期和观测状态。 | 同一记录格式；错误信息可能显示在下一缩进行 |
| `ego-lite-bridge remote retry <名称或配置ID>` | 重试当前处于 `Active/Error` 的 remote。 | 更新后的 remote 记录 |
| `ego-lite-bridge remote remove <名称或配置ID>` | 删除 remote 并清理其 worker。 | `removed <配置ID>` |
| `ego-lite-bridge stop` | 停止 daemon 及其 worker。 | `ego-lite-bridge stopped`，已停止时为 `ego-lite-bridge is stopped` |

Remote 记录格式为 `<配置ID>\t<名称>\t<SSH-target>\t<生命周期>/<观测状态>`。所有 `<名称或配置ID>` 参数都接受 remote 名称或配置 ID。控制命令仅支持 macOS；Linux 提供 `ego-browser` shim。

## 从源码构建和安装

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

确保 `~/.local/bin` 位于 `PATH`。发行版可用后，`distribution/install.sh` 会执行相同的平台安装步骤。

## 当前限制

- 仅支持 macOS executor 和 Linux caller。
- 最多可并发执行 8 个 `ego-browser` 调用；达到容量后新增调用会立即被拒绝，阻塞或断开的请求不会阻塞其他请求。
- Linux broker 路径固定为 `~/.local/bin/ego-lite-bridge`。
- bridge 只转发命令参数和标准流，不映射 Mac 文件系统或环境变量。

## 信任边界

- Mac 与 Linux 之间的信任由 SSH 认证和主机密钥校验决定；启动 bridge 前应完成配置和验证。
- Linux runtime endpoint 为 `/tmp/ego-lite-bridge-<uid>/broker.sock` 和 `/tmp/ego-lite-bridge-<uid>/owner.sock`。目录权限为 `0700`，socket 权限为 `0600`，只有对应 Linux 用户可以连接。
- 以该 Linux 用户运行的任何进程都可以要求 Mac 使用任意参数和 stdin 启动固定的 `ego-browser`。只应面向可信的 Linux 账户运行 bridge。
- 浏览器输出和退出状态来自已连接的 Mac executor。系统不会回退到本地或其他浏览器。

## 故障排查

- **`ego-browser bridge is not connected`**：在 Mac 运行 `ego-lite-bridge start` 和 `ego-lite-bridge remote add <name> user@linux-host`。
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
