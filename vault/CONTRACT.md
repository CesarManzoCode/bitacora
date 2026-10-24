# Contrato de bitacora

Fecha de cierre del diseño: 2026-10-24 (K0). Este documento dice qué debe hacer el sistema y fija cada decisión de diseño. Lo que aquí está cerrado no se reabre en un mini sprint; si algo resulta inviable, se registra en `vault/adr/` (solo si cambia un resultado) y en `CHANGELOG.md`.

Convención: **[hecho]** es algo observado o citado (fuente en `vault/RESEARCH.md`); **[diseño]** es una decisión aceptada; **[hipótesis]** es algo que se espera y todavía no se midió.

## 1. Qué es el sistema

bitacora es un motor de almacenamiento de bloques log-structured, escrito desde cero en Rust, que sirve volúmenes de bloques de 4 KiB guardados en un archivo ordinario. Ofrece durabilidad explícita (FLUSH/FUA), checksums, recolección de basura medida, snapshots copy-on-write, backups incrementales en línea, migración en vivo, replicación entre procesos con épocas y fencing, un dispositivo de bloque real vía ublk cuando el sistema lo permite, medición de rendimiento, proyección de capacidad y un comando de estado.

Es la versión ejecutable de la tarea de entrevista «Design a Storage Engine to power something like Railway's Volumes». No afirma nada sobre el motor interno de Railway; la única cifra de Railway que usa es la publicada: 3,000 IOPS de lectura y 3,000 de escritura por volumen.

Los criterios de aceptación son los C1–C26 de la idea, que se reproducen en la sección 12 con su forma mecánica exacta.

## 2. Organización del repositorio

```
Cargo.toml                 workspace (resolver 3), miembros crates/*
Cargo.lock                 versionado; todo comando de cargo usa --locked
rust-toolchain.toml        channel = "1.98.1", components rustfmt y clippy
Makefile                   check (comando único de chequeo) y criteria (C23)
thresholds.json            umbrales de C4, C6, C12, C15, C22 y C25 (congelados en K0)
schemas/status.schema.json esquema del JSON de estado (C25)
crates/bitacora/           EL PRODUCTO: biblioteca + binario `bitacora`
  src/                     solo motor; prohibido `#[cfg(test)]` aquí
  tests/                   pruebas de integración del motor
crates/bitacora-verify/    APOYO: disco simulado, generadores, modelo, corredores cNN
  src/ y tests/
scripts/                   c19-ublk.sh y c20-ublk-recovery.sh (únicos scripts de shell)
docs/engineering-requirements.md   documento de requisitos de ingeniería (C24)
results/                   resultados medidos versionados (c06.json, c21.json)
vault/                     CONTRACT.md, RESEARCH.md (y adr/ solo si hace falta)
```

- Módulos de `crates/bitacora/src/`: `lib.rs`, `error.rs`, `medium.rs`, `format.rs`, `index.rs`, `volume.rs`, `gc.rs`, `snapshot.rs`, `backup.rs`, `wire.rs`, `server.rs`, `client.rs`, `replica.rs`, `migrate.rs`, `status.rs`, `capacity.rs`, `frontend.rs`, `ublk.rs` (solo con la feature `ublk`), `main.rs` (CLI). Un mini sprint puede partir un módulo en submódulos, no fusionar ni renombrar los de arriba.
- Las pruebas del motor van en `crates/bitacora/tests/*.rs` y usan la API pública; por eso los módulos son `pub`. Así el conteo de C26 separa motor de verificación por ruta.
- Dependencias permitidas, con versión exacta (`=x.y.z`) en `Cargo.toml`: `crc32c =0.6.8`, `libc =0.2.191`, `serde =1.0.229` (feature `derive`), `serde_json =1.0.154`, `sha2 =0.11.1` (solo en `bitacora-verify`), `libublk =0.4.8` (opcional, feature `ublk`, solo en `bitacora`), y `bitacora` por ruta en `bitacora-verify`. Valen para `[dependencies]` y `[dev-dependencies]`. Ninguna otra sin un ADR. El CLI se escribe sin crates de parseo de argumentos.

## 3. Formato en disco (`bitacora-volume/1`)

Unidad: bloque de 4096 bytes. Todos los enteros en little-endian. Todos los checksums son CRC32C (Castagnoli).

### 3.1 Distribución del archivo

| Desplazamiento | Contenido |
| --- | --- |
| 0 | superbloque, ranura A (4 KiB) |
| 4096 | superbloque, ranura B (4 KiB) |
| 8192 – 1 MiB | reservado (ceros) |
| 1 MiB + i × tamaño_segmento | segmento i |

### 3.2 Superbloque

Campos: `magic = "BITACSB1"`, `format_version = 1`, `sb_seq` (u64, crece en cada escritura del superbloque), `volume_id` (16 bytes aleatorios de `/dev/urandom` al crear), `volume_size` (bytes, múltiplo de 4096), `segment_size`, `max_segments`, `durable_lsn` (último LSN confirmado durable al escribir el superbloque), `clean_shutdown` (u8), `current_epoch` (u64), `epoch_history` (hasta 64 pares `(epoch, start_lsn)`), tabla de snapshots (hasta 64 entradas `(snapshot_id u64, lsn u64, flags u32)`; flag 1 = borrado), y `crc32c` de todo lo anterior en los últimos 4 bytes del bloque.

Escritura: se escribe la ranura que no contiene el superbloque vigente y se hace `fdatasync`. Lectura: gana la ranura con CRC válido y `sb_seq` mayor. Se escribe al crear, al cerrar de forma ordenada (`durable_lsn`, `clean_shutdown = 1`), al crear o borrar un snapshot, al cambiar de época y al abrir (`clean_shutdown = 0`).

### 3.3 Segmentos, lotes y registros

- Segmento: tamaño por defecto 8 MiB (2048 bloques); configurable a cualquier potencia de dos entre 256 KiB y 64 MiB para pruebas.
- Bloque 0 del segmento: cabecera con `magic = "BITACSG1"`, `volume_id`, `seg_gen` (u64 global y único, crece cada vez que se abre un segmento), `stream` (0 = caliente, escrituras del cliente; 1 = frío, reubicaciones del GC; 2 = resincronización de réplica), `first_lsn` y `crc32c`.
- Después vienen lotes. Un lote es un bloque resumen seguido de 1 a 128 bloques de datos. Resumen: `magic = "BITACBT1"`, `volume_id`, `seg_gen`, `stream`, `epoch`, `first_lsn`, `count`, `synced_through_lsn` (último LSN cubierto por un `fdatasync` terminado antes de emitir este lote), `count` entradas `(lba u64, birth u64, data_crc u32, flags u32)` y `crc32c` del resumen.
- `data_crc = crc32c(lba ‖ birth ‖ 4096 bytes de datos)`, de modo que un bloque mal dirigido o de otra versión también falla.
- LSN y `birth`: cada bloque escrito por el cliente recibe un LSN global, contiguo y creciente desde 1; `birth` es ese LSN. Una petición de n bloques recibe n LSN consecutivos. En el flujo caliente, `birth = lsn` y los LSN de un segmento son contiguos desde `first_lsn`. Una reubicación del GC o una resincronización conserva el `birth` original.
- Antes de escribir la cabecera de un segmento nuevo de un flujo, el segmento anterior de ese flujo se sincroniza (`fdatasync`). Por eso, en cada flujo, solo el último segmento puede tener escrituras no sincronizadas.
- Al reutilizar un segmento se escribe una cabecera con `seg_gen` nuevo; los lotes con otro `seg_gen` se ignoran.

### 3.4 Recuperación

1. Se lee el superbloque vigente. Se recorre cada ranura de segmento dentro de la longitud del archivo y se acepta su cabecera si su CRC y `volume_id` son válidos.
2. En cada segmento se leen lotes en orden hasta el primer resumen inválido (CRC, `volume_id`, `seg_gen`, o en el flujo caliente un `first_lsn` distinto del esperado). En los segmentos que no son el último de su flujo no se verifican los datos al recuperar (se verifican al leer).
3. En el último segmento de cada flujo se verifican los datos de cada lote. Sea D = máx(`durable_lsn` del superbloque, máx `synced_through_lsn` de los resúmenes válidos del flujo caliente). Un fin de archivo a mitad de un lote termina el log ahí (truncado = prefijo). Un bloque con bytes completos y `data_crc` inválido termina el log si el lote tiene algún LSN > D (escritura rota); si todo el lote es ≤ D es corrupción: las entradas se conservan y la lectura de ese bloque devuelve `IntegrityError`. En el flujo frío un lote inválido termina el segmento (las reubicaciones son idempotentes).
4. Si después del punto de corte del último segmento caliente existe un resumen válido del mismo `seg_gen` cuyo `synced_through_lsn` cubre LSN perdidos, la apertura falla con `Error::Corrupt` (pérdida detectada, nunca oculta).
5. Se reconstruye todo ordenando las entradas por `(lba, birth)`: el volumen vivo toma el `birth` máximo por LBA; cada snapshot vigente S toma el `birth` máximo ≤ S.lsn. Los registros muertos o duplicados no cambian ese máximo, por eso reaparecer en un segmento ya reclamado es inofensivo.
6. Tras recuperar nunca se anexa al último segmento caliente: se abre uno nuevo con `first_lsn = último LSN recuperado + 1`.

No hay checkpoints del índice en la versión 1: la recuperación recorre los resúmenes. El costo es lineal en el tamaño físico y se mide en K3.

## 4. Ruta de escritura, lectura y durabilidad

- API de `Volume` (en `volume.rs`): `create(medium, VolumeConfig)`, `open(medium, VolumeConfig)`, `write(lba, &[u8])` (múltiplo de 4096), `write_fua(lba, &[u8])`, `read(lba, nblocks) -> Vec<u8>`, `flush()`, `close()`, `status()`. `Volume` es `Send + Sync`; el estado interno va detrás de un `Mutex`, y las lecturas del medio se hacen fuera del candado.
- Semántica de caché volátil, como un disco con caché de escritura: `write` se reconoce cuando el bloque está en el lote abierto en memoria y en el índice. `flush` escribe los lotes pendientes y hace `fdatasync`; al volver, toda escritura reconocida antes de la llamada es durable. `write_fua` = `write` + `flush` de esa escritura.
- Un lote se emite al llenarse (128 bloques) o en cada `flush`.
- Lecturas: LBA nunca escrito → ceros; bloque en el lote abierto → desde memoria; si no, `pread` y verificación de `data_crc`; si falla → `Error::Integrity { lba }` y nunca datos.
- Atomicidad: se promete atomicidad por bloque de 4 KiB (un bloque se ve entero viejo o entero nuevo). No se promete atomicidad de una petición de varios bloques. Se promete orden: el estado recuperado es un prefijo de las escrituras emitidas.
- Fail-stop: cualquier error de escritura, de `fdatasync` o de `fallocate` del medio pone el volumen en estado `Failed`; desde ahí toda operación devuelve `Error::Failed` y nada más se reconoce. No hay reintentos de `fdatasync` (fsyncgate).
- Espacio: `max_segments = ceil(volume_size × 1.25 / segment_size) + 4`. El archivo crece de forma perezosa y siempre se usa el segmento libre de número más bajo. Si no hay segmento libre ni el GC puede liberar uno, `write` devuelve `Error::NoSpace`.
- `Medium` (en `medium.rs`): trait con `read_at`, `write_at`, `sync`, `len`, `punch_hole`. `FileMedium` usa `pread`/`pwrite`, `fdatasync` y `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`; con `direct = true` abre con `O_DIRECT` y usa búferes alineados a 4096. El disco simulado implementa el mismo trait desde `bitacora-verify`.

## 5. Índice

- [diseño] Índice paginado copy-on-write: un `Vec<Option<Arc<Page>>>` de páginas de 512 entradas `u64`; cada entrada guarda el `birth` del bloque vigente (0 = nunca escrito). Un snapshot clona el vector de páginas (las páginas se comparten) y `Arc::make_mut` copia una página al escribirla.
- Tabla de registros: `HashMap<birth, Record { segment, block, refcount }>`. `refcount` = número de versiones (vivo + snapshots) cuyo índice apunta a ese `birth`. Sobrescribir un LBA resta 1 al `birth` viejo; crear un snapshot suma 1 a cada `birth` vivo; borrarlo resta 1 a cada `birth` del snapshot. Un registro con `refcount = 0` está muerto y resta del conteo de vivos de su segmento.
- El GC y la replicación cambian solo la tabla de registros (la ubicación de un `birth`), nunca el índice. Por eso una reubicación no puede perder una actualización concurrente.
- Memoria: 8 bytes por bloque en el índice más una entrada de tabla por registro vivo; para 4 GiB son unos 8 MiB más la tabla. [hipótesis] Se mide en C23.

## 6. Recolección de basura

- Política [diseño]: cost-benefit de LFS, `puntaje = (1 − u) × edad / (1 + u)`, con `u` = bloques vivos / bloques del segmento y `edad` = LSN actual − mayor `birth` del segmento. Solo son candidatos los segmentos sellados y sincronizados.
- Separación caliente/fría: las escrituras del cliente van al flujo caliente; las reubicaciones del GC, al flujo frío.
- Ciclo: copiar los registros vivos del segmento víctima al flujo frío, `fdatasync` del segmento frío, actualizar la tabla de registros si el `birth` sigue existiendo, liberar la víctima con `punch_hole` y marcarla libre. Una víctima no se reutiliza mientras haya una lectura en curso sobre ella (candado de lectura/escritura de reutilización).
- Disparo: el GC corre en un hilo propio cuando los segmentos libres son menos del 10 % de `max_segments`. Ancho de banda limitado por cubeta de fichas a 256 MiB/s; el límite se ignora cuando quedan 2 segmentos libres o menos, y entonces las escrituras del cliente esperan (backpressure) en vez de fallar.
- Modos: `GcMode::Auto` (por defecto), `GcMode::Continuous` (pruebas: limpia sin pausa aunque haya espacio) y `GcMode::Off`. `Volume::compact()` limpia todos los segmentos sellados con datos muertos (lo usa C12).

## 7. Snapshots y backups

- Snapshot [diseño]: versión congelada del índice (sección 5). `snapshot_create` hace `flush`, clona el índice, suma referencias, guarda `(id, lsn)` en el superbloque y lo sincroniza antes de volver. `snapshot_delete` marca el flag de borrado en el superbloque, resta referencias y deja el espacio al GC. Lectura de snapshot: `read_snapshot(id, lba, n)`. `export(version, path)` escribe la imagen cruda (volume_size bytes) de una versión.
- Backup (`bitacora-backup/1`) [diseño]: cabecera (`magic = "BITACBK1"`, versión 1, `volume_id`, `volume_size`, `base_lsn` (0 = completo), `target_lsn`, `count`), luego `count` entradas ordenadas por LBA `(lba u64, crc32c u32, pad u32)` + 4096 bytes, y un pie con el CRC32C de la cabecera y de todas las entradas. Completo = todos los bloques escritos de la versión destino. Incremental = los LBA cuyo `birth` difiere entre la versión base y la destino (las páginas compartidas por puntero se saltan).
- En línea: el backup lee de un snapshot tomado al empezar mientras el volumen sigue recibiendo escrituras. La restauración aplica el completo y luego los incrementales en orden; falla si `base_lsn` de un incremental no es el `target_lsn` del anterior.
- En modo replicado y en la migración, los snapshots son locales al proceso y no se replican ni se migran (límite registrado).

## 8. Servidor, protocolo y cliente

- Interfaz sin root [diseño]: biblioteca (`Volume`) y servidor por socket Unix de flujo (`bitacora serve`). No hay NBD: el cliente NBD del kernel requiere root.
- Trama: `u32` de longitud + cuerpo. Petición: `op u8, req_id u64, epoch u64, lba u64, nblocks u32, payload`. Respuesta: `req_id u64, status u8, payload`. Ops: `READ, WRITE, WRITE_FUA, FLUSH, STATUS, SNAPSHOT_CREATE, SNAPSHOT_DELETE, EXPORT` y las de las secciones 9 y 10. Status: `OK, IO_ERROR, INTEGRITY_ERROR, NO_SPACE, FAILED, FENCED, NOT_PRIMARY, MOVED` (payload = ruta del socket destino).
- Un hilo por conexión; las peticiones de una conexión se responden en orden.
- `client.rs`: `Client::connect(path)` con `read`, `write`, `write_fua`, `flush`, `status`. Ante `MOVED` reconecta al destino y reenvía la petición; una petición respondida con `MOVED` no se ejecutó.
- Estado (`bitacora status --socket S --json`): JSON que valida contra `schemas/status.schema.json`, con `volume`, `replicas`, `gc` y `space`. Los estados de réplica son `"activa"`, `"atrasada"` y `"caida"`. Un volumen sin réplicas lista una sola entrada con `role = "standalone"`. El esquema se escribió en K0 y solo usa `type`, `required`, `properties`, `additionalProperties`, `enum`, `items` y `minimum`; no se cambia.

## 9. Replicación con épocas y fencing

- [diseño] Tres procesos réplica (`bitacora replica --id N --volume F --socket S --peers ...`), cada uno con su archivo de volumen. Un principal; los demás son seguidores.
- Escritura replicada: el principal asigna LSN, anexa localmente y envía `APPEND(epoch, prev_lsn, entradas)` a los seguidores. En modo replicado toda escritura se reconoce al cliente solo cuando es durable (`fdatasync`) en una mayoría (2 de 3, el principal incluido); se agrupan escrituras concurrentes (group commit). `FLUSH` es entonces inmediato.
- Época: `current_epoch` persistido en el superbloque de cada réplica. Toda trama de replicación lleva la época; una réplica rechaza con `FENCED` cualquier trama con época menor que la suya, y un principal que recibe `FENCED` deja de reconocer y pasa a seguidor.
- Elección (`bitacora elect --replicas S1,S2,S3`): (1) `PREPARE(e)` a todas, con e = máx época conocida + 1; una réplica acepta si e > `current_epoch`, persiste e, deja de aceptar `APPEND` de épocas menores y responde su `(last_epoch, last_lsn)` congelado; (2) con mayoría de aceptaciones, se elige la de mayor `(last_epoch, last_lsn)` y se le envía `PROMOTE(e)`; (3) el nuevo principal anota `(e, last_lsn + 1)` en `epoch_history` y empieza a servir. Mayorías que se intersectan garantizan un solo principal por época y que el elegido tiene toda escritura reconocida.
- Reincorporación de un seguidor con `(e_f, l_f)`: si `l_f` ≤ fin de la época `e_f` en el historial del principal, resincronización delta (bloques con `birth > l_f`, escritos en el flujo 2); si no (sufijo divergente), resincronización completa (volumen nuevo).
- Latido: el principal envía `HEARTBEAT` cada 100 ms; un seguidor sin respuesta durante 500 ms queda `"caida"` en el estado del principal; un seguidor con `last_lsn` detrás del principal queda `"atrasada"`.
- La partición se simula con `SIGSTOP`/`SIGCONT` del proceso; la caída, con `SIGKILL`. No se demuestra comportamiento de red real (límite).

## 10. Migración en vivo

- [diseño] Origen S y destino D son procesos `bitacora serve`; D arranca con `--migrate-target`. `bitacora migrate --from S --to D`:
  1. S toma un snapshot interno y envía sus bloques a D (`MIGRATE_DATA` con `birth`).
  2. Rondas: nuevo snapshot interno, envío de los LBA cuyo `birth` cambió desde el anterior; se repite hasta que la diferencia sea menor de 1024 bloques o se cumplan 8 rondas. Desde la ronda 5, S retrasa 1 ms el reconocimiento de cada escritura (auto-converge).
  3. Congelación: S deja de atender peticiones nuevas (las encola), drena las que están en curso, envía la diferencia final, D hace `fdatasync`, S sube la época del volumen y D la adopta.
  4. S responde `MOVED(D)` a lo encolado y a todo lo nuevo; el cliente reconecta y reenvía.
- La pausa es el tiempo entre la congelación y la primera respuesta servida por D. Ninguna escritura reconocida por S queda fuera de D porque la diferencia final se calcula después del drenado.

## 11. Frontend ublk, rendimiento y capacidad

- ublk [diseño]: `libublk =0.4.8` detrás de la feature `ublk` (desactivada por defecto, porque necesita libclang para compilar). `bitacora ublk --volume F` crea el dispositivo con bloque lógico de 4096 (`logical_bs_shift = 12`), `UBLK_ATTR_VOLATILE_CACHE` y `UBLK_ATTR_FUA`, una cola de profundidad 64, y flags `UBLK_F_USER_RECOVERY | UBLK_F_USER_RECOVERY_REISSUE` (las reescrituras repetidas son idempotentes en este motor). `UBLK_IO_OP_FLUSH` → `flush()`; `UBLK_IO_F_FUA` → `write_fua`; `UBLK_IO_OP_DISCARD` y `WRITE_ZEROES` no se anuncian. `bitacora ublk --recover DEV_ID --volume F` reconecta tras matar el proceso. La traducción de peticiones (sectores de 512 a bloques de 4096, operación a llamada del motor) vive en `frontend.rs`, sin dependencia de libublk, para poder probarla sin root.
- io_uring solo se usa a través de libublk; el motor usa `pread`/`pwrite` en hilos. [hecho] io_uring funciona sin root en esta máquina (ver RESEARCH.md); no se necesita para C1–C18.
- Rendimiento (C21): lo genera `bitacora-verify c21` sobre `FileMedium` con `O_DIRECT`, volumen de 4 GiB, 8 hilos, lecturas y escrituras aleatorias de 4 KiB, escrituras con `flush` cada 32 escrituras por hilo; 30 s por fase; una fase de sobrescritura con el GC activo mide `p99_ms_durante_gc`. El archivo de resultados declara máquina, comando, fecha y commit. Conclusión permitida por la medición: **distinto** respecto a Railway (no hay mismo contrato ni misma máquina); solo se informa si se alcanza la referencia pública de 3,000 IOPS.
- Capacidad (C22): `bitacora capacity --history H.json --days 30` recibe `{volume_size, segment_size, write_blocks_per_day, overwrite_fraction, snapshots_per_day, snapshot_retention_days}` y devuelve `{"projected_physical_bytes": n}`. El modelo usa la regla de espacio de la sección 4 y la de liberación del GC de la sección 6.

## 12. Criterios de aceptación y su forma mecánica

Todos los corredores son subcomandos de `target/release/bitacora-verify <cNN> [--seed N]`. Cada uno escribe su salida estándar también en `target/criteria/<cNN>/stdout.log`, deja ahí sus artefactos (en el disco del repositorio, nunca en `/tmp`, que en esta máquina es tmpfs y no admite `O_DIRECT`) y sale con 0 solo si su criterio se cumple. Las líneas entre comillas son exactas (en español, tal como las fija la idea); puede haber más líneas. `bitacora-verify all` corre c01–c22 (c19 y c20 a través de los scripts), c24, c25 y c26, escribe `target/criteria/report.txt` con una línea `cNN: PASS|FAIL|OMITIDO <razón>` por criterio y sale con 0 si ninguno falló; 77 cuenta como OMITIDO. `make criteria` = compilar en release + `bitacora-verify all` (es el comando de C23).

Datos: el contenido de cada escritura es `f(semilla, lba, versión)` con el PRNG de `bitacora-verify` (SplitMix64 para sembrar, xoshiro256** para generar); el modelo guarda la versión por bloque. Una falla imprime `falla reproducible: semilla <n>` y la misma semilla reproduce la misma salida byte a byte; por eso un corredor de semillas no imprime tiempos en su salida estándar. `<n>` e `<i>` cuentan desde 1.

| Id | Corredor y parámetros | Salida y artefactos exactos |
| --- | --- | --- |
| C1 | c01: volumen 1 GiB (262,144 bloques), 100,000 operaciones de 4 KiB lectura/escritura 50/50, semilla 1; cierre ordenado al final | `operaciones: 100000`, `diferencias: 0`; deja `volume.img` |
| C2 | c02: volumen nuevo de 1 GiB leído completo; escribe 1,000 bloques al azar y relee todo | `bloques: 262144`, `no_escritos_no_cero: 0` |
| C3 | c03: proceso nuevo; reabre `target/criteria/c01/volume.img` (corre c01 si falta), regenera el modelo de la semilla 1 y relee todo | `bloques: 262144`, `diferencias: 0` |
| C4 | c04: 1 GiB, 10 sobrescrituras completas con datos distintos, GC Auto | `factor_fisico: <x>`, `diferencias: 0`; deja `volume.img` (el checker mide su `st_size`) |
| C5 | c05: semillas 1–20; 4 hilos escriben los mismos 256 bloques con GC Continuous; regla: el valor final de cada bloque es una escritura no seguida en tiempo real por otra escritura reconocida del mismo bloque | por semilla `semilla <n>: actualizaciones perdidas: <k>`; final `actualizaciones perdidas: 0` |
| C6 | c06: 1 GiB lleno y luego 3 × volumen de escrituras aleatorias, `flush` cada 256; cargas `caliente_fria` (20 % de bloques recibe 80 %) y `uniforme`; la amplificación se mide con un medio contador de `bitacora-verify` (bytes escritos al medio / bytes del cliente) | `results/c06.json` con `cargas.<carga>.amplificacion_escritura` y `cargas.<carga>.p99_ms` para las dos cargas, más `maquina`, `comando`, `fecha`, `commit` |
| C7 | c07: volumen 64 MiB escrito y cerrado; 1,000 posiciones al azar dentro de bloques de datos vivos (ubicación vía `Volume::locate(lba)`); voltear un byte, reabrir, leer el bloque, restaurar el byte | por prueba `posicion <i>: integridad: detectada` o `posicion <i>: datos erroneos sin error`; final `integridad: detectada` y `lecturas_erroneas_sin_error: 0` |
| C8 | c08: volumen 64 MiB, segmento 1 MiB, escrituras con `flush` intercalados y **sin** cierre ordenado; 1,000 truncados en puntos al azar dentro del último segmento, cada uno sobre una copia | por prueba `prueba <i>: prefijo: valido` o `prueba <i>: prefijo: invalido`; final `prefijo: valido 1000 de 1000` |
| C9 | c09: semillas 1–1000; disco simulado, volumen 16 MiB, segmento 1 MiB, 20,000 operaciones con `flush` al azar y GC Auto; corte en un punto elegido por la semilla; recuperación | por semilla `semilla <n>: escrituras reconocidas perdidas: <k>`; final `escrituras reconocidas perdidas: 0` |
| C10 | c10: N = 1–200, para cada N un caso `write` (falla la escritura N del medio) y un caso `sync` (falla el `sync` N); tras el error se siguen emitiendo escrituras y `flush`, luego corte y recuperación | por caso `caso <write|sync> <N>: reconocidas_tras_error_sin_sync: <k>`; final `reconocidas_tras_error_sin_sync: 0` |
| C11 | c11: 256 MiB; snapshot; exportar snapshot; 100,000 escrituras; exportar otra vez | `snapshot-antes.img`, `snapshot-despues.img`, `modelo.sha256` (hash del modelo en el instante de la toma); líneas `sha256_modelo: <hex>`, `snapshot: inmutable` |
| C12 | c12: 256 MiB; 5 snapshots encadenados con escrituras entre ellos; borrar el 2.º; `compact()`; `bytes_justificados` = 4096 × bloques distintos (lba, versión) del modelo que referencian el vivo y los 4 snapshots | `hashes_snapshots_intactos: 4 de 4`, `bytes_justificados: <n>`; deja `volume.img` (el checker mide `st_blocks × 512`) |
| C13 | c13: 256 MiB; backup completo; N = 5,000 bloques escritos; incremental tomado con un escritor activo; restaurar en volumen nuevo | `incremental.bkp`, `restaurado.img`, `snapshot.sha256`; líneas `bloques_modificados: <n>`, `restauracion: identica` |
| C14 | c14: semillas 1–20; dos procesos `serve` (64 MiB), escritor continuo por `Client`, migración y conmutación | por semilla `semilla <n>: escrituras reconocidas perdidas: <k>; sha256 destino=<hex> modelo=<hex>` y `seed-<n>/responses.log` con el instante (ns, monótono) de cada respuesta, una por línea; final `escrituras reconocidas perdidas: 0` |
| C15 | c15: usa los `responses.log` de c14 (corre c14 si faltan) y calcula la mayor separación entre respuestas consecutivas | `pausa_max_ms: <x>`; 0 si `<x>` ≤ umbral |
| C16 | c16: 3 réplicas de 64 MiB; carga; `SIGKILL` a un seguidor; la carga sigue; se reinicia; convergencia | `escrituras_tras_caida: <n>` (n > 0), `sha256 replica<i>: <hex>` para i = 1, 2, 3, `convergencia: identica` |
| C17 | c17: semillas 1–200; `SIGKILL` del principal en un punto elegido por la semilla; `elect`; verificación | por semilla `semilla <n>: escrituras reconocidas perdidas: <k>; hashes vivos identicos: si|no`; final `escrituras reconocidas perdidas: 0` |
| C18 | c18: semillas 1–200; `SIGSTOP` del principal, elección, `SIGCONT`; el principal viejo intenta `APPEND` con su época | por semilla `semilla <n>: escrituras_aceptadas_de_epoca_vieja: <k>`; final `escrituras_aceptadas_de_epoca_vieja: 0` |
| C19 | `scripts/c19-ublk.sh` | 0 con `sha256: identico`; 77 con `omitido: <razón>` si falta root, `/dev/ublk-control` o el permiso |
| C20 | `scripts/c20-ublk-recovery.sh` | 0 con `fsck: limpio` y `sha256: identico`; misma regla de 77 |
| C21 | c21 (sección 11) | `results/c21.json` con `iops_lectura`, `iops_escritura`, `p50_ms`, `p99_ms`, `p999_ms`, `p99_ms_durante_gc` (números), `bajo_referencia` (booleano, verdadero si algún IOPS < 3000), `maquina`, `comando`, `fecha`, `commit` |
| C22 | c22: historial fijo de 30 días acelerados (1 día = una ronda de carga) sobre 256 MiB; `bitacora capacity` antes de la corrida | `projection.json`, `volume.img`; líneas `proyectado_bytes: <n>`, `real_bytes: <n>`, `error_relativo: <x>` (real = `st_blocks × 512`) |
| C23 | clon limpio + `/usr/bin/time -v make criteria` | código 0, «Maximum resident set size» ≤ 4,194,304 KB, `report.txt` con la lista de omitidos |
| C24 | c24: encabezados de `docs/engineering-requirements.md` | `encabezados: 6 de 6`. Encabezados exactos: `## Requisitos`, `## Garantías prometidas`, `## Garantías no prometidas`, `## Modos de falla conocidos`, `## Plan de capacidad`, `## Con 100 veces más tamaño` |
| C25 | c25: 3 réplicas; `status` antes; `SIGKILL` a un seguidor; `status` cada 50 ms hasta ver `"caida"` | `status-antes.json`, `status-despues.json`, `events.log` con `kill <id> <t_ms>` y `caida <id> <t_ms>` |
| C26 | c26: conteo de líneas (sección 13) | `lineas_motor: <a>`, `lineas_verificacion: <b>`; 0 solo si a > b |

## 13. Regla de conteo de C26

- Motor: `crates/bitacora/src/**/*.rs`. Verificación: `crates/bitacora/tests/**/*.rs`, `crates/bitacora-verify/**/*.rs` y `scripts/**`.
- Se cuentan líneas no vacías que no son comentario: en Rust, las que no empiezan (tras quitar espacios) por `//` y no están dentro de `/* … */`; en shell y Python, las que no empiezan por `#`.
- `crates/bitacora/src/` no contiene `#[cfg(test)]`.

## 14. Umbrales

Están en `thresholds.json`, se fijaron en K0 y no se editan nunca: los checkers comparan su SHA-256 con el valor congelado. Se fijaron antes de que exista el motor, a partir de la cota que da el diseño y de la medición de la plataforma en esta máquina (RESEARCH.md, sección «Medición de la máquina»). Cada valor lleva su razón dentro del archivo.

## 15. Decisiones delegadas al K0

**Lenguaje y organización del repositorio.**
Decisión: Rust 1.98.1 fijado con `rust-toolchain.toml`, workspace con dos crates (`bitacora` producto, `bitacora-verify` apoyo) y la organización de la sección 2.
Razón: la vacante nombra Rust, la herramienta está instalada aquí y separar crates hace que C26 se mida por ruta, sin interpretación.

**Formato en disco y tamaño de segmento.**
Decisión: `bitacora-volume/1` de la sección 3, un solo archivo, segmentos de 8 MiB, lotes de un resumen y hasta 128 bloques.
Razón: el resumen por lote permite recuperar leyendo 1 de cada 129 bloques y validar cada bloque con su CRC; 8 MiB hace que el `fdatasync` de cambio de segmento cueste una vez cada 2048 bloques.

**Estructura del índice.**
Decisión: índice paginado copy-on-write con `birth` por LBA y tabla de registros con conteo de referencias (sección 5).
Razón: un snapshot cuesta copiar punteros a páginas, y el GC mueve datos sin tocar el índice, lo que elimina la carrera GC–escritura por construcción.

**Puntos de control y recuperación.**
Decisión: sin checkpoints del índice; recuperación por recorrido de resúmenes con la regla de prefijo, escritura rota y corrupción de la sección 3.4.
Razón: es la opción más pequeña que cumple C3, C7, C8 y C9; el costo de arranque se mide en K3 antes de justificar checkpoints.

**Política de GC, separación caliente/fría y límite de ancho de banda.**
Decisión: cost-benefit, flujo caliente para el cliente y frío para el GC, 256 MiB/s con backpressure bajo 2 segmentos libres (sección 6).
Razón: la literatura citada muestra que separar por edad baja la amplificación con datos calientes; el límite protege la latencia de cola y la reserva evita quedarse sin destino para el GC.

**Checksum y atomicidad.**
Decisión: CRC32C por bloque sobre `lba ‖ birth ‖ datos`, por resumen, cabecera y superbloque; se promete atomicidad de 4 KiB por bloque, no por petición.
Razón: CRC32C tiene aceleración por hardware en x86_64 y detecta escrituras rotas y mal dirigidas; NVMe no garantiza atomicidad general, el log con checksum sí la da por bloque.

**Modelo de snapshots y formato de backup.**
Decisión: versión congelada del índice con conteo de referencias; backup `bitacora-backup/1` como diferencia de índices entre dos versiones (sección 7).
Razón: el incremental sale de comparar páginas sin recorrer datos, y su tamaño es 4112 bytes por bloque cambiado, dentro del 1.1 × 4 KiB de C13.

**Replicación, elección, épocas y fencing.**
Decisión: principal-seguidores con reconocimiento por mayoría durable, elección en dos fases `PREPARE`/`PROMOTE` y rechazo de épocas viejas en cada réplica (sección 9).
Razón: es el núcleo de Paxos/Raft reducido a una configuración fija de 3, suficiente para C16–C18 y demostrable con procesos y señales.

**Migración y conmutación.**
Decisión: copia por snapshots iterativos, congelación con drenado, diferencia final y redirección `MOVED` (sección 10).
Razón: es el esquema de espejo y conmutación de QEMU/LXD llevado al motor; la pausa queda acotada por la diferencia final de menos de 1024 bloques.

**Interfaz sin root y ublk.**
Decisión: biblioteca + servidor por socket Unix; ublk con `libublk =0.4.8` detrás de la feature `ublk`.
Razón: NBD necesita root del lado cliente; libublk es la biblioteca del proyecto ublk y su compilación pide libclang, que no debe bloquear C1–C18.

**Disco simulado y generador por semilla.**
Decisión: `SimDisk` en `bitacora-verify` con imagen durable en memoria y lista de escrituras no sincronizadas; al cortar conserva o descarta cada una de forma independiente según la semilla y puede truncar una al múltiplo de 512 bytes; inyecta error en la escritura N o el `sync` N; tras un error de `sync` puede descartar escrituras no sincronizadas (como fsyncgate). PRNG propio SplitMix64 + xoshiro256**.
Razón: cubre corte, escritura rota y error de E/S sin root, y un PRNG propio no cambia de salida con versiones de dependencias.

**Valores concretos.**
Decisión: segmento 8 MiB (1 MiB en C8 y C9), volúmenes de prueba de la tabla de la sección 12, semillas de la idea, y los umbrales de `thresholds.json`.
Razón: caben con margen en 16 GB y en el disco; los umbrales salen de la cota del diseño y de la medición de esta máquina (sección 14).

**Orden de fases.**
Decisión: tres K. K1 entrega un volumen durable usable como biblioteca (C1–C10); K2, los servicios de datos y la operación distribuida (C11–C18, C25); K3, la ruta del kernel, rendimiento, capacidad, documento de requisitos y clon limpio (C19–C24, C26).
Razón: cada K deja algo que se puede usar y cada uno depende del anterior; menos K obligaría a cerrar más de nueve criterios difíciles de golpe.

**Herramientas con root (dm-flakey, dm-log-writes, loop).**
Decisión: no se incluyen.
Razón: el disco simulado cubre esos modos sin root, y añadir comprobaciones opcionales sería ceremonia sin un criterio que la pida.

**Huecos de 99.**
Decisión: checksums por bloque, resuelto con diseño propio (sin comparar con dm-integrity ni ZFS); vhost-user-blk y SPDK, fuera de alcance; licencias de CrashMonkey y LSVD, irrelevantes porque no se usa ni se copia su código; medición en Arch, hecha en K0 (RESEARCH.md).
Razón: ningún criterio depende de esas fuentes, y la medición sí condicionaba el diseño.

**Comparación contra dm-thin o ZFS.**
Decisión: no se incluye.
Razón: ambas requieren root en esta máquina y la restricción exige medir en la misma máquina; queda en LIMITATIONS.md.

**Primera medición en la máquina de César.**
Decisión: hecha el 2026-10-24: `ublk_drv` existe como módulo pero no está cargado y `sudo` pide contraseña; io_uring funciona sin root; 14 GiB de RAM total. La memoria de las pruebas se mide en C23.
Razón: el diseño no podía apoyarse en el contenedor de investigación.

### Decisiones abiertas por el diseño

**Semántica de reconocimiento.**
Decisión: un volumen local tiene caché volátil (reconoce antes de durable y `flush` hace durable); un volumen replicado reconoce solo lo durable en mayoría.
Razón: la primera es la que el kernel espera de un disco con caché y la que C9 prueba; la segunda es la única con la que C16–C17 pueden decir «reconocida» sin ambigüedad.

**Evidencia de cada K.**
Decisión: el checker de cada K escribe `vault/private/evidence/kN.md` con sus resultados al correr sin `--self-test`.
Razón: así la evidencia sale de las salidas crudas que el checker ya leyó y ningún mini sprint gasta en escribirla.

**Resultados medidos versionados.**
Decisión: `results/c06.json` y `results/c21.json` se escriben al correr los corredores y se versionan con el commit de cierre del K.
Razón: la restricción pide que toda cifra publicada salga de una corrida registrada con máquina y comando.

**Licencia.**
Decisión: MIT, en `LICENSE`.
Razón: el proyecto es una pieza abierta, y MIT es compatible con todas las dependencias permitidas (MIT, Apache-2.0 o ambas).

## 16. Contradicciones resueltas

1. **Umbrales «fijados por medición previa» frente a mini sprints que no ejecutan nada.** Los mini sprints no pueden medir. Se fijan en K0 con la cota del diseño y la medición de la plataforma, y se congelan por hash. Se cumple lo que la restricción protege: nunca se ajustan después de ver el resultado.
2. **C23 exige `/usr/bin/time -v`, que no está instalado.** No se sustituye por otra medición; el checker de K3 falla con el motivo hasta que César instale el paquete `time`.
3. **libublk necesita libclang, que no está instalado.** La feature `ublk` queda fuera de `make check`; el checker de K3 exige que compile y falla con el motivo hasta que César instale `clang`. Las razones de omisión 77 de C19/C20 siguen siendo solo las de la idea.
4. **Presupuesto de verificación del método (~15 %) frente a C26 (motor > resto).** Manda la idea en el producto: C26 es el criterio; el 15 % se reporta como referencia en el cierre de cada K y no es un gate.
5. **Idioma de los mensajes.** El código y sus mensajes van en inglés, salvo las líneas exactas de los criterios, que la idea fija en español y se conservan literales.
