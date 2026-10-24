# Changelog

## [Sin publicar]

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
