# nido

Gestor de sesiones de terminal, pensado para Claude Code. Cada sesión es una terminal con su
programa (casi siempre Claude) que sigue en marcha aunque cierres la terminal, y que se vuelve a
abrir al reiniciar el equipo: Claude retoma su conversación. Las manejas desde la consola, el
navegador o el móvil, y si tienes varios equipos, desde un mismo sitio.

**Funciona en Windows, macOS y Linux** (también en WSL 2).

Este repositorio es la herramienta que se instala en cada equipo: las sesiones, la API y la
consola (`nido …`). Desde el navegador o el móvil se maneja con la web o la app de nido,
emparejándolas con tu equipo; todo lo que pasa entre ellos va **cifrado de extremo a extremo**.

| Sistema | Cómo corre |
|---|---|
| Linux con systemd | Servicio de usuario de systemd |
| WSL 2 (con systemd) | Igual que en Linux |
| macOS | Agente de launchd (sin límite de memoria por sesión) |
| Windows 10/11 | Tarea al iniciar sesión (sin `nido attach`: las sesiones se ven en la web y la app) |

## Úsalo gratis con nido.utebo.info

No hace falta montar nada: **[nido.utebo.info](https://nido.utebo.info)** es un hub público y
gratuito con la web de nido. Tu equipo se conecta a él y tú entras desde cualquier navegador (o
desde la app), sin abrir puertos ni tocar el router.

Y es **privado**: el hub solo reenvía bytes cifrados entre tu equipo y tus dispositivos, que son
los únicos que tienen las claves. No puede leer tus sesiones, lo que escribes ni lo que te
contesta Claude. Lo que sí ve: qué equipos están conectados (por un identificador sacado de su
clave, no por su nombre), cuántos dispositivos y cuánto tráfico, y el identificador para mandar
avisos al teléfono (su contenido también va cifrado). La web la sirve el propio hub: guarda la
versión que usaste y te avisa si alguna vez recibes otra distinta.

```sh
# Con nido ya instalado (ver «Instalar», más abajo):
nido hub set wss://nido.utebo.info
nido pair          # da un código nido://pair?… (vale 10 minutos y una vez)
```

Abre [nido.utebo.info](https://nido.utebo.info), pega el código y listo (en Windows, lo mismo
con `nido.exe`). Para añadir otro equipo al mismo sitio, mira [Varios equipos](#varios-equipos).

## Uso

```sh
nido ls                          # sesiones y su estado
nido new api                     # Claude nuevo en la carpeta actual
nido new web -- npm run dev      # cualquier otra orden
nido attach api                  # engancharse desde la terminal (Ctrl+\ para soltar)
nido attach                      # elegir de una lista
nido stop|start|restart|rm <nombre>
nido new api --folder trabajo    # carpetas lógicas, para agrupar
nido mv api personal             # mover de carpeta (sin carpeta: `nido mv api`)
nido import-tmux [--dry-run]     # registrar los Claude que corren en tmux
nido accounts                    # las cuentas de Claude y lo que gasta cada una
nido accounts use 3              # pasar Claude (todas las sesiones) a la cuenta 3
nido accounts login [3]          # entrar en una cuenta nueva, o volver a entrar en la 3
```

Las cuentas de Claude las lleva nido: para entrar da un enlace a la página de Claude (se puede
abrir en cualquier navegador, también el del móvil) y se pega el código que enseña al acabar. Si
había cuentas de cswap (claude-swap), la primera vez se traen con sus números; después, mejor no
usar cswap a la vez: cada uno renovaría las sesiones por su lado y las del otro dejarían de valer.

### Desde Windows Terminal

Un perfil por sesión, o uno que pregunte cuál. En *Configuración → Agregar perfil*, línea de
comandos:

```
wsl.exe -d Debian -- nido attach api
wsl.exe -d Debian -- nido attach
```

`Ctrl+\` suelta la sesión (sigue en marcha); cerrar la pestaña hace lo mismo.

### El móvil y el navegador

```sh
nido pair                        # QR para emparejar un dispositivo (vale 10 minutos y una vez)
nido phones                      # dispositivos emparejados; `nido phones rm <id>` quita uno
nido hub                         # cómo va la conexión con el hub
nido hub set wss://hub.ejemplo   # usar un hub; `nido hub off` lo quita
nido hub test                    # aviso de prueba a los dispositivos
```

Cómo funciona:

- **Sin puertos abiertos.** nido abre una conexión saliente con un *hub*, y el móvil o el
  navegador se unen a ella. El hub solo reenvía bytes cifrados y manda los avisos (con su
  contenido también cifrado).
- **Cifrado de extremo a extremo** con [Noise](https://noiseprotocol.org/)
  (`Noise_IK_25519_ChaChaPoly_SHA256`, y `IKpsk1` con el secreto del QR para emparejar). Ni el
  hub ni nadie en el camino puede leer las sesiones, la conversación o lo que se teclea.
- **Identidad.** La de tu equipo es la clave de `~/.config/nido/hub.key`: no la compartas, y si
  se pierde hay que volver a emparejar los dispositivos. Cada dispositivo tiene su propia clave y
  su propio token, y `nido phones rm` lo deja fuera en el momento.
- **En casa**, si nido escucha en la LAN, el móvil puede ir directo, por el mismo canal cifrado.

### Varios equipos

```sh
nido group invite                # en un equipo del grupo: un código (10 minutos, una vez)
nido group join '<código>'       # en el equipo nuevo, conectado al mismo hub
nido group                       # los equipos del grupo; `nido group rm <id>` saca uno
```

Los equipos de un grupo comparten qué dispositivos están autorizados: un móvil o navegador
emparejado con cualquiera sirve para todos, y quitarlo en uno lo quita en todos. Todos los equipos
son iguales. Se sincronizan entre ellos por el hub, cifrado de extremo a extremo; el hub no ve la
lista. El nombre de cada equipo es el del sistema, o el de `NIDO_NAME`.

## La API

Solo se atiende en este equipo (`127.0.0.1`), con `Authorization: Bearer <token>`: el de
`~/.config/nido/token` (la consola) o el de un dispositivo emparejado (que llega por el canal
cifrado). Desde fuera de este equipo solo responde el canal cifrado (`/api/v1/e2e`).

## Instalar

Hay ejecutables para Linux (x86_64 y ARM64), macOS (Apple Silicon e Intel) y Windows (x86_64) en
[Releases](https://github.com/jleal52/nido/releases). Cada versión trae `SHA256SUMS` para
comprobarlos.

**Linux** (y WSL 2), con systemd de usuario. Cambia `x86_64` por `aarch64` en ARM:

```sh
curl -fsSL https://github.com/jleal52/nido/releases/latest/download/nido-x86_64-unknown-linux-gnu.tar.gz | tar -xz
cd nido-x86_64-unknown-linux-gnu
install -Dm755 nido ~/.local/bin/nido
install -Dm644 contrib/nido.service ~/.config/systemd/user/nido.service
systemctl --user daemon-reload && systemctl --user enable --now nido
```

**macOS**, con un agente de launchd. Cambia `aarch64` por `x86_64` en un Mac con Intel:

```sh
curl -fsSL https://github.com/jleal52/nido/releases/latest/download/nido-aarch64-apple-darwin.tar.gz | tar -xz
cd nido-aarch64-apple-darwin
install -d ~/.local/bin ~/Library/LaunchAgents
install -m755 nido ~/.local/bin/nido
sed "s/USUARIO/$(whoami)/g" contrib/nido.plist > ~/Library/LaunchAgents/io.github.jleal52.nido.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.jleal52.nido.plist
```

El ejecutable no está firmado por Apple. Bajado con `curl`, como arriba, macOS lo deja correr; si
lo bajas con el navegador y no te deja abrirlo, quítale la marca de descarga con
`xattr -d com.apple.quarantine ~/.local/bin/nido`.

**Windows 10/11** (PowerShell), con una tarea programada que arranca nido al iniciar sesión, con
tu usuario y sin ventana (`nidow.exe` lo lanza sin consola):

```powershell
$zip = "$env:TEMP\nido.zip"
Invoke-WebRequest https://github.com/jleal52/nido/releases/latest/download/nido-x86_64-pc-windows-msvc.zip -OutFile $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\nido" -Force
New-Item -ItemType Directory -Force ~\.local\bin | Out-Null
Copy-Item "$env:TEMP\nido\nido*.exe" ~\.local\bin\
~\.local\bin\nido.exe service install          # --name otro-nombre: como sale en la web y la app
```

Para que `nido` funcione en cualquier terminal, añade `~/.local/bin` (en Windows,
`%USERPROFILE%\.local\bin`) al `PATH` si no lo está.

### Compilarlo tú

Con Rust 1.88 o más nuevo ([rustup](https://rustup.rs)):

```sh
git clone https://github.com/jleal52/nido && cd nido
cargo build --release      # deja target/release/nido (y en Windows, también nidow.exe)
```

y luego los mismos pasos de arriba con `target/release/nido` en lugar del ejecutable descargado.

En Windows escucha en `127.0.0.1:7691` (el 7690 puede ser del WSL del mismo equipo), el registro
está en `~\.local\state\nido\serve.log` y `nido service uninstall` quita la tarea. Cerrar la
sesión de Windows cierra las sesiones de nido; al volver a entrar se abren las de autoarranque.

Para actualizarlo: Windows no deja reemplazar un `.exe` en marcha, pero sí renombrarlo. Renombra
`nido.exe` y `nidow.exe` (p. ej. a `nido.old.exe`), copia los nuevos y reinicia la tarea
(`schtasks /End /TN nido` y `schtasks /Run /TN nido`): el gestor se reinicia y las sesiones siguen.

Por defecto escucha en `127.0.0.1:7690`. Para que el móvil llegue directo en casa, añade la
dirección de la LAN con `NIDO_BIND` en un *drop-in* de la unidad
(`~/.config/systemd/user/nido.service.d/bind.conf`): por ella solo se atiende el canal cifrado.

Reiniciar el gestor no toca las sesiones: cada una es su propia unidad (`nido-s-<id>`, con
`MemoryMax=4G`).
