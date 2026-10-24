# Límites

Fuente única de lo que bitacora no demuestra. Ningún otro archivo los repite.

## Alcance

- No describe ni reproduce el motor interno de Railway: no hay documento público de su diseño. La única cifra de Railway usada es la referencia publicada de 3,000 IOPS de lectura y 3,000 de escritura por volumen.
- Todo corre en una sola máquina. La replicación se demuestra entre procesos del mismo host: una caída es `SIGKILL` y una partición es `SIGSTOP`/`SIGCONT`. No se demuestra comportamiento de red real (latencia, pérdida o reordenamiento de paquetes).
- Un corte de energía se aproxima con un disco simulado que descarta escrituras no sincronizadas y con `SIGKILL`. No cubre la caché interna de un SSD real.
- SPDK, vhost-user-blk y la especificación de hardware de servidores quedan fuera: requieren hardware, hugepages, VFIO o root.
- No hay comparación medida contra dm-thin, ZFS u otro sistema: requieren root en esta máquina y la comparación solo vale en la misma máquina.
- No se usan dm-flakey, dm-log-writes ni loop devices.

## Diseño

- Atomicidad garantizada por bloque de 4 KiB, no por petición de varios bloques.
- Sin checkpoints del índice: el tiempo de apertura crece con el tamaño físico del volumen.
- Sin TRIM/DISCARD ni WRITE_ZEROES.
- Si un bloque del último lote escrito antes de un corte (sin cierre ordenado) se corrompe, no se distingue de una escritura rota y se trata como no escrito.
- Los snapshots son locales a cada proceso: no se replican ni se migran.
- La tabla de snapshots del superbloque tiene 64 entradas, y los borrados cuentan hasta que una creación necesita su lugar. Con 64 snapshots vigentes, `snapshot_create` falla.
- Un backup guarda solo los bloques de una versión: ni los snapshots ni el `volume_id` viajan en él. El volumen restaurado recibe un `volume_id` nuevo y no tiene snapshots. Para tomar un incremental sobre él hay que empezar con un backup completo nuevo.
- Si se borra el snapshot mientras se hace su backup, el backup falla y deja un archivo incompleto, que `restore` rechaza.
- `export` del volumen vivo no es una imagen de un instante: lee por tramos, y una escritura concurrente puede aparecer en unos tramos y no en otros. Para una imagen de un instante hay que exportar un snapshot.
- Tras reabrir, cada snapshot se reconstruye recorriendo el log. Las páginas del índice iguales a las de la versión siguiente se vuelven a compartir, pero la reconstrucción usa memoria temporal proporcional a los bloques escritos por cada snapshot.
- En `GcMode::Auto` el GC no deja colchón de segmentos libres: corre solo con 2 o menos (ADR 0001). La cubeta de 256 MiB/s nunca lo frena, y una escritura que necesita un segmento espera a que termine un ciclo del GC si este no acabó antes.
- El volumen corre sobre un archivo en btrfs en la máquina de medición; el copy-on-write del sistema de archivos de abajo añade su propia amplificación, que no se descuenta.
- Un segmento con un bloque vivo que no pasa su `data_crc` se marca dañado y el GC no lo reclama: su espacio no vuelve al volumen.
- Los contadores del GC de `status()` no se guardan en disco y vuelven a cero al reabrir el volumen.

## Verificación

- C19 y C20 (ublk) se omiten con código 77 cuando falta root, el módulo `ublk_drv` o el permiso; una omisión no es un aprobado.
- Los umbrales de C4, C6, C12, C15, C22 y C25 se fijaron en K0 con la cota del diseño y la medición de la plataforma, antes de existir el motor, no con una corrida previa del motor.
- C24 comprueba solo que el documento de requisitos y sus encabezados existen; si su contenido es correcto es un juicio humano.
- En C6 la amplificación de escritura varía de una corrida a otra con la misma semilla, porque el hilo del GC no es determinista. Dos corridas dieron 2.4538 y 2.4599 en `caliente_fria`, frente al umbral de 2.5. La dispersión no se midió, así que el margen de aprobación no está acotado.
- El `p99_ms` de C6 se mide sobre un archivo sin `O_DIRECT`, a través de la caché de páginas. No es la latencia del disco.
- El campo `maquina.disco` de `results/c06.json` lista dos dispositivos y no dice en cuál estaba el archivo medido.
- C7, C8 y C10 corren con el GC apagado. No prueban la detección de corrupción, los truncados ni los errores de E/S mientras se reubica un segmento. C9 y C5 sí corren con el GC activo.
- La salida de C9 es idéntica byte a byte entre corridas solo mientras ninguna semilla falla. El hilo del GC no es determinista, así que una semilla que falla puede no repetir la misma salida.
