# ego-lite-bridge

[English](README.md)

`ego-lite-bridge` 让 Linux 主机使用运行在 Mac 上的 `ego-browser`。浏览器进程和登录状态保留在 Mac；Linux 获得一个本地命令形态的 `ego-browser`，其参数、stdin、stdout、stderr、信号和退出状态通过持久 SSH 通道转发。

本项目派生自 [Herdr](https://github.com/herdrdev/herdr)，并继续采用 Apache-2.0 许可证。

## 架构

```text
Linux ego-browser shim -> Linux broker -> SSH 通道 -> Mac executor -> ego-browser
```

- macOS 运行 `ego-lite-bridge serve <linux-host>` 并持有通道。
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

在 Mac 启动持久 bridge：

```bash
ego-lite-bridge serve user@linux-host
```

然后在 Linux 像使用本地命令一样调用：

```bash
ego-browser --help
ego-browser <args...>
```

保持 Mac 命令运行。短暂的 SSH 或网络故障后它会自动重连；按 `Ctrl-C` 停止。

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
- Linux broker socket 为 `/tmp/ego-lite-bridge-<uid>.sock`，权限为 `0600`，只有对应 Linux 用户可以提交请求。
- 以该 Linux 用户运行的任何进程都可以要求 Mac 使用任意参数和 stdin 启动固定的 `ego-browser`。只应面向可信的 Linux 账户运行 bridge。
- 浏览器输出和退出状态来自已连接的 Mac executor。系统不会回退到本地或其他浏览器。

## 故障排查

- **`ego-browser bridge is not connected`**：在 Mac 启动并保持运行 `ego-lite-bridge serve user@linux-host`。
- **SSH 反复重连**：确认 `ssh user@linux-host true` 无需密码或确认即可成功；bridge 使用 SSH batch mode。
- **远端二进制缺失**：在 Linux 的 `~/.local/bin/ego-lite-bridge` 安装可执行文件。
- **Linux 找不到 `ego-browser`**：创建上述软链接，并将 `~/.local/bin` 加入 `PATH`。
- **Mac 启动进程失败**：确认真正的 `ego-browser` 位于 `ego-lite-bridge` 继承的 `PATH` 中。
- **Linux socket 残留**：停止 Mac bridge，确认没有 broker 运行后再删除 `/tmp/ego-lite-bridge-$(id -u).sock`，然后重新启动。

Mac bridge 和 Linux broker 都会将生命周期及请求诊断写入 stderr。

## 开发

```bash
just test             # Rust 测试
just installer-test   # Unix 安装器测试
just check            # 格式、Clippy、Rust 测试和安装器测试
```

迭代时运行最小相关测试，提交前运行 `just check`。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)。代码库派生自 Herdr；此归属说明不代表 Herdr 项目为本项目背书。
