# Requisitos de ingeniería de bitacora

Documento de C24. Resume qué debe hacer el motor, qué promete, cómo falla, cuánto espacio y memoria pide y qué cambiaría con volúmenes 100 veces más grandes. La fuente de cada decisión es `vault/CONTRACT.md`; los límites detallados viven solo en `LIMITATIONS.md` y aquí se enlazan, no se repiten.

Convención, la misma del contrato:

- **[hecho]**: algo medido, con el archivo de `results/` que lo registra (máquina, comando, fecha y commit), o algo que se lee en el código.
- **[diseño]**: una decisión aceptada en el contrato o en un ADR.
- **[hipótesis]**: algo que se espera y no se ha medido. Las cuentas de este documento que no salen de `results/` son hipótesis aunque la aritmética sea exacta: dicen lo que el código debería hacer, no lo que hizo.

## Requisitos

Funcionales [diseño] (contrato, secciones 1, 3–11):

1. Servir volúmenes de bloques de 4 KiB guardados en un archivo ordinario, en el formato `bitacora-volume/1`, como biblioteca (`Volume`) y como servidor por socket Unix (`bitacora serve`), sin root.
2. Durabilidad explícita: `write` se reconoce con el bloque en memoria; `flush` hace durable todo lo reconocido antes; `write_fua` es durable al volver.
3. Integridad: CRC32C sobre `lba ‖ birth ‖ datos` en cada bloque, y CRC en resúmenes de lote, cabeceras de segmento y superbloque. Una lectura dañada devuelve error, nunca datos.
4. Recuperación tras un corte a un prefijo de las escrituras emitidas que contiene todo lo durable.
5. Recolección de basura cost-benefit con flujo caliente (cliente) y frío (reubicaciones), en modo `Auto`, `Continuous` u `Off`, más `compact()`.
6. Snapshots copy-on-write, exportación de imágenes y backups `bitacora-backup/1` completos e incrementales, tomados en línea.
7. Replicación entre tres procesos con reconocimiento por mayoría durable, épocas, fencing y elección manual en dos fases (`bitacora elect`).
8. Migración en vivo entre dos servidores con redirección `MOVED`.
9. Dispositivo de bloque real vía ublk (feature `ublk`) cuando hay root y `ublk_drv`.
10. Comando de estado JSON (`bitacora status --json`) que valida contra `schemas/status.schema.json`, y proyección de capacidad (`bitacora capacity`).

No funcionales [diseño]:

- Un solo binario de producto, `crates/bitacora`, con las dependencias de la sección 2 del contrato y nada más.
- Toda cifra publicada sale de un archivo de `results/` con máquina y comando (C6 y C21).
- `make criteria` en un clon limpio corre todos los criterios posibles sin root con un RSS máximo de 4 GiB (C23).
- El motor tiene más líneas que su verificación (C26, regla de la sección 13 del contrato).
- Referencia externa: la cifra pública de Railway, 3,000 IOPS de lectura y 3,000 de escritura por volumen. Se informa si se alcanza; no es una promesa del motor (contrato, sección 11).

Cada requisito tiene su criterio mecánico en la tabla de la sección 12 del contrato (C1–C26), y `bitacora-verify all` los corre en orden.

## Garantías prometidas

[diseño] Lo que el motor promete y qué criterio lo comprueba:

| Garantía | Dónde se define | Criterio |
| --- | --- | --- |
| Lo que se escribe se lee igual; un LBA nunca escrito lee ceros | Contrato, sección 4 | C1, C2, C3 |
| Al volver `flush` (o `write_fua`), toda escritura reconocida antes es durable | Sección 4 | C9 |
| Tras un corte, el estado recuperado es un prefijo de las escrituras emitidas y contiene todo lo durable | Secciones 3.4 y 4 | C8, C9 |
| Atomicidad por bloque de 4 KiB: un bloque se ve entero viejo o entero nuevo | Sección 4 | C8, C9 |
| Un bloque dañado devuelve `Error::Integrity`, nunca datos; la pérdida de un lote ya sincronizado hace fallar la apertura con `Error::Corrupt` | Secciones 3.4 y 4 | C7 |
| Fail-stop: tras un error de escritura, `fdatasync` o `fallocate` del medio no se reconoce nada más | Sección 4 | C10 |
| El GC no pierde una actualización concurrente: reubica registros sin tocar el índice | Secciones 5 y 6 | C5 |
| Un snapshot es inmutable y su backup incremental restaura una imagen idéntica | Sección 7 | C11, C12, C13 |
| La migración no pierde ninguna escritura reconocida por el origen | Sección 10 | C14, C15 |
| En modo replicado, una escritura reconocida es durable en 2 de 3 réplicas y sobrevive a la caída del principal y a una elección | Sección 9 | C16, C17 |
| Una réplica rechaza toda trama de una época vieja (`FENCED`) | Sección 9 | C18 |
| El estado marca `"caida"` a un seguidor sin respuesta durante 500 ms | Sección 9 | C25 |

[hecho] La última corrida registrada de C6 (`results/c06.json`, commit `457fd5c` con cambios sin commit, 2026-10-24T22:15:52Z, AMD Ryzen 5 5600G, kernel 7.2.11-arch1-1, comando `target/release/bitacora-verify c06`) midió una amplificación de escritura de 2.4544 en `caliente_fria` y 2.9185 en `uniforme`, bajo los umbrales congelados de 2.5 y 3.5. Es una medición, no una garantía: la cifra varía entre corridas (ver `LIMITATIONS.md`).

## Garantías no prometidas

[diseño] El motor no promete, a propósito:

- Atomicidad de una petición de varios bloques; solo por bloque.
- Durabilidad de una escritura local antes de `flush` o `write_fua`: el volumen local se comporta como un disco con caché de escritura volátil.
- Un rendimiento mínimo. C21 informa IOPS y latencias y si quedan bajo la referencia pública de 3,000 IOPS; no falla por ello. [hecho] La corrida registrada (`results/c21.json`, commit `88a5e82`) midió 28816.9875 IOPS de lectura y 11723.7302 de escritura, por encima de la referencia de 3,000.
- Elección automática del principal, cambio de membresía, comportamiento en una red real o con la caché interna de un SSD.
- Que los snapshots viajen con la replicación, la migración o un backup.
- Que las lecturas de un principal aislado sean recientes, ni que una escritura respondida con error no haya quedado aplicada.
- Que el dispositivo ublk funcione donde C19 y C20 salen omitidos (77): sin root, sin `/dev/ublk-control` o sin permiso, el frontend no queda demostrado.

La lista completa y sus condiciones está en `LIMITATIONS.md`, que es la fuente única de límites.

## Modos de falla conocidos

Cómo reacciona el motor ante cada falla [diseño], con el criterio que lo ejercita:

| Falla | Reacción | Criterio |
| --- | --- | --- |
| Corte de energía o `SIGKILL` sin cierre ordenado | Recuperación por recorrido de resúmenes al abrir; se descarta el lote roto del final y se abre un segmento nuevo | C8, C9 |
| Escritura rota (lote con LSN > D y un bloque con `data_crc` inválido) | El log termina ahí; esas escrituras no eran durables | C8, C9 |
| Corrupción en reposo de un bloque durable | Las entradas se conservan y la lectura de ese bloque devuelve `Error::Integrity`; el GC no reclama el segmento dañado | C7 |
| Lote sincronizado perdido detrás del punto de corte | La apertura falla con `Error::Corrupt`: la pérdida se detecta, nunca se oculta | pruebas de `crates/bitacora/tests/` |
| Error de E/S en escritura o `fdatasync` | Estado `Failed`; toda operación devuelve `Error::Failed`; no hay reintentos (fsyncgate) | C10 |
| Volumen sin segmentos libres que el GC pueda liberar | `Error::NoSpace`; con 2 segmentos libres o menos, la escritura espera al GC en vez de fallar | C4, C6 |
| Caída de un seguidor | El principal sigue con mayoría; el seguidor vuelve con resincronización delta o completa | C16 |
| Caída del principal | No se reconocen escrituras hasta que alguien corre `bitacora elect`; la nueva época conserva toda escritura reconocida | C17 |
| Principal pausado (partición simulada con `SIGSTOP`) | Al reanudar, su primer `APPEND` o latido recibe `FENCED` y deja de reconocer | C18 |
| Origen sin espacio durante una migración | El escritor recibe `NO_SPACE`; la migración no reserva espacio | prueba de migración (16 MiB) |
| Destino de migración caído tras subir la época | El origen vuelve a servir con la época nueva; la migración se repite con otro destino | pruebas de `crates/bitacora/tests/migrate.rs` |
| Proceso ublk muerto | `bitacora ublk --recover DEV_ID` retoma el dispositivo; las peticiones se reenvían y son idempotentes | C20 (omitido sin root) |

[hipótesis] Fallas sin criterio que las ejerza: los dos superbloques dañados a la vez (el volumen no abre), un disco que miente sobre `fdatasync` y un sistema de archivos de abajo lleno antes de `max_segments`.

## Plan de capacidad

Espacio en disco [diseño] (contrato, sección 4):

- `max_segments = ceil(volume_size × 1.25 / segment_size) + 4`. El archivo crece de forma perezosa hasta `1 MiB + max_segments × segment_size`.
- Para 1 GiB con segmentos de 8 MiB: 164 segmentos, 1,313 MiB, 1.282 veces el volumen. Es la cota de diseño detrás del umbral 1.30 de C4 en `thresholds.json`.
- Con el disparo del ADR 0001, el GC solo corre con 2 segmentos libres o menos, así que un volumen con sobrescrituras sostenidas se acerca a ese tope y se queda ahí. Los snapshots retenidos fijan versiones muertas que el GC no puede liberar; si esas versiones no caben, las escrituras reciben `NoSpace`.
- `bitacora capacity --history H.json --days N` proyecta los bytes ocupados (`st_blocks × 512`) a partir de la tasa de escritura, la fracción de sobrescrituras y la retención de snapshots, con las mismas reglas de espacio y de GC del motor. [hecho] C22 compara esa proyección con el archivo real; aún no se ha corrido, así que no hay error medido que citar.

Ancho de banda de escritura [hecho] (`results/c06.json`): el medio recibe 2.4544 veces los bytes del cliente con la carga `caliente_fria` y 2.9185 veces con `uniforme`. [hipótesis] Para un volumen que recibe la referencia de 3,000 IOPS de escritura de 4 KiB (11.7 MiB/s del cliente), el medio recibe del orden de 29 a 34 MiB/s, lejos de los 420 MiB/s de escritura secuencial medidos en la plataforma (RESEARCH.md, que describe el disco, no el motor).

Memoria [diseño] (contrato, sección 5) e [hipótesis] de su tamaño, leída del código:

- Índice: 8 bytes por bloque del volumen (`birth` en páginas de 512 entradas). 4 GiB → 1,048,576 bloques → 8 MiB por versión; un snapshot comparte las páginas que no cambian.
- Tabla de registros: un `HashMap<u64, Record>` con `Record` de 16 bytes (segmento, bloque, `data_crc`, `refcount`), es decir 24 bytes más un byte de control por entrada, en una tabla cuya capacidad es potencia de dos. Para 1,048,576 registros vivos son 2,097,152 ranuras, unos 50 MiB.
- Recuperación: una entrada de 40 bytes por versión de bloque encontrada en el log, ordenada por `(lba, birth)`. Para 4 GiB (644 segmentos de 2,047 bloques de datos como máximo) son unos 50 MiB temporales.
- Total estimado para un volumen de 4 GiB lleno: unos 60 MiB en régimen y unos 110 MiB durante la apertura. C23 mide el RSS máximo de `make criteria` con `/usr/bin/time -v` contra un tope de 4 GiB; no hay cifra en `results/` todavía.

Tiempo de apertura [hipótesis]: lineal en el tamaño físico (sin checkpoints, contrato sección 3.4). Una lectura de 4 KiB por lote: con lotes llenos de 128 bloques, unos 10,300 resúmenes para 4 GiB; con la lectura aleatoria `O_DIRECT` de QD1 medida en la plataforma (8,687 IOPS, RESEARCH.md) serían del orden de 1.2 s. No se ha medido sobre el motor.

## Con 100 veces más tamaño

Escenario: el volumen de C21 pasa de 4 GiB a 400 GiB (104,857,600 bloques), con segmentos de 8 MiB. Todo lo que sigue es [hipótesis] derivada de la aritmética del diseño y del código; nada se midió a esa escala.

**Índice y tabla de registros.** El índice ocupa 8 bytes por bloque: 800 MiB por versión en lugar de 8 MiB. La tabla de registros, con 104,857,600 registros vivos, necesita 134,217,728 ranuras de 25 bytes: unos 3.1 GiB. Un solo volumen lleno pide unos 3.9 GiB de memoria en régimen, casi el tope de 4 GiB que C23 fija para toda la batería, y cada snapshot añade 4 KiB por página de 512 bloques que se sobrescribe después de tomarlo. En la máquina de medición (14 GiB) cabrían tres volúmenes así, no más.

**Recuperación sin checkpoints.** `max_segments` sube de 644 a 64,004, con hasta 131,016,188 bloques de datos en el log. Con lotes llenos son 1,023,564 lecturas de resumen; a 8,687 lecturas por segundo, unos 118 s. Con `flush` cada 32 escrituras, como en C21, los lotes tienen del orden de 32 bloques y las lecturas se multiplican por cuatro: unos 8 minutos de apertura. El orden de `(lba, birth)` de todas las versiones encontradas necesita 40 bytes por versión: unos 4.9 GiB temporales, que se suman a los 3.9 GiB del índice y la tabla que se construyen al mismo tiempo. La apertura pasa a ser el modo de falla dominante: tarda minutos y puede quedarse sin memoria antes de terminar.

**Amplificación de escritura.** [hecho] A 1 GiB: 2.4544 (`caliente_fria`) y 2.9185 (`uniforme`), `results/c06.json`. [hipótesis] La amplificación de un GC de segmentos depende de la fracción de reserva, no del tamaño absoluto, así que a 100 veces debería quedar cerca de esas cifras. Puede subir un poco: los 4 segmentos fijos de `max_segments` dejan de pesar (32 MiB: 3.1 % de reserva extra a 1 GiB, 0.008 % a 400 GiB), y la reserva efectiva baja hacia el 25 % de la cota greedy de 2.5. El ancho de banda del GC sigue la tasa de escritura del cliente, no el tamaño: con la misma carga, el medio recibe los mismos MiB/s. Lo que crece es el tiempo para elegir víctima (recorrido de 64,004 segmentos por ciclo) y el riesgo de backpressure, porque el GC sigue corriendo solo con 2 segmentos libres o menos.

**Replicación y migración.** Una resincronización completa o la primera ronda de una migración copia 400 GiB: a los 420 MiB/s de escritura secuencial de la plataforma, unos 16 minutos como mínimo, durante los que el origen retiene la versión fijada y puede agotar su espacio. La cola de 16,384 bloques por seguidor representa una fracción mucho menor del volumen, así que un seguidor atrasado cae antes en resincronización desde el volumen.

**Qué cambiaría en el diseño** (cada cambio pediría un ADR):

1. Checkpoints del índice y de la tabla de registros, escritos de forma incremental (solo las páginas sucias) junto con el LSN que cubren. La apertura leería el checkpoint y recorrería solo los resúmenes posteriores: de minutos a segundos, y sin el orden de 4.9 GiB.
2. Guardar la ubicación (segmento y bloque, 8 bytes) directamente en la entrada del índice y llevar el `refcount` por segmento en lugar de por registro. Eliminaría la tabla hash de 3.1 GiB a cambio de que el GC actualice el índice, lo que exige otra forma de evitar la carrera GC–escritura (por ejemplo, comparar y sustituir la ubicación vieja).
3. Índice paginado en disco con una caché acotada de páginas calientes, para que la memoria dependa del conjunto de trabajo y no del tamaño del volumen.
4. Disparo del GC con un colchón de segmentos libres proporcional al tamaño y víctimas mantenidas en una estructura ordenada por puntaje, para que la elección no recorra todos los segmentos ni la escritura espere al GC.
5. Resincronización y migración por rangos de páginas del índice, con reanudación tras una interrupción y reserva de espacio en el origen antes de fijar la versión.
6. Segmentos de 64 MiB (el máximo del formato) para dividir por ocho el número de segmentos y de cabeceras, al costo de una víctima de GC más grande.
