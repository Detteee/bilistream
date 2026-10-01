# Install dependencies

[Back to the quick start](../README.md) · [中文](dependencies.zh_CN.md)

Bilistream uses ffmpeg to rebroadcast video. YouTube uses yt-dlp and Deno for playback and JavaScript challenges. Twitch and Niconico use streamlink; Twitch also needs the streamlink-ttvlol plugin. Install dependencies on the computer running Bilistream.

## Windows

Run `bilistream.exe` from an extracted folder you can write to. Bilistream downloads missing `ffmpeg.exe` and `yt-dlp.exe` into that folder and attempts to install Deno. If Deno installation fails, follow the [official Deno installation instructions](https://docs.deno.com/runtime/getting_started/installation/). Restart Bilistream after installing Deno so it can find it.

For Twitch or Niconico, install streamlink separately using the [Windows installer](https://github.com/streamlink/windows-builds/releases). Ensure `streamlink` is available on `PATH`; open a new terminal and run `streamlink --version` to check. For Twitch, also follow the [streamlink-ttvlol installation instructions](https://github.com/2bc4/streamlink-ttvlol#installation). Restart Bilistream after installation.

## Linux

Install ffmpeg with your distribution's package manager. For Debian/Ubuntu, the following also installs the tools used below:

```bash
sudo apt update
sudo apt install ffmpeg pipx curl unzip
pipx ensurepath
pipx install 'yt-dlp[default]'
```

Install Deno using its [official installer](https://docs.deno.com/runtime/getting_started/installation/):

```bash
curl -fsSL https://deno.land/install.sh | sh
```

For Twitch or Niconico, also run `pipx install streamlink`. For Twitch, follow the [streamlink-ttvlol installation instructions](https://github.com/2bc4/streamlink-ttvlol#installation).

Open a new terminal after installation so `PATH` changes take effect. If Deno is still not found, add its directory to your shell startup file: `export PATH="$HOME/.deno/bin:$PATH"`. If you run Bilistream as a service, make these executables available to the service account too.

On other distributions, use their ffmpeg package and the upstream instructions for [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation) and [streamlink](https://streamlink.github.io/install.html). The repository's Debian `install.sh` also installs Deno, along with the other dependencies.

If the downloaded Bilistream binary is not executable, run `chmod +x bilistream` before `./bilistream`.

## macOS

With [Homebrew](https://brew.sh/) installed:

```bash
brew install ffmpeg yt-dlp deno
```

For Twitch or Niconico, also run `brew install streamlink`. For Twitch, follow the [streamlink-ttvlol installation instructions](https://github.com/2bc4/streamlink-ttvlol#installation). Open a new terminal after installation. If needed, run `chmod +x bilistream` before `./bilistream`.

## Check installation

On Linux/macOS, check the commands from the same account that runs Bilistream:

```bash
ffmpeg -version
yt-dlp --version
deno --version
```

For Twitch/Niconico, also check `streamlink --version`. On Windows, ffmpeg and yt-dlp live next to Bilistream; Deno and streamlink need to be discoverable on `PATH`.

Some sources need cookies. Configure YouTube cookies and the Niconico `user_session` value in the Web UI; Bilibili login is handled by the wizard. Continue with [first-run setup](first-run.md).
