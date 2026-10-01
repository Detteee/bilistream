# Remote access and password

[Back to the quick start](../README.md) · [中文](remote-access.zh_CN.md)

The control panel listens on `127.0.0.1:3150` by default. For use on the same computer, you can leave it without a password and open `http://localhost:3150`.

## Set a password in the Web UI

On the wizard’s last step, enable **设置面板密码** and enter a password, or skip it for local use. If a password already exists, the wizard preserves it. After setup, use **System Settings → 安全** to set, change or clear the panel password. Changing or clearing it requires the current password and immediately signs out every session. After setting or changing it in Settings, sign in again; the wizard keeps you signed in.

The first password can only be set through a direct local connection to a loopback listener. Set it before exposing the panel through a proxy or tunnel, even if the program itself still listens on localhost. Passwords cannot be cleared while listening on a non-loopback address.

To allow LAN access, set the password locally, stop the program, then restart with:

```bash
./bilistream --bind 0.0.0.0
```

On another computer, use the server’s address rather than `localhost`. Use HTTPS through a reverse proxy for remote access, with matching browser Origin and Host. Five failed password attempts from one connection IP temporarily block further attempts for up to one minute.

## Headless and service bootstrap

For a new server with no saved password choice, `--password-file`, `--password` or `BILISTREAM_PASSWORD` can supply the initial password. For example, on Linux/bash:

```bash
umask 077
mkdir -p ~/.config/bilistream
read -rsp 'Web UI password: ' bilistream_password
printf '\n'
printf '%s' "$bilistream_password" > ~/.config/bilistream/webui-password
unset bilistream_password
chmod 600 ~/.config/bilistream/webui-password
./bilistream --bind 0.0.0.0 --password-file ~/.config/bilistream/webui-password
```

The file is plaintext protected by account permissions. Its password is imported once into encrypted installation-local storage. Later starts use the saved choice; changing the file or environment variable cannot override a saved password or restore an explicitly cleared one. Remove the plaintext bootstrap file and its launch argument after a successful import if no other installation needs it. Use the Web UI for subsequent changes. Automatic restarts and updates retain the saved choice without a password argument or temporary password file.

An ordinary local launch or skipping the wizard password leaves bootstrap available. Explicitly clearing the password, including offline recovery, disables bootstrap until a password is set locally in the Web UI.

## Forgotten password

On the server, stop bilistream and any service that restarts it. Using the same OS account, executable and data/key environment as the normal service, run:

```bash
./bilistream --reset-panel-password
```

On Windows, use `bilistream.exe --reset-panel-password` in a terminal. This command clears the saved password and revokes sessions, then exits. It refuses to run while that installation is active or its encrypted storage cannot be unlocked. Keep the database and key files.

Restart locally with `./bilistream --bind 127.0.0.1`, open the panel directly on the server and set a new password in **System Settings → 安全** (or the setup wizard for an unfinished installation). Only then restore remote listening. Existing bootstrap files cannot undo the reset. Recovery is a local command; the login page has no passwordless reset.

## Launch options and multi-server access

Bind and port can also come from `BILISTREAM_BIND` and `BILISTREAM_PORT`. See [command-line options](advanced-settings.md#command-line-options) for other launch settings.

Multi-server mode has no shared node key. Adding a server checks that server’s panel password once; afterwards servers authenticate each other with their own signing identities. Each server’s admin address must use HTTPS with a valid certificate, or a loopback address for an authenticated tunnel you run yourself. See [multi-server mode](advanced-settings.md#before-you-start).
