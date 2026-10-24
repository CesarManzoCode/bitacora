# Investigación de bitacora

Fuentes primarias con su revisión fijada. Una fuente web sin control de versiones lleva la fecha de lectura; una herramienta, la versión instalada o el tag y commit consultados. La investigación de origen (vacante, Railway, dominio, garantías, software y huecos) se resume aquí solo en lo que el diseño usa.

## Medición de la máquina (2026-10-24)

Comandos ejecutados en la máquina de César durante K0, como usuario normal (uid 1000).

| Qué | Resultado | Comando |
| --- | --- | --- |
| Kernel | 7.2.11-arch1-1 | `uname -r` |
| CPU y RAM | AMD Ryzen 5 5600G, 12 hilos; 14 GiB total, 12 GiB disponibles; 3 GiB de swap | `nproc`, `free -g`, `/proc/cpuinfo` |
| Disco del repositorio | ADATA SU630 (SATA SSD), btrfs, 380 GB libres | `lsblk`, `findmnt`, `df -h` |
| ublk | `CONFIG_BLK_DEV_UBLK=m`; `ublk_drv.ko.zst` existe; no está cargado; no existe `/dev/ublk-control` | `zcat /proc/config.gz`, `modinfo ublk_drv`, `lsmod`, `ls /dev/ublk-control` |
| root | `sudo -n true` → «a password is required» | `sudo -n true` |
| io_uring sin root | `io_uring_setup` (syscall 425) devolvió el descriptor 3; `kernel.io_uring_disabled = 0` | script Python con ctypes; `/proc/sys/kernel/io_uring_disabled` |
| 4 KiB + `fdatasync` | p50 1.051 ms, p99 1.445 ms, máx 1.582 ms (300 muestras) | script Python temporal, archivo de 256 MiB |
| Lectura aleatoria 4 KiB `O_DIRECT`, QD1 | 8,687 IOPS, p50 0.114 ms, p99 0.147 ms (20,000 muestras) | ídem |
| Escritura secuencial + `fdatasync` | 420 MiB/s | ídem |
| Herramientas | rustc 1.98.1, cargo 1.98.1, rustup 1.29.2, clippy 0.1.98, rustfmt 1.9.0, Python 3.14.8, GNU Make 4.4.1, git 2.56.0, e2fsprogs 1.47.4 (`mkfs.ext4`, `fsck.ext4`) | `--version` de cada una, `pacman -Q` |
| Faltan | `/usr/bin/time` (paquete `time` 1.10-1), `clang`/libclang (paquete `clang` 23.1.3-4), `fio` (paquete `fio` 3.42-1) | `which`, `pacman -Si` |
| libublk 0.4.8 | no compila aquí: su `libublk-rs-sys` usa bindgen 0.69.5 y falla con «Unable to find libclang» | compilación de prueba en un directorio temporal, ya borrado |

Conclusión permitida: estas cifras describen la plataforma, no el motor. Sirven para fijar umbrales con la cota del diseño (`thresholds.json`), no como resultados del producto.

## Dependencias y herramientas, con versión fijada

| Pieza | Versión fijada | Licencia | Fuente |
| --- | --- | --- | --- |
| Rust toolchain | 1.98.1 (48a229cea 2026-09-01) | MIT/Apache-2.0 | `rustc --version` |
| crc32c | 0.6.8 (tag v0.6.8, commit 254a861f7cc71bfe455e86f0e1d86e3c83c33390) | Apache-2.0/MIT | `cargo info crc32c`; https://github.com/zowens/crc32c |
| libc | 0.2.191 | MIT OR Apache-2.0 | `cargo info libc@0.2` |
| serde | 1.0.229 | MIT OR Apache-2.0 | `cargo info serde` |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | `cargo info serde_json` |
| sha2 | 0.11.1 (rust-version 1.88) | MIT OR Apache-2.0 | `cargo info sha2` |
| libublk | 0.4.8 (tag v0.4.8, commit 7ae2f0997d2aa00b58df7a628a013b4d19fbe2c1); rust-version 1.80; depende de smol, async-lock, futures-timer, io-uring 0.7.15 y bindgen 0.69.5 | MIT OR Apache-2.0 | `cargo info libublk`; https://docs.rs/libublk/0.4.8 ; https://github.com/ublk-org/libublk-rs |
| Cabecera uapi de ublk | `/usr/include/linux/ublk_cmd.h` de linux-api-headers 7.3-1: `UBLK_F_USER_RECOVERY` (1<<3), `UBLK_F_USER_RECOVERY_REISSUE` (1<<4), `UBLK_F_UNPRIVILEGED_DEV` (1<<5), `UBLK_ATTR_VOLATILE_CACHE` (1<<2), `UBLK_ATTR_FUA` (1<<3), `UBLK_PARAM_TYPE_BASIC`, `logical_bs_shift`, `dev_sectors`, `UBLK_IO_OP_READ/WRITE/FLUSH/DISCARD/WRITE_ZEROES` (0/1/2/3/5), `UBLK_IO_F_FUA` (1<<13) | GPL-2.0 WITH Linux-syscall-note | `grep` sobre la cabecera instalada; `pacman -Qo` |
| `fallocate` | `FALLOC_FL_KEEP_SIZE` 0x01, `FALLOC_FL_PUNCH_HOLE` 0x02 | — | `/usr/include/linux/falloc.h` |
| `O_DIRECT` | `0o40000` en x86_64 | — | `/usr/include/asm-generic/fcntl.h`; `python3 -c 'import os; print(oct(os.O_DIRECT))'` |
| GitHub Actions checkout | `actions/checkout@v7` (v7.0.1, publicado 2026-07-20; tag v7 → 3d3c42e5aac5ba805825da76410c181273ba90b1) | MIT | `gh api repos/actions/checkout/releases/latest` |
| Runner de CI | `ubuntu-24.04` trae rustup 1.29.1, Python 3.12.3 y make 4.3 (leído 2026-10-24) | — | https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md |
| GNU time | 1.10 (paquete Arch `time` 1.10-1); `-v` imprime informe detallado; `%M` = RSS máximo en KB | GPL-3.0 | https://man.archlinux.org/man/time.1 ; `pacman -Si time` |

## Kernel y garantías

- ublk: comandos de control (`UBLK_CMD_ADD_DEV`, `SET_PARAMS`, `START_DEV`, `STOP_DEV`, `DEL_DEV`, `START_USER_RECOVERY`, `END_USER_RECOVERY`), nodos `/dev/ublk-control`, `/dev/ublkc*`, `/dev/ublkb*`, comandos por E/S `UBLK_U_IO_FETCH_REQ` y `UBLK_U_IO_COMMIT_AND_FETCH_REQ`, semántica de `UBLK_F_USER_RECOVERY`, `_REISSUE` y `_FAIL_IO` (las dos últimas excluyentes) y de `UBLK_F_UNPRIVILEGED_DEV`. https://docs.kernel.org/block/ublk.html (leído 2026-10-24, mainline).
- Caché de escritura volátil, `REQ_PREFLUSH` y `REQ_FUA`. https://docs.kernel.org/block/writeback_cache_control.html (2026-10-24).
- dm-thin confirma metadata con FLUSH/FUA o cada segundo; snapshots recursivos. https://docs.kernel.org/admin-guide/device-mapper/thin-provisioning.html (2026-10-24).
- dm-flakey y dm-log-writes como inyección de fallas con root (no se usan). https://docs.kernel.org/admin-guide/device-mapper/dm-flakey.html ; https://docs.kernel.org/admin-guide/device-mapper/log-writes.html (2026-10-24).
- NVMe no garantiza atomicidad general (AWUPF/NAWUPF). https://lists.infradead.org/pipermail/linux-nvme/2025-July/057360.html
- fsyncgate: un error de `fsync` no se reintenta. https://danluu.com/fsyncgate/ ; https://wiki.postgresql.org/wiki/Fsync_Errors

## Literatura

- Rosenblum y Ousterhout, LFS: log, cleaner y política cost-benefit. https://people.eecs.berkeley.edu/~kubitron/courses/cs194-24-S14/hand-outs/LFS.pdf
- Desnoyers (SYSTOR 2012): amplificación de escritura greedy y efecto de separar datos calientes y fríos. https://www.ccs.neu.edu/home/pjd/papers/pjd-systor12.pdf
- Comparación cost-benefit frente a greedy. https://arxiv.org/pdf/1807.09313
- MiDAS (FAST 2024): agrupar por vida esperada. https://www.usenix.org/conference/fast24/presentation/oh
- LSVD (EuroSys 2022): disco virtual log-structured. https://infoscience.epfl.ch/record/301026 (doi 10.1145/3492321.3524271). No se copia código.
- Robustez de SSD ante cortes de energía (FAST 2013). https://www.usenix.org/conference/fast13/understanding-robustness-ssds-under-power-fault
- ALICE (OSDI 2014) y CrashMonkey/ACE (OSDI 2018): exploración de estados tras un fallo; el disco simulado aplica la idea a nivel de bloque. https://www.usenix.org/node/186195 ; https://arxiv.org/abs/1810.02904
- Simulación determinista con semilla: TigerBeetle VOPR y S2. https://docs.tigerbeetle.com/about/vopr ; https://s2.dev/blog/dst
- Migración de almacenamiento por espejo y conmutación en QEMU/LXD. https://canonical.com/lxd/docs/latest/reference/vm_live_migration_internals/ ; https://fedoraproject.org/wiki/Features/Virt_Storage_Migration

## Railway (solo lo publicado)

- La vacante y la tarea de entrevista. https://railway.com/careers/platform-engineer-storage (2026-10-24)
- Volúmenes: 3,000 IOPS de lectura y 3,000 de escritura. https://docs.railway.com/volumes/reference (2026-10-24)
- Backups incrementales y copy-on-write. https://docs.railway.com/volumes/backups (2026-10-24)
- No hay documento público del diseño interno del motor actual (99 de la investigación de origen).
