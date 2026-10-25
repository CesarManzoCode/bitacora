# Changelog

## [Sin publicar]

### K3 — Frontend, rendimiento y operación (2026-10-24)

- Traducción de peticiones de bloque en `frontend.rs`, sin dependencia de libublk: sectores de 512 bytes a bloques de 4096, `FLUSH` a `flush`, `FUA` a `write_fua`. Rechaza peticiones no alineadas a 4096, fuera del volumen, `DISCARD` y `WRITE_ZEROES`.
- `bitacora ublk --volume F [--create SIZE]` (feature `ublk`, `make ublk`): dispositivo con bloque lógico de 4096, caché volátil y FUA, una cola de profundidad 64 y `UBLK_F_USER_RECOVERY | UBLK_F_USER_RECOVERY_REISSUE`. Imprime el id, sirve hasta `SIGTERM` y entonces cierra el volumen y borra el dispositivo. `bitacora ublk --recover DEV_ID --volume F` retoma el dispositivo tras un `SIGKILL`. Sin la feature, el subcomando avisa que el binario se compiló sin ella.
- `scripts/c19-ublk.sh` y `scripts/c20-ublk-recovery.sh`, llamados por `bitacora-verify all` como c19 y c20; salen con 77 (omitido) sin root, sin `/dev/ublk-control` o sin permiso. En esta máquina, sin root, salen omitidos.
- `bitacora capacity --history H.json [--days N]` (`capacity.rs`): lee `{volume_size, segment_size, write_blocks_per_day, overwrite_fraction, snapshots_per_day, snapshot_retention_days}` e imprime `{"projected_physical_bytes": n}` para N días (30 por defecto). El modelo recorre la historia día por día con el empaquetado en lotes y segmentos del flujo caliente, la regla de espacio de la sección 4 y el disparo del GC del ADR 0001, y nunca pasa del tope de `max_segments` segmentos.
- `bitacora-verify c21`: 4 GiB sobre `FileMedium` con `O_DIRECT`, llenado secuencial, 8 hilos, fases de 30 s de lectura y de escritura aleatoria de 4 KiB (`flush` cada 32 escrituras por hilo) y una fase de sobrescritura con el GC activo. Escribe `results/c21.json` y borra el volumen al terminar. Un resultado bajo 3,000 IOPS se informa en `bajo_referencia`, no falla. Medido en `results/c21.json` sobre `187d5ba`: 28500.6826 IOPS de lectura, 11348.1566 de escritura y p99 de 22.0662 ms durante el GC, sobre la referencia de 3,000 IOPS.
- `bitacora-verify c22`: historial fijo de 30 días sobre 256 MiB (5,000 bloques al día, 70 % de sobrescrituras, un snapshot al día retenido 2 días). Proyecta con el binario `bitacora capacity` antes de aplicar la carga y compara con `st_blocks × 512` de `volume.img`.
- Documento de requisitos de ingeniería (`docs/engineering-requirements.md`): requisitos, garantías prometidas y no prometidas, modos de falla, plan de capacidad de disco y memoria, y qué cambia con volúmenes 100 veces más grandes (checkpoints del índice, ubicación en el índice en vez de tabla hash, índice paginado en disco, GC con colchón).
- `bitacora-verify c24` comprueba los seis encabezados del documento de requisitos; `bitacora-verify c26` cuenta las líneas del motor y de la verificación con la regla de la sección 13 del contrato y falla si el motor no es mayor o si contiene `#[cfg(test)]`.
- `bitacora-verify all` corre c01–c22, c24, c25 y c26, y `target/criteria/report.txt` termina con la lista de criterios omitidos y su razón (vacía si no hay).
- `socket.rs`: el servidor, el cliente, la replicación, la migración y los corredores abren los sockets Unix con `socket::bind` y `socket::connect`. Una ruta que no cabe en `sun_path` (108 bytes) se abre como `/proc/self/fd/<fd>/<nombre>` sobre el directorio padre. Antes, en el clon que C23 crea bajo `target/`, la ruta del socket destino de c14 mide 119 bytes: el servidor destino no arrancaba y `make criteria` salía con 2.
- `README.md` documenta el comando único para un clon limpio (`make criteria`), cómo correr C19 y C20 con root (`make ublk` y los scripts), los archivos de `results/` y las condiciones de omisión.
- Criterios C21–C24 y C26 aprobados en el commit `187d5ba`. C19 y C20 quedaron omitidos (77, sin root) y no cuentan como aprobados. C1–C18 y C25 siguen aprobados en ese commit. C6 medido de nuevo en `results/c06.json` sobre `187d5ba`: 2.4526 en `caliente_fria` y 2.9135 en `uniforme`.

**Rutas de socket más largas que `sun_path`.**
Decisión: el servidor, el cliente, la replicación y la migración abren los sockets Unix con `socket.rs`. Una ruta de más de 107 bytes se abre como `/proc/self/fd/<fd>/<nombre>` sobre el directorio padre.
Razón: en el clon limpio de C23, la ruta del socket destino de c14 medía 119 bytes, y `make criteria` salía con 2. Así el motor sirve sockets bajo directorios profundos sin acortar las rutas de los corredores. El requisito de `/proc` queda en `LIMITATIONS.md`.

### K2 — Servicios de datos (2026-10-24)

- Snapshots copy-on-write: `Volume::snapshot_create`, `snapshot_delete`, `snapshots` y `read_snapshot`, más `export` (imagen cruda del volumen vivo o de un snapshot). La tabla de snapshots y su flag de borrado van en el superbloque, que se sincroniza antes de volver. Tras un corte, cada snapshot vigente se reconstruye desde el log. Al borrar un snapshot, el espacio que solo él retenía vuelve al GC y a `compact()`. CLI: `bitacora snapshot create|delete|list` y `bitacora export`.
- Backups en línea `bitacora-backup/1`. Un backup completo guarda todos los bloques escritos de un snapshot. Uno incremental guarda solo los bloques que cambiaron entre dos snapshots, y las partes del índice que ambos comparten se saltan sin leerlas. El backup corre mientras el volumen sigue recibiendo escrituras. `restore` comprueba checksums, orden y la cadena completo → incrementales antes de escribir el volumen nuevo. CLI: `bitacora backup full|incremental` y `bitacora restore`.
- Servidor por socket Unix sin root: `bitacora serve`, con un hilo por conexión y las respuestas de cada conexión en orden. Los errores del motor se responden con los status del protocolo (`IO_ERROR`, `INTEGRITY_ERROR`, `NO_SPACE`, `FAILED`). Se apaga en orden con `SIGTERM` o `SIGINT`. Una trama truncada, demasiado grande o desconocida se rechaza sin pánico.
- Cliente (`Client`) con lectura, escritura, `write_fua`, `flush`, estado, snapshots y exportación. Ante `MOVED`, reconecta al destino y reenvía. Con una lista de réplicas, busca al principal ante `NOT_PRIMARY`, `FENCED` o una conexión caída. Solo una respuesta `OK` reconoce una escritura.
- Estado operativo: `bitacora status --socket S --json` produce un JSON que valida contra `schemas/status.schema.json`, con volumen, réplicas, GC y espacio.
- Replicación entre tres procesos (`bitacora replica`). El principal reconoce una escritura solo cuando es durable en él y en al menos un seguidor, y las escrituras concurrentes se agrupan (group commit). Toda trama lleva la época del volumen, persistida en el superbloque. Una réplica rechaza con `FENCED` una época vieja, y un principal que recibe `FENCED` deja de reconocer escrituras.
- Elección manual en dos fases: `bitacora elect --replicas S1,S2,S3` elige, con mayoría, la réplica de mayor `(última época, último LSN)`.
- Reincorporación de una réplica caída o atrasada: resincronización delta si su log es prefijo del principal, completa si diverge. Latido cada 100 ms. En el estado del principal, cada seguidor aparece como `activa`, `atrasada` o `caida` (tras 500 ms sin respuesta).
- Migración en vivo: `bitacora serve --migrate-target` arranca el destino y `bitacora migrate --from S --to D` copia el volumen en rondas mientras el origen sigue sirviendo. Al final congela y drena las peticiones en curso, envía la diferencia final, sube la época y responde `MOVED` al cliente, que pasa al destino y reenvía. Un destino ya migrado puede migrarse otra vez.
- `bitacora-verify`: corredores c11–c18 y c25. c14, c16, c17, c18 y c25 usan procesos `bitacora` reales; c16, c17, c18 y c25 los detienen con `SIGKILL` o `SIGSTOP`/`SIGCONT`.
- Criterios C11–C18 y C25 aprobados en el commit `457fd5c`, sin omitidos. C1–C10 siguen aprobados en ese commit.

**Espacio del origen durante la migración.**
Decisión: el motor no reserva espacio adicional para la versión que fija la migración ni frena al escritor por ella; si el origen se queda sin segmentos, la escritura recibe `NO_SPACE`.
Razón: es la regla de espacio de la sección 4 del contrato. La prueba de migración con 16 MiB agotó el origen con un escritor sin pausa; se dejó el motor como está y la prueba usa 64 MiB, como C14. El límite queda en `LIMITATIONS.md`.

### K1 — Volumen durable (2026-10-24)

- Volumen de bloques de 4 KiB sobre un archivo ordinario, usable como biblioteca (`Volume`): `create`, `open`, `write`, `write_fua`, `read`, `flush`, `close`, `status` y `locate`, en el formato `bitacora-volume/1`. Se puede compartir entre hilos.
- Durabilidad explícita, como un disco con caché de escritura. Una escritura se reconoce al quedar en memoria; al volver `flush`, toda escritura reconocida antes es durable; `write_fua` es durable al volver. Ante un error de escritura o de `fdatasync` del medio, el volumen pasa a `Failed` y deja de reconocer escrituras. No reintenta.
- Checksums CRC32C por bloque (sobre LBA, `birth` y datos), por resumen de lote, por cabecera de segmento y por superbloque. Si un bloque está dañado, la lectura devuelve `Error::Integrity` y nunca los datos. Si al abrir se detecta la pérdida de un lote ya sincronizado, la apertura falla con `Error::Corrupt`.
- Recuperación tras un corte sin cierre ordenado: el volumen vuelve a un prefijo de las escrituras emitidas que incluye todo lo que ya era durable. Al abrir, el motor borra los restos de un lote roto para que no se lean como válidos más adelante.
- Recolector de basura cost-benefit, con un flujo caliente para las escrituras del cliente y uno frío para las reubicaciones. Modos `Auto`, `Continuous` y `Off`, más `Volume::compact()`. Cuando quedan 2 segmentos libres, la escritura del cliente que necesita uno espera al GC en vez de fallar. Si el GC no puede liberar nada, devuelve `Error::NoSpace`.
- `bitacora-verify`: disco simulado con cortes, escrituras rotas y errores de E/S inyectados según una semilla; generador SplitMix64 + xoshiro256**; modelo de versiones por bloque; corredores c01–c10 y `bitacora-verify all`, que escribe `target/criteria/report.txt`.
- Criterios C1–C10 aprobados en el commit `5e91d04`, sin omitidos. C6 medido en `results/c06.json` (última corrida, sobre `457fd5c`): amplificación de escritura 2.4544 en `caliente_fria` (umbral 2.5) y 2.9185 en `uniforme` (umbral 3.5).

**Disparo del GC en modo Auto (ADR 0001).**
Decisión: `GcMode::Auto` corre cuando quedan 2 segmentos libres o menos, en lugar de por debajo del 10 % de `max_segments` (contrato, sección 6).
Razón: con el 10 %, C6 midió 3.9964 y 4.8663, sobre los umbrales congelados de 2.5 y 3.5 (`results/c06-aa68e3d.json`). Ese disparo dejaba sin usar unos 16 segmentos de la reserva de espacio en la que se basan los umbrales.

### K0 — Fundación (2026-10-24)

- Contrato del motor (`vault/CONTRACT.md`): formato en disco `bitacora-volume/1`, índice paginado copy-on-write, GC cost-benefit con separación caliente/fría, snapshots por versión de índice, backup `bitacora-backup/1`, replicación con épocas y fencing, migración por snapshots iterativos, frontend ublk opcional.
- Investigación con versiones fijadas y medición de la máquina (`vault/RESEARCH.md`).
- Umbrales de C4, C6, C12, C15, C22 y C25 congelados en `thresholds.json`.
- Esquema del estado operativo (`schemas/status.schema.json`).
- `make check` y CI mínimo que solo corre las pruebas.
