# nido

[English](README.md) · **Español**

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

Los mensajes de nido siguen tu idioma: la web y la app están en inglés, o en español si el
navegador o el teléfono están en español. La consola, de momento, en español.

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
Con `NIDO_ACCOUNTS=off`, un nido no toca las cuentas (para uno de pruebas o de demostración junto
al de verdad).

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
nido pair --days 30              # código reutilizable 30 días, solo para equipos de demo (`--stop` lo retira)
nido phones                      # dispositivos emparejados; `nido phones rm <id>` quita uno
nido hub                         # cómo va la conexión con el hub
nido hub set wss://hub.ejemplo   # usar un hub; `nido hub off` lo quita
nido hub test                    # aviso de prueba a los dispositivos
```

Si nido está conectado a un hub, el QR de `nido pair` es un enlace a la web del hub
(`https://<hub>/app#nido=…`, con el código detrás del `#`, que el navegador no manda al servidor):
la cámara del móvil abre la web, y la app de nido también lo lee. Sin hub, el QR es el código
`nido://pair?…`, para la app. `nido pair` escribe siempre también ese código, para pegarlo en la
app o en la web.

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

## Sesiones de Claude que trabajan juntas (el MCP de nido)

Cada Claude que lanza nido tiene el servidor MCP de nido (`nido mcp`), con estas herramientas:

- `sessions`: las sesiones de Claude de todos los equipos de tu grupo, con su estado y `since`
  (cuándo arrancó, si está en marcha, o cuándo se paró).
- `send`: un mensaje a otra sesión. Le llega por un
  [channel](https://code.claude.com/docs/en/channels) de Claude Code, y la respuesta vuelve igual.
- `read`: lo que ha dicho una sesión desde la última vez que se leyó, su estado y su pregunta
  pendiente.
- `open`, `answer` y `close`: abrir una sesión de Claude nueva (una hija) en una carpeta de
  cualquier equipo del grupo, contestar sus preguntas y peticiones de permiso, y cerrarla.
- `start` y `stop`: arrancar una sesión de Claude parada de cualquier equipo del grupo, retomando
  su conversación, y parar una (su conversación se guarda). `stop` no para una sesión que está
  trabajando, tiene una pregunta pendiente o tiene subagentes u órdenes en segundo plano, salvo con
  `force`; una sesión no se para a sí misma.
- `alert`: un aviso a tu móvil (y una 🔔 en `nido ls` y en las apps), para cuando pediste que te
  avisaran de algo.
- `secret`: te pide un token o una contraseña sin que pase por la conversación (ver
  [Secretos, textos y ficheros](#secretos-textos-y-ficheros)).
- `clipboard`: un texto para que lo copies (un recuadro con un botón Copiar en la web y la app),
  y un aviso.
- `send_file`: una copia de un fichero para abrirla o guardarla desde la web o la app, y un aviso.
- `plan_set`, `plan_task`, `plan_get` y `plan_clear`: un plan de fases y tareas del trabajo de la
  sesión, que ves al día en la web y en la app (un anillo con el % en la lista de sesiones y un
  panel en la conversación). `plan_set` sustituye el plan entero (`{title?, phases: [{id, name,
  tasks: [{id, name, status?, note?, by?}]}]}`); `plan_task` cambia una tarea (`{id, status,
  note?, by?}`; `""` quita la nota o `by`). Los estados son `pending`, `in_progress`, `done`,
  `skipped`, `blocked` y `failed`; las horas (`started`, `finished`) las pone nido. `by` dice quién
  hace una tarea: otra sesión (`equipo/id`, que la web y la app enlazan) o un texto corto.
  `plan_get` da el plan con sus horas (el de otra sesión, solo para leer, con `session`);
  `plan_clear` lo borra. El plan se guarda con la sesión (sigue tras reiniciar y tras `/clear`) y
  se va con `nido rm`. Ids de hasta 64 letras, dígitos, `.`, `_` o `-`, únicos en el plan; como
  mucho 50 fases y 500 tareas.

Lo que nido cambia en tu configuración de Claude:

- Cada vez que arranca `nido serve`, registra el servidor en la configuración de usuario de
  Claude (`claude mcp add -s user nido -- <ruta de nido> mcp`), en `~/.claude` y en cada carpeta
  de perfil (`CLAUDE_CONFIG_DIR`) que usen tus sesiones. Si ya está, no lo toca; si apunta a otro
  `nido`, lo cambia.
- Las sesiones de Claude que lanza llevan `--dangerously-load-development-channels server:nido`
  (los channels están en vista previa, y el de nido es un channel de desarrollo) y
  `--allowedTools mcp__nido__sessions mcp__nido__read mcp__nido__send mcp__nido__alert
  mcp__nido__secret mcp__nido__clipboard mcp__nido__send_file mcp__nido__plan_set
  mcp__nido__plan_task mcp__nido__plan_get mcp__nido__plan_clear`. Al cargar un channel de
  desarrollo, Claude enseña un aviso al arrancar: nido lo acepta por ti, pero solo si la lista es
  exactamente `server:nido`.
- Solo esas once herramientas van sin preguntar: miran, hablan, o te piden o te mandan algo a ti
  (`secret` lo controla su propio diálogo), o llevan el plan de la propia sesión. `open`, `answer`, `close`, `start` y `stop` actúan
  sobre otras sesiones, así que pasan por el permiso de siempre de Claude (en modo auto, decide su
  clasificador).
- También llevan `--disallowedTools "Read(//<state>/secrets/**)" "Edit(//<state>/secrets/**)"`,
  con la ruta absoluta de la carpeta de secretos de nido (y también la de verdad, si hay un enlace
  por medio). Claude aplica la regla de `Read` a su herramienta Read y a `cat`, `head`, `<`… en
  Bash, y la de `Edit` a Edit y Write: así Claude no mete un secreto en la conversación por un
  descuido.
- Las opciones del channel solo se ponen si tu Claude Code las conoce. Con uno más antiguo, la
  sesión arranca sin ellas y `sessions` enseña un `channel_error`.

**Planes Team y Enterprise**: un Owner tiene que activar los channels en la organización
(*Organization settings → Claude Code → Channels*, o `channelsEnabled` en los ajustes
gestionados). Mientras tanto las herramientas funcionan pero los mensajes no llegan: nido lee el
aviso de Claude y `sessions` enseña `channel_error: "blocked by org policy"` en esa sesión.

### Secretos, textos y ficheros

Cuando una sesión llama a `secret`, se abre un diálogo en la web y la app de nido con el nombre
del secreto y el motivo de la sesión, y te llega un aviso al móvil. Lo que escribes va cifrado de
extremo a extremo a ese equipo y se guarda en un fichero privado; la sesión recibe la ruta del
fichero, nunca el valor. Puedes hacer que se recuerde 2 horas, 7 días o siempre, para todas las
sesiones de ese equipo (nunca en otros equipos); si no, es solo para esa sesión. `secret` espera
hasta 15 minutos a que contestes.

La sesión usa el secreto de una de estas dos maneras:

- Con su ruta, para los programas que lo leen de un fichero (`--password-file <ruta>`).
- Con `nido secrets run <nombre> --env VAR -- <orden> [args…]`, para los que lo quieren en una
  variable de entorno: ejecuta la orden con el secreto en `VAR` y sin enseñarlo, y sale con el
  código de la orden. Usa el mismo que devolvió `secret`, así que solo funciona dentro de una
  sesión de nido (`NIDO_SESSION`). Si `nido` no está en el `PATH` de la sesión, `secret` devuelve
  también su ruta completa: va entre comillas dobles, como la ruta del secreto, porque pueden
  tener espacios. En Windows, las órdenes `.cmd` y `.bat` (`npm`, `npx`…) van con `cmd /c`:
  `nido secrets run npm-token --env NPM_TOKEN -- cmd /c npm publish`.

Si un programa solo admite el secreto como argumento, Claude te lo dice en lugar de usarlo.

Dónde se guardan y cuánto duran (`<state>` es `~/.local/state/nido`, `$XDG_STATE_HOME/nido` o
`$NIDO_HOME/state`; en Windows, `%USERPROFILE%\.local\state\nido`):

- Los secretos, en `<state>/secrets/`, que solo puede leer tu usuario. Uno recordado dura hasta
  que caduca o lo olvidas; uno sin recordar se borra al cerrar su sesión, o a las 24 horas. Un
  secreto puede tener hasta 16 KB.
- Lo que mandan `clipboard` y `send_file`, en `<state>/outbox/`: dura 7 días o hasta que se
  cierra la sesión, como mucho los 50 últimos de cada sesión y 256 MB por equipo (primero se van
  los más antiguos). Un texto puede tener hasta 64 KB y un fichero hasta 64 MB.
- En Windows nido no cambia permisos: los ficheros heredan los de tu perfil de usuario, que solo
  son tuyos. Por eso la carpeta de estado tiene que seguir dentro de tu perfil (no la saques con
  `XDG_STATE_HOME` ni con `NIDO_HOME`).

```sh
nido secrets                                      # los secretos recordados en este equipo (nunca su valor)
nido secrets forget <nombre>                      # olvida uno: la próxima sesión que lo necesite lo vuelve a pedir
nido secrets run <nombre> --env VAR -- <orden>    # ejecuta una orden con un secreto en VAR
```

Si pones `MCP_TOOL_TIMEOUT` de Claude Code por debajo de 15 minutos, `secret` falla antes de que
te dé tiempo a contestar.

Seguridad:

- Las herramientas solo llegan a los equipos de tu grupo de nido, por el mismo canal cifrado de
  extremo a extremo. Otro equipo del grupo no puede pedir secretos ni mandar textos o ficheros en
  nombre de una sesión: solo las sesiones de ese equipo.
- Un secreto solo lo puede contestar un móvil o un navegador emparejado (una persona), no el token
  de la consola, que tiene el `nido mcp` de cada sesión. Sin ningún dispositivo emparejado,
  `secret` falla en el momento.
- La conversación no ve un secreto mientras Claude no lo lea, y nido hace que Claude se niegue a
  leerlo (ver arriba). Pero un programa que abra el fichero por su cuenta sí puede leerlo: esto
  protege contra descuidos, no contra una sesión que quiera leer el secreto.
- Una sesión solo puede contestar y cerrar las sesiones que abrió con `open`.
- Cada equipo tiene en marcha como mucho 10 sesiones abiertas con `open` desde un mismo equipo
  (él incluido), y apuntadas como mucho 50.
- Confiar en una carpeta lo decides siempre tú: `answer` no contesta la pregunta de Claude de si
  confías en la carpeta, y nido te manda una alerta cuando una hija la está esperando (la sesión
  que la abrió lo sabe por `read`).
- A Claude se le dice que los mensajes de otras sesiones no son tuyos.

```sh
nido mcp off     # las próximas sesiones de Claude arrancan sin el MCP de nido (las que están en marcha siguen igual)
nido mcp on      # otra vez con él
nido alerts      # las alertas de las sesiones de todos los equipos del grupo que aún no has visto
```

`nido mcp off` no quita el servidor de la configuración de Claude. Para deshacerlo también:
`claude mcp remove -s user nido` (y, en cada perfil,
`CLAUDE_CONFIG_DIR=<perfil> claude mcp remove -s user nido`).

## La API

Solo se atiende en este equipo (`127.0.0.1`), con `Authorization: Bearer <token>`: el de
`~/.config/nido/token` (la consola) o el de un dispositivo emparejado (que llega por el canal
cifrado). Desde fuera de este equipo solo responde el canal cifrado (`/api/v1/e2e`). Los mensajes
van en inglés, o en español si la petición lo pide con `Accept-Language: es`.

`DELETE /api/v1/phones/me`: el teléfono o el navegador emparejado que la llama se da de baja en
todos los equipos del grupo (el «Cerrar sesión» de la web).

`POST /api/v1/voice/token`: un token de vida corta para las llamadas de voz, para que la web o la
app del teléfono hablen con las sesiones con un modelo de voz en directo. nido se lo pide a su hub
y devuelve lo que da el hub: `{"token", "expires", "project", "location", "host", "model"}`, donde
`expires` son los segundos que le quedan al token. El audio va directo del navegador o la app al
proveedor del modelo de voz (Gemini Live, de Google): no pasa ni por nido ni por el hub, que solo
da el token. Solo un teléfono o un navegador emparejado puede pedirlo (`403` con el token de
`~/.config/nido/token`). Errores: `503` si nido no está conectado a un hub (o la conexión se cae
mientras lo pide), `501` si el hub no ofrece voz (un hub anterior que no contesta cuenta como eso,
a los 30 segundos), `429` si este equipo llegó al límite de voz del hub, `502` para cualquier otro
error del hub. Un nido anterior, sin esta ruta, contesta `404`.

La memoria de la voz, para que una llamada siga donde se quedaron las anteriores: las notas que
apunta el agente de voz cuando le pides que recuerde algo (hasta 50, de hasta 300 caracteres; una
más larga se corta), el resumen de cada una de las últimas 20 llamadas (o, si el resumen no llegó,
una bitácora de lo que se dijo y se hizo, hasta 4000 caracteres; de un texto más largo se queda el
final, desde el principio de una línea). No guarda el handle para retomar una conversación con el
modelo de voz: eso es del hub. Se guarda en `<state>/voice-memory.json` (`<state>` como en
«Secretos, textos y ficheros»: normalmente `~/.local/state/nido`), solo legible por tu usuario, en
el equipo que recibe las peticiones (la web y la app usan el equipo con el que se emparejaron). Cada equipo del
grupo tiene la suya y no se sincronizan; tampoco va al hub. Solo un teléfono o un navegador
emparejado puede leerla o cambiarla (`403` con el token de `~/.config/nido/token`). Para borrarla,
borra ese fichero: nido lo lee en cada petición, así que vale al momento, también con nido en
marcha.

- `GET /api/v1/voice/memory` → `{"notes": [{"id", "text", "at"}], "calls": [{"id", "at",
  "kind": "resumen" | "bitácora", "text"}]}` (`at` en ms desde 1970).
- `POST /api/v1/voice/memory/notes` `{"text"}` → `{"id"}`; `409` si ya hay 50 notas.
  `DELETE /api/v1/voice/memory/notes/{id}` → `204`, o `404` si no hay esa nota.
- `POST /api/v1/voice/memory/calls` `{"id", "kind", "text"}` → `204`: guarda el resumen
  (`"resumen"`) o la bitácora (`"bitácora"`) de la llamada `id`, en lugar de la que tuviera ese id
  (una bitácora nunca sustituye un resumen).

Lo que no vale (un texto vacío, un `kind` desconocido, un id de llamada mal) da `400`; un cuerpo
de más de 64 KiB, `413` (en texto plano, del servidor web), y un fichero de memoria que nido no
puede leer o escribir, `500`. Al leerlo, uno que nido no entiende (por ejemplo, tocado a mano y
mal) se aparta como `voice-memory.json.bad` (o `voice-memory.json.bad.<ms>`, si ya hay uno) y la
memoria empieza vacía; si no se puede apartar, también `500`, para que nada lo pise. Un nido
anterior contesta `404`.

`POST /api/v1/voice/consultas` → `{"cwd": "<ruta absoluta>"}`: la carpeta vacía donde trabaja la
sesión de Claude del agente de voz para las preguntas generales, para que no lea lo que hay en tu
carpeta personal (en modo manual Claude lee sin preguntar los ficheros de su carpeta, y lo que lee
vuelve al modelo de voz). Es siempre `<state>/consultas`, se crea si falta y solo es legible por tu
usuario (`0700`); la petición no puede elegir otra, y nido no pone nada dentro. Como la memoria,
solo puede pedirla un teléfono o un navegador emparejado (`403` con el token de
`~/.config/nido/token`). Si en esa ruta ya hay algo que no es una carpeta (un fichero, o un
enlace, aunque lleve a una carpeta), o la carpeta no se puede crear, contesta `500` y deja lo que
haya como está. Un nido anterior contesta `404`.

`POST /api/v1/pair` (con `{"days": 30}` opcional, para un código reutilizable) crea un código para
emparejar y devuelve `{"uri": "nido://pair?…", "expires", "reusable",
"direct", "hub"}`. Si nido está conectado al hub, también `link`, la web del hub con el código
detrás del `#`: `<hub>/app#nido=<uri codificado como encodeURIComponent>` (por ejemplo
`https://hub.example.org/app#nido=nido%3A%2F%2Fpair%3F…`); sin hub, no va `link`.
`DELETE /api/v1/pair` retira el código reutilizable.

### Buscar en las conversaciones

`GET /api/v1/search?q=<texto>&limit=50` busca en la conversación actual de cada sesión con Claude
de este equipo (la que enseña la vista chat; no las de antes de un `/clear` o de reiniciar). Solo
lo que escribiste tú y lo que contestó Claude, como texto: ni las llamadas a herramientas ni lo que
devuelven, ni las órdenes (`/model`…), ni los avisos de tareas en segundo plano, ni los subagentes,
ni los mensajes internos de Claude Code, ni los que pone Claude Code en nombre de Claude (límite de
uso, errores de la API, `<synthetic>`), ni el resumen que se escribe al compactar la conversación.

- Mayúsculas y acentos no cuentan: los dos lados se pasan a minúsculas y se descomponen (NFD) sin
  las marcas combinantes, así que `codigo` encuentra `Código`.
- `q` se parte por los espacios y todas las palabras tienen que estar en el mensaje (Y); se
  resaltan todas. `400` (`{"error": …}`) si `q`, sin los espacios de los lados, tiene menos de 2
  caracteres, o si al normalizarla no queda nada que buscar.
- `limit`: 50 por defecto, como mucho 200 (uno mayor cuenta como 200).

```json
{
  "sessions": ["3f9c1a2b"],
  "messages": [
    {
      "session": "3f9c1a2b",
      "sid": "8e1b…",
      "at": 1048576,
      "id": "5d2e…",
      "ts": 1794478830123,
      "role": "assistant",
      "text": "…el código del login está en src/auth.rs, la función que comprueba el token…",
      "hits": [[4, 10]]
    }
  ],
  "truncated": false
}
```

- `sessions`: los ids de las sesiones cuyo nombre, carpeta o directorio de trabajo contienen todas
  las palabras (normalizadas igual).
- `messages`: lo más reciente primero (por `ts` y luego por posición), como mucho `limit`.
  `session` es la sesión de nido, `sid` su conversación de Claude (`claude.session_id`), `at` e
  `id` los del mensaje en el chat (ver abajo: se abre con `chat?around=<at>&sid=<sid>`), `ts`
  cuándo se escribió (ms desde 1970; `0` si no se sabe), `role` `"user"` o `"assistant"`.
- `text`: unos 160 caracteres del mensaje alrededor de la primera coincidencia, con `…` donde se
  corta y los saltos de línea como espacios. `hits`: dónde están las palabras en `text`, como
  posiciones `[inicio, fin)` en **caracteres Unicode (code points), no en unidades UTF-16 ni en
  bytes** (un emoji es un carácter; en JavaScript, sobre `[...text]`, no sobre `text`). Ordenadas,
  y las que se solapan, juntas.
- `truncated`: alguna conversación tiene más texto del que nido guarda para buscar (8 MB de texto
  por conversación, lo más reciente), así que en lo más antiguo no se ha buscado.

El índice está en memoria: se hace la primera vez que se busca (lee las transcripciones enteras,
así que tarda lo que se tarde en leerlas del disco) y después solo se lee lo nuevo de cada una,
así que las búsquedas siguientes tardan milisegundos. Vuelve a empezar si cambia la conversación
de la sesión o se rehace la transcripción.

### El chat alrededor de un mensaje

`GET /api/v1/sessions/{id}/chat` (y los mensajes `chat` de `/api/v1/sessions/{id}/live`) dan a cada
elemento un `at`: dónde empieza su línea en la transcripción, en bytes. Identifica el mensaje (los
de una misma línea, p. ej. el texto de Claude y una llamada a una herramienta, lo comparten) y es lo
que devuelve la búsqueda.

`GET /api/v1/sessions/{id}/chat?around=<at>&before=30&after=30&sid=<sid>` devuelve, en vez del final
de la conversación, hasta `before` elementos antes de `at` y hasta `after` desde `at` en adelante,
con la misma forma que sin `around` y unos campos más:

```json
{
  "session_id": "8e1b…",
  "items": [{"kind": "user", "id": "…", "at": 1040002, "ts": "…", "text": "…", "images": 0}, …],
  "results": [],
  "start": 1040002,
  "offset": 1061377,
  "more_before": true,
  "more_after": true,
  "truncated": true,
  "reset": false
}
```

- Son líneas enteras: a cada lado puede venir uno o dos elementos más de los pedidos, para no
  partir los de una línea. Cada llamada a una herramienta de la ventana viene con su resultado si
  ya está en la transcripción, aunque la línea del resultado quede después de la ventana (nido
  mira hasta 8 MB más allá).
- `start`: dónde empieza la primera línea de la ventana. Lo **anterior**:
  `around=<start>&before=N&after=0` (solo lo que hay antes de `start`).
- `offset`: justo después de la última línea de la ventana. Lo **siguiente**:
  `around=<offset>&before=0&after=N` (solo lo que hay desde `offset`). `results` trae los
  resultados que traen las líneas de la ventana para herramientas de ventanas anteriores
  (`tool_id`, como en `/live`).
- `more_before` / `more_after`: si hay algo antes de `start` / después de `offset`. `truncated` es
  `more_before` y `reset` es `false`, así que un cliente antiguo lo lee como un primer trozo.
- **Volver al final y en vivo**: si `more_after` es `false`, después de `offset` no falta nada:
  conecta `/live?sid=<session_id>&offset=<offset>` y desde ahí llegan los mensajes y resultados
  nuevos. Si no, abre `/live` desde el principio como siempre (`offset=0&limit=…`).
- `before` y `after` son 30 por defecto y como mucho 1000; `0` vale (`before=0&after=0` solo dice,
  con `more_*`, si hay algo a cada lado).
- Un `at` que no es el principio de una línea cuenta como el principio de la siguiente; uno más
  allá del final devuelve los últimos `before` elementos con `more_after: false`. Si la sesión aún
  no tiene transcripción, la ventana va vacía.
- `sid`: la conversación de ese `at` (el `sid` de la búsqueda). `409` si ya no es la actual de la
  sesión (p. ej. tras un `/clear`): `at` apuntaría a otra cosa.
- Solo se lee la ventana (hacia atrás desde `at`): no depende del tamaño de la transcripción. Lo
  mismo vale para la primera carga sin `around` (`offset=0` con `limit`, en `/chat` y en `/live`):
  se leen los últimos mensajes hacia atrás desde el final.

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

Los ejecutables de macOS van firmados con un Developer ID: macOS reconoce nido de una versión a
otra y mantiene los permisos de privacidad que le diste (a Documentos o Descargas, por ejemplo),
así que las sesiones no se quedan esperando otra vez ese diálogo tras cada actualización. No están
notarizados. Bajado con `curl`, como arriba, macOS lo deja correr; si lo bajas con el navegador
y no te deja abrirlo, quítale la marca de descarga con
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
