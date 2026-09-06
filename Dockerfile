# syntax=docker/dockerfile:1
# browser-wayland with the default Xfce apps, the Xfce panel, Firefox and Chromium, and applications for
# what the desktop can do (guvcview for the webcam, Audacity, GIMP, mpv with VA-API decode, Ristretto,
# pavucontrol), with nano
# and a passwordless sudo for the `bw` user, on Arch Linux.
#
#   make docker-run          builds the image and runs it (the two commands below)
#   docker build -t browser-wayland .
#   docker run --rm --device /dev/dri --shm-size 1g -p 8443:8443 -p 8443:8443/udp -v bw-data:/home/bw/.config/browser-wayland browser-wayland
#
# The desktop starts empty: the viewer's own menu lists the installed applications and launches them,
# and its power menu shuts browser-wayland down. To have the Xfce panel as well, add
# `--exec xfce4-panel` after the image name. Without a usable GPU encoder, add `--software-encoding`
# (rendering still uses the GPU); without any GPU, drop `--device /dev/dri`: llvmpipe renders and the CPU
# encodes. For the browser's webcam, load v4l2loopback on the host
# and add `--device /dev/videoN --group-add $(stat -c %g /dev/videoN)` to docker run and `--webcam /dev/videoN`
# after the image name.
# Open the https://<host>:8443/#token=... URL that `docker logs` prints, with <host> being the Docker
# host's address (the log shows the container's own), and accept the self-signed certificate. The
# volume keeps the token and certificate across runs; without it every run prints a new token and
# old URLs get "wrong token". The page keeps the token in its sessionStorage (no cookies). Two URLs are
# printed: the control token and a view-only one; any number of viewers, one controls at a time.
# Arguments after the image name go to browser-wayland, e.g. `... browser-wayland --codec h264`.
# If /dev/dri/renderD128 isn't world-accessible on the host, add `--group-add $(stat -c %g /dev/dri/renderD128)`.
# Hardware encoding uses the host GPU through VA-API: Intel (iHD) and AMD (Mesa) drivers are included,
# as are Mesa's OpenGL and Vulkan drivers for both. To check them from the desktop: `glxinfo -B`,
# `vulkaninfo --summary`, and `glxgears` / `vkcube --wsi wayland` as spinning windows (spawn them
# from a terminal or the API).

# The viewer (React, built by Vite into web/dist); the binary embeds it.
FROM node:24-alpine AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ /src/web/
COPY LICENSE /src/LICENSE
RUN npm run build

FROM archlinux:latest AS build
RUN pacman -Sy --noconfirm archlinux-keyring \
    && pacman -Syu --noconfirm --needed rust pkgconf clang libpipewire gstreamer gst-plugins-base mesa libxkbcommon \
    && rm -rf /var/cache/pacman/pkg/*
WORKDIR /src
COPY . .
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --locked

FROM archlinux:latest
RUN pacman -Sy --noconfirm archlinux-keyring \
    && pacman -Syu --noconfirm --needed \
        gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-plugin-va \
        mesa vulkan-intel vulkan-radeon libva intel-media-driver libva-mesa-driver xorg-xwayland \
        mesa-utils mesa-demos vulkan-tools \
        dbus pipewire pipewire-pulse pipewire-alsa wireplumber libpulse gst-plugin-pipewire \
        xfce4 firefox chromium ttf-dejavu \
        guvcview audacity gimp mpv ristretto pavucontrol nano sudo \
    && rm -rf /var/cache/pacman/pkg/*
COPY --from=build /src/target/release/browser-wayland /usr/local/bin/
COPY --from=web /src/web/dist/THIRD_PARTY.txt /usr/share/licenses/browser-wayland/THIRD_PARTY.txt
# GTK hides menu icons unless told otherwise, and on Wayland it takes the title-bar buttons of
# client-decorated windows (GTK apps, Firefox, Chromium) from GSettings, where GNOME's default keeps
# only Close. On a real Xfce session xfsettingsd provides both.
RUN printf '[Settings]\ngtk-menu-images=1\n' > /etc/gtk-3.0/settings.ini \
    && printf "[org.gnome.desktop.wm.preferences]\nbutton-layout='menu:minimize,maximize,close'\n" > /usr/share/glib-2.0/schemas/50-browser-wayland.gschema.override \
    && glib-compile-schemas /usr/share/glib-2.0/schemas \
    && install -d /etc/mpv && printf 'hwdec=auto-safe\n' > /etc/mpv/mpv.conf
# Seed the default panel layout so a run with `--exec xfce4-panel` doesn't stop at the "first start" dialog.
# The data dir exists (bw-owned) so a `-v` named volume mounted there is writable from the first run.
# Chromium (its launcher reads ~/.config/chromium-flags.conf): Wayland when it can, its accessibility
# tree for --elements, and no sandbox, since containers usually lack the user namespaces it needs. The
# user has no password, so sudo works through a NOPASSWD rule. PipeWire's ALSA plugin is for Audacity,
# whose PortAudio speaks ALSA: it lands on the server's default devices, which browser-wayland makes its
# own. guvcview reads the webcam's V4L2 device itself (GNOME's Snapshot sees only PipeWire camera nodes,
# which the loopback never becomes here).
RUN useradd -m bw \
    && printf 'bw ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/bw && chmod 440 /etc/sudoers.d/bw && visudo -cf /etc/sudoers.d/bw \
    && install -D /etc/xdg/xfce4/panel/default.xml /home/bw/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml \
    && install -d /home/bw/.config/browser-wayland \
    && printf -- '--ozone-platform-hint=auto\n--force-renderer-accessibility\n--no-sandbox\n' > /home/bw/.config/chromium-flags.conf \
    && chown -R bw:bw /home/bw
# The menu uses the loopback selected by --webcam, including nonzero device numbers.
COPY --chmod=755 <<'EOF' /usr/local/bin/bw-guvcview
#!/bin/sh
if [ -n "$BW_WEBCAM_DEVICE" ]; then
    exec /usr/bin/guvcview --device="$BW_WEBCAM_DEVICE" "$@"
fi
exec /usr/bin/guvcview "$@"
EOF
RUN sed -i 's/^Exec=guvcview$/Exec=bw-guvcview/' /usr/share/applications/guvcview.desktop \
    && grep -q '^Exec=bw-guvcview$' /usr/share/applications/guvcview.desktop
# One session bus for xfconfd and the clients. The compositor owns its private audio services.
COPY --chmod=755 <<'EOF' /usr/local/bin/start
#!/bin/sh
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
DBUS_SESSION_BUS_ADDRESS=$(dbus-daemon --session --fork --print-address) || exit
export DBUS_SESSION_BUS_ADDRESS
exec browser-wayland --elements "$@"
EOF
# Programs launched from the desktop inherit the compositor's working directory: a terminal opens at home.
WORKDIR /home/bw
USER bw
ENV XDG_RUNTIME_DIR=/tmp/runtime-bw HOME=/home/bw
EXPOSE 8443
ENTRYPOINT ["start"]
