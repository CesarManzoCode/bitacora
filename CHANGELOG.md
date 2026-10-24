# Changelog

## [Sin publicar]

### K1 — Volumen durable (2026-10-24)

- Volumen de bloques de 4 KiB sobre un archivo ordinario, usable como biblioteca (`Volume`): `create`, `open`, `write`, `write_fua`, `read`, `flush`, `close`, `status` y `locate`, en el formato `bitacora-volume/1`. Se puede compartir entre hilos.
- Durabilidad explícita, como un disco con caché de escritura. Una escritura se reconoce al quedar en memoria; al volver `flush`, toda escritura reconocida antes es durable; `write_fua` es durable al volver. Ante un error de escritura o de `fdatasync` del medio, el volumen pasa a `Failed` y deja de reconocer escrituras. No reintenta.
- Checksums CRC32C por bloque (sobre LBA, `birth` y datos), por resumen de lote, por cabecera de segmento y por superbloque. Si un bloque está dañado, la lectura devuelve `Error::Integrity` y nunca los datos. Si al abrir se detecta la pérdida de un lote ya sincronizado, la apertura falla con `Error::Corrupt`.
- Recuperación tras un corte sin cierre ordenado: el volumen vuelve a un prefijo de las escrituras emitidas que incluye todo lo que ya era durable. Al abrir, el motor borra los restos de un lote roto para que no se lean como válidos más adelante.
- Recolector de basura cost-benefit, con un flujo caliente para las escrituras del cliente y uno frío para las reubicaciones. Modos `Auto`, `Continuous` y `Off`, más `Volume::compact()`. Cuando quedan 2 segmentos libres, la escritura del cliente que necesita uno espera al GC en vez de fallar. Si el GC no puede liberar nada, devuelve `Error::NoSpace`.
- `bitacora-verify`: disco simulado con cortes, escrituras rotas y errores de E/S inyectados según una semilla; generador SplitMix64 + xoshiro256**; modelo de versiones por bloque; corredores c01–c10 y `bitacora-verify all`, que escribe `target/criteria/report.txt`.
- Criterios C1–C10 aprobados en el commit `5e91d04`, sin omitidos. C6 medido en `results/c06.json`: amplificación de escritura 2.4599 en `caliente_fria` (umbral 2.5) y 2.9134 en `uniforme` (umbral 3.5).

**Disparo del GC en modo Auto (ADR 0001).**
Decisión: `GcMode::Auto` corre cuando quedan 2 segmentos libres o menos, en lugar de por debajo del 10 % de `max_segments` (contrato, sección 6).
Razón: con el 10 %, C6 midió 3.9964 y 4.8663, sobre los umbrales congelados de 2.5 y 3.5 (`results/c06-aa68e3d.json`). Ese disparo dejaba sin usar unos 16 segmentos de la reserva de espacio en la que se basan los umbrales.

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
