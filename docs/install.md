# Installing AmberSight

AmberSight is a camera recorder (NVR) that runs on your own computer and is used from a web
browser. This guide covers Windows, macOS and Linux: getting it running, keeping it running as
a background service, and the first steps in the browser.

The quickest way is the installer for your system (see
[Easiest: the installer](#easiest-the-installer)). The step-by-step manual install is further
down, under each system's name.

---

## Before you start

**You need**

- A computer that stays on: a desktop, a mini PC or a small server. Recording uses little
  processing power, but AI object detection is faster with a graphics card: on Windows any
  DirectX 12 card is used automatically, on Apple silicon Macs the chip's Neural Engine and
  GPU, and on 64-bit Linux an NVIDIA card once NVIDIA's CUDA libraries are installed (see
  [Linux: an NVIDIA card for the AI](#linux-an-nvidia-card-for-the-ai)).
- Enough disk space. A typical camera records 10–30 GB a day. A separate large drive for
  recordings is best.
- Cameras that send video over the network: RTSP (most IP cameras, ONVIF cameras, Reolink,
  Amcrest, Dahua, Hikvision, many doorbells) or HTTP. USB webcams and capture cards work too.
- The cameras' user name and password.

**Supported systems**

| System | Package | Notes |
|---|---|---|
| Windows 10 / 11, 64-bit | `ambersight-<version>-windows-x86_64-setup.exe` (installer) or `ambersight-<version>-windows-x86_64.zip` | Built-in AI runs on the graphics card (DirectML) |
| macOS on Apple silicon (M1 and later) | `ambersight-<version>-darwin-arm64.tar.gz` | Built-in AI runs on the Neural Engine and GPU (Core ML) |
| macOS on Intel | `ambersight-<version>-darwin-x86_64.tar.gz` | Everything works except built-in AI detection. To check events for people and vehicles, connect your own detector program (Settings > Connections > External detector) |
| Linux, 64-bit (Debian, Ubuntu, Fedora, Arch, ...) | `ambersight_<version>_amd64.deb`, `ambersight-<version>-1.x86_64.rpm` or `ambersight-<version>-linux-x86_64.tar.gz` | The packages need Debian 13, Ubuntu 25.04, Fedora 42 or newer. Built-in AI runs on the processor, or on an NVIDIA card (CUDA) when NVIDIA's libraries are installed |
| Raspberry Pi 4 / 5 (and other 64-bit ARM Linux) | `ambersight_<version>_arm64.deb`, `ambersight-<version>-1.aarch64.rpm` or `ambersight-<version>-linux-aarch64.tar.gz` | Needs the **64-bit** Raspberry Pi OS, trixie (2025) or newer. Built-in AI runs on the processor. See [Raspberry Pi](#raspberry-pi). |
| Docker (and Unraid) | `ghcr.io/huskerminion/ambersight:latest` (`amd64` and `arm64`), `:cuda` for AI on an NVIDIA card | See the [Docker guide](docker.md); on Unraid, the AmberSight template ([Unraid guide](../unraid/README.md)) |

**Built-in AI costs nothing and needs nothing downloaded in advance.** The packages above are
all you install. When you turn AI on (Settings > Detection & alerts > Objects (AI)), AmberSight
downloads the model you pick, once: Tiny about 20 MB, Small about 35 MB, Large about 217 MB. On
Linux with an NVIDIA card it also downloads its NVIDIA add-on (about 70 MB) by itself. All of
these are free, are checked before use, and are kept for next time.

The `.zip` and `.tar.gz` packages hold the AmberSight program, an example settings file
(`config.example.toml`), the README, and a short install note (`install.cmd` on Windows,
`INSTALL.txt` elsewhere). The installers (`setup.exe`, `.deb`, `.rpm`) do the whole install.

**ffmpeg** is a free video tool AmberSight uses for some cameras, clips and pictures.

- **Windows and macOS:** nothing to do. If it is missing, AmberSight downloads it by itself the
  first time it starts. This is a one-time download of about 105 MB on Windows and about 30 MB
  on a Mac, and it is checked against a fixed checksum.
- **Linux:** install it with your package manager (see the Linux section). The `.deb` package
  installs it for you; for the `.rpm` on Fedora, add RPM Fusion first.

---

## Easiest: the installer

Each of these leaves AmberSight installed as a background service that starts with the
computer, with an **AmberSight** shortcut (Start menu, Applications or the app menu) that opens
it in the browser. Your settings and recordings are kept when you update or remove it.

### Windows: setup.exe

1. Download `ambersight-<version>-windows-x86_64-setup.exe` and double-click it.
2. Windows asks whether to allow it to make changes: choose **Yes** (it installs a service).
   The program is not signed, so SmartScreen may first say "Windows protected your PC": choose
   **More info**, then **Run anyway**.
3. Follow the pages: license, install folder (`C:\Program Files\AmberSight NVR` is fine; it will not install into a folder that already holds another program), Install.
4. On the last page leave **Open AmberSight** ticked and choose **Finish**.

What it sets up:

| What | Where |
|---|---|
| Program | `C:\Program Files\AmberSight NVR` |
| Settings | `C:\ProgramData\AmberSight\config.toml` (only Administrators can read it) |
| Recordings and database | `C:\ProgramData\AmberSight\data` (limited to Administrators and the service, like the program folder) |
| Log file | `C:\ProgramData\AmberSight\ambersight.log` |
| Service | **AmberSight** ("AmberSight NVR"), starts at boot, restarts if it stops |
| Firewall | rule "AmberSight NVR" allowing TCP port 8090 in |
| Shortcuts | "AmberSight" on the desktop and in the Start menu |

To update, run the newer `setup.exe`; it keeps your settings. To remove AmberSight, use
**Settings > Apps > Installed apps > AmberSight > Uninstall**. The settings and recordings in
`C:\ProgramData\AmberSight` stay; delete that folder too if you no longer need them.

### Linux: .deb or .rpm package

For Debian, Ubuntu, Linux Mint and Raspberry Pi OS (`.deb`), and Fedora (`.rpm`). Use `amd64` /
`x86_64` on a PC and `arm64` / `aarch64` on a Raspberry Pi with the 64-bit OS (see
[Raspberry Pi](#raspberry-pi)).

**Recommended: add the AmberSight repository once.** Then `sudo apt upgrade` or
`sudo dnf upgrade` (or unattended upgrades) bring new releases. The repository is signed; setup
installs the signing key and one repository file, nothing else:

```
curl -fsSL https://huskerminion.github.io/ambersight-packages/setup.sh | sudo sh
```

Then, on Debian, Ubuntu, Mint, Raspberry Pi OS:

```
sudo apt update && sudo apt install ambersight
```

On Fedora, first add RPM Fusion (https://rpmfusion.org/Configuration) for the full ffmpeg, then:

```
sudo dnf install ambersight --allowerasing
```

By hand instead of the script (Debian and relatives): put the key at
`/etc/apt/keyrings/ambersight.gpg` and write `/etc/apt/sources.list.d/ambersight.sources`:

```
sudo mkdir -p /etc/apt/keyrings
sudo curl -fsSL https://huskerminion.github.io/ambersight-packages/ambersight-archive-keyring.gpg \
  -o /etc/apt/keyrings/ambersight.gpg
sudo tee /etc/apt/sources.list.d/ambersight.sources >/dev/null <<'EOF'
Types: deb
URIs: https://huskerminion.github.io/ambersight-packages
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/ambersight.gpg
EOF
```

On Fedora, write `/etc/yum.repos.d/ambersight.repo`:

```
[ambersight]
name=AmberSight
baseurl=https://huskerminion.github.io/ambersight-packages/rpm/$basearch
enabled=1
gpgcheck=0
repo_gpgcheck=1
gpgkey=https://huskerminion.github.io/ambersight-packages/ambersight-archive-keyring.asc
```

The repository's metadata is signed and lists the checksum of each package. The `.rpm` files
themselves are not signed, so `gpgcheck` is off. The apt side keeps the two newest versions
(`sudo apt install ambersight=<version>` goes back one release); the dnf side has the newest
only. The signing key's fingerprint is `5433 0C65 BECD 2BBF 8F7B 12FD DF5E 8B45 1B6B EC76`.

**Without the repository:** download the package from the
[releases page](https://github.com/HuskerMinion/AmberSight/releases) and install the file:
`sudo apt install ./ambersight_<version>_amd64.deb` (on a Raspberry Pi, `_arm64.deb`), or on Fedora
`sudo dnf install ./ambersight-<version>-1.x86_64.rpm --allowerasing`. This also installs ffmpeg.
Later releases are installed the same way.

What it sets up:

| What | Where |
|---|---|
| Program | `/usr/bin/ambersight` |
| Settings | `/etc/ambersight/config.toml` |
| Recordings and database | `/var/lib/ambersight` |
| Service | `ambersight` (systemd), running as the system user `ambersight`, starts at boot |
| Logs | `journalctl -u ambersight -f` |
| Shortcut | "AmberSight" in the app menu |

Then open **http://localhost:8090**. If a firewall is on, open port 8090 (see
[Firewall](#5-firewall)). Day to day: `sudo systemctl stop ambersight` / `start` / `status`.

To update, run `sudo apt update && sudo apt upgrade` (or `sudo dnf upgrade`) when you use the repository, or install the newer package file. To remove it: `sudo apt remove ambersight`
or `sudo dnf remove ambersight`. The recordings in `/var/lib/ambersight` are kept (and the
settings too, unless you use `apt purge`).

USB cameras: the `ambersight` user is added to the `video` group. If you choose a recordings
drive in Settings > Recording > Drives & folders, that user must be allowed to write there:
`sudo chown ambersight: /mnt/recordings`.

### Who can open AmberSight's folders

AmberSight keeps ordinary accounts on the same computer out of its own folders, so a housemate's
account or a program running as a normal user cannot swap the program, edit the two-step and
token data, or read the stored passwords.

- **Windows:** the program folder and the data folder (it holds the database) are limited to
  SYSTEM, Administrators and any other account that was already named there by name (a custom
  administrator account keeps its access). It is done by the installer, by `ambersight setup` and
  `ambersight service install`, and again when the service starts, which also covers installs
  from before this existed. Nothing is changed when a folder is already limited, and only the top
  folder is changed (the files inside inherit it), so a data folder with many recordings can take
  several minutes the first time, in the background; recording is not held up. A drive root, a
  user's own folder (Downloads, Desktop...), Program Files itself, and a program folder that also
  holds other programs are left alone: the log says so as a warning, and Settings > Recording >
  Drives & folders shows the real state of the program and data folders and why one was skipped
  or refused (no notification is sent). A failure is logged as a warning and AmberSight carries
  on; a folder that stays open after an attempt is tried again once a day at most. Ordinary
  accounts keep read-and-run access to `ambersight.exe`, its libraries, `ffmpeg.exe` and the icon
  only (the capture helper runs from there in a person's own session). The ffmpeg the service
  downloaded lives in the private data folder, so at each start the service gives ordinary
  accounts read-and-run on those two files alone (`ffmpeg.exe`, `ffprobe.exe`; the folders stay
  closed) and writes `ffmpeg-path.txt` (read only) beside the program with the path of ffmpeg.exe,
  which the capture helper tries after "next to the program" and before the PATH. The server never
  reads that note, and it is removed when AmberSight is uninstalled. The folder's owner is set
  to Administrators, and stays so if a recording folder is later switched off. A folder made by hand, or a
  drive plugged in later, is closed at the next service start (switched-on recording folders are
  checked then too). Not done: a file or folder inside that has its own deny entry may keep what
  it has (the command still reports success), so if a folder looks open in Settings, see the
  log. In File Explorer an administrator without elevation is asked to Continue before
  opening such a folder; Continue adds that account to the folder's permissions. As a result, program commands such as `ambersight user reset-two-step`
  must be run from an **Administrator** terminal; a normal terminal gets "access denied".
- **Linux and macOS:** the service already runs as a normal user. At start AmberSight makes
  `config.toml`, the database and its `-wal` and `-shm` files readable by its own account only
  (mode 600) and shuts the data folder to other accounts (750). It does not change owners.
  `ambersight setup --system` and the `.deb` and `.rpm` packages give the service account its
  settings and data folders (750) and the settings file 640; the program narrows the file to 600
  when it starts. A recordings
  drive mounted elsewhere keeps its own modes unless you switch its folder to private (below).
- **Recording folders** you add under Settings > Recording > Drives & folders are not touched
  unless you switch on **Keep this folder private** for that folder (on by default for a new or
  empty folder, off for one that already holds files). Only the folder itself is changed, never
  the files in it one by one. Windows: the folder is limited like the data folder; Unix: others
  are shut out (750). Browsing such a folder in File Explorer or over a network share, or reading
  it from another program, stops working for ordinary accounts. Turning the switch off puts it
  back to inheriting from its parent (Windows) or to 755 (Unix). Network drives and FAT or exFAT
  drives cannot be made private. To limit a folder by hand instead, on Windows run from an
  Administrator prompt: `icacls "D:\Cameras" /inheritance:d`, then
  `icacls "D:\Cameras" /remove:g *S-1-5-11 *S-1-5-32-545 *S-1-1-0` (two commands, in this order,
  on the top folder only: never add `/T`); on Linux: `chmod 750 /mnt/recordings`.

### Any system, including macOS: `ambersight setup`

The program can install itself. Unpack the `.zip` or `.tar.gz` package anywhere (for example
your Downloads folder), open a terminal there and run:

| System | Command |
|---|---|
| Windows | `ambersight.exe setup` in an **Administrator** terminal |
| macOS | `./ambersight setup` |
| Linux | `./ambersight setup` (no `sudo`) |

It first prints what it will do and where everything goes, then:

1. copies the program to its place,
2. creates the settings file from the starter settings (an existing one is never overwritten),
3. installs and starts the background service,
4. opens the firewall port (Windows; on Linux it prints the command if ufw or firewalld is on;
   on macOS, choose **Allow** when asked; macOS 15 and later also ask whether AmberSight may
   find devices on local networks: choose **Allow** there too, or cameras cannot connect),
5. adds an **AmberSight** shortcut with the AmberSight icon,
6. opens AmberSight in the browser.

| System | Program | Settings and recordings | Service |
|---|---|---|---|
| Windows | `C:\Program Files\AmberSight NVR` | `C:\ProgramData\AmberSight` | Windows service, starts at boot |
| macOS | `~/Applications/AmberSight` | `~/Library/Application Support/AmberSight` | LaunchAgent, starts when you sign in |
| Linux | `~/.local/share/ambersight` | `~/.config/ambersight` and `~/.local/share/ambersight/data` | systemd user service, starts at boot |

Options:

| Option | What it does |
|---|---|
| `--dry-run` | Only print what it would do |
| `--system` | macOS and Linux: install for the whole computer, starting at boot without anyone signed in. Run it with `sudo`. macOS: `/Applications/AmberSight` and `/Library/Application Support/AmberSight`, a LaunchDaemon. Linux: `/opt/ambersight`, `/etc/ambersight/config.toml`, `/var/lib/ambersight`, a system service. The service runs as the user who ran `sudo`. |
| `--here` | Keep the program and `config.toml` in the current folder; only add the service, firewall rule and shortcut |
| `--no-browser` | Do not open the browser at the end |
| `--uninstall` | Remove the service, firewall rule, shortcuts and the program copy. Settings and recordings are kept, and it prints where they are. Use the same `--system` or `--here` as when installing. |

To update, run `setup` again from the newer package: it replaces the program and the service and
keeps the settings.

On Linux, install ffmpeg first (see [Linux](#linux), step 1); setup reminds you if it is
missing.

---

## Manual install

The sections below install AmberSight step by step from the `.zip` or `.tar.gz` package, for
when you want to choose every folder yourself.

---

## Windows

### 1. Unpack

1. Unzip `ambersight-<version>-windows-x86_64.zip` to a permanent place, for example
   `C:\AmberSight`.
2. Keep every file together. The `.dll` files next to `ambersight.exe` are needed for AI.

### 2. Create the settings file

1. In `C:\AmberSight`, double-click **`install.cmd`**.
2. The first time, it creates `config.toml` from the example and opens it in Notepad.
3. You can add cameras in the browser later (step 5), so for now you can simply close Notepad.
   The defaults work.

### 3. Install the service

1. Right-click `install.cmd` and choose **Run as administrator**.
2. AmberSight is installed as the Windows service **"AmberSight NVR"**. It starts at boot and
   restarts if it ever stops.

To do the same from an **Administrator** PowerShell or Command Prompt:

```
cd C:\AmberSight
ambersight.exe service install --config C:\AmberSight\config.toml
ambersight.exe service status
```

### 4. Allow it through the firewall

AmberSight listens on port **8090**. Other computers and phones on your network can only reach it
if Windows Firewall allows that port. Run this once from an **Administrator** prompt:

```
netsh advfirewall firewall add rule name="AmberSight NVR" dir=in action=allow protocol=TCP localport=8090
```

### 5. Open it

- On the same computer, open **http://localhost:8090**.
- From another device, open **http://<computer name or IP>:8090**.

Then continue with [First steps in the browser](#first-steps-in-the-browser).

### Day-to-day on Windows

| Task | How (Administrator prompt in `C:\AmberSight`) |
|---|---|
| Stop / start | `ambersight.exe service stop` / `ambersight.exe service start` |
| Status | `ambersight.exe service status` |
| Log file | `C:\AmberSight\ambersight.log` (also Settings > System > Logs in the browser) |
| Remove the service | `ambersight.exe service uninstall` |

---

## macOS

### 1. Unpack

1. Open Terminal and unpack the package into a folder of your choice, for example
   `~/AmberSight`:

   ```
   mkdir -p ~/AmberSight
   tar -xzf ~/Downloads/ambersight-<version>-darwin-arm64.tar.gz -C ~/AmberSight --strip-components=1
   cd ~/AmberSight
   ```

2. The AmberSight program is not signed by Apple. If the package came through a web browser,
   macOS may block it ("cannot be opened because the developer cannot be verified"). To allow
   it, run this once:

   ```
   xattr -d com.apple.quarantine ambersight
   ```

   Alternatively, try to run it once, then choose **System Settings > Privacy & Security > Open
   Anyway**.

### 2. Create the settings file and try it

```
cp config.example.toml config.toml
./ambersight run --config config.toml
```

1. Open **http://localhost:8090** in a browser.
2. If macOS asks whether `ambersight` may accept incoming network connections, choose
   **Allow**; otherwise phones and other computers cannot reach it.
3. Stop it with **Ctrl+C** when you are ready to install the service.

### 3. Install the service

```
./ambersight service install --config ~/AmberSight/config.toml
./ambersight service status
```

This installs a **LaunchAgent**. It starts when you log in and restarts if it stops. To start it
at boot without anyone logging in, install a LaunchDaemon instead:

```
sudo ./ambersight service install --system --config ~/AmberSight/config.toml
```

Keep the Mac from sleeping (System Settings > Energy, or Battery > Options on a laptop):
recording stops while it sleeps.

### 4. Allow access to the local network (macOS 15 and later)

macOS 15 (Sequoia) and later ask before a program connects to devices on your local network,
which includes your cameras. The first time AmberSight connects to a camera, macOS asks:
**Allow "ambersight" to find devices on local networks?** Choose **Allow**. Cameras then connect
within a few seconds; nothing needs a restart. You can change the answer later in **System
Settings > Privacy & Security > Local Network** (the entry is called `ambersight`).

- If you never saw the question, or chose **Don't Allow**, every camera fails with
  "No route to host" and the camera shows "macOS blocked AmberSight from the local network".
  Turn `ambersight` on in **Privacy & Security > Local Network**.
- macOS only asks for a code-signed program. The release packages are signed (ad hoc; that
  needs no Apple account), and `ambersight setup` and `ambersight service install` sign the
  program if it is not, for example one you built yourself on an Intel Mac. If `ambersight` is
  missing from the Local Network list, run `ambersight setup` (or `ambersight service install`)
  again, or sign it by hand and restart the service:

  ```
  codesign --force --sign - --identifier com.ambersight.ambersight ~/AmberSight/ambersight
  ./ambersight service stop
  ./ambersight service start
  ```

- A program you build from source on an Apple-silicon Mac is already signed by the linker, but
  with an identifier that changes with every build, so macOS asks again after each one. Build
  with `scripts/package.sh` (it signs with the fixed identifier), or sign it by hand as above.

- The `--system` LaunchDaemon does not get around this: it runs as your user, and macOS asks
  the same way. Commands run from Terminal or over ssh are allowed, so a camera that works with
  `./ambersight run` but not as a service points to this setting.
- The answer is kept across updates.

### Day-to-day on macOS

| Task | How (Terminal in `~/AmberSight`) |
|---|---|
| Stop / start | `./ambersight service stop` / `./ambersight service start` |
| Status | `./ambersight service status` |
| Logs | `ambersight.out.log`, `ambersight.err.log` in the folder (also Settings > System > Logs) |
| Cameras fail with "No route to host" | Allow `ambersight` in System Settings > Privacy & Security > Local Network (see step 4) |
| Remove the service | `./ambersight service uninstall` |

---

## Linux

### 1. Install ffmpeg

| Distribution | Command |
|---|---|
| Debian, Ubuntu, Mint, Raspberry Pi OS | `sudo apt install ffmpeg` |
| Arch, Manjaro | `sudo pacman -S ffmpeg` |
| Fedora | First add RPM Fusion (https://rpmfusion.org/Configuration), then `sudo dnf install ffmpeg --allowerasing`. Fedora's own `ffmpeg-free` cannot play H.265 cameras. |

### 2. Unpack

```
mkdir -p ~/ambersight
tar -xzf ambersight-<version>-linux-x86_64.tar.gz -C ~/ambersight --strip-components=1
cd ~/ambersight
cp config.example.toml config.toml
```

### 3. Try it

```
./ambersight run --config config.toml
```

Open **http://localhost:8090**, then stop it with **Ctrl+C**.

### 4. Install the service

```
./ambersight service install --config ~/ambersight/config.toml
loginctl enable-linger $USER      # start at boot, even without logging in
./ambersight service status
```

- This writes a systemd **user** service. No root is needed.
- Use `--system` for a system-wide service instead; it tells you the `sudo` command to run.
- The service uses the `PATH` you had when you installed it, so `ffmpeg` must be findable then.

### 5. Firewall

If a firewall is on, open port 8090:

| Firewall | Command |
|---|---|
| ufw (Ubuntu) | `sudo ufw allow 8090/tcp` |
| firewalld (Fedora) | `sudo firewall-cmd --add-port=8090/tcp --permanent && sudo firewall-cmd --reload` |

### Day-to-day on Linux

| Task | How |
|---|---|
| Stop / start | `./ambersight service stop` / `./ambersight service start` |
| Status | `./ambersight service status` |
| Logs | `journalctl --user -u ambersight -f` (also Settings > System > Logs) |
| Remove the service | `./ambersight service uninstall` |

### Hardware decoding for motion detection

Motion detection decodes each camera's picture (the sub stream when the camera has one). On a
graphics card that takes most of that work off the processor. **Settings > System > Hardware**
has one setting for the whole server, **Hardware decoding**, which saves when you change it and
applies within seconds, without a restart:

| Choice | What it does |
|---|---|
| Auto (recommended) | An NVIDIA card first (NVDEC), then Intel Quick Sync, then VAAPI on Intel or AMD; the processor when none works |
| Prefer NVIDIA | NVIDIA first (today the same order as Auto) |
| Prefer Intel / AMD | The built-in or Intel/AMD graphics first, the NVIDIA card last (for example when the NVIDIA card is busy with the AI) |
| Off | The processor |

At start AmberSight decodes a short H.264 and H.265 sample with each way it finds and compares
the pictures with the processor's; a way that fails is never used by Auto, and the page's
**Start-up test** list says why. On Intel chips of Gen12 and later (Tiger Lake, Rocket Lake,
Alder Lake, Arc and newer) Quick Sync is tried before VAAPI: VAAPI decoding on those chips can
fail with *Failed to sync surface*; on older Intel chips VAAPI comes first. **Per camera** on the
same page shows what each camera uses now and why, for example *NVIDIA (NVDEC)*, *Intel Quick
Sync on /dev/dri/renderD128* or *processor: VAAPI on /dev/dri/renderD128 failed 3 times (...);
trying it again at 10:42*.

A decoder that fails while running (it stops, stalls or reports a burst of errors) is replaced at
once by the next way that works, or the processor. That camera tries the failed way again after
15 minutes, and later each time it fails again (30 minutes, an hour, up to 4 hours). If it fails
a second time in a row, that camera leaves it until AmberSight restarts or a Hardware setting
changes (the preference or **Keep free on the NVIDIA card**; its note says so), since a way that
fails twice on one camera's stream keeps failing on it. "In a row" means without half an hour
of good running in between: a decoder that runs well for 30 minutes clears the camera's earlier
failures of that way, and so does a failure more than 6 hours after the one before. A failure
because the card's memory was full (an error that says out of memory, such as
`CUDA_ERROR_OUT_OF_MEMORY` or *Cannot allocate memory*) never makes a camera leave a way: it is
only waited out, since the card may have room later. Other allocation errors (Quick Sync's
*memory allocation error*, VAAPI's *Failed to allocate ... surface*) count as usual. A decoder
that could not start because the system would not start another process or thread (see
[Cameras fail with "Resource temporarily unavailable"](#cameras-fail-with-resource-temporarily-unavailable-process-limit))
is not counted against the card at all. A way that stopped while the
processor then decoded the same stream fine is left alone by every camera for an hour; the other
ways (another card, or Quick Sync instead of VAAPI) stay in use.

A camera's own **Hardware decoding** (camera settings, More settings) overrides the server
setting: Off, NVIDIA (NVDEC), VAAPI or Quick Sync, and which card when there are several.

What each way needs:

- **NVIDIA (NVDEC):** NVIDIA's driver (`nvidia-smi` lists the card) and an ffmpeg built with
  CUDA/NVDEC (`ffmpeg -hwaccels` lists `cuda`; the Debian, Ubuntu and Fedora RPM Fusion builds do).
  No CUDA toolkit is needed for decoding. Works on Linux and Windows.
- **Intel Quick Sync and VAAPI:** Linux, the account AmberSight runs as in the `render` group, and
  the media driver (`intel-media-va-driver` / `intel-media-driver` for Intel; Mesa's VAAPI driver
  for AMD).
- **Intel Quick Sync** also needs Intel's GPU runtime, which is a separate package and is not
  installed with the media driver. Install it on Gen12 and later Intel (Tiger Lake, Rocket Lake,
  Alder Lake, Arc and newer), then restart AmberSight:

  ```sh
  sudo apt install libmfx-gen1.2        # Debian, Ubuntu
  sudo dnf install intel-vpl-gpu-rt     # Fedora
  ```

  Without it Quick Sync fails the start-up test (the Hardware page says *Intel's Quick Sync
  runtime is missing*) and those chips fall back to VAAPI, which on some cameras fails within
  minutes with *Failed to sync surface* and moves them to another way or the processor. Seen on a
  Rocket Lake (UHD 750) with Amcrest IP8M-2496E-V2 cameras: VAAPI failed on them every time,
  Quick Sync decoded them without errors. The Docker image includes it.
- Windows: Auto uses an NVIDIA card when it passed the test, and otherwise the processor. macOS:
  the processor.

Auto also looks after the NVIDIA card's video memory. Each NVDEC decoder takes about 150 to
300 MB (16 cameras took 2.9 GB), which can starve the AI or another program on the same card
(a local language model, for example). This applies under **Prefer Intel / AMD** too, where
cameras whose Intel or AMD way fails would otherwise all land on the NVIDIA card. A camera starts or moves onto NVDEC only while the card
keeps **Keep free on the NVIDIA card** (default 1536 MB) free after it, plus about 300 MB for
the new decoder. When there is no room the camera uses the next way (Intel, then the
processor) and its note says why ("NVIDIA card memory is nearly full (14.9 of 16.3 GB used);
using ..."). About once a minute Auto looks again, and moves the camera back once there is room
for the decoder plus 1 GB more than that. Cameras already on the card stay, also when a Hardware
setting changes and through short reconnects or a stream change (new resolution or codec); a
camera gives up its place when it is turned off, a battery camera falls asleep, its decoder
moves to another way, or it has been offline for more than about 2 minutes (its memory is free
then, so other cameras may use it; when it comes back it is judged like a new camera). A camera read with ffmpeg (`ingest = "ffmpeg"`, or one the built-in reader
could not play) decodes in the same ffmpeg that records it, so moving it to another way (any
move Auto makes, including this move back) restarts its recording briefly; with the built-in
reader only its motion decoder moves. The free memory is
read with `nvidia-smi` (installed with NVIDIA's driver; if it is not found there is no check,
and the Hardware page says so). **Prefer NVIDIA** and a camera's own NVIDIA setting ignore the
check; 0 turns it off. The card checked is the first one `nvidia-smi` lists, the one NVDEC
uses.

In the config file: `[hwdecode] prefer = "auto"` (or `"nvidia"`, `"intel"`, `"off"`) and
`nvidia_min_free_mb = 1536`.

### Linux: an NVIDIA card for the AI

On a 64-bit Intel/AMD Linux computer with an NVIDIA graphics card, the built-in AI runs on the
card (CUDA): checks take a few milliseconds instead of tens to hundreds on the processor, and
the Large model keeps up with many cameras. Without it everything still works on the processor.

If the card gives errors while the AI runs (typically it is out of video memory because another
program took it), the checks that failed are not lost: the saved pictures of an event whose checks
all failed are looked at again after 1, 5 and 15 minutes, and what they show is added to the event
and its AI review (no alert is sent for it then). After three errors in a row within a minute the
AI runs on the processor for 5 minutes (Settings > System > Hardware says so), then tries the card
again.

It needs three things:

1. **NVIDIA's driver** (`nvidia-smi` lists the card). Most distributions install it for you
   (Ubuntu: *Additional Drivers*, or `sudo ubuntu-drivers install`).
2. **NVIDIA's CUDA 13 and cuDNN 9 libraries**: the CUDA runtime (cudart), cuBLAS, cuRAND and
   cuDNN 9 for CUDA 13. These are not part of Ubuntu or Debian: they come from NVIDIA's own
   package repository (about 1.3 GB installed). On Ubuntu 26.04:

   ```sh
   wget https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2604/x86_64/cuda-keyring_1.1-1_all.deb
   sudo dpkg -i cuda-keyring_1.1-1_all.deb
   sudo apt update
   sudo apt install cuda-cudart-13-5 libcublas-13-5 libcurand-13-5 libcudnn9-cuda-13
   ldconfig -p | grep -E 'libcudart.so.13|libcublasLt.so.13|libcurand.so.10|libcudnn.so.9'
   ```

   The last line should list all four. Other systems: use the matching folder instead of
   `ubuntu2604` (`ubuntu2404`, `debian13`, `fedora42`, ... see
   [NVIDIA's CUDA downloads](https://developer.nvidia.com/cuda-downloads)); any CUDA 13.x works.
   This adds NVIDIA's repository and these libraries only; the driver you have stays.
3. **AmberSight's NVIDIA add-on**: ONNX Runtime's CUDA part (about 70 MB to download). The
   packages leave it out, since most computers have no NVIDIA card. When AmberSight finds a
   card and NVIDIA's libraries (step 2) it downloads the add-on of its own version from the
   release by itself, checks it against the release's signed checksums like an update, and
   keeps it in `<data folder>/tools/cuda-<version>/` (with the packages:
   `/var/lib/ambersight/tools/`). A failed download is tried again after 5 and 15 minutes, then
   hourly, and given up after about a day (until AmberSight restarts or the AI settings change).
   After an update it downloads the new version's add-on. With `[update] check = false`
   nothing is downloaded: then, or without internet, put the two `.so` files from that
   version's `ambersight-cuda-<version>-linux-x86_64.tar.gz` in that folder yourself. The
   folder and both files must be real files (not links), belong to the user AmberSight runs as
   (or root) and not be writable by others, or they are not loaded:

   ```sh
   sudo install -d -o ambersight -g ambersight -m 755 /var/lib/ambersight/tools
   sudo install -d -o ambersight -g ambersight -m 755 /var/lib/ambersight/tools/cuda-<version>
   sudo install -o ambersight -g ambersight -m 644 libonnxruntime_providers_*.so /var/lib/ambersight/tools/cuda-<version>/
   ```

Then restart AmberSight (`sudo systemctl restart ambersight`). **Settings > Detection & alerts >
Objects (AI)** shows *Running on: NVIDIA GeForce ...*, or why it is not on the card (for
example *libcudnn.so.9 not found*), and lets you choose the card when there are several (two
cards of the same model are told apart by their number, e.g. *NVIDIA GeForce RTX 3060 #1*).
AmberSight finds the chosen card in CUDA by its PCI address, so `CUDA_DEVICE_ORDER` does not
mix the cards up; a card hidden with `CUDA_VISIBLE_DEVICES` is reported as not usable. To try
it by hand, run the AI test as the user AmberSight runs as (run as root, AmberSight loads the
add-on only from beside the program, owned by root, and downloads nothing):
`sudo -u ambersight ambersight ai-test --config /etc/ambersight/config.toml --model <model.onnx> <picture.jpg>`.
A copy you put beside the program yourself (root-owned) is used as it is and is not replaced by
updates: put the add-on of the new version there after each update, or remove it and let
AmberSight download its own into the data folder.

cuFFT (`libcufft-13-x`) is not needed: only models with FFT operations use it, and the YOLOX
models do not.

The AI's memory pool on the card is capped at 1.5 GB so other programs on the same card keep
theirs (CUDA adds a few hundred MB on top: the Small model uses about 400 MB in all, the Large
one about 750 MB). Change it with `gpu_memory_mb` in `[ai]` in the settings file.

---

## Raspberry Pi

A Raspberry Pi 4 or 5 makes a small, quiet, low-power recorder for a few cameras. The easiest
way is the package: `sudo apt install ./ambersight_<version>_arm64.deb` (see
[Linux: .deb or .rpm package](#linux-deb-or-rpm-package)). For a manual install, follow the
[Linux](#linux) section with the `linux-aarch64` package
(`ambersight-<version>-linux-aarch64.tar.gz` in the unpack step).

**Check first**

- **The 64-bit OS:** `uname -m` must print `aarch64`. The 32-bit Raspberry Pi OS (`armv7l`)
  cannot run it; reinstall with the 64-bit image from Raspberry Pi Imager.
- **Version:** Raspberry Pi OS **trixie** (released October 2025) or newer. `cat /etc/os-release`
  shows it. The older bookworm is not supported: the AI part needs newer system libraries.
  Install trixie fresh with Raspberry Pi Imager. Ubuntu 25.04 or newer for the Pi works too.

**Good to know**

- **Record to a USB SSD or hard drive, not the SD card.** Constant recording wears out SD cards
  quickly. Mount the drive (for example at `/mnt/recordings`), then choose it in
  **Settings > Recording > Drives & folders**.
- **Number of cameras:** the Pi decodes video on its processor. Give each camera its
  lower-resolution **sub stream** (camera settings > Stream > Sub stream), so motion detection
  uses that instead of the full picture. A Pi 4 or 5 then handles a handful of cameras.
- **AI** runs on the Pi's processor. Choose the **Tiny** model in Settings > Detection & alerts > Objects (AI); do not choose **Large**, which is far too slow on a Pi. It is slower than
  on a PC but fine for checking motion events.
- **Power:** use the official power supply. Under-voltage makes a Pi slow down or restart.
- **Cooling:** a case with a fan or a heatsink keeps a Pi 5 from slowing down under load.

---

## First steps in the browser

The first time you open AmberSight, the Live page shows a **Getting started** checklist. Each step
ticks itself off when it is done; the checklist is also under Help.

1. **Add your cameras.** On a new install the Live page shows **Add your first camera**. Choose
   **Find cameras** (also **Settings > Cameras > Find cameras**), enter the user name and password
   you set in the camera's own app, and press **Scan for cameras**. AmberSight finds ONVIF cameras
   on your network; press **Add** on each one. For other cameras, use **Add by address**: enter the
   camera's address (for example `192.168.1.50`), port and protocol, its user name and password,
   and the main and sub stream paths (for example `/stream1` and `/stream2`). **Brand** fills both
   paths for common makes, and **Find via ONVIF** asks the camera for them. You can also paste a
   whole stream address such as `rtsp://192.168.1.50:554/stream1` into the address field and it is
   split for you. The camera's manual lists its paths, and the [camera guide](cameras.md) has the
   usual ones by brand.
2. **Get alerts on your phone.** Open AmberSight on the phone (see **Phones** below), then in the
   Getting started list choose **Turn on notifications on this device** and **Send a test**. Other
   ways (Telegram, email, ntfy, Pushover, Home Assistant) are in **Settings > Detection & alerts >
   Alerts**.
3. **Add a sign-in account.** Go to **Settings > Users & security**. Until you do, anyone who can
   reach this computer can watch every camera and change every setting. The first account is an
   admin. Give everyone else their own account, with the least they need (see **Accounts: viewers,
   operators and admins** below).
4. **Choose where recordings go.** **Settings > Recording > Drives & folders** picks the drive;
   **Settings > Recording > Storage & retention** sets how long recordings are kept and how much
   space they may use. By default (Automatic) they use what the drive can spare and keep 10% of
   it free; choose a fixed size in GB instead if the drive is shared with other things.
5. **Use HTTPS (optional, recommended).**
   1. Go to **Settings > Users & security > HTTPS** and turn on **Use HTTPS** (it asks first).
   2. Press **Restart now**. AmberSight restarts, creates its own private certificate and opens
      the new https:// address (the browser warns once; continue, and sign in again).
      (Not running as a service? Stop and start AmberSight by hand. You can also set
      `tls = true` in the `[server]` section of `config.toml` and restart.)
   3. On each phone, tablet or computer, sign in and choose **Trust this server** in the account
      menu once. The browser then stops warning.
   Some features (notifications on phones, keep screen awake, talking through a camera) only work
   over HTTPS.
6. **Turn on smart detection (optional).** Go to **Settings > Detection & alerts > Objects (AI)**
   and choose a model (Tiny, Small or Large; Small suits most; Large is the most accurate but needs a graphics card to keep up with several cameras). It downloads once. Alerts then come only when a person,
   vehicle or animal is seen, rather than for shadows, rain and insects.

**Phones:** open the same address in the phone's browser and use **Add to Home Screen**. It then
works like an app, including notifications. On iPhone this needs iOS 16.4 or newer.

**Home Assistant:** AmberSight can show up in Home Assistant through MQTT. See Help > Home
Assistant and MQTT.

---

## Text on the picture

**Privacy:** AmberSight has no privacy masks of its own. Use the camera's own privacy mask if it has
one: Reolink, Dahua/Amcrest and Hikvision cameras do (in the camera's own web page or app). The area
never leaves the camera, it costs nothing here, and it covers everything at once. In **Edit zones**,
an ignore zone keeps motion in an area from counting.

**Downloads:** a clip or export you download carries the time as a subtitle track a player can
show; the exception is an event clip that is sent from its saved clip file as it is, which has no
subtitle track. **Burn the text into this clip** (next to where clips are saved) also draws the
camera's text into the file; that re-encodes the clip, so it takes longer. Burned-in clock text
follows a change to or from daylight saving time inside the clip.

**Text on the picture** (camera settings, Audio & text): the app draws your text over live view and
playback (playback shows the recording's own date and time), and it is put into snapshots, event
pictures and time-lapse frames, at no cost. **Also burn it into recordings** makes it part of the
video files too, which re-encodes the camera's video; leave it off unless the files must carry it.

**Upgrading to 0.11.2:** privacy masks made in 0.11.1 or earlier are gone. Each camera's masks
became ignore zones (motion there still does not count) and areas that don't notify (objects there
are still found by AI and search, but ordinary object alerts there are not sent). A mask with no
size goes, and one partly outside the picture is cut to it. Each list is filled while it has room
(32 ignore zones, 12 areas that don't notify); Status says how many masks had no room in each, so
you can draw them again (motion counts again where a mask had no room as an ignore zone). A camera
that burned its text into the recordings because of masks now has the text drawn on screen
instead (**Also burn it into recordings** is off; turn it back on if the files must carry the
text). Every camera with masks has **Share** (RTSP) turned off, because its picture now includes
the areas the masks hid; turn it back on if you want it shared. Status lists those cameras for 90
days and suggests the camera's own privacy mask. The picture, recordings (older ones too, unless
the masks were burned into them), downloads, linked servers and RTSP sharing are no longer changed
for masks: they show the full picture.

---

## A wall TV or tablet (display mode)

Display mode shows cameras full screen on a TV, an old tablet or a spare monitor, and keeps
running on its own: when something happens on a camera, that camera goes big for a while, then
the grid comes back. It reconnects by itself after a Wi-Fi drop or a restart of AmberSight, and
reloads itself after an update.

1. **Make an account for displays.** In **Settings > Users & security > Users**, add a **viewer**
   account (for example `display`) and limit it to the cameras the display should show. A display
   never signs in as an admin.
2. **Open display mode on the device.** In its browser, open `https://<server address>:8090/display`
   and bookmark it (or add it to the home screen). It shows a code such as `ABCD-EFGH`.
3. **Approve the code.** On your phone or computer, signed in as an admin, open **Settings > Users
   & security > Displays**, enter the code, pick the account and give the display a name. The
   display signs in within a few seconds. It stays signed in while it is on, signs out by itself
   when it has not been used for two weeks, and asks to be paired again every six months. The
   display can only show live cameras (no recordings or events). Admins get an alert if it is
   used from another network. The same page lists every display with a **Sign out** button.
   Treat a display like a signed-in phone: someone who can use its browser could copy its
   sign-in. A copy cannot keep itself signed in, but it works as long as the real display stays
   on (up to the six months). If a display is lost or stolen, sign it out from the Displays list.
4. **Choose what it shows.** Move the mouse, tap the screen or press a key on the remote to show
   **Settings**: which cameras (all, a layout, a group or picked by hand), what makes a camera go
   big (alerts the AI confirmed, with motion for cameras the AI does not check; any motion; or never) and for how long, the clock and armed state,
   and dimming at night. These settings are kept on that device.

Use HTTPS (step 5 of First steps) if you can: only then can the browser keep the screen on by
itself. Over plain HTTP, set the device's own screen timeout to Never as below.

**Fire TV or Android TV.** Install a browser from the app store (Silk on Fire TV; on Android TV,
any Chromium-based browser), open the address and bookmark it. Turn off the screen saver
(Fire TV: Settings > Display & Sounds > Screen Saver > Start Time: Never; Android TV: Settings >
Device Preferences > Screen saver). A browser's own full-screen mode hides its address bar.

**iPad (or iPhone).** Open the address in Safari, choose **Share > Add to Home Screen**, and open
it from there (it runs without Safari's bars). Set **Settings > Display & Brightness > Auto-Lock**
to Never. To keep it on display mode only, turn on **Guided Access** (**Settings > Accessibility
> Guided Access**), open display mode and triple-click the side (or Home) button to start it;
triple-click again and enter the passcode to leave. With **Night** dimming in the display's
settings, a bedroom tablet stays dark until something happens.

**Raspberry Pi (or any small computer with a monitor).** Raspberry Pi OS with the desktop can
start Chromium full screen at login. For example, in `~/.config/labwc/autostart` (Raspberry Pi
OS Bookworm and newer):

```sh
chromium-browser --kiosk --noerrdialogs --disable-infobars --no-first-run \
  --autoplay-policy=no-user-gesture-required --check-for-update-interval=31536000 \
  https://nvr.example:8090/display &
```

(Replace the address with your server's; newer releases name the browser `chromium`.) Turn off screen blanking in **Raspberry Pi
Configuration > Display**. `--kiosk` hides the browser's bars; leave it with `Alt+F4`. For
AmberSight's own certificate, trust it once in Chromium first (open the server's address, sign
in with a password, account menu, **Trust this server**), or use plain HTTP on a home network.

Keep the load low on an old device: the grid uses the light picture stream and the big view the
sub stream by default. Two or four cameras suit most TVs; a display shows at most 16.

---

## Filling gaps from a camera's memory card

When a camera that records to its own card drops off for a while (a Wi-Fi hiccup, a router
restart, AmberSight restarting or updating), the recordings have a hole although the camera
kept recording. With **Fill gaps from the camera's card** (camera settings, Recording) AmberSight
finds the hole once the camera is back, asks the camera for what its card holds for that time,
downloads only the part that fits the hole, and adds it to the camera's recordings. It then plays
on the timeline like anything else; a thin amber line under the timeline marks the stretches that
came from the card. The camera's settings show what it last filled.

- **Cameras:** Reolink cameras and hubs with a memory card, read over RTSP and recording
  continuously (including "sub stream all the time"). The switch is offered only after the camera
  answers with a card in it. Tested on one camera: a Reolink Duo Floodlight WiFi v2 (Wi-Fi), a hole
  of two and a half minutes in its sub-stream recording filled from its card and played back. A Reolink E1
  Outdoor SE (PoE) and a Reolink Home Hub were only listed and downloaded from, not filled; other models and
  firmware are untested (a camera that records its card only on motion has only those events to give).
  Battery Argus cameras already get the start of each event from their card. Cameras of other brands
  (ONVIF Profile G) are not supported yet.
- **What is filled:** holes between 20 seconds and 2 hours, from the last 12 hours, that ended at
  least 3 minutes ago. Holes are filled from the card's recording of the same stream (main, or sub
  when the camera records its sub stream all the time); the other stream is used when the card has
  nothing of the first. A hole is tried a few times at most, then left.
- **Gentle:** one download at a time on the server, in small pieces with pauses, at most about
  1 GB and 20 minutes for one hole (what came by then is kept). Footage is placed by the camera's
  clock corrected for its time zone and its clock error; a camera whose clock is more than 5
  minutes off is not used. A hole from before a daylight-saving change is left alone, since the
  camera's clock is read after the change.
- **Holes made on purpose stay:** only time when the camera was recording continuously is filled.
  A hole from while recording was off or motion-only (in the camera's settings or under a
  profile, including the weekly schedule) is left empty.
- **Sign-in:** AmberSight signs in to the camera's web interface over HTTPS and signs out when
  done. Only when nothing answers on the camera's HTTPS port is plain HTTP used, and then the
  camera's user name and password go over the network unencrypted.
- **Limits:** the picture starts at the card's next keyframe, so a few seconds at the start of a
  hole can stay empty. Recordings are never overwritten or doubled: only time with nothing
  recorded is filled. Pieces from the card count toward recording space and are deleted by retention like
  any other recording.
- **Camera load:** the camera reads its card for each download, which it does alongside recording.

---

## Battery Reolink cameras without a hub (Argus)

Battery Reolink cameras such as the Argus 2 and Argus 3 Pro have no RTSP stream and sleep most of
the time. AmberSight connects to them directly (no hub, no cloud): the camera's motion sensor wakes
it, the camera tells AmberSight, and AmberSight records. Between events nothing stays connected, so
the battery lasts.

**1. In the Reolink app, for each camera** (AmberSight cannot change these for you):

- **PIR motion sensor: on**, with no schedule turning it off. Without it the camera never wakes on
  motion.
- **Push notifications: on** for this camera (in the camera's settings in the app). The camera
  tells AmberSight about motion through the same message it sends for a phone notification; with
  push off it sends nothing, and the camera only wakes when you open it. If push won't turn on,
  turn it on before adding the DNS entry below, or restart the camera after turning it on.
- **SD card in, recording on.** AmberSight adds the first seconds of each event from the camera's own
  recording (see below). If the app shows no recordings for a while although things moved, format
  the card in the app.
- **Time zone** set to where the camera lives, with automatic time sync on. (AmberSight reads the
  camera's clock itself, so a different zone does not break anything, but the time burned into the
  picture should be local.)
- Note the camera's **UID** (in the app's device info: 16 letters and digits) and its address on
  your network.

**2. On your network:** point the name `pushx.reolink.com` at the AmberSight computer with a local
DNS entry (a Pi-hole "Local DNS record", or your router's DNS override). The cameras send their
motion message to that name. Use every DNS server your network hands out, or a camera may ask one
without the entry. While the entry is in place, the Reolink phone app no longer gets notifications
from these cameras: AmberSight's own alerts take over.

**3. Add the camera in AmberSight.** In Settings > Cameras, add a camera, pick **Reolink battery
(Argus, no hub)** under Brand, and type the camera's address, its **UID**, and the user name and
password. AmberSight turns on **Battery camera** with a 30 second idle time and motion-only
recording. (A camera added before this type existed: open it and click **Use battery settings**.)
The camera's motion message always starts an event, and while the camera is awake its picture's
motion counts too, so the `trigger` setting does not apply to these cameras. Picture motion counts only for 90 seconds after the camera's last motion message or wake (up to 3 minutes once AI has confirmed a person, vehicle or other object the camera alerts on, and always while someone watches it live). An event keeps the camera awake for at most 3 minutes after that message or wake; the camera then stays awake for its idle time before it sleeps. So rain or a swaying bush cannot keep it awake. A live viewer, another camera's trigger while that camera sees motion, and a manual recording (until it ends) still do. If the log says `a battery Argus needs its UID`, the
UID is missing. Then turn on the listener below (the `[pushx]` block, which is not in the camera
form).

The same thing in `config.toml`:

```toml
[pushx]
enabled = true            # listen for the cameras' motion message
listen = "0.0.0.0:80"     # the cameras always use port 80
motion_seconds = 20       # how long one motion message counts as motion

[[cameras]]
name = "Back Yard"
url = "baichuan://192.168.1.60"   # the camera's IP address (give it a fixed one in the router)
uid = "95270000XXXXXXXX"          # the camera's UID from the Reolink app
username = "admin"
password = "the camera password"
on_demand = true                  # sleeps until motion, the wake button or a viewer
record_motion_only = true
idle_timeout = 30                 # back to sleep 30 s after the last activity
```

Port 80 must be free and reachable: allow it in the firewall. On Linux the service runs as a
normal user, so give it the right to use port 80, for example with a systemd drop-in
(`sudo systemctl edit ambersight`):

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

then `sudo systemctl restart ambersight`. The log says `pushx: listening on 0.0.0.0:80` when it
works (or why not).

The motion message has no password (the cameras cannot send one), so AmberSight takes it only from
the camera's own address. That is why the camera needs a fixed IP address.

Burned-in text is not applied to these cameras (see [Text on the picture](#text-on-the-picture)).

**What happens on motion:** the camera wakes and sends its message; AmberSight marks motion on the
timeline, connects (2–3 s), records the live picture, and lets the camera sleep again about 30 s
after the motion ends. The live recording starts a few seconds into the event, so about a minute and
a half later AmberSight fetches the camera's own recording of those first seconds from its SD card
and adds it in front. The Wake button and opening the camera's live view also wake it, and so does
anything asking for its picture (`/api/snapshot`, an MJPEG or RTSP viewer, a Home Assistant camera
entity): each request keeps it awake for another idle time. While it is awake it sends no motion
messages. The log line `waking up for ...` says what woke it.

**Events AmberSight missed (catching up from the card):** the camera records to its card even when
AmberSight never hears about the motion (push off in the app, a DNS or Wi-Fi problem, AmberSight
stopped or updating). AmberSight lists the card's recordings of the last 12 hours (without
downloading them), and each one it has no event or recording for is downloaded and becomes an
ordinary event: recording, thumbnail, AI check. It is marked **from card** in the event list, and
its recording shows with the thin amber "from the card" line on the timeline. An event that ended
more than 5 minutes before it was imported sends **no notification** (no push, email or other
alert channel): it is history by then. The camera's Recording settings show a line such as
"Imported 2 events from the card at 07:40."

Each look wakes the camera, so AmberSight looks only:

- once after AmberSight starts (when the camera is next awake anyway, or one wake 5 minutes after
  the start);
- after the camera could not be reached and is back;
- a few minutes after a motion message that brought no recording;
- otherwise about once an hour, and only while the camera is awake anyway; if it stays asleep, one
  wake of its own every 3 hours. A camera whose card lists nothing three times in a row is no
  longer woken for it, and neither is one whose card could not be listed three times in a row
  (it stopped answering, or its answers made no sense) until it reconnects or AmberSight restarts.

Never twice within 15 minutes. One look imports at most 5 recordings (150 MB, 15 minutes); the rest
come at the next look. Downloads run at about half real time, so the camera stays awake for about
two minutes per minute of video imported. Recordings from times the camera was disarmed, switched
off, not recording or with motion off (as AmberSight saw it then, or as the profile schedule says)
are left on the card, and so are recordings from before a daylight-saving change. It is on by
default for battery cameras (Settings > Cameras > the camera > Recording, **Import missed events
from the camera's card**). In `config.toml`:

```toml
catch_up_from_card = false   # off for this camera (on by default for battery cameras)
catch_up_every = 180         # minutes between looks while awake (at least 60; 0 = only after a
                             # start, a reconnect or a missed motion message)
```

The card has no file list these cameras answer, so AmberSight asks the camera for its recordings
one at a time from a moment on, reading only each one's first bytes; a look of 12 hours takes a few
seconds per recording. Tested on a Reolink Argus 2; other battery models are not tested yet.

---

## Apple Home (HomeKit)

AmberSight can show cameras in the Home app on an iPhone, iPad or Mac, with live video, a motion
sensor (Home can notify you of motion) and a snapshot.

**Not yet tested with real Apple devices.** It follows the HomeKit specification and works with
open-source HomeKit controllers. If you try it, please tell us how it went in GitHub Discussions.

1. Settings, Connections, **Apple Home**: turn on **Apple Home (HomeKit)** and switch on the
   cameras to show (or use **Show in Apple Home** in a camera's settings).
2. Press **Pair a device** next to the camera (or the bridge). Pairing is open for 5 minutes, and
   the setup code and QR code show.
3. In the Home app: **+**, **Add Accessory**, and scan the code shown in Settings (or enter the
   setup code). By default each camera is added on its own, with the same setup code for all;
   **Behind one bridge** in the same page adds them all at once instead. Home says the accessory is
   not certified; choose **Add Anyway**.

Apple devices can pair only while pairing is open. It closes once a device is added, when you
press **Stop**, or after 5 minutes; until then Home does not list AmberSight as an accessory to
add. While it is open, the page shows the address of the device pairing now. If that is not your
phone or tablet, press **Stop and start again**: that pairing ends, the address cannot pair again
until pairing closes, and you get 5 fresh minutes. Sharing your home with other people in the Home
app needs none of this.

The Apple device must be on the same network (away from home, through a home hub: a HomePod or
Apple TV). AmberSight is found over Bonjour (mDNS, UDP port 5353). Each camera on its own listens on
its own TCP port: 51831, 51832, ... (`[homekit] port` is 51830, which the bridge uses when
cameras are bridged; Homebridge uses 51826, so both can run on one computer). Live video goes to
the Apple device over UDP (SRTP), and the Apple device sends RTCP back to a UDP port of
`[homekit] rtp_ports` (default 51900-51999, two per live view). Allow these in the firewall:

- **Windows:** while HomeKit is on, the AmberSight service adds an inbound rule
  "AmberSight HomeKit" for `ambersight.exe` that opens only HomeKit's ports: TCP for the bridge or
  the cameras on their own that run now, UDP 5353 and the `rtp_ports` range (all network profiles,
  like the "AmberSight NVR" rule for the web page; it never opens the web page's port). It follows
  the ports as cameras are added or removed, is removed when HomeKit is turned off or AmberSight
  is uninstalled, and a rule left behind (AmberSight was killed) is removed at the next start.
  When AmberSight does not run as Administrator (for example `ambersight run` from an ordinary
  prompt), it cannot add the rule: the Apple Home page then lists the ports and gives the commands
  to run as Administrator, for example
  `netsh advfirewall firewall add rule name="AmberSight HomeKit" dir=in action=allow program="C:\Program Files\AmberSight NVR\ambersight.exe" protocol=TCP localport=51831-51832`
  and the same with `protocol=UDP localport=5353,51900-51999`.
- **Linux, ufw:** `sudo ufw allow 51830:51894/tcp`, `sudo ufw allow 5353/udp` and
  `sudo ufw allow 51900:51999/udp`.
- **Linux, firewalld:** `sudo firewall-cmd --permanent --add-port=51830-51894/tcp
  --add-port=51900-51999/udp --add-service=mdns`, then `sudo firewall-cmd --reload`.
- **macOS:** allow AmberSight when macOS asks (System Settings, Network, Firewall).

AmberSight answers mDNS next to the system's own (Avahi, Bonjour on macOS, Windows).

Live video is encoded for Home at the size and bit rate it asks for, on the graphics card when
there is one (else on the processor: about a third of a core for 1280x720), with the camera's sound
as Opus. At most 2 live views per camera and 4 in all, within `[transcode]`'s limits. A camera read
by ffmpeg instead of the built-in reader (privacy masks, burned-in text, ...) is read once more for
each live view in Home, with its privacy masks and text drawn in, so Home never shows what a mask
hides (masks are placed by fraction of the picture, on the main or sub stream alike, as in the live
view; if a camera's sub stream is cropped differently from its main stream, check the Home view);
changing a camera's masks, text or address ends its live views in Home at once; this costs an extra camera connection and a little more processor time, and a capture
device (which opens only once) has no live video in Home. Each live view's encryption keys
(SRTP) are passed to ffmpeg on its command line, so other accounts on the same computer can see
them while it runs (they are never written to the log); on a computer you share with people you do
not trust, keep that in mind.

HomeKit does not know AmberSight's accounts: everyone in that Apple Home sees the cameras switched
on there. A camera removed in the Home app can be added again with the same setup code, after
**Pair a device**. If the setup
code got out, **Reset pairing** in the same page forgets every paired Apple device and makes a new
code; Home then sees new accessories, so their rooms, scenes and automations start over.

HomeKit listens on all addresses (`[homekit] bind = "[::]"`, IPv6 and IPv4), apart from the web
page's `[server] bind`; set one address to limit it. It is announced only on real network
interfaces (not Docker, VM or VPN ones such as Tailscale).

## Updating

### From the web page (the built-in updater)

**Settings > System > Status & updates** has an **Updates** row: it shows this version and, when
a newer one is out, an **Update to vX.Y.Z** button. Click **Check for
updates** to look now. AmberSight also looks every 12 hours (it asks api.github.com and sends
only its name and version); to stop that, add this to `config.toml`:

```toml
[update]
check = false
```

Nothing is installed until an admin clicks **Update** and confirms in a window that says what
happens:

- When AmberSight restarts itself after the update (a service, a container that restarts), it stops
  while it updates and starts again: no recording and no live view for about a minute (longer on
  big sites). When it does not restart itself, it keeps running the old version until you restart
  it, and the restart takes the same minute. `ambersight update` on a terminal says the same.
- Right after the restart, playback and the timeline can be slow for a few minutes while it
  checks its recordings. A small notice at the bottom of the page says so until it is done (a big site with a
  spinning recordings drive can take longer).
- If the update changes the database, a copy of it is saved first, if there is room, in the `backups` folder of the
  data folder (`pre-upgrade-v<old version>-<date>/ambersight.db`; the two newest are kept; skipped
  with a warning when the drive is nearly full). The copy is the database file copied as it is, which on a
  big database on a spinning drive can add a minute or more to the restart. Going back to the older version afterwards is not
  recommended: it runs, but warns, and ignores the newer parts. To really go back, restore that
  copy with the older program.

AmberSight then:

1. downloads the package for this computer from the project's published releases,
2. checks that it is signed with the AmberSight release key and unchanged
   (an unsigned or altered package is refused, and nothing changes),
3. copies the program in use (on Windows also its `.dll` files) into a
   **`previous`** folder next to it,
4. puts the new program in place and restarts, when AmberSight runs as a
   service. Run by hand (`ambersight run`), it says "restart AmberSight"
   instead.

Your settings, recordings and database are not touched.

On a computer without a browser, the same from a terminal:
`ambersight update --config config.toml` (add `--check` to only look). It prints the same
warning and asks `Update now? [y/N]`; `--yes` skips the question, and with no terminal to ask on
(a script) it goes ahead.

**Going back**: stop the service, copy the files from the `previous` folder
over the program next to it, start the service.

**Installed with a .deb or .rpm package?** The updater refuses (the program
lives under `/usr`); update with `sudo apt update && sudo apt upgrade` or `sudo dnf upgrade` (with the
repository set up as above, the panel says so; otherwise it shows the package's link).
Updates made with apt, dnf or the Windows installer cannot warn first, but have the same effects: AmberSight is
stopped for about a minute, can be slow for a few minutes afterwards, and a database change is copied
to `backups` first. Pick a quiet time.
It also refuses when it cannot write to the program's folder; then update
by hand as below.

### By hand

With an installer: run or install the newer `setup.exe`, `.deb` or `.rpm`, or run
`ambersight setup` from the newer package. The settings and recordings are kept.

For a manual install:

1. Stop the service.
2. Replace the AmberSight program (on Windows also the `.dll` files) with the new one.
3. Start the service.

Your settings (`config.toml`), recordings and database are kept.

| System | Commands |
|---|---|
| Windows (Administrator) | `ambersight.exe service stop`, copy the new files over, `ambersight.exe service start` |
| macOS / Linux | `./ambersight service stop`, replace `ambersight`, `./ambersight service start` |

---

## Moving recordings or moving to a new computer

AmberSight's database stores each recording as the name of its folder ("data folder", or an
extra folder's name such as `new` or `stored`) plus a path inside that folder. It does not store
`D:\...` or `/mnt/...` paths, so the database works on another drive or another operating system
as it is. To move:

1. Stop AmberSight. Copy the data folder (with `ambersight.db`, `clips/`, `thumbs/` and the rest)
   and any recording folders you want to keep to the new place, or plug the drives into the new
   computer.
2. Put `config.toml` in place and change the paths in it: `data_dir` under `[storage]`, and the
   `path` of each `[[storage.folders]]`. Keep each folder's `name` as it was. The extra folders
   can also be changed later in Settings > Recording > Drives & folders.
3. Start AmberSight. Users, cameras, events, faces and plates you named, layouts, clips and
   pictures carry over.
4. If some recordings did not come along (a folder left behind, a drive reformatted), AmberSight
   notices at start: the log says so and Settings > Recording shows a note. Check the folder
   paths first: a wrong path looks the same as deleted recordings. Then remove the ones that are
   gone from the database under Drives & folders, **Recordings whose files are gone**: **Check
   now**, look at what it found, **Remove**. Or from the command line, as the account AmberSight
   runs as (it refuses to run as root on a data folder that belongs to another account). The
   server can be running or stopped. One check or removal runs at a time: while one runs (on the
   page or the command line), starting another is refused until it has finished. A running
   server shows the result at once, apart from playback it had already prepared, which catches
   up within a minute.

   ```
   ambersight storage prune-missing --config config.toml           # what is missing; changes nothing
   ambersight storage prune-missing --config config.toml --apply   # remove them from the database
   ```

   It backs the database up first (into `backups/` in the data folder) and never deletes a file.
   That backup is one of the `backup_keep` backups (at least 2 are kept), so newer ones replace it
   in time; putting it back undoes the removal but loses the events recorded since.
   A folder whose recordings could just as well be behind a wrong path or a drive that is not
   connected is left alone unless you name it with `--include-folder <name>` (`data` for the data
   folder), or tick it on the page: a folder that is not there, holds none or most of its
   recordings, has a camera none of whose recordings are there, or (Linux) looks like the mount
   point of a drive that is not mounted: it is on a mount point in `/etc/fstab` that is not
   mounted now, or, for drives not in fstab, under `/mnt`, `/media` or `/run/media` with nothing
   mounted there. In that last case mount the drive first: until then new recordings go to the
   system disk. If the folder really is meant to be on the system disk, name or tick it.
   Events whose video was all in the removed recordings stay in the event list with their
   pictures, marked "No clip", until retention removes them by age; protected events are not
   changed.

Example: from Windows to Linux. The Windows config had

```toml
[storage]
data_dir = "D:/AmberSight/data"

[[storage.folders]]
name = "stored"
path = "F:/AmberSight/Stored"
```

With the data folder copied to `/var/lib/ambersight` and the F: drive mounted at `/mnt/f`, the
Linux config gets

```toml
[storage]
data_dir = "/var/lib/ambersight"

[[storage.folders]]
name = "stored"
path = "/mnt/f/AmberSight/Stored"
```

Make sure the service account can read and write both (`sudo chown -R ambersight: /var/lib/ambersight /mnt/f/AmberSight`
for the packages), start AmberSight, then run the dry run above as that account
(`sudo -u ambersight ambersight storage prune-missing --config /etc/ambersight/config.toml`) and
`--apply` when what it lists is really gone.

## Uninstalling

- **Windows installer:** Settings > Apps > Installed apps > AmberSight > Uninstall.
- **.deb / .rpm:** `sudo apt remove ambersight` or `sudo dnf remove ambersight`.
- **`ambersight setup`:** `ambersight setup --uninstall` (with `--system` or `--here` if you
  used them).

These keep your settings and recordings and leave them where they were (the tables in
[Easiest: the installer](#easiest-the-installer) list the folders).

For a manual install:

1. Remove the service: `ambersight service uninstall` (as Administrator on Windows).
2. Delete the AmberSight folder.

The recordings are in the data folder (`amberdata` next to `config.toml` unless you chose another
place in Settings > Recording > Drives & folders). Delete that folder too if you no longer need the recordings.

---

## Troubleshooting

### Behind a reverse proxy or tunnel

AmberSight counts wrong passwords and wrong codes per address. Behind a reverse proxy or tunnel
(nginx, Caddy, Traefik, cloudflared, Tailscale Funnel) every visitor arrives from the proxy, so they
would all share those limits: someone who gets too many wrong would make everyone behind the proxy
wait, and logs and new-device alerts would show the proxy's address. Tell AmberSight the proxy's own
address and it takes each visitor's address from the `X-Forwarded-For` header the proxy adds:

```toml
[server]
trusted_proxies = ["127.0.0.1"]   # the proxy's address as AmberSight sees it
```

- **List only the proxy.** Use its exact address (`127.0.0.1` for a proxy on the same computer;
  Docker's bridge gateway, e.g. `172.17.0.1`, for a proxy on the host of a container on a bridge
  network). Never a LAN or internet range: anyone connecting from a listed address could claim any
  address and get past the sign-in limits. For the same reason, don't also expose AmberSight's port
  directly to the clients the proxy serves.
- **An HTTP proxy that adds the visitor to `X-Forwarded-For`.** Caddy, Traefik, cloudflared and
  Tailscale do. nginx needs `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`. A TCP
  (layer 4) proxy passes the client's own header through and must not be listed. With several proxies
  in a row, list only the one that connects to AmberSight.
- **Keep the host name.** AmberSight refuses changes from another web site by comparing the page's
  origin with the `Host` header, so the proxy must pass the original host (nginx:
  `proxy_set_header Host $host;`) and allow websockets for live video (nginx:
  `proxy_http_version 1.1; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";`).
- **HTTPS.** Either have the proxy connect to AmberSight's HTTPS (`https://`, trusting its private
  CA or skipping verification on localhost), or turn AmberSight's HTTPS off and bind it to
  `127.0.0.1` so only the proxy reaches it. With HTTPS off, the proxy's `X-Forwarded-Proto: https`
  keeps sign-in cookies marked secure.
- AmberSight says at start when an entry is not an address or is a very wide range.

Without `trusted_proxies`, everyone behind the proxy shares one address as described above. The alert
for a password sent directly by a program (HTTP Basic or RTSP) is once per account and network, so
behind a proxy that is not listed it sees only one network.

### Cameras fail with "Resource temporarily unavailable" (process limit)

The camera's note says the system would not start another process or thread, or ffmpeg prints
`pthread_create() failed` or `Error while opening decoder: Resource temporarily unavailable`
(then `error code: -22`). The server hit a limit on processes and threads. In a Docker container
it is usually `--pids-limit` (see [docker.md](docker.md#cameras-fail-with-resource-temporarily-unavailable));
as a service, `ulimit -u` or systemd's `TasksMax`. It mostly happens to cameras read by ffmpeg
(burned-in text or `ingest = "ffmpeg"`), 4K ones first. Raise the limit, or let the
camera use the built-in reader. The camera report shows the limit and what is in use.

### Recordings on a merged drive (Unraid, mergerfs)

At a start, AmberSight skips a camera's folder when its modified time has not changed since
the last full look. A merged drive (Unraid's shared folders, mergerfs and similar) might not update that
time when a file lands on one of the disks behind it. A full look at every folder happens weekly
and after any unclean stop, so nothing is lost: a recording the index missed might only be noticed later.

### Event clips not saved to the copy folder

A camera's **Also save event clips to** folder (camera settings, Recording) shows its last copy or
what is wrong under the field; **Test** writes and removes a test file there. When copies keep
failing, Status lists it and a system alert goes out; copies wait (up to 3 days, also over a
restart) and are made once the folder works again.

**Where to put it.** Pick a folder other users of the computer cannot rename or delete: inside your
own user folder (your OneDrive or Google Drive folder, for example), or a folder you have locked
down. On Windows, any local user may create and rename folders at the top of a second drive (`D:\`)
by default; for a folder there, take that away, for example (in an Administrator terminal):
`icacls "D:\Camera copies" /inheritance:r /grant:r SYSTEM:(OI)(CI)F Administrators:(OI)(CI)F "%USERNAME%":(OI)(CI)F`.

- **"The copy folder was replaced".** The folder at that path is not the same folder as when you saved
  or tested it (deleted and made again, restored from a backup, or swapped for another). AmberSight
  stops copying there until you open the camera and press **Test**, which accepts the folder now there.

- **The folder is missing or offline.** AmberSight never creates the folder, so an unplugged drive or
  a share that is down is noticed instead of filling the system drive. Plug it in or create it.
- **Windows: not allowed to write there.** The AmberSight service runs as the Local System account,
  not as you. A folder in your own user folder (`C:\Users\you\OneDrive\Cameras`) usually works,
  since Local System may write to user folders, and OneDrive uploads the files while you are signed in
  to Windows. If it does not: in the folder's Properties, Security, give SYSTEM "Modify"; in OneDrive,
  set the folder to "Always keep on this device"; or use a folder outside your user folder (such as
  `D:\Camera copies`) and have your sync app sync that one (Google Drive for desktop and Dropbox can
  sync any folder). A network share needs to let this computer's account in (Local System uses the
  computer's account on the network).
- **Linux and macOS: not allowed to write there.** The package install runs as the `ambersight`
  account: `sudo chown ambersight: /path/to/folder`, or give that account write access through a
  group. In Docker, mount the folder into the container (for example `-v /mnt/cloud/cams:/copies`)
  and set `/copies`.
- **The drive is full.** Free space there or set **Keep copies** to a number of days. AmberSight stops
  copying while that drive has less than 2 GB (or 5%) free, 10 GB (or 10%) when it is the system drive
  or the drive of AmberSight's data. The copy folder is not part of the space for recordings, so
  recording clean-up does not free it.
- **Windows: a mapped drive letter.** The service does not see drive letters mapped in your own
  sign-in (`Z:`): use the share's own name (`\\nas\share\cameras`).
- **Linux and macOS: "anyone on this computer may change things in the folder".** AmberSight does not
  write into a folder every user may write to (`/tmp`, a folder with mode 777): take away other users'
  write access (`chmod o-w`) or pick another folder.
- **The copy folder is not responding.** A copy has been running for minutes: a network share or a
  drive stopped answering. Other cameras' copies to other folders carry on, and recording is not
  affected.
- A renamed camera saves into a new folder; its old folder is no longer trimmed by **Keep copies**.
  In Docker the file names use the container's time zone (`TZ`).

### An app gets the sub stream instead of a converted stream

An app that says what its device can decode gets streams that fit, converted on the graphics card
when needed. When it gets the sub stream (or a smaller stream) instead, the app is told why: `busy_fallback`
means every conversion slot was in use, `off` means conversion is turned off. Check
**Settings > System > Status**, **Connections** tab: it lists the encoders found and the running
conversions (camera, size, encoder, viewers). NVIDIA consumer cards allow only a few encodes at a
time; lower `max_hardware` in `[transcode]` if conversions fail to start. To turn conversion off
entirely, set `[transcode] encoder = "off"` in config.toml (apps then get the best stream that fits
as it is).

### Accounts: viewers, operators and admins

**Settings > Users & security** gives each account a **Role**:

- **Viewer**: watches live video, events, clips and recordings, and marks events "looks fine" in
  their own browser. Changes nothing.
- **Operator**: a viewer who can also do the day-to-day things. Each of these is ticked for a new
  operator and can be unticked: moving PTZ cameras (go to presets, click to aim; saving presets
  stays with admins), talking through a camera and recording a clip now, arming and disarming,
  switching profiles and pausing notifications, keeping events forever and marking them "not a real
  alert", and changing the detection and alert settings of their cameras (zones, areas that don't
  notify, motion sensitivity, AI objects and confidence, alert times and limits, loud sounds). An
  operator never sees or changes anything else: no stream addresses or camera passwords, recording
  and storage, other settings, users, automations, updates or restarts.
- **Admin**: everything, on every camera.

Viewers and operators can be limited to some cameras: they then see, and control, only those. A
camera-limited operator can't arm, disarm or switch profiles (those change every camera); they can
still pause notifications of their own cameras.
Making an operator, or giving one more rights or cameras, asks for your own password, as making an
admin does, and the other admins are told. **Every change an operator makes to a camera's detection
or alert settings is reported to the admins** (one message per camera every few minutes, naming
what changed, such as "detection zones, alert schedule"), and so is marking an event "not a person"
(or car ...) so that spot stops alerting. What operators do is also in the audit log (Settings >
System > Audit). An operator pauses a camera's notifications for a day at most, and their changes
can restart a camera at most 3 times in 10 minutes.
An operator can make a device token with control rights in **Your account**; it does what the
operator may.

### Who is signed in, and new device alerts

**Your account** (the menu at the top) lists the browsers and phones signed in to your account, with the address each last came from, when it signed in and when it was last used. Press **Sign out** on one you do not recognize, or **Sign out all other devices**, and then change your password.

When an account signs in from a browser it has not used before, admins get one message: the account, the kind of device, the address and how it signed in. It uses a small cookie in that browser (it gives no access and is kept up to two years); clearing cookies makes the next sign-in look new. A burst of new devices for one account is joined into one more message. These messages go out even with **Alert when something is wrong** off. After an update nobody is signed out, but each browser is told once, the next time it signs in without a working session. A program that sends the password directly (HTTP Basic or RTSP) has no session to sign out, so its message says to change the password; it comes once per account and network.

Signing another device out also forgets that browser and, when it was turned on from that device, its notifications, so signing in there again is told again. Notifications turned on before this existed cannot be matched: remove older devices in the account menu under **Notifications on this device**. Changing a password forgets every known browser except the one it was changed from, so each of your own browsers is told once at its next sign-in. It also signs the other devices out and takes off the notifications they had turned on (a phone with no sign-in left keeps its alerts); AmberSight turns a browser's notifications back on when you sign in on it again. An admin's device that loses alerts this way, or by signing it out, is told, and the admins too. Signing out another session of the browser you are using forgets nothing. Signing out of a device with a plain **Sign out** keeps it known. Video open on your other devices stops for a moment when a device is signed out, and reconnects by itself.

### Locked out of two-step codes

Lost your phone and your recovery codes, and there is no other admin to turn the codes off for
you in Settings > Users? Anyone with administrator access to the computer can. It works with the
server running or stopped (run it from an account that can write to the data folder: an Administrator
prompt on Windows, `sudo` elsewhere):

```
ambersight user reset-two-step <name> --config <path to config.toml>
```

The account can then sign in with just its password, and can turn two-step codes on again in Your
account. It is written in the audit log.

If someone else may know the password (for example: you were warned about wrong two-step codes),
change it too. It asks for the password, or reads one line from stdin; it is never given on the
command line (on Windows what you type at the prompt shows on the screen). It will apply within a few
seconds on a running server and sign the account out everywhere, or when a stopped one starts. Device
tokens are not revoked unless you add `--revoke-tokens`:

```
ambersight user set-password <name> --config <path to config.toml> [--revoke-tokens]
```

An account disabled by mistake, or by someone with a stolen admin session, can be enabled the same way:

```
ambersight user enable <name> --config <path to config.toml>
```

The secrets for two-step codes and the hashes of device tokens and recovery codes are in the database
(`ambersight.db`), not in `config.toml`. The database is in your backups: the local backups folder is
readable by administrators only, and off-site backups are encrypted if you set a passphrase, so set
one. Restoring an old backup brings back the two-step state and device tokens of that day: revoke
anything that was revoked since.


| Problem | What to check |
|---|---|
| The page does not open from another device | The firewall step for your system; use the computer's IP address; the service is running (`service status`). |
| A camera stays offline | Its address, user name and password; that it is on the same network; that RTSP is turned on in the camera's own settings. Status and Settings > System > Logs give the reason in plain words (for example "the camera has no stream at that address (404)" means a wrong stream path: see the [camera guide](cameras.md)). A camera that only streams over UDP: set Transport to Auto or UDP in its settings. On macOS, "No route to host" means Local Network access is blocked (macOS step 4). Still stuck: save its **Camera report** (Settings > Cameras > the camera > Network) and attach it to a bug report. |
| One camera uses a lot of processor time | Usually a large (4K) camera without a sub stream, or burned-in text. Status notes a missing sub stream and **Find the sub stream** looks for one. A graphics card (NVIDIA, Intel or AMD) decodes for motion detection when the camera's Hardware decoding is Auto (the default; see [Hardware decoding](#hardware-decoding-for-motion-detection)); Settings > System > Hardware shows what each camera uses and why. |
| "ffmpeg was not found" (Linux) | Install ffmpeg (Linux step 1), then restart the service. |
| The browser warns about the certificate | Choose "Trust this server" in the account menu on that device. |
| A linked server or capture helper says `invalid peer certificate: MalformedNameConstraint` | The private certificate authority made earlier lists a computer name that is not a valid network name (for example a name ending in a hyphen, like `pc-`). Settings > System > Logs says so at start. New certificate authorities leave such names out. To replace the old one: stop AmberSight, delete `ca.pem`, `ca-key.pem`, `ca.json`, `cross.pem`, `cert.pem`, `key.pem` and `server.json` in the `tls` folder of the data folder, and start it again. Every device must then "Trust this server" again and linked servers must be linked again. Or open the server by its IP address instead. |
| Recordings fill the disk | Settings > Recording > Storage & retention sets the space limit and how long recordings are kept; Drives & folders picks the drive. |
| Nothing records while the computer sleeps | Turn off sleep for that computer. |

---

## Building from source (optional)

For developers or systems without a package:

1. Install Rust (https://rustup.rs).
2. Run:

   ```
   cargo build --release
   ```

   For a Raspberry Pi package, on a 64-bit x86 Linux computer with Docker, run
   `scripts/package-linux-arm64.sh`.

3. The program is `target/release/ambersight` (`ambersight.exe` on Windows). On Windows, copy the
   `.dll` files from `target\release` along with it.

`scripts/package.sh` (macOS and Linux) and `scripts/package.ps1` (Windows) build the release
packages described above. The installers are built on a 64-bit x86 Linux computer with Docker:

| Script | Makes |
|---|---|
| `scripts/build-linux-x86_64.sh` | the x86_64 Linux program, built in Debian 13 so it also runs on older systems than the build computer |
| `scripts/package-linux-arm64.sh` | the Raspberry Pi program and `.tar.gz` |
| `scripts/package-linux-deb-rpm.sh` | the `.deb` and `.rpm` packages from those two programs |
| `scripts/package-repo.sh` | the signed apt and dnf repository for a release (see `RELEASING.md`) |
| `scripts/package-windows-installer.sh` | `setup.exe` (NSIS) from the folder `scripts/package.ps1` made on Windows, copied into `dist/` |
