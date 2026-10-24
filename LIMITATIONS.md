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
- El frontend ublk tiene una cola de profundidad 64, pero un solo hilo la atiende petición por petición, con llamadas bloqueantes al motor: no hay más de una petición en el motor a la vez por dispositivo.
- C19 y C20 necesitan root y el módulo `ublk_drv` cargado; sin ellos salen omitidos (77) y el frontend ublk no queda demostrado.
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
- El servidor atiende las peticiones de cada conexión una a una: un cliente no tiene más de una petición en curso por conexión. La concurrencia sale de abrir varias conexiones.
- `EXPORT` por el socket escribe la imagen en el sistema de archivos del servidor, en la ruta que manda el cliente; el servidor no restringe esa ruta, así que el socket solo debe ser accesible para usuarios de confianza.
- Una petición con época 0 no se compara con la del volumen; solo se responde `FENCED` a una época distinta de 0 y menor que la del volumen.
- La replicación tiene tres réplicas fijas, dadas al arrancar con `--peers`; no hay cambio de membresía.
- La elección no es automática: un principal caído se reemplaza solo cuando alguien corre `bitacora elect`. Hasta entonces las escrituras no se reconocen.
- Sin ningún seguidor que conteste, el principal no pasa a modo sin réplicas: cada escritura espera 5 s y se responde `IO_ERROR`. Puede volverse durable más tarde.
- Una escritura respondida con `NOT_PRIMARY`, `IO_ERROR` o sin respuesta puede haber quedado aplicada. El cliente la reenvía a otra réplica; en un bloque es el mismo contenido, pero sobrevivir no está garantizado.
- El principal lee de su volumen local, que incluye escrituras aún no durables en mayoría. Un principal aislado que todavía no recibió `FENCED` puede servir lecturas viejas.
- Una resincronización completa reemplaza el volumen del seguidor: sus snapshots locales se pierden.
- Un delta interrumpido obliga a una resincronización completa la siguiente vez, aunque la mayor parte del volumen ya coincida.
- `epoch_history` guarda hasta 64 épocas. Un seguidor cuya última época ya salió del historial del principal recibe una resincronización completa.
- La cola de bloques pendientes del principal guarda hasta 16,384 bloques (64 MiB) por seguidor; uno que se atrasa más se resincroniza desde el volumen.
- Un principal fijo en `SIGSTOP` no se entera de la nueva época hasta que reanuda: al reanudar, su primer `APPEND` o latido recibe `FENCED`.
- La migración en vivo solo mueve un volumen servido sin réplicas (`bitacora serve`), a un destino nuevo del mismo tamaño. Hay una migración a la vez por origen.
- Si el destino falla después de que el origen subió la época y antes de confirmar el commit, el origen vuelve a servir con la época nueva y la migración hay que repetirla con un destino nuevo; no hay reanudación.
- Mientras la migración está congelada, las peticiones nuevas del origen esperan sin plazo. Si el servidor de origen se apaga en ese momento, esas conexiones no terminan hasta que la migración acaba.
- Desde la ronda 5, cada escritura reconocida por el origen tarda al menos 1 ms más; el retraso se quita al congelar.
- Mientras dura una migración, el origen guarda la versión fijada además de las escrituras nuevas. La migración no reserva espacio para eso y no frena al escritor, salvo el retraso de 1 ms desde la ronda 5. Un escritor sin pausa puede agotar los segmentos del origen antes de que termine la copia, y desde ahí sus escrituras reciben `NO_SPACE`. Se observó con un volumen de 16 MiB.

## Verificación

- C19 y C20 (ublk) se omiten con código 77 cuando falta root, el módulo `ublk_drv` o el permiso; una omisión no es un aprobado.
- Los umbrales de C4, C6, C12, C15, C22 y C25 se fijaron en K0 con la cota del diseño y la medición de la plataforma, antes de existir el motor, no con una corrida previa del motor.
- C24 comprueba solo que el documento de requisitos y sus encabezados existen; si su contenido es correcto es un juicio humano.
- En C6 la amplificación de escritura varía de una corrida a otra con la misma semilla, porque el hilo del GC no es determinista. Dos corridas dieron 2.4538 y 2.4599 en `caliente_fria`, frente al umbral de 2.5. La dispersión no se midió, así que el margen de aprobación no está acotado.
- El `p99_ms` de C6 se mide sobre un archivo sin `O_DIRECT`, a través de la caché de páginas. No es la latencia del disco.
- El campo `maquina.disco` de `results/c06.json` lista dos dispositivos y no dice en cuál estaba el archivo medido.
- C7, C8 y C10 corren con el GC apagado. No prueban la detección de corrupción, los truncados ni los errores de E/S mientras se reubica un segmento. C9 y C5 sí corren con el GC activo.
- C14 y C15 miden la migración con un solo escritor por `Client` sobre volúmenes de 64 MiB, y la pausa de C15 es la mayor separación entre respuestas consecutivas de ese escritor. No se midió con varios clientes ni con volúmenes más grandes.
- C16, C17, C18 y C25 usan tres procesos réplica con volúmenes de 64 MiB. No se midió cuánto tarda una resincronización, delta o completa, con volúmenes más grandes. Las pruebas de `crates/bitacora/tests/replication.rs` corren las tres réplicas en hilos de un solo proceso.
- La salida de C9 es idéntica byte a byte entre corridas solo mientras ninguna semilla falla. El hilo del GC no es determinista, así que una semilla que falla puede no repetir la misma salida.
