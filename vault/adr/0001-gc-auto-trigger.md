# ADR 0001 — Disparo del GC en modo Auto

Estado: aceptado en K1 (2026-10-24). Cambia el resultado de C6 y el tamaño de archivo que mide C4. Sustituye la frase de disparo de la sección 6 del contrato; el resto de la sección 6 no cambia.

## Contexto

- [hecho] La sección 6 dice: «el GC corre en un hilo propio cuando los segmentos libres son menos del 10 % de `max_segments`». Con ese disparo, `bitacora-verify c06` en el commit `aa68e3d` midió una amplificación de escritura de 3.9964 en `caliente_fria` y 4.8663 en `uniforme`, sobre umbrales congelados de 2.5 y 3.5 (`results/c06-aa68e3d.json`, el archivo que dejó esa corrida, con máquina y comando). C6 fallaba.
- [hecho] La razón de los umbrales en `thresholds.json` es la cota greedy con 25 % de reserva, `(1 + 0.25) / (2 × 0.25) = 2.5`: supone que toda la reserva de la regla de espacio de la sección 4 recibe datos.
- [hecho] Para 1 GiB con segmentos de 8 MiB, `max_segments = ceil(1.25 × 128) + 4 = 164`, y los datos vivos más sus resúmenes ocupan unos 129 segmentos. Con el disparo del 10 %, el GC mantiene libres unos 16 segmentos que nunca reciben datos, y la reserva que de verdad absorbe sobrescrituras baja de unos 33 segmentos a unos 19. La amplificación de un GC de segmentos depende de esa reserva, no del código del ciclo. El disparo y los umbrales congelados se contradicen.
- [hecho] Una simulación aparte del mismo GC (cost-benefit, un flujo caliente y uno frío, sin código del motor) dio las mismas dos cifras con el disparo del 10 %. De ahí sale que la implementación sigue el diseño y que lo que hay que cambiar es el disparo. La simulación no está versionada y sus cifras no se publican.

## Decisión

[diseño] `GcMode::Auto` corre cuando quedan `GC_RESERVE_SEGMENTS` (2) segmentos libres o menos. Es el mismo nivel en el que la sección 6 ya ignora el límite de ancho de banda y hace esperar a las escrituras del cliente. El hilo del GC se despierta cuando el flujo caliente abre el segmento que deja los libres en 2, así que el ciclo corre en segundo plano mientras el cliente llena ese segmento. Si el ciclo no ha terminado cuando el cliente necesita otro segmento, la escritura espera (backpressure, como antes).

Sin cambios: la política cost-benefit, los flujos, el ciclo, la reserva de 2 segmentos para el flujo frío, `GcMode::Continuous`, `GcMode::Off` y `Volume::compact()`.

## Consecuencias

- [hecho] `bitacora-verify c06` con este cambio: 2.4538 en `caliente_fria` y 2.9102 en `uniforme` (`results/c06.json`). El margen en `caliente_fria` es pequeño (0.046 bajo el umbral).
- [hipótesis] La cifra varía poco entre corridas de la misma semilla, porque el hilo del GC no es determinista. La dispersión no se midió.
- [diseño] En `Auto`, la cubeta de 256 MiB/s ya no retrasa nunca un ciclo: el GC solo corre en el régimen donde la sección 6 la ignora. El código de la cubeta se conserva. El límite está en `LIMITATIONS.md`.
- [diseño] El archivo crece hasta casi `max_segments` antes de que corra el GC, así que el factor físico de C4 se acerca a la cota de diseño de 1.282 de `thresholds.json` (umbral 1.3). Así lo anticipa el umbral.
- [hipótesis] La latencia de escritura durante el GC puede subir porque no hay colchón de segmentos libres. Se mide en C21 (`p99_ms_durante_gc`).

## Alternativas descartadas

- Reserva de 1 segmento con disparo en 1: deja más margen en C6, pero cambia también la cifra de 2 segmentos de la sección 6 y el reparto de espacio del flujo frío.
- Limpiar varias víctimas por ciclo y ordenar los bloques reubicados por `birth`: en la simulación aumentó la amplificación de `caliente_fria`.
- Mantener el 10 % y cambiar los umbrales: `thresholds.json` está congelado.
