# bitacora

bitacora es un motor de almacenamiento de bloques log-structured, escrito desde cero en Rust. Sirve volúmenes de bloques de 4 KiB guardados en un archivo ordinario, con durabilidad explícita (FLUSH/FUA), checksums, recolección de basura medida, snapshots copy-on-write, backups incrementales en línea, migración en vivo y replicación entre procesos con épocas y fencing. Cuando el sistema lo permite, se expone como dispositivo de bloque real vía ublk.

Es la versión ejecutable y abierta de la tarea de entrevista «Design a Storage Engine to power something like Railway's Volumes» de la vacante Senior Infra Engineer: Storage de Railway. No describe ni imita el motor interno de Railway: la única cifra suya que usa es la publicada, 3,000 IOPS de lectura y 3,000 de escritura por volumen.

## Estado

| Fase | Contenido | Criterios | Estado |
| --- | --- | --- | --- |
| K0 | Fundación: contrato, investigación, plan | — | lista |
| K1 | Volumen durable: log, índice, recuperación, checksums, GC, disco simulado | C1–C10 | Cerrada |
| K2 | Snapshots, backups, servidor, migración en vivo, réplica, estado | C11–C18, C25 | Cerrada |
| K3 | ublk, rendimiento, capacidad, documento de requisitos, clon limpio | C19–C24, C26 | Implementada; falta la verificación de cierre |

## Comandos

- `make check`: formato, lints y pruebas (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`). Es lo que corre el CI.
- `make criteria`: compila en release y corre `bitacora-verify all`, que ejecuta c01–c22, c24, c25 y c26 en ese orden. Escribe `target/criteria/report.txt` con una línea `cNN: PASS|FAIL|OMITIDO <razón>` por criterio y termina con la lista de omitidos (vacía si no hay). Cada corredor deja su salida y sus artefactos en `target/criteria/<cNN>/`.
- `make ublk`: compila el binario con la feature `ublk` en `target/ublk/release/bitacora`; lo usan C19 y C20.

Requisitos: Rust 1.98.1 (lo fija `rust-toolchain.toml`). Para la feature `ublk` hace falta `clang`; para C19 y C20, root y el módulo `ublk_drv`. C23 mide `make criteria` con `/usr/bin/time -v` (paquete `time`).

### Desde un clon limpio

```sh
git clone <repositorio> bitacora && cd bitacora
make criteria
```

Es el único comando necesario sin root. C19 y C20 necesitan además el binario con ublk y root:

```sh
make ublk
sudo modprobe ublk_drv
sudo bash scripts/c19-ublk.sh
sudo bash scripts/c20-ublk-recovery.sh
```

## Documentos

- [vault/CONTRACT.md](vault/CONTRACT.md): qué hace el sistema y las decisiones de diseño.
- [vault/RESEARCH.md](vault/RESEARCH.md): fuentes y mediciones de la plataforma.
- [docs/engineering-requirements.md](docs/engineering-requirements.md): requisitos, garantías, modos de falla, plan de capacidad y qué cambia con 100 veces más tamaño.
- [LIMITATIONS.md](LIMITATIONS.md): lo que el proyecto no demuestra.
- [CHANGELOG.md](CHANGELOG.md): cambios por fase.

## Resultados

Toda cifra de esta sección sale de un archivo de `results/` con la máquina, el comando, la fecha y el commit que la produjeron. Los dos archivos los escriben los corredores al correr `make criteria` (o `target/release/bitacora-verify c06` y `c21` por separado) y se versionan con el commit de cierre de cada fase.

- [`results/c06.json`](results/c06.json): amplificación de escritura del GC y p99 por operación (C6).
- [`results/c21.json`](results/c21.json): IOPS de lectura y de escritura, p50, p99 y p999, p99 durante el GC y si alguna cifra queda bajo la referencia pública de 3,000 IOPS (C21).

C1–C10 pasaron en el commit `5e91d04`, y C11–C18 y C25 en `457fd5c`, sin omitidos. Las cifras de C4, C12, C13, C15 y C25 no tienen archivo en `results/`, así que no se publican aquí.

### C6: amplificación de escritura del GC

Fuente: [`results/c06.json`](results/c06.json). Máquina: AMD Ryzen 5 5600G, kernel 7.2.11-arch1-1. Comando: `target/release/bitacora-verify c06`. Fecha: 2026-10-24T22:15:52Z. Commit: `457fd5c` con cambios sin commit (`-dirty` en el archivo). Volumen de 1 GiB lleno, luego 3 × volumen de escrituras aleatorias de 4 KiB con `flush` cada 256.

| Carga | Amplificación de escritura | Umbral | p99 por operación (ms) |
| --- | --- | --- | --- |
| `caliente_fria` (20 % de los bloques recibe 80 %) | 2.4544 | 2.5 | 0.0492 |
| `uniforme` | 2.9185 | 3.5 | 0.0735 |

Con el disparo de GC anterior al ADR 0001, la misma medición dio 3.9964 y 4.8663 y C6 fallaba ([`results/c06-aa68e3d.json`](results/c06-aa68e3d.json)).

### C21: IOPS y latencia

Fuente: [`results/c21.json`](results/c21.json). Máquina: AMD Ryzen 5 5600G, kernel 7.2.11-arch1-1. Comando: `target/release/bitacora-verify c21`. Fecha: 2026-10-24T22:57:41Z. Commit: `88a5e82`. Volumen de 4 GiB sobre `FileMedium` con `O_DIRECT`, 8 hilos, fases de 30 s de lecturas y de escrituras aleatorias de 4 KiB, con `flush` cada 32 escrituras por hilo.

| Cifra | Valor |
| --- | --- |
| IOPS de lectura | 28816.9875 |
| IOPS de escritura | 11723.7302 |
| p50 / p99 / p999 (ms, lectura y escritura juntas) | 0.2441 / 3.8341 / 23.9722 |
| p99 durante el GC (ms) | 22.5526 |
| Bajo la referencia de 3,000 IOPS | no |

El disparo del GC se alcanzó antes de la fase con GC (`disparo_gc_alcanzado: true`). La referencia de 3,000 IOPS es la cifra pública de Railway: no es el mismo contrato ni la misma máquina.

## Omitidos

C19 (`scripts/c19-ublk.sh`, ext4 sobre el dispositivo ublk) y C20 (`scripts/c20-ublk-recovery.sh`, recuperación tras matar el proceso ublk) salen con código 77 y `omitido: <razón>` cuando el proceso no corre como root, cuando no existe `/dev/ublk-control` (módulo `ublk_drv` sin cargar) o cuando no hay permiso de lectura y escritura sobre él. `make criteria` como usuario normal los marca `OMITIDO` en `report.txt` y los lista al final. Un omitido no cuenta como aprobado: sin esas dos corridas, el frontend ublk no queda demostrado.

## Licencia

MIT. Ver [LICENSE](LICENSE).
