#!/usr/bin/env bash
# Build a bootable AgentOS disk image for x86-64 UEFI PCs.
#
# The result is a raw GPT disk image (ESP + ext4 root) that can be written to
# a USB stick with balenaEtcher / Rufus / dd. It boots through the
# distribution's signed shim + grub, so it works with Secure Boot enabled.
# From the USB stick the user can try AgentOS and then install it to the
# internal disk with `agentos-install`.
#
# Usage (as root, on a Debian/Ubuntu host):
#   sudo image/build.sh                       # Debian 13 (trixie)
#   sudo DISTRO=ubuntu image/build.sh         # Ubuntu 24.04 (noble)
#
# Environment:
#   DISTRO        debian | ubuntu                       (default: debian)
#   MIRROR        package mirror URL                    (default: distro's)
#   OUT           output directory                      (default: out/)
#   AGENTOSD_BIN  prebuilt agentosd binary (skips cargo build)
#   EXTRA_MB      free space left in the root fs, in MiB (default: 1024)
#   COMPRESS      1 to also write <image>.xz            (default: 0)
#
# Host requirements: debootstrap, sfdisk (fdisk), mkfs.vfat (dosfstools),
# mtools, mkfs.ext4 (e2fsprogs >= 1.47), cargo (unless AGENTOSD_BIN is set).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

DISTRO="${DISTRO:-debian}"
OUT="${OUT:-$REPO/out}"
EXTRA_MB="${EXTRA_MB:-1024}"
COMPRESS="${COMPRESS:-0}"
ESP_MB=256

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "需要 root 权限运行（debootstrap 和 chroot 需要）"
for t in debootstrap sfdisk mkfs.vfat mcopy mkfs.ext4; do
  command -v "$t" >/dev/null || die "缺少工具：$t"
done

case "$DISTRO" in
  debian)
    SUITE=trixie
    MIRROR="${MIRROR:-http://deb.debian.org/debian}"
    COMPONENTS=main,contrib,non-free,non-free-firmware
    KERNEL_PKGS="linux-image-amd64 firmware-linux firmware-iwlwifi firmware-realtek firmware-atheros"
    ;;
  ubuntu)
    SUITE=noble
    MIRROR="${MIRROR:-http://archive.ubuntu.com/ubuntu}"
    COMPONENTS=main,restricted,universe
    KERNEL_PKGS="linux-image-generic"
    ;;
  *) die "不支持的 DISTRO=$DISTRO（可选 debian、ubuntu）" ;;
esac

# Everything a desktop AgentOS session needs. Kept explicit and
# --no-install-recommends so the image stays small and predictable.
PACKAGES=(
  systemd-sysv udev dbus kmod sudo locales tzdata ca-certificates
  initramfs-tools zstd e2fsprogs dosfstools fdisk efibootmgr rsync whiptail
  network-manager systemd-resolved wpasupplicant iproute2 iputils-ping curl less nano
  # Graphical session: sway in kiosk mode + foot terminal, CJK fonts, pinyin IME.
  sway foot fonts-noto-cjk fcitx5 fcitx5-chinese-addons
  libgl1-mesa-dri libpam-systemd dbus-user-session
)

IMAGE="$OUT/agentos-$DISTRO-x86_64.img"
WORK="$OUT/work-$DISTRO"
ROOT="$WORK/rootfs"

cleanup() {
  for m in dev/pts dev proc sys run; do
    mountpoint -q "$ROOT/$m" 2>/dev/null && umount -l "$ROOT/$m" || true
  done
}
trap cleanup EXIT

# --- 1. agentosd -------------------------------------------------------------
if [[ -n "${AGENTOSD_BIN:-}" ]]; then
  BIN="$AGENTOSD_BIN"
else
  log "编译 agentosd（release）"
  (cd "$REPO" && cargo build --release -p agentosd)
  BIN="$REPO/target/release/agentosd"
fi
[[ -x "$BIN" ]] || die "找不到 agentosd 可执行文件：$BIN"

# --- 2. base system ----------------------------------------------------------
rm -rf "$WORK"
mkdir -p "$ROOT" "$OUT"
log "debootstrap $DISTRO $SUITE ← $MIRROR"
debootstrap --variant=minbase --components="$COMPONENTS" "$SUITE" "$ROOT" "$MIRROR"

mount -t proc proc "$ROOT/proc"
mount -t sysfs sys "$ROOT/sys"
mount --bind /dev "$ROOT/dev"
mount --bind /dev/pts "$ROOT/dev/pts"
mount -t tmpfs tmpfs "$ROOT/run"

in_root() { chroot "$ROOT" /usr/bin/env -i \
  PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root LANG=C.UTF-8 \
  DEBIAN_FRONTEND=noninteractive "$@"; }

# Never start services inside the build chroot.
printf '#!/bin/sh\nexit 101\n' > "$ROOT/usr/sbin/policy-rc.d"
chmod +x "$ROOT/usr/sbin/policy-rc.d"

if [[ $DISTRO == ubuntu ]]; then
  cat > "$ROOT/etc/apt/sources.list" <<EOF
deb $MIRROR $SUITE ${COMPONENTS//,/ }
deb $MIRROR $SUITE-updates ${COMPONENTS//,/ }
deb $MIRROR $SUITE-security ${COMPONENTS//,/ }
EOF
else
  cat > "$ROOT/etc/apt/sources.list" <<EOF
deb $MIRROR $SUITE ${COMPONENTS//,/ }
deb $MIRROR $SUITE-updates ${COMPONENTS//,/ }
deb http://security.debian.org/debian-security $SUITE-security ${COMPONENTS//,/ }
EOF
fi

# Kernel symlinks at /boot/vmlinuz and /boot/initrd.img: grub.cfg boots those,
# so kernel upgrades through apt keep working without touching the ESP.
cat > "$ROOT/etc/kernel-img.conf" <<EOF
do_symlinks = yes
link_in_boot = yes
EOF

log "安装软件包"
in_root apt-get update
# shellcheck disable=SC2086
in_root apt-get install -y --no-install-recommends "${PACKAGES[@]}" $KERNEL_PKGS

[[ -e $ROOT/boot/vmlinuz && -e $ROOT/boot/initrd.img ]] || {
  # Fall back to linking the newest kernel ourselves.
  k=$(cd "$ROOT/boot" && ls vmlinuz-* | sort -V | tail -1)
  ln -sf "$k" "$ROOT/boot/vmlinuz"
  ln -sf "initrd.img-${k#vmlinuz-}" "$ROOT/boot/initrd.img"
}

# --- 3. signed boot chain (shim → grub), extracted rather than installed so
#        no grub-install runs inside the chroot. ------------------------------
log "取出签名的 shim 和 grub"
BOOTPKG="$WORK/bootpkgs"
mkdir -p "$BOOTPKG"
SHIM_PKGS="shim-signed grub-efi-amd64-signed"
[[ $DISTRO == debian ]] && SHIM_PKGS="$SHIM_PKGS shim-helpers-amd64-signed"
# shellcheck disable=SC2086
in_root sh -c "cd /tmp && apt-get download $SHIM_PKGS"
for deb in "$ROOT"/tmp/*.deb; do dpkg-deb -x "$deb" "$BOOTPKG"; done
rm -f "$ROOT"/tmp/*.deb

pick() { find "$BOOTPKG" -name "$1" -type f | sort | head -1; }
SHIM=$(pick 'shimx64.efi.signed.latest'); [[ -n $SHIM ]] || SHIM=$(pick 'shimx64.efi.signed')
MM=$(pick 'mmx64.efi.signed'); [[ -n $MM ]] || MM=$(pick 'mmx64.efi')
GRUB=$(pick 'grubx64.efi.signed')
[[ -n $SHIM && -n $GRUB ]] || die "没找到签名的 shim/grub"

# The installer re-creates the ESP from this copy.
EFI_SRC="$ROOT/usr/share/agentos/efi"
mkdir -p "$EFI_SRC/EFI/BOOT"
cp "$SHIM" "$EFI_SRC/EFI/BOOT/BOOTX64.EFI"
cp "$GRUB" "$EFI_SRC/EFI/BOOT/grubx64.efi"
[[ -n $MM ]] && cp "$MM" "$EFI_SRC/EFI/BOOT/mmx64.efi"

# --- 4. AgentOS layer --------------------------------------------------------
log "安装 AgentOS 用户态"
cp -a "$HERE/rootfs/." "$ROOT/"
install -m 0755 "$BIN" "$ROOT/usr/local/bin/agentosd"
chmod 0755 "$ROOT"/usr/local/bin/agentos-*
chmod 0440 "$ROOT/etc/sudoers.d/agentos"
echo "$DISTRO" > "$ROOT/usr/share/agentos/distro"
git -C "$REPO" rev-parse --short HEAD > "$ROOT/usr/share/agentos/version" 2>/dev/null \
  || echo unknown > "$ROOT/usr/share/agentos/version"

echo agentos > "$ROOT/etc/hostname"
cat > "$ROOT/etc/hosts" <<EOF
127.0.0.1 localhost
127.0.1.1 agentos
::1       localhost ip6-localhost ip6-loopback
EOF

sed -i 's/^# *\(zh_CN.UTF-8\)/\1/; s/^# *\(en_US.UTF-8\)/\1/' "$ROOT/etc/locale.gen"
in_root locale-gen
echo 'LANG=zh_CN.UTF-8' > "$ROOT/etc/default/locale"
ln -sf /usr/share/zoneinfo/Asia/Shanghai "$ROOT/etc/localtime"
echo Asia/Shanghai > "$ROOT/etc/timezone"

in_root useradd -m -s /bin/bash -c "AgentOS" agent
for g in sudo video input render audio netdev; do
  if in_root getent group "$g" >/dev/null; then in_root usermod -aG "$g" agent; fi
done
in_root passwd -l agent >/dev/null
in_root passwd -l root >/dev/null
install -d -m 0750 -o root -g "$(in_root id -g agent)" "$ROOT/etc/agentos"
install -d -m 0755 "$ROOT/var/lib/agentos"
in_root chown agent:agent /var/lib/agentos

in_root systemctl enable NetworkManager.service agentos-growroot.service agentos-selfcheck.service
in_root systemctl set-default multi-user.target

# --- 5. disk image -----------------------------------------------------------
ROOT_UUID=$(cat /proc/sys/kernel/random/uuid)
ESP_SERIAL=$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')

cat > "$ROOT/etc/fstab" <<EOF
UUID=$ROOT_UUID  /  ext4  errors=remount-ro  0 1
EOF
mkdir -p "$EFI_SRC/EFI/$DISTRO"
sed "s/@ROOT_UUID@/$ROOT_UUID/g" "$ROOT/usr/share/agentos/grub.cfg.in" > "$EFI_SRC/EFI/BOOT/grub.cfg"
cp "$EFI_SRC/EFI/BOOT/grub.cfg" "$EFI_SRC/EFI/$DISTRO/grub.cfg"

log "清理"
rm -f "$ROOT/usr/sbin/policy-rc.d"
in_root apt-get clean
rm -rf "$ROOT"/var/lib/apt/lists/* "$ROOT"/var/cache/apt/*.bin "$ROOT"/tmp/* "$ROOT"/var/log/*.log
: > "$ROOT/etc/machine-id"
rm -f "$ROOT/var/lib/dbus/machine-id"
cleanup

log "生成磁盘镜像"
used_mb=$(du -sxm "$ROOT" | cut -f1)
root_mb=$(( used_mb * 115 / 100 + EXTRA_MB ))
mkfs.ext4 -q -F -L agentos-root -U "$ROOT_UUID" -E root_owner=0:0 -d "$ROOT" "$WORK/root.img" "${root_mb}M"

rm -f "$WORK/esp.img"
mkfs.vfat -F 32 -n AGENTOS -i "$ESP_SERIAL" -C "$WORK/esp.img" $(( ESP_MB * 1024 )) >/dev/null
MTOOLS_SKIP_CHECK=1 mcopy -s -i "$WORK/esp.img" "$EFI_SRC/EFI" ::

total_mb=$(( 1 + ESP_MB + root_mb + 1 ))
rm -f "$IMAGE"
truncate -s "${total_mb}M" "$IMAGE"
sfdisk -q "$IMAGE" <<EOF
label: gpt
start=1MiB, size=${ESP_MB}MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="AgentOS ESP"
start=$(( 1 + ESP_MB ))MiB, size=${root_mb}MiB, type=4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709, name="agentos-root"
EOF
dd if="$WORK/esp.img" of="$IMAGE" bs=1M seek=1 conv=notrunc,sparse status=none
dd if="$WORK/root.img" of="$IMAGE" bs=1M seek=$(( 1 + ESP_MB )) conv=notrunc,sparse status=none
rm -f "$WORK/esp.img" "$WORK/root.img"

(cd "$OUT" && sha256sum "$(basename "$IMAGE")" > "$(basename "$IMAGE").sha256")
if [[ $COMPRESS == 1 ]]; then
  log "压缩"
  xz -T0 -6 -k -f "$IMAGE"
  (cd "$OUT" && sha256sum "$(basename "$IMAGE").xz" > "$(basename "$IMAGE").xz.sha256")
fi

log "完成：$IMAGE（$(du -h --apparent-size "$IMAGE" | cut -f1)）"
