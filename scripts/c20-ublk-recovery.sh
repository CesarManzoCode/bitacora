#!/usr/bin/env bash
# C20: recovery of the ublk frontend after SIGKILL (contract section 12).
# Files written with fsync keep their SHA-256 and ext4 stays clean after the
# engine is killed during a write and `bitacora ublk --recover` takes over
# the device. Exit 0 with "fsck: limpio" and "sha256: identico"; 77 with
# "omitido: <razón>" when root, /dev/ublk-control or the permission on it is
# missing. Nothing is loaded or changed on the system before those checks.
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

DIR=target/criteria/c20
rm -rf "$DIR"
mkdir -p "$DIR"
VOL="$DIR/volume.img"
HASHES_FILE="$PWD/$DIR/hashes.sha256"
MNT="$(mktemp -d "$PWD/$DIR/mnt.XXXXXX")"
PID=""
WRITER=""
MOUNTED=0

cleanup() {
    if [ -n "$WRITER" ] && kill -0 "$WRITER" 2>/dev/null; then
        kill -KILL "$WRITER" || true
        wait "$WRITER" || true
    fi
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
    for _ in $(seq 1 600); do
        if [ -s "$out" ]; then
            id="$(head -n 1 "$out")"
            break
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "bitacora ublk termino antes de servir el dispositivo" >&2
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

# Files made durable with fsync (data, inode and directory entry).
for i in 1 2 3 4; do
    dd if=/dev/urandom of="$MNT/f$i.bin" bs=1M count=32 conv=fsync status=none
done
sync "$MNT"
(cd "$MNT" && sha256sum f1.bin f2.bin f3.bin f4.bin) >"$HASHES_FILE"
echo "archivos_con_fsync: 4"

# A write in flight at the device (O_DIRECT) when the engine dies.
dd if=/dev/urandom of="$MNT/en-curso.bin" bs=1M count=256 oflag=direct status=none &
WRITER=$!
sleep 1
kill -KILL "$PID"
wait "$PID" || true
PID=""
echo "sigkill: enviado"

"$BIN" ublk --recover "$DEV_ID" --volume "$VOL" >"$DIR/recover.out" 2>"$DIR/recover.err" &
PID=$!
RECOVERED="$(wait_for_device "$PID" "$DIR/recover.out")"
if [ "$RECOVERED" != "$DEV_ID" ]; then
    echo "recuperacion: id $RECOVERED distinto de $DEV_ID"
    exit 1
fi
echo "recuperacion: $BDEV"

WRITER_RC=0
wait "$WRITER" || WRITER_RC=$?
WRITER=""
echo "escritura_en_curso: codigo $WRITER_RC"

umount "$MNT"
MOUNTED=0
blockdev --flushbufs "$BDEV"
if fsck.ext4 -fn "$BDEV" >"$DIR/fsck.log" 2>&1; then
    echo "fsck: limpio"
else
    echo "fsck: errores (ver $DIR/fsck.log)"
    exit 1
fi

mount "$BDEV" "$MNT"
MOUNTED=1
if (cd "$MNT" && sha256sum --quiet -c "$HASHES_FILE") >"$DIR/sha256-check.log" 2>&1; then
    HASHES=identico
else
    HASHES=distinto
fi

umount "$MNT"
MOUNTED=0
kill -TERM "$PID"
if ! wait "$PID"; then
    PID=""
    echo "cierre ordenado: fallo (ver $DIR/recover.err)"
    exit 1
fi
PID=""
echo "cierre ordenado: ok"

echo "sha256: $HASHES"
[ "$HASHES" = identico ]
