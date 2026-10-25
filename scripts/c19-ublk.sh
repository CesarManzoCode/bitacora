#!/usr/bin/env bash
# C19: ext4 over the ublk frontend. A 256 MiB pseudorandom file keeps its
# SHA-256 across an unmount and a mount (contract section 12).
# Exit 0 with "sha256: identico"; 77 with "omitido: <razón>" when root,
# /dev/ublk-control or the permission on it is missing. Nothing is loaded or
# changed on the system before those checks pass.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=target/ublk/release/bitacora

skip() {
    echo "omitido: $1"
    exit 77
}

[ "$(id -u)" -eq 0 ] || skip "requiere root"
[ -e /dev/ublk-control ] || skip "no existe /dev/ublk-control (modulo ublk_drv sin cargar)"
{ [ -r /dev/ublk-control ] && [ -w /dev/ublk-control ]; } || skip "sin permiso de lectura y escritura sobre /dev/ublk-control"
if [ ! -x "$BIN" ]; then
    echo "falta $BIN: compila con make ublk" >&2
    exit 1
fi

DIR=target/criteria/c19
rm -rf "$DIR"
mkdir -p "$DIR"
VOL="$DIR/volume.img"
MNT="$(mktemp -d "$PWD/$DIR/mnt.XXXXXX")"
PID=""
MOUNTED=0

cleanup() {
    if [ "$MOUNTED" -eq 1 ]; then
        umount "$MNT" || true
    fi
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        kill -TERM "$PID" || true
        wait "$PID" || true
    fi
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

# Waits until the process $1 printed its device id in $2; prints the id.
wait_for_device() {
    local pid="$1" out="$2" id=""
    for _ in $(seq 1 300); do
        if [ -s "$out" ]; then
            id="$(head -n 1 "$out")"
            break
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "bitacora ublk termino antes de crear el dispositivo" >&2
            return 1
        fi
        sleep 0.1
    done
    [ -n "$id" ] || { echo "bitacora ublk no imprimio el id" >&2; return 1; }
    for _ in $(seq 1 300); do
        [ -b "/dev/ublkb$id" ] && { echo "$id"; return 0; }
        sleep 0.1
    done
    echo "no aparecio /dev/ublkb$id" >&2
    return 1
}

"$BIN" ublk --volume "$VOL" --create 1G >"$DIR/ublk.out" 2>"$DIR/ublk.err" &
PID=$!
DEV_ID="$(wait_for_device "$PID" "$DIR/ublk.out")"
BDEV="/dev/ublkb$DEV_ID"
echo "dispositivo: $BDEV"

mkfs.ext4 -q -F "$BDEV"
mount "$BDEV" "$MNT"
MOUNTED=1

head -c $((256 << 20)) /dev/urandom >"$MNT/data.bin"
sync "$MNT/data.bin"
BEFORE="$(sha256sum "$MNT/data.bin" | cut -d ' ' -f 1)"
echo "sha256_antes: $BEFORE"

umount "$MNT"
MOUNTED=0
# Drop the device's page cache so the second read comes from the engine.
blockdev --flushbufs "$BDEV"
mount "$BDEV" "$MNT"
MOUNTED=1
AFTER="$(sha256sum "$MNT/data.bin" | cut -d ' ' -f 1)"
echo "sha256_despues: $AFTER"

umount "$MNT"
MOUNTED=0
kill -TERM "$PID"
if ! wait "$PID"; then
    PID=""
    echo "cierre ordenado: fallo (ver $DIR/ublk.err)"
    exit 1
fi
PID=""
echo "cierre ordenado: ok"

if [ "$BEFORE" = "$AFTER" ]; then
    echo "sha256: identico"
else
    echo "sha256: distinto"
    exit 1
fi
