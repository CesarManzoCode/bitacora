# Changelog

## [Sin publicar]

### K1 — Volumen durable (en curso)

- Workspace de Cargo (`crates/bitacora`, `crates/bitacora-verify`) con toolchain 1.98.1 y `Cargo.lock`.
- Tipo `Error`, trait `Medium` con `FileMedium` (`pread`/`pwrite`, `fdatasync`, perforación con `fallocate`, `O_DIRECT` opcional) y codificación del formato `bitacora-volume/1`: superbloque de dos ranuras, cabecera de segmento, resumen de lote y `data_crc`.
- `Volume` (`volume.rs`) sin GC: `create`, `open`, `write`, `write_fua`, `read`, `flush`, `close`, `status` y `locate`; lote abierto en memoria de hasta 128 bloques, `synced_through_lsn` en cada resumen, `fdatasync` del segmento anterior antes de cada cabecera nueva, segmento libre de número más bajo, `Error::NoSpace` al llegar a `max_segments` y fail-stop (`Error::Failed`). Índice paginado copy-on-write y tabla de registros con `refcount` y vivos por segmento (`index.rs`).
- Recuperación de la sección 3.4 (`volume/recovery.rs`). Comportamiento implementado que el contrato no detalla: la comprobación de pérdida del punto 4 se aplica a todos los segmentos calientes, no solo al último; al abrir se pone a ceros el bloque donde termina la cadena del último segmento de cada flujo y se sincroniza el medio antes del superbloque, para que los restos de un lote roto no se lean como válidos cuando ese segmento deje de ser el último; el superbloque de apertura registra como `durable_lsn` el último LSN recuperado. Nuevo error `Error::Closed` para operaciones tras `close`.

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
