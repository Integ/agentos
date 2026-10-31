#!/usr/bin/env bash
# End-to-end smoke test of an AgentOS image in QEMU (UEFI, Secure Boot on
# when the firmware supports it):
#
#   1. boot the image as if it were a USB stick, with a blank second disk;
#      the boot self-check must pass, then the VM installs AgentOS onto the
#      blank disk unattended (fw_cfg hook in agentos-selfcheck) and powers off
#   2. boot from the installed disk alone; the self-check must pass again,
#      and a screenshot of the graphical session is saved
#
# Usage: image/test/qemu-smoke.sh out/agentos-debian-x86_64.img [workdir]
set -euo pipefail

IMAGE="${1:?usage: $0 IMAGE [WORKDIR]}"
WORK="${2:-$(dirname "$IMAGE")/smoke}"
TIMEOUT="${TIMEOUT:-1500}"   # seconds per boot; TCG without KVM is slow
SECURE_BOOT="${SECURE_BOOT:-1}"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

rm -rf "$WORK"; mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"

fw_dir=/usr/share/OVMF
if [[ $SECURE_BOOT == 1 && -f $fw_dir/OVMF_CODE_4M.secboot.fd && -f $fw_dir/OVMF_VARS_4M.ms.fd ]]; then
  CODE=$fw_dir/OVMF_CODE_4M.secboot.fd VARS=$fw_dir/OVMF_VARS_4M.ms.fd
  MACHINE="q35,smm=on"
  SB_ARGS=(-global driver=cfi.pflash01,property=secure,value=on)
  log "固件：OVMF，安全启动开启（微软密钥）"
else
  CODE=$fw_dir/OVMF_CODE_4M.fd VARS=$fw_dir/OVMF_VARS_4M.fd
  MACHINE=q35 SB_ARGS=()
  log "固件：OVMF，安全启动关闭"
fi
[[ -f $CODE ]] || die "找不到 OVMF 固件（apt install ovmf）"
cp "$VARS" "$WORK/vars.fd"

if [[ -w /dev/kvm ]]; then ACCEL=(-accel kvm -cpu host); else ACCEL=(-accel tcg -cpu max); fi

# The "USB stick": a copy-on-write overlay that is larger than the image, so
# first-boot partition growing is exercised too.
qemu-img create -q -f qcow2 -F raw -b "$(realpath "$IMAGE")" "$WORK/usb.qcow2" 12G
qemu-img create -q -f qcow2 "$WORK/disk.qcow2" 16G

run_vm() { # name, extra qemu args...
  local name=$1; shift
  qemu-system-x86_64 -machine "$MACHINE" "${ACCEL[@]}" -smp 4 -m 3072 \
    "${SB_ARGS[@]}" \
    -drive if=pflash,format=raw,unit=0,readonly=on,file="$CODE" \
    -drive if=pflash,format=raw,unit=1,file="$WORK/vars.fd" \
    -device virtio-vga -display none \
    -serial file:"$WORK/serial-$name.log" \
    -monitor unix:"$WORK/monitor-$name.sock",server,nowait \
    -nic user,model=virtio-net-pci \
    "$@" &
  QEMU_PID=$!
}

wait_for() { # log, pattern, timeout
  local deadline=$((SECONDS + $3))
  while ((SECONDS < deadline)); do
    grep -aq "$2" "$1" 2>/dev/null && return 0
    kill -0 "$QEMU_PID" 2>/dev/null || { grep -aq "$2" "$1" 2>/dev/null; return; }
    sleep 5
  done
  return 1
}

monitor() { # name, command
  python3 - "$WORK/monitor-$1.sock" "$2" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
time.sleep(0.5); s.recv(65536)
s.sendall((sys.argv[2] + "\n").encode()); time.sleep(2); s.close()
PY
}

ppm_to_png() { # in, out
  python3 - "$1" "$2" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], "rb").read()
parts = data.split(maxsplit=4)
w, h = int(parts[1]), int(parts[2])
px = parts[4]
raw = b"".join(b"\0" + px[y*w*3:(y+1)*w*3] for y in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) \
    + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
open(sys.argv[2], "wb").write(png)
PY
}

fail() {
  kill "$QEMU_PID" 2>/dev/null || true
  echo "---- serial log ($1) ----"; tail -n 80 "$WORK/serial-$1.log" || true
  die "$2"
}

log "第 1 步：从「U 盘」启动，自检后自动安装到第二块盘"
run_vm usb \
  -drive if=virtio,format=qcow2,file="$WORK/usb.qcow2" \
  -drive if=virtio,format=qcow2,file="$WORK/disk.qcow2" \
  -fw_cfg name=opt/agentos/autoinstall,string=/dev/vdb
wait_for "$WORK/serial-usb.log" 'AGENTOS-SELFCHECK: OK' "$TIMEOUT" || fail usb "U 盘启动自检失败"
log "U 盘启动自检通过：$(grep -ao 'AGENTOS-SELFCHECK: [^)]*)' "$WORK/serial-usb.log" | tail -1)"
wait_for "$WORK/serial-usb.log" 'AGENTOS-NETWORK: OK' 120 || fail usb "网络没有自动连上（NetworkManager + DNS）"
log "有线网络自动连接、DNS 正常"
wait_for "$WORK/serial-usb.log" 'AGENTOS-AUTOINSTALL: OK' "$TIMEOUT" || fail usb "自动安装失败"
log "安装完成，等待关机"
for _ in $(seq 120); do kill -0 "$QEMU_PID" 2>/dev/null || break; sleep 1; done
kill "$QEMU_PID" 2>/dev/null || true; wait "$QEMU_PID" 2>/dev/null || true

log "第 2 步：只从安装好的硬盘启动"
run_vm disk -drive if=virtio,format=qcow2,file="$WORK/disk.qcow2"
wait_for "$WORK/serial-disk.log" 'AGENTOS-SELFCHECK: OK' "$TIMEOUT" || fail disk "硬盘启动自检失败"
log "硬盘启动自检通过"
sleep "${SCREENSHOT_DELAY:-60}"   # let sway + foot + the setup wizard come up
# Wiggle the pointer so the compositor repaints the whole output first.
for xy in "40 40" "-40 -40" "40 40"; do monitor disk "mouse_move $xy"; done
sleep 5
monitor disk "screendump $WORK/screen.ppm"
[[ -s $WORK/screen.ppm ]] && ppm_to_png "$WORK/screen.ppm" "$WORK/screen.png" && rm -f "$WORK/screen.ppm" \
  && log "截图：$WORK/screen.png"
monitor disk quit || true
wait "$QEMU_PID" 2>/dev/null || true

log "冒烟测试通过"
