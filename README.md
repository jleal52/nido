# nido

Gestor de sesiones de terminal, pensado para Claude Code. Cada sesión es una terminal con su
programa (casi siempre Claude) que sigue en marcha aunque cierres la terminal, y que se vuelve a
abrir al reiniciar el equipo: Claude retoma su conversación.

Este repositorio es el **cliente** que corre en tu equipo: las sesiones, la API y la consola
(`nido …`). Desde el móvil o el navegador se maneja con la app de nido, emparejándola con tu
equipo; todo lo que pasa entre ellos va **cifrado de extremo a extremo**.

| Sistema | Estado |
|---|---|
| Linux con systemd | Funciona |
| WSL 2 (con systemd) | Funciona |
| macOS | Pendiente (arranque con launchd y leer procesos sin `/proc`) |

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
nido accounts                    # uso de cada cuenta de Claude (vía cswap)
nido accounts use 3              # pasar a la cuenta 3
```

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

Hace falta Rust 1.88 o más nuevo y systemd de usuario.

```sh
git clone https://github.com/jleal52/nido && cd nido
cargo build --release
install -Dm755 target/release/nido ~/.local/bin/nido
install -Dm644 contrib/nido.service ~/.config/systemd/user/nido.service
systemctl --user daemon-reload && systemctl --user enable --now nido
```

Por defecto escucha en `127.0.0.1:7690`. Para que el móvil llegue directo en casa, añade la
dirección de la LAN con `NIDO_BIND` en un *drop-in* de la unidad
(`~/.config/systemd/user/nido.service.d/bind.conf`): por ella solo se atiende el canal cifrado.

Reiniciar el gestor no toca las sesiones: cada una es su propia unidad (`nido-s-<id>`, con
`MemoryMax=4G`).
