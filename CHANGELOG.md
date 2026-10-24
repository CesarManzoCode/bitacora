# Changelog

## [Sin publicar]

### K1 — Volumen durable (en curso)

- Workspace de Cargo (`crates/bitacora`, `crates/bitacora-verify`) con toolchain 1.98.1 y `Cargo.lock`.
- Tipo `Error`, trait `Medium` con `FileMedium` (`pread`/`pwrite`, `fdatasync`, perforación con `fallocate`, `O_DIRECT` opcional) y codificación del formato `bitacora-volume/1`: superbloque de dos ranuras, cabecera de segmento, resumen de lote y `data_crc`.
- `Volume` (`volume.rs`) sin GC: `create`, `open`, `write`, `write_fua`, `read`, `flush`, `close`, `status` y `locate`; lote abierto en memoria de hasta 128 bloques, `synced_through_lsn` en cada resumen, `fdatasync` del segmento anterior antes de cada cabecera nueva, segmento libre de número más bajo, `Error::NoSpace` al llegar a `max_segments` y fail-stop (`Error::Failed`). Índice paginado copy-on-write y tabla de registros con `refcount` y vivos por segmento (`index.rs`).
- Recuperación de la sección 3.4 (`volume/recovery.rs`). Comportamiento implementado que el contrato no detalla: la comprobación de pérdida del punto 4 se aplica a todos los segmentos calientes, no solo al último; al abrir se pone a ceros el bloque donde termina la cadena del último segmento de cada flujo y se sincroniza el medio antes del superbloque, para que los restos de un lote roto no se lean como válidos cuando ese segmento deje de ser el último; el superbloque de apertura registra como `durable_lsn` el último LSN recuperado. Nuevo error `Error::Closed` para operaciones tras `close`.

- Recolector de basura (`gc.rs`, sección 6): flujo frío (`stream = 1`) con su propio segmento abierto; selección cost-benefit `(1 − u) × edad / (1 + u)` entre segmentos sellados con datos muertos; ciclo copiar vivos → `fdatasync` → reubicar registros cuyo `birth` sigue vivo en la víctima → `punch_hole` bajo el candado de reutilización → libre. Hilo de GC por volumen con `GcMode::Auto` (bajo 10 % de segmentos libres, cubeta de fichas de 256 MiB/s), `Continuous` y `Off`, detenido en `close` y al soltar el volumen. `Volume::compact()`, `Volume::segment_usage()` y contadores `gc` en `status()` (modo, en curso, segmentos limpiados, bytes reubicados). Comportamiento implementado que el contrato no detalla: con el GC activo, los 2 últimos segmentos libres se reservan al flujo frío y la escritura del cliente que necesita uno espera al GC; si el GC no puede liberar nada, recibe `Error::NoSpace`. El `fdatasync` del ciclo también emite el lote caliente abierto, para que toda escritura que mató un registro de la víctima sea durable antes de liberarla. Una víctima con un bloque vivo que no pasa su `data_crc` se marca dañada y no se reclama. Los contadores del GC no se persisten.

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
