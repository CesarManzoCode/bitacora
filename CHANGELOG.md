# Changelog

## [Sin publicar]

### K1 — Volumen durable (en curso)

- Workspace de Cargo (`crates/bitacora`, `crates/bitacora-verify`) con toolchain 1.98.1 y `Cargo.lock`.
- Tipo `Error`, trait `Medium` con `FileMedium` (`pread`/`pwrite`, `fdatasync`, perforación con `fallocate`, `O_DIRECT` opcional) y codificación del formato `bitacora-volume/1`: superbloque de dos ranuras, cabecera de segmento, resumen de lote y `data_crc`.

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
