# AmberSight .rpm, built by scripts/package-linux-deb-rpm.sh from a staged
# file tree (--define "stage <folder>", "pkgversion <version>").
%global debug_package %{nil}
# The program is already stripped (and may be for another processor).
%global __strip /bin/true
%define _build_id_links none

Name:           ambersight
Version:        %{pkgversion}
Release:        1
Summary:        Self-hosted camera recorder (NVR) used from a web browser
License:        PolyForm-Noncommercial-1.0.0
URL:            https://github.com/HuskerMinion/AmberSight
Packager:       HuskerMinion <28571744+HuskerMinion@users.noreply.github.com>
Requires:       ffmpeg
Requires(pre):  shadow-utils

%description
AmberSight records IP cameras (RTSP, ONVIF, HTTP), USB cameras and capture
cards, detects motion and objects (built-in AI) and is used from a web
browser on port 8090. It runs as the system service "ambersight".

It needs ffmpeg. On Fedora add RPM Fusion first
(https://rpmfusion.org/Configuration), then install this package; Fedora's
own ffmpeg-free cannot play H.265 cameras.

%install
cp -a %{stage}/. %{buildroot}/

%pre
# The user comes from sysusers.d (newer rpm); this covers older ones.
getent group ambersight >/dev/null || groupadd -r ambersight
getent passwd ambersight >/dev/null || useradd -r -g ambersight -d /var/lib/ambersight \
  -s /sbin/nologin -c "AmberSight camera recorder" ambersight
exit 0

%post
# First install: the settings file from the template (never replaced later;
# AmberSight rewrites it when settings change in the web page).
if [ ! -e /etc/ambersight/config.toml ]; then
  cp /usr/share/ambersight/config.toml /etc/ambersight/config.toml
  chown ambersight:ambersight /etc/ambersight/config.toml
  chmod 0640 /etc/ambersight/config.toml
fi
# As the .deb does on every install and upgrade: the service user owns its
# settings and data folders, and other accounts cannot open them.
mkdir -p /var/lib/ambersight /etc/ambersight
chown ambersight:ambersight /var/lib/ambersight /etc/ambersight /etc/ambersight/config.toml
chmod 0750 /var/lib/ambersight /etc/ambersight
chmod 0640 /etc/ambersight/config.toml
# USB cameras and hardware video decoding need these groups.
for g in video render; do
  getent group "$g" >/dev/null && usermod -a -G "$g" ambersight
done
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  if [ "$1" -eq 1 ]; then
    systemctl enable --now ambersight.service || :
  else
    systemctl try-restart ambersight.service || :
  fi
elif command -v systemctl >/dev/null 2>&1 && [ "$1" -eq 1 ]; then
  # No running systemd (a container, an image being built): enable only.
  systemctl enable ambersight.service || :
fi
exit 0

%preun
if [ "$1" -eq 0 ] && command -v systemctl >/dev/null 2>&1; then
  if [ -d /run/systemd/system ]; then
    systemctl stop ambersight.service || :
  fi
  systemctl disable ambersight.service || :
fi
exit 0

%postun
# The recordings in /var/lib/ambersight and the service user are kept.
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload || :
fi
exit 0

%files
%attr(0755,root,root) /usr/bin/ambersight
%dir %attr(0750,ambersight,ambersight) /etc/ambersight
/usr/share/ambersight/config.toml
%dir %attr(0750,ambersight,ambersight) /var/lib/ambersight
/usr/lib/systemd/system/ambersight.service
/usr/lib/sysusers.d/ambersight.conf
/usr/share/applications/ambersight.desktop
/usr/share/icons/hicolor/512x512/apps/ambersight.png
/usr/share/icons/hicolor/scalable/apps/ambersight.svg
/usr/share/doc/ambersight
