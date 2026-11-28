<p align="center">
  <img src="assets/icon.png" alt="YT-Niusic icon" width="96" />
</p>

# YT-Niusic

Reproductor de consola (TUI) ultraliviano y retro (**Amber Monochrome Console**) para **YouTube Music**, escrito 100% en **Rust** (`ratatui` + `rustypipe` + `rodio`/`symphonia` + `stream-download` + `souvlaki`), sin Chromium/Electron ni dependencias externas obligatorias.

## Funcionalidades

- **Estética Amber Monochrome Console**: interfaz de terminal en negro obsidiana (`#110D08`) y ámbar cálido (`#FFB02E`), ultraliviana (~10–20 MB de RAM).
- **Buscador de canciones y álbumes**: resultados directos de YouTube Music con vista completa de pistas de cada álbum.
- **Radio / Autoplay infinito de Google**: al reproducir cualquier canción o álbum, carga automáticamente la cola *"A continuación"* generada por Google y la recarga de forma indefinida cuando quedan 3 pistas o menos.
- **Teclas multimedia de Windows (SMTC)**: Play/Pausa/Siguiente/Anterior desde tu teclado físico o auriculares Bluetooth incluso con la terminal minimizada, mostrando título y artista en el overlay multimedia de Windows.
- **Me gusta / No me gusta sincronizados**: visualiza si la canción actual tiene Like o Dislike y califícala en tiempo real con `f`/`L` o `D` directamente contra tu cuenta de YouTube Music.
- **Inicio de sesión directo con Google + cifrado Windows DPAPI**: al abrir la app sin sesión, puedes iniciar sesión directamente en la página oficial de Google (ventana aislada de Edge/Chrome con soporte completo de 2FA/Passkeys). La sesión se captura automáticamente, se cifra con Windows DPAPI y persiste en el dispositivo para no volver a pedir credenciales al reabrir.
- **Actualizador automático integrado**: al iniciar, comprueba en segundo plano si existe una nueva versión en GitHub Releases y permite actualizar `yt-niusic.exe` presionando `u` directamente desde la app.
- **Audio eficiente y de alta calidad**: selecciona el stream AAC/M4A de mayor bitrate disponible (hasta 256 kbps con Premium) usando streaming por bloques `Range` de 512 KB, *prefetch* de la siguiente canción y normalización de sonoridad opcional.

## Privacidad y seguridad de tu cuenta

- **Inicio de sesión oficial sin exponer contraseñas**: tus credenciales de Google se introducen únicamente en `accounts.google.com` dentro de un perfil temporal aislado que se cierra y borra de forma segura en cuanto se valida la sesión.
- **Cifrado nativo con Windows DPAPI**: la sesión se almacena cifrada en `%LOCALAPPDATA%\yt-niusic\YT-Niusic\data\session.dpapi` (vinculada exclusivamente a tu usuario de Windows en esta PC y fuera del repositorio), manteniendo la sesión activa al reabrir la app.
- **Filtrado de mínimo privilegio**: tanto en el login por navegador como al importar `cookies.txt` o pegar la cabecera `Cookie`, se descartan cookies innecesarias (tracking/anuncios/otros dominios) conservando solo las requeridas por `music.youtube.com`.
- **Limpieza de memoria y disco**: los buffers en RAM se limpian con `zeroize`, el perfil temporal del login y cualquier `cookies.txt` importado se sobrescriben y eliminan automáticamente, y `.no_reporter()` evita volcados en disco.

## Cómo ejecutar

```powershell
# Compilar versión optimizada
cargo build --release

# Ejecutar en Windows Terminal / PowerShell
.\target\release\yt-niusic.exe
```

## Atajos de teclado

- `/` o `s`: buscar canciones o álbumes (`Enter` ejecuta, `Esc` sale)
- `1`–`4` (o `F1`–`F4`): cambiar entre `SONGS`, `ALBUMS`, `LIBRARY` y `AUTH`
- `Tab`: alternar foco entre el catálogo y la cola `UP NEXT`
- `↑/↓` o `j/k` + `Enter`: navegar y reproducir (`Esc`/`Backspace` vuelve desde un álbum)
- `Espacio`: Play / Pausa · `n`/`p`: Siguiente / Anterior · `←`/`→`: Seek ±10s · `+`/`-`: Volumen
- `f` / `L`: alternar Me gusta (Like) · `D`: alternar No me gusta (Dislike)
- `a`: toggle Autoplay (Google Radio) · `e`: agregar a la cola · `v`: toggle normalización · `q`: salir
- En la pantalla inicial (sin sesión): `Enter`/`g` (iniciar sesión con Google), `o` (OAuth TV), `i`/`c` (cookies), `s`/`Esc` (continuar como invitado)
- En `LIBRARY`: `h` (Historial), `l` (Me gusta), `b` (Álbumes guardados)
- En `AUTH`: `g` (iniciar sesión con Google en navegador), `o` (OAuth TV), `i` (importar `cookies.txt`), `c` (pegar Cookie enmascarado), `t` (auto-borrar `.txt`), `x` (cerrar sesión completa)

## Pruebas

```powershell
# Pruebas unitarias (cola, radio, DPAPI y filtrado de cookies)
cargo test --lib

# Prueba de integración real contra YouTube Music (búsqueda, álbumes, radio + decodificación de audio)
cargo test --test ytm_integration -- --ignored --nocapture
```

## Licencia

Distribuido bajo licencia [MIT](LICENSE).
