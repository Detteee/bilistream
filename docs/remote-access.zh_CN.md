# 远程访问与密码

[返回快速开始](../README.zh_CN.md) · [English](remote-access.md)

控制面板默认监听 `127.0.0.1:3150`。在同一台电脑使用时，可以不设置密码，直接打开 `http://localhost:3150`。

## 在 Web UI 设置密码

在向导最后一步勾选「设置面板密码」并填写密码；只在本机使用也可跳过。已有密码时向导会保留。完成配置后，在「系统设置 → 安全」设置、更改或清除密码。更改和清除均需当前密码，并立即退出所有登录。在设置中设置或更改后需重新登录；向导创建密码后保持登录。

首次设置必须通过服务器本机直接连接回环监听地址。在开放反向代理或隧道前先设好密码，即使程序仍监听 localhost。监听非本机回环地址时不能清除密码。

需要局域网访问时，先在本机设置密码，停止程序，再运行：

```bash
./bilistream --bind 0.0.0.0
```

在其他电脑访问时，使用服务器地址而非 `localhost`。远程访问请通过反向代理使用 HTTPS，并保持浏览器 Origin 与 Host 一致。同一连接 IP 密码错误 5 次后，最多限制 1 分钟。

## 无桌面服务器与服务首次启动

尚未保存密码选择的新安装，可通过 `--password-file`、`--password` 或 `BILISTREAM_PASSWORD` 提供初始密码。Linux/bash 示例：

```bash
umask 077
mkdir -p ~/.config/bilistream
read -rsp 'Web UI 密码: ' bilistream_password
printf '\n'
printf '%s' "$bilistream_password" > ~/.config/bilistream/webui-password
unset bilistream_password
chmod 600 ~/.config/bilistream/webui-password
./bilistream --bind 0.0.0.0 --password-file ~/.config/bilistream/webui-password
```

文件以明文保存密码，由账号权限保护。密码只导入一次，随后在本安装的加密存储中保存。后续启动使用已保存的选择；修改文件或环境变量不能覆盖已有密码，也不能恢复已明确清除的密码。成功导入且其他安装不再需要后，可移除明文文件和相应启动参数。之后通过 Web UI 更改密码；自动重启和更新直接使用保存的选择，无需密码参数或临时密码文件。

普通本机启动或跳过向导密码仍允许以后首次导入。明确清除密码（包括离线恢复）会禁用导入，需在本机 Web UI 重新设置密码。

## 忘记密码

在服务器上停止 bilistream，以及会自动重启它的服务。使用与正常服务相同的系统账号、程序及数据/密钥环境变量，运行：

```bash
./bilistream --reset-panel-password
```

Windows 在终端运行 `bilistream.exe --reset-panel-password`。该命令清除已保存密码并注销会话后退出；安装仍在运行或加密存储无法解锁时拒绝操作。请保留数据库和密钥文件。

用 `./bilistream --bind 127.0.0.1` 在本机重启，直接在服务器打开面板，在「系统设置 → 安全」设置新密码（尚未完成配置时使用向导），之后再恢复远程监听。旧的初始密码文件不能撤销清除操作。恢复密码需要本机命令，登录页不提供免密码重置。

## 启动选项与多服务器访问

监听地址和端口也可通过 `BILISTREAM_BIND`、`BILISTREAM_PORT` 设置。其他启动参数见[命令行选项](advanced-settings.zh_CN.md#命令行选项)。

添加服务器时只验证一次该服务器的面板密码。每台服务器的管理地址必须是证书有效的 HTTPS，或自行建立的认证隧道上的本机回环地址。见[多服务器](advanced-settings.zh_CN.md#准备工作)。
