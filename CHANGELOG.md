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
- Replicación (`replica.rs`, sección 9 del contrato): `bitacora replica --id N --volume F --socket S --peers ID=S,... [--create SIZE] [--segment-size SIZE]` arranca una réplica como seguidor, y `bitacora elect --replicas S1,S2,S3` elige un principal en dos fases (`PREPARE` a todas con la mayor época conocida + 1; `PROMOTE` a la aceptante de mayor `(last_epoch, last_lsn)` si aceptó una mayoría).
- Escritura replicada: el principal asigna el LSN, anexa y envía `APPEND(epoch, prev_lsn, entradas)` a cada seguidor desde un hilo por seguidor, con todo lo encolado desde el envío anterior. Un hilo sincroniza el volumen local con todo lo anexado desde la sincronización anterior. Las escrituras concurrentes comparten ambos envíos (group commit). Una escritura se responde `OK` solo cuando es durable en el principal y en al menos un seguidor. `FLUSH` responde de inmediato. El seguidor aplica las entradas con el LSN y el `birth` del principal y hace `fdatasync` antes de contestar.
- Fencing: toda trama de replicación lleva la época. Una réplica responde `FENCED` a una época menor que la suya y adopta una mayor, persistida antes de seguir. Un principal que recibe `FENCED` deja el rol, y sus escrituras pendientes reciben `NOT_PRIMARY`, nunca `OK`. Un seguidor responde `NOT_PRIMARY` a las operaciones de cliente.
- `current_epoch` y `epoch_history` se escriben en el superbloque con `fdatasync` antes de responder a `PREPARE` y antes de servir como principal (`Volume::set_epoch`).
- Reincorporación: antes de aceptar `APPEND`, el principal de la época resincroniza al seguidor con la regla del contrato. Si `l_f` ≤ fin de la época `e_f` en su historial, hace un delta con los bloques de una versión fijada en memoria (`Volume::pin`) con `birth > l_f`, escritos en el flujo 2 (`Volume::apply_resync`, `finish_resync`). Si no, hace una resincronización completa en un volumen nuevo, construido en `<volumen>.full` y renombrado al terminar. Después sigue con `APPEND` de lo escrito mientras tanto. Un seguidor que se atrasa más de 16,384 bloques pierde su lugar en la cola y se resincroniza.
- Un delta interrumpido deja el archivo `<volumen>.resync` con `l_f`. Mientras existe, la réplica declara `(e_f, l_f)`, su siguiente resincronización es completa y, si la eligen, su época empieza en `l_f + 1`.
- Latido cada 100 ms. En el estado del principal, cada seguidor aparece `caida` tras 500 ms sin respuesta, `atrasada` si no está incorporado o su `last_lsn` va detrás del principal, y `activa` en otro caso.
- El `Client` acepta una lista de sockets de réplica (`Client::connect_replicas`). Ante `NOT_PRIMARY`, `FENCED` o una conexión caída, pasa a la réplica siguiente y reenvía la petición hasta que una responde o vence el plazo (30 s por defecto). Solo una respuesta `OK` reconoce una escritura.
- El servidor delega cada petición en un `Handler`: el volumen sin réplicas (`Server::bind`) o una réplica (`Server::with_handler`).
- Pruebas escritas en `crates/bitacora/tests/replication.rs` (tres réplicas en hilos del mismo proceso), todavía sin ejecutar.
- Migración en vivo (`migrate.rs`, sección 10 del contrato). El destino arranca con `bitacora serve ... --migrate-target` y responde `NOT_PRIMARY` a las operaciones de cliente hasta el commit; `bitacora migrate --from S --to D` pide al origen que migre. El origen fija una versión en memoria (`Volume::pin`) y envía todos sus bloques con su `birth` (`MIGRATE_DATA`); después hace rondas con los LBA cuyo `birth` es mayor que el LSN de la versión anterior, hasta una diferencia de menos de 1024 bloques o 8 rondas, y desde la ronda 5 retrasa 1 ms la respuesta de cada escritura. Al congelar, encola las peticiones nuevas, espera a las que están en curso, envía la diferencia final y `MIGRATE_FREEZE`, con el que el destino hace `fdatasync`; luego sube la época del volumen y el destino la adopta (`MIGRATE_COMMIT`). Desde ahí el origen responde `MOVED(D)` a lo encolado y a lo nuevo. Las versiones fijadas de la migración se sueltan al terminar. Si algo falla antes del commit, el origen vuelve a servir.
- El destino aplica los bloques en el flujo 2 (`apply_resync`) y sincroniza cada trama, así que el `fdatasync` de la congelación solo cubre la diferencia final. Un destino ya migrado puede ser origen de otra migración.
- Nueva op `MIGRATE` (código 35). El `Client` reintenta la conexión al destino de un `MOVED` durante 5 s; una petición respondida `MOVED` no se ejecutó y se reenvía una sola vez por salto.
- `bitacora-verify`: corredores c14 (semillas 1–20, dos procesos `serve` de 64 MiB, escritor continuo por `Client`, `seed-<n>/responses.log`) y c15 (`pausa_max_ms` frente a `c15_max_pause_ms`). Pruebas escritas en `crates/bitacora/tests/migrate.rs`, todavía sin ejecutar.

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
