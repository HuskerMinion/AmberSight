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
| Windows 10 / 11, 64-bit | `ambersight-<version>-windows-x86_64-setup.exe` (installer) or `ambersight-<version>-windows-x86_64.zip` | AI uses the graphics card (DirectML) |
| macOS on Apple silicon (M1 and later) | `ambersight-<version>-darwin-arm64.tar.gz` | AI uses the Neural Engine and GPU (Core ML) |
| macOS on Intel | `ambersight-<version>-darwin-x86_64.tar.gz` | Built-in AI is not available; an external AI server can be used |
| Linux, 64-bit (Debian, Ubuntu, Fedora, Arch, ...) | `ambersight_<version>_amd64.deb`, `ambersight-<version>-1.x86_64.rpm` or `ambersight-<version>-linux-x86_64.tar.gz` | The packages need Debian 13, Ubuntu 25.04, Fedora 42 or newer. AI uses an NVIDIA card (CUDA) when NVIDIA's libraries are installed |
| Raspberry Pi 4 / 5 (and other 64-bit ARM Linux) | `ambersight_<version>_arm64.deb`, `ambersight-<version>-1.aarch64.rpm` or `ambersight-<version>-linux-aarch64.tar.gz` | Needs the **64-bit** Raspberry Pi OS, trixie (2025) or newer. See [Raspberry Pi](#raspberry-pi). |

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

**Debian, Ubuntu, Mint, Raspberry Pi OS:**

```
sudo apt install ./ambersight_<version>_amd64.deb
```

(on a Raspberry Pi: `sudo apt install ./ambersight_<version>_arm64.deb`). This also installs
ffmpeg.

**Fedora:** first add RPM Fusion (https://rpmfusion.org/Configuration) for the full ffmpeg, then:

```
sudo dnf install ./ambersight-<version>-1.x86_64.rpm --allowerasing
```

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

To update, install the newer package the same way. To remove it: `sudo apt remove ambersight`
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
   on macOS, choose **Allow** when asked),
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

### Day-to-day on macOS

| Task | How (Terminal in `~/AmberSight`) |
|---|---|
| Stop / start | `./ambersight service stop` / `./ambersight service start` |
| Status | `./ambersight service status` |
| Logs | `ambersight.out.log`, `ambersight.err.log` in the folder (also Settings > System > Logs) |
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

### Linux: an NVIDIA card for the AI

On a 64-bit Intel/AMD Linux computer with an NVIDIA graphics card, the built-in AI runs on the
card (CUDA): checks take a few milliseconds instead of tens to hundreds on the processor, and
the Large model keeps up with many cameras. Without it everything still works on the processor.

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
  lower-resolution **sub stream** (camera settings > Stream > Sub stream URL), so motion detection
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
   on your network; press **Add** on each one. For other cameras, use **Add by address** with the
   camera's stream address (for example `rtsp://192.168.1.50:554/stream1`; the camera's manual
   lists it).
2. **Get alerts on your phone.** Open AmberSight on the phone (see **Phones** below), then in the
   Getting started list choose **Turn on notifications on this device** and **Send a test**. Other
   ways (Telegram, email, ntfy, Pushover, Home Assistant) are in **Settings > Detection & alerts >
   Alerts**.
3. **Add a sign-in account.** Go to **Settings > Users & security**. Until you do, anyone who can
   reach this computer can watch every camera and change every setting.
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

## Battery Reolink cameras without a hub (Argus)

Battery Reolink cameras such as the Argus 2 and Argus 3 Pro have no RTSP stream and sleep most of
the time. AmberSight connects to them directly (no hub, no cloud): the camera's motion sensor wakes
it, the camera tells AmberSight, and AmberSight records. Between events nothing stays connected, so
the battery lasts.

**1. In the Reolink app, for each camera** (AmberSight cannot change these for you):

- **PIR motion sensor: on**, with no schedule turning it off. Without it the camera never wakes on
  motion.
- **Push notifications: on.** The camera tells AmberSight about motion through the same message it
  sends for a phone notification; with push off it sends nothing.
- **SD card in, recording on.** AmberSight adds the first seconds of each event from the camera's own
  recording (see below). If the app shows no recordings for a while although things moved, format
  the card in the app.
- **Time zone** set to where the camera lives, with automatic time sync on. (AmberSight reads the
  camera's clock itself, so a different zone does not break anything, but the time burned into the
  picture should be local.)
- Note the camera's **UID** (in the app's device info) and its address on your network.

**2. On your network:** point the name `pushx.reolink.com` at the AmberSight computer with a local
DNS entry (a Pi-hole "Local DNS record", or your router's DNS override). The cameras send their
motion message to that name. Use every DNS server your network hands out, or a camera may ask one
without the entry. While the entry is in place, the Reolink phone app no longer gets notifications
from these cameras: AmberSight's own alerts take over.

**3. In AmberSight's `config.toml`:**

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
trigger = "camera"                # the camera's own motion starts recording
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

Privacy masks and burned-in text are not applied to these cameras: their recordings and live view
show the whole picture.

**What happens on motion:** the camera wakes and sends its message; AmberSight marks motion on the
timeline, connects (2–3 s), records the live picture, and lets the camera sleep again about 30 s
after the motion ends. The live recording starts a few seconds into the event, so about a minute and
a half later AmberSight fetches the camera's own recording of those first seconds from its SD card
and adds it in front. The Wake button and opening the camera's live view also wake it.

---

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

Nothing is installed until an admin clicks **Update** (and confirms).
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
`ambersight update --config config.toml` (add `--check` to only look).

**Going back**: stop the service, copy the files from the `previous` folder
over the program next to it, start the service.

**Installed with a .deb or .rpm package?** The updater refuses (the program
lives under `/usr`); update with `sudo apt upgrade` or `sudo dnf upgrade`.
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
| A camera stays offline | Its address, user name and password; that it is on the same network; that RTSP is turned on in the camera's own settings. Settings > System > Logs shows the reason. |
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
| `scripts/package-windows-installer.sh` | `setup.exe` (NSIS) from the folder `scripts/package.ps1` made on Windows, copied into `dist/` |
