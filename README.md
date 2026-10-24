# bitacora

bitacora es un motor de almacenamiento de bloques log-structured, escrito desde cero en Rust. Sirve volúmenes de bloques de 4 KiB guardados en un archivo ordinario, con durabilidad explícita (FLUSH/FUA), checksums, recolección de basura medida, snapshots copy-on-write, backups incrementales en línea, migración en vivo y replicación entre procesos con épocas y fencing. Cuando el sistema lo permite, se expone como dispositivo de bloque real vía ublk.

Es la versión ejecutable y abierta de la tarea de entrevista «Design a Storage Engine to power something like Railway's Volumes» de la vacante Senior Infra Engineer: Storage de Railway. No describe ni imita el motor interno de Railway: la única cifra suya que usa es la publicada, 3,000 IOPS de lectura y 3,000 de escritura por volumen.

## Estado

| Fase | Contenido | Criterios | Estado |
| --- | --- | --- | --- |
| K0 | Fundación: contrato, investigación, plan | — | lista |
| K1 | Volumen durable: log, índice, recuperación, checksums, GC, disco simulado | C1–C10 | Cerrada |
| K2 | Snapshots, backups, servidor, migración en vivo, réplica, estado | C11–C18, C25 | pendiente |
| K3 | ublk, rendimiento, capacidad, documento de requisitos, clon limpio | C19–C24, C26 | pendiente |

## Comandos

- `make check`: formato, lints y pruebas (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`). Es lo que corre el CI.
- `make criteria`: compila en release y corre todos los criterios posibles sin root; escribe `target/criteria/report.txt` con los aprobados, los fallados y los omitidos.

Requisitos: Rust 1.98.1 (lo fija `rust-toolchain.toml`). Para la feature `ublk` hace falta `clang`; para C19 y C20, root y el módulo `ublk_drv`.

## Documentos

- [vault/CONTRACT.md](vault/CONTRACT.md): qué hace el sistema y las decisiones de diseño.
- [vault/RESEARCH.md](vault/RESEARCH.md): fuentes y mediciones.
- [LIMITATIONS.md](LIMITATIONS.md): lo que el proyecto no demuestra.
- [CHANGELOG.md](CHANGELOG.md): cambios por versión.

## Resultados

Toda cifra de esta sección sale de un archivo de `results/` con la máquina y el comando que la produjeron.

En el commit `5e91d04` pasan los criterios C1–C10 (corredores `bitacora-verify c01` a `c10`). Ninguno se omitió.

### C6: amplificación de escritura del GC

Fuente: [`results/c06.json`](results/c06.json). Máquina: AMD Ryzen 5 5600G, kernel 7.2.11-arch1-1. Comando: `target/release/bitacora-verify c06`. Fecha: 2026-10-24T19:49:22Z. Commit: `5e91d04`. Volumen de 1 GiB lleno, luego 3 × volumen de escrituras aleatorias de 4 KiB con `flush` cada 256.

| Carga | Amplificación de escritura | Umbral | p99 por operación (ms) |
| --- | --- | --- | --- |
| `caliente_fria` (20 % de los bloques recibe 80 %) | 2.4599 | 2.5 | 0.0779 |
| `uniforme` | 2.9134 | 3.5 | 0.0745 |

Con el disparo de GC anterior al ADR 0001, la misma medición dio 3.9964 y 4.8663 y C6 fallaba ([`results/c06-aa68e3d.json`](results/c06-aa68e3d.json)).

## Licencia

MIT. Ver [LICENSE](LICENSE).
