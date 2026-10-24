#!/bin/sh
# Build and install periph for the current user. Only the udev rule needs
# sudo; everything else goes under ~/.local and ~/.config.
set -eu
cd "$(dirname "$0")"

cargo build --release

install -Dm755 target/release/periphd "$HOME/.local/bin/periphd"
install -Dm755 target/release/periph "$HOME/.local/bin/periph"
install -Dm644 dist/periphd.service "$HOME/.config/systemd/user/periphd.service"
install -Dm644 dist/io.github.periph.desktop "$HOME/.local/share/applications/io.github.periph.desktop"
# Start in the tray on login.
sed 's/^Exec=periph$/Exec=periph --hidden/' dist/io.github.periph.desktop \
    | install -Dm644 /dev/stdin "$HOME/.config/autostart/io.github.periph.desktop"

echo "Installing udev rule (needs sudo)..."
sudo install -Dm644 dist/70-periph.rules /etc/udev/rules.d/70-periph.rules
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=hidraw --subsystem-match=misc

systemctl --user daemon-reload
systemctl --user enable --now periphd.service
systemctl --user restart periphd.service
echo "Done. Launch 'periph' from the menu, or run: periph"
