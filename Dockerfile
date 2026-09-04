# syntax=docker/dockerfile:1
# browser-wayland with the Xfce panel, the default Xfce apps and Firefox, on Arch Linux.
#
#   docker build -t browser-wayland .
#   docker run --rm --device /dev/dri --shm-size 1g -p 8443:8443 -v bw-data:/home/bw/.config/browser-wayland browser-wayland
#
# Open the https://<host>:8443/?token=... URL that `docker logs` prints, with <host> being the Docker
# host's address (the log shows the container's own), and accept the self-signed certificate. The
# volume keeps the token and certificate across runs; without it every run prints a new token and
# old URLs get "wrong token". The token stays in the page URL (no cookies). One viewer at a time.
# Arguments after the image name go to browser-wayland, e.g. `... browser-wayland --codec h264`.
# If /dev/dri/renderD128 isn't world-accessible on the host, add `--group-add $(stat -c %g /dev/dri/renderD128)`.
# Hardware encoding uses the host GPU through VA-API: Intel (iHD) and AMD (Mesa) drivers are included.

FROM archlinux:latest AS build
RUN pacman -Sy --noconfirm archlinux-keyring \
    && pacman -Syu --noconfirm --needed rust pkgconf gstreamer gst-plugins-base mesa libxkbcommon \
    && rm -rf /var/cache/pacman/pkg/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM archlinux:latest
RUN pacman -Sy --noconfirm archlinux-keyring \
    && pacman -Syu --noconfirm --needed \
        gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugin-va \
        mesa libva intel-media-driver libva-mesa-driver xorg-xwayland \
        dbus pipewire pipewire-pulse wireplumber libpulse \
        xfce4 firefox ttf-dejavu \
    && rm -rf /var/cache/pacman/pkg/*
COPY --from=build /src/target/release/browser-wayland /usr/local/bin/
# GTK hides menu icons unless told otherwise; on a real Xfce session xfsettingsd does this.
RUN printf '[Settings]\ngtk-menu-images=1\n' > /etc/gtk-3.0/settings.ini
# Seed the default panel layout so the first run doesn't stop at the "first start" dialog.
# The data dir exists (bw-owned) so a `-v` named volume mounted there is writable from the first run.
RUN useradd -m bw \
    && install -D /etc/xdg/xfce4/panel/default.xml /home/bw/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml \
    && install -d /home/bw/.config/browser-wayland \
    && chown -R bw:bw /home/bw
# One session bus for xfconfd, PipeWire and the clients. PipeWire provides the null sink the
# compositor captures for browser audio.
COPY --chmod=755 <<'EOF' /usr/local/bin/start
#!/bin/sh
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
exec dbus-run-session -- sh -c '
    pipewire & pipewire-pulse & wireplumber &
    until pactl info >/dev/null 2>&1; do sleep 0.2; done
    exec browser-wayland --elements --exec "xfce4-panel & exec firefox" "$@"' sh "$@"
EOF
USER bw
ENV XDG_RUNTIME_DIR=/tmp/runtime-bw HOME=/home/bw
EXPOSE 8443
ENTRYPOINT ["start"]
