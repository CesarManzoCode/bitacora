# Changelog

## [Sin publicar]

### K2 — Servicios de datos (en curso)

- Snapshots copy-on-write: `Volume::snapshot_create`, `snapshot_delete`, `snapshots`, `read_snapshot` y `export` (imagen cruda de `volume_size` bytes del vivo o de un snapshot). La tabla de snapshots y el flag de borrado se guardan en el superbloque, que se sincroniza antes de volver. Tras un corte, la recuperación reconstruye cada snapshot vigente con el `birth` máximo ≤ su LSN.
- El GC libera solo los registros sin referencias; al borrar un snapshot, el espacio que solo él retenía vuelve a ser reclamable por `compact()` y por el GC.
- CLI: `bitacora snapshot create|delete ID|list --volume F` y `bitacora export --volume F [--snapshot ID] --out IMG`.
- `bitacora-verify`: corredores c11 y c12.
- Backups `bitacora-backup/1`: `Volume::backup_full(snapshot, out)` guarda todos los bloques escritos de un snapshot; `Volume::backup_incremental(base, destino, out)` guarda los LBA cuyo `birth` difiere entre dos snapshots y salta sin leerlas las páginas del índice que comparten por puntero. El backup lee del snapshot mientras el volumen sigue recibiendo escrituras de otros hilos.
- `backup::restore(backups, medio, config)` comprueba primero cada archivo (CRC de cada entrada y del pie, orden de los LBA, `volume_id`, `volume_size` y la cadena `base_lsn` → `target_lsn`) y solo entonces escribe el volumen nuevo.
- CLI: `bitacora backup full|incremental --volume F [--base ID] [--snapshot ID] --out BKP` y `bitacora restore --out F BKP...`.
- `bitacora-verify`: corredor c13.
- Protocolo por socket Unix (`wire.rs`): tramas de longitud `u32` con los códigos de op y de status de la sección 8 del contrato. La decodificación rechaza sin pánico una trama truncada, demasiado grande, con bytes de sobra o con op o status desconocidos. Los códigos 16–31 quedan reservados para la replicación y los 32–47 para la migración.
- Servidor (`server.rs`): `bitacora serve --volume F --socket S [--create SIZE]`, con un hilo por conexión y las respuestas de cada conexión en orden. Los errores del motor se responden así: `Io` e `InvalidArgument` → `IO_ERROR`; `Integrity` y `Corrupt` → `INTEGRITY_ERROR`; `NoSpace` → `NO_SPACE`; `Failed` y `Closed` → `FAILED`. Con `SIGTERM` o `SIGINT` deja de aceptar conexiones, termina las peticiones en curso, cierra el volumen y borra el socket.
- Cliente (`client.rs`): `Client::connect(path)` con `read`, `write`, `write_fua`, `flush`, `status`, `snapshot_create`, `snapshot_delete` y `export`. Ante `MOVED` reconecta al socket destino y reenvía la petición.
- Estado `bitacora-status/1` (`status.rs`) con exactamente las claves requeridas por `schemas/status.schema.json`. Un volumen sin réplicas lista una sola entrada `standalone` en `activa`. CLI: `bitacora status --socket S [--json]`.

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
