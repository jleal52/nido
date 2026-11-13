//! El idioma de lo que la API manda a la app y a la web para enseñar (errores, avisos). nido
//! escribe sus mensajes en español; si la petición no prefiere español (`Accept-Language`), salen
//! en inglés, traducidos en la frontera con [`tr`]. La terminal no pasa por aquí, salvo lo que
//! leen otros programas (`nido clip`, con [`env_lang`]).
//!
//! El idioma va con la tarea de la petición (`LANG`): lo pone [`lang_layer`] y lo leen [`lang`] y
//! [`tr`]. Lo que no corre en esa tarea (`spawn_blocking`, `on_upgrade`, tareas de fondo) no lo
//! ve: se traduce al devolverlo, en el handler, o se le pasa con [`scope`].

use std::future::Future;

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Es,
    En,
}

tokio::task_local! {
    static LANG: Lang;
}

/// El idioma de la petición en curso; fuera de una, inglés.
pub fn lang() -> Lang {
    LANG.try_with(|l| *l).unwrap_or(Lang::En)
}

/// Corre `f` con este idioma (para lo que sale de la tarea de la petición, como un WebSocket).
pub async fn scope<F: Future>(lang: Lang, f: F) -> F::Output {
    LANG.scope(lang, f).await
}

/// El idioma de la terminal, para las órdenes que leen otros programas (`nido clip`, que corre
/// en una sesión y lee Claude): `Es` si el primero de `LC_ALL`, `LC_MESSAGES` y `LANG` que tenga
/// valor es español (`es_ES.UTF-8`…); si no, `En`.
pub fn env_lang() -> Lang {
    lang_of_env(|k| std::env::var(k).ok())
}

fn lang_of_env(get: impl Fn(&str) -> Option<String>) -> Lang {
    let value = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let primary = value.split(['_', '-', '.', '@']).next().unwrap_or("");
    if primary.eq_ignore_ascii_case("es") {
        Lang::Es
    } else {
        Lang::En
    }
}

/// `msg` en `lang`: tal cual en español; si no, su traducción.
pub fn in_lang(lang: Lang, msg: &str) -> String {
    match lang {
        Lang::Es => msg.to_owned(),
        Lang::En => english(msg),
    }
}

/// `Es` si el idioma preferido (el de más `q`; a igualdad, el primero) es español (`es`,
/// `es-ES`…); si no, `En`.
pub fn parse_accept_language(h: &str) -> Lang {
    let mut best: Option<(f32, &str)> = None;
    for part in h.split(',') {
        let mut it = part.split(';');
        let tag = it.next().unwrap_or("").trim();
        if tag.is_empty() || tag == "*" {
            continue;
        }
        let mut q = 1.0f32;
        for p in it {
            if let Some(v) = p.trim().strip_prefix("q=") {
                q = v.trim().parse().unwrap_or(0.0);
            }
        }
        if q <= 0.0 {
            continue;
        }
        if best.is_none_or(|(bq, _)| q > bq) {
            best = Some((q, tag));
        }
    }
    match best {
        Some((_, tag)) => {
            let primary = tag.split(['-', '_']).next().unwrap_or("");
            if primary.eq_ignore_ascii_case("es") {
                Lang::Es
            } else {
                Lang::En
            }
        }
        None => Lang::En,
    }
}

/// Lee `Accept-Language` y atiende la petición con ese idioma.
pub async fn lang_layer(req: Request, next: Next) -> Response {
    let lang = req
        .headers()
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Lang::En, parse_accept_language);
    LANG.scope(lang, next.run(req)).await
}

/// El mensaje en el idioma de la petición: tal cual en español; si no, su traducción (o el
/// original, si no está en la tabla).
pub fn tr(msg: &str) -> String {
    match lang() {
        Lang::Es => msg.to_owned(),
        Lang::En => english(msg),
    }
}

/// Traduce al inglés con la tabla. Si casan varios patrones, el de más texto fijo (el más
/// concreto: «no existe la carpeta {}» antes que «no existe {}»). Lo que captura cada hueco
/// también se traduce, por si es otro mensaje de nido (rutas, nombres y errores del sistema no
/// casan con nada y se quedan como vienen).
pub fn english(msg: &str) -> String {
    let mut best: Option<(usize, &str, Vec<&str>)> = None;
    for (pattern, template) in TABLE {
        let pieces: Vec<&str> = pattern.split("{}").collect();
        let fixed: usize = pieces.iter().map(|p| p.len()).sum();
        if best.as_ref().is_some_and(|(f, _, _)| *f >= fixed) {
            continue;
        }
        if let Some(caps) = matches(&pieces, msg) {
            best = Some((fixed, template, caps));
        }
    }
    match best {
        Some((_, template, caps)) => {
            let caps: Vec<String> = caps.into_iter().map(english).collect();
            fill(template, &caps)
        }
        None => msg.to_owned(),
    }
}

/// Casa `msg` con los trozos fijos de un patrón (separados por los huecos): devuelve lo que
/// queda en cada hueco (nunca vacío). Prueba cada aparición del trozo siguiente, de la primera a
/// la última, hasta que el resto case.
fn matches<'a>(pieces: &[&str], msg: &'a str) -> Option<Vec<&'a str>> {
    let (first, rest) = pieces.split_first()?;
    let msg = msg.strip_prefix(first)?;
    let mut caps = Vec::with_capacity(rest.len());
    if fill_holes(rest, msg, &mut caps) {
        Some(caps)
    } else {
        None
    }
}

fn fill_holes<'a>(pieces: &[&str], msg: &'a str, caps: &mut Vec<&'a str>) -> bool {
    let Some((next, rest)) = pieces.split_first() else {
        return msg.is_empty();
    };
    if rest.is_empty() {
        // El último hueco llega hasta el trozo final.
        return match msg.strip_suffix(next) {
            Some(c) if !c.is_empty() => {
                caps.push(c);
                true
            }
            _ => false,
        };
    }
    if next.is_empty() {
        return false; // dos huecos seguidos: no se sabe dónde partir
    }
    // Al menos un carácter en el hueco.
    let Some(start) = msg.chars().next().map(char::len_utf8) else {
        return false;
    };
    let mut from = start;
    while let Some(i) = msg[from..].find(next) {
        let at = from + i;
        caps.push(&msg[..at]);
        if fill_holes(rest, &msg[at + next.len()..], caps) {
            return true;
        }
        caps.pop();
        from = at + msg[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// La plantilla con `{0}`, `{1}`… cambiados por lo capturado.
fn fill(template: &str, caps: &[String]) -> String {
    let mut out =
        String::with_capacity(template.len() + caps.iter().map(String::len).sum::<usize>());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        match (digits, after.as_bytes().get(digits)) {
            (1.., Some(b'}')) => {
                let n: usize = after[..digits].parse().unwrap_or(usize::MAX);
                out.push_str(caps.get(n).map_or("", String::as_str));
                rest = &after[digits + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// (mensaje en español, como lo escribe el código con `{}` donde va algo variable; inglés con
/// `{0}`, `{1}`… en el orden que pida la frase).
const TABLE: &[(&str, &str)] = &[
    // server.rs
    ("no existe la carpeta {}", "folder {0} does not exist"),
    ("no se pudo arrancar: {}", "could not start: {0}"),
    ("no hay ninguna sesión {}", "there is no session {0}"),
    ("la sesión no está en marcha", "the session is not running"),
    ("la alerta no tiene texto", "the alert has no text"),
    ("el guardián cerró", "the session holder closed"),
    (
        "el guardián no respondió",
        "the session holder did not respond",
    ),
    (
        "en esta sesión no hay ningún Claude",
        "there is no Claude in this session",
    ),
    (
        "esa conversación ya no es la actual de la sesión",
        "that conversation is no longer the session's current one",
    ),
    (
        "hay que buscar al menos 2 caracteres",
        "search for at least 2 characters",
    ),
    (
        "no se pudo guardar el código: {}",
        "could not save the code: {0}",
    ),
    (
        "para formar un grupo, este equipo tiene que estar conectado al hub (nido hub set …)",
        "to form a group, this machine has to be connected to the hub (nido hub set …)",
    ),
    (
        "para sacar este equipo, hazlo desde otro del grupo",
        "to remove this machine, do it from another one in the group",
    ),
    (
        "ese equipo no está en el grupo",
        "that machine is not in the group",
    ),
    (
        "ese teléfono no está emparejado",
        "that phone is not paired",
    ),
    ("cuenta no válida", "invalid account"),
    (
        "este equipo no está conectado al hub (nido hub)",
        "this machine is not connected to the hub (nido hub)",
    ),
    (
        "el otro equipo usa otro hub ({}); este usa {}",
        "the other machine uses a different hub ({0}); this one uses {1}",
    ),
    (
        "{}: ya no recibe avisos",
        "{0}: no longer receives notifications",
    ),
    (
        "{}: sin conexión con el hub",
        "{0}: not connected to the hub",
    ),
    ("{}: el hub no contestó", "{0}: the hub did not answer"),
    // La voz (`POST /api/v1/voice/token`)
    ("sin conexión con el hub", "not connected to the hub"),
    ("el hub no ofrece voz", "the hub does not offer voice"),
    ("límite de voz", "voice limit reached"),
    (
        "solo un teléfono o una web emparejados pueden llamar",
        "only a paired phone or browser can make a call",
    ),
    // mcp_api.rs
    (
        "la API del MCP solo va con el token de este equipo",
        "the MCP API only takes this machine's token",
    ),
    (
        "no se llamó desde una sesión de nido",
        "not called from a nido session",
    ),
    ("no hay ninguna herramienta {}", "there is no tool {0}"),
    ("argumentos no válidos: {}", "invalid arguments: {0}"),
    ("no existe esa sesión", "no such session"),
    ("modelo no válido: {}", "invalid model: {0}"),
    ("falta el mensaje", "the prompt is missing"),
    (
        "el mensaje pasa de {} KB",
        "the prompt is longer than {0} KB",
    ),
    (
        "no es el Claude de esta sesión",
        "not this session's claude",
    ),
    (
        "solo puedes contestar las sesiones que abriste",
        "you can only answer sessions you opened",
    ),
    (
        "solo puedes cerrar las sesiones que abriste",
        "you can only close sessions you opened",
    ),
    (
        "la sesión espera a que el usuario confíe en la carpeta {}; solo el usuario puede hacerlo (ya se le ha avisado)",
        "the session is waiting for the user to trust the folder {0}; only the user can do that (they have been told)",
    ),
    (
        "la sesión espera a que el usuario confíe en la carpeta {}; solo el usuario puede hacerlo",
        "the session is waiting for the user to trust the folder {0}; only the user can do that",
    ),
    (
        "falta la opción o el texto",
        "the option or the text is missing",
    ),
    (
        "no hay ninguna pregunta pendiente",
        "no question is pending",
    ),
    (
        "la pregunta no admite texto libre",
        "the pending question has no free-text field",
    ),
    (
        "el modelo pasa de {} caracteres",
        "the model is longer than {0} characters",
    ),
    ("falta el texto", "the text is missing"),
    ("el texto pasa de {} KB", "the text is longer than {0} KB"),
    (
        "no puedes mandarte mensajes a ti misma",
        "you cannot send a message to yourself",
    ),
    (
        "«{}» no se puede pedir desde otro equipo",
        "“{0}” cannot be requested from another machine",
    ),
    (
        "falta la sesión que llama",
        "the calling session is missing",
    ),
    (
        "sesión que llama no válida: {}",
        "invalid calling session: {0}",
    ),
    (
        "esa sesión no es de este equipo",
        "that session is not on this machine",
    ),
    ("«{}» no es este equipo", "“{0}” is not this machine"),
    (
        "la sesión que llama no puede ser de este equipo",
        "the calling session cannot be on this machine",
    ),
    (
        "demasiadas sesiones abiertas desde ese equipo; cierra alguna",
        "too many sessions opened from that machine; close some",
    ),
    (
        "demasiadas sesiones abiertas con open en este equipo; cierra alguna",
        "too many sessions opened with open on this machine; close some",
    ),
    // start y stop
    (
        "está trabajando (o tiene una pregunta pendiente): para pararla igualmente, force: true",
        "it is working (or has a pending question): to stop it anyway, force: true",
    ),
    (
        "tiene subagentes u órdenes en segundo plano: para pararla igualmente, force: true",
        "it has subagents or commands running in the background: to stop it anyway, force: true",
    ),
    (
        "una sesión no puede pararse a sí misma",
        "a session cannot stop itself",
    ),
    ("ya estaba parada", "it was already stopped"),
    ("ya estaba en marcha", "it was already running"),
    (
        "arrancó pero no llegó a estar en marcha",
        "it was started but did not come up",
    ),
    // server.rs (canal «peer»)
    (
        "«{}» no es ningún equipo del grupo",
        "“{0}” is not a machine in the group",
    ),
    (
        "hay varios equipos «{}» en el grupo: usa su id",
        "there are several machines “{0}” in the group: use its id",
    ),
    (
        "esa sesión no admite mensajes (no tiene channel)",
        "this session does not accept messages (no channel)",
    ),
    (
        "esa sesión no admite mensajes (no tiene channel): la organización de su cuenta de Claude no permite channels (channelsEnabled)",
        "this session does not accept messages (no channel): the organization of its Claude account does not allow channels (channelsEnabled)",
    ),
    (
        "esa sesión no admite mensajes (no tiene channel): su Claude no tiene el MCP de nido",
        "this session does not accept messages (no channel): its Claude does not have nido's MCP server",
    ),
    (
        "esa sesión no admite mensajes (no tiene channel): su Claude no está en marcha",
        "this session does not accept messages (no channel): its Claude is not running",
    ),
    (
        "esa sesión no admite mensajes (no tiene channel): su Claude Code es anterior a los channels",
        "this session does not accept messages (no channel): its Claude Code predates channels",
    ),
    (
        "la bandeja de esa sesión está llena",
        "the inbox of that session is full",
    ),
    (
        "entregado; la sesión decide si lo atiende",
        "delivered; the session decides whether to act on it",
    ),
    (
        "en cola; se entregará cuando arranque la sesión",
        "queued; it will be delivered when the session starts",
    ),
    (
        "en cola; se entregará cuando arranque la sesión, y ella decide si lo atiende",
        "queued; it will be delivered when the session starts, and the session decides \
         whether to act on it",
    ),
    (
        "en cola; se entregará cuando la sesión acabe de arrancar",
        "queued; it will be delivered when the session has finished starting",
    ),
    (
        "en cola; se entregará cuando la sesión acabe de arrancar, y ella decide si lo atiende",
        "queued; it will be delivered when the session has finished starting, and the \
         session decides whether to act on it",
    ),
    // auth.rs
    (
        "falta el token o no es válido",
        "the token is missing or not valid",
    ),
    (
        "La API de nido solo se atiende en este equipo",
        "The nido API is only served on this machine",
    ),
    ("Host no permitido", "Host not allowed"),
    // registry.rs
    (
        "nombre no válido: {} (letras, números, «-», «_» o «.»; hasta 64)",
        "invalid name: {0} (letters, numbers, “-”, “_” or “.”; up to 64)",
    ),
    (
        "carpeta no válida: {} (hasta 64 caracteres, sin «/»)",
        "invalid folder: {0} (up to 64 characters, no “/”)",
    ),
    (
        "ya hay una sesión llamada {}",
        "there is already a session called {0}",
    ),
    // browse.rs
    ("«{}» no es una ruta completa", "“{0}” is not a full path"),
    ("no existe {}", "{0} does not exist"),
    ("sin permiso para ver {}", "no permission to view {0}"),
    ("no se puede ver {}: {}", "cannot view {0}: {1}"),
    // upload.rs
    ("archivo vacío", "empty file"),
    ("no se pudo guardar: {}", "could not save: {0}"),
    // accounts.rs
    (
        "las cuentas de Claude están desactivadas en este nido (NIDO_ACCOUNTS=off)",
        "Claude accounts are disabled on this nido (NIDO_ACCOUNTS=off)",
    ),
    ("no hay ninguna cuenta así", "there is no such account"),
    (
        "La cuenta {} ya es la activa.",
        "Account {0} is already the active one.",
    ),
    (
        "la sesión guardada de la cuenta {} ya no vale: vuelve a entrar en ella",
        "the saved session of account {0} is no longer valid: sign in to it again",
    ),
    (
        "no hay sesión guardada de esa cuenta",
        "there is no saved session for that account",
    ),
    (
        "no hay sesión guardada de esta cuenta",
        "there is no saved session for this account",
    ),
    (
        "la sesión guardada está incompleta",
        "the saved session is incomplete",
    ),
    (
        "la sesión guardada no se puede renovar",
        "the saved session cannot be renewed",
    ),
    (
        "no se pudo guardar la sesión renovada: {}",
        "could not save the renewed session: {0}",
    ),
    (
        "la sesión ha caducado: hay que volver a entrar",
        "the session has expired: you need to sign in again",
    ),
    (
        "no se pudo renovar la sesión: {}",
        "could not renew the session: {0}",
    ),
    (
        "no se pudo cambiar la cuenta de Claude: {}",
        "could not switch Claude's account: {0}",
    ),
    (
        "no se pudo guardar la sesión de la cuenta {}: {}",
        "could not save the session of account {0}: {1}",
    ),
    (
        "ese código es de otro inicio de sesión: abre el enlace de este",
        "that code belongs to another sign-in: open this one's link",
    ),
    (
        "falta el final del código (lo que va tras «#»): cópialo entero",
        "the end of the code is missing (the part after “#”): copy all of it",
    ),
    (
        "ese inicio de sesión ya no vale (caducó o ya se usó): vuelve a empezar",
        "that sign-in is no longer valid (it expired or was already used): start again",
    ),
    (
        "Claude no dice de quién es la cuenta",
        "Claude does not say whose account it is",
    ),
    (
        "no se pudo poner la sesión a Claude: {}",
        "could not hand the session to Claude: {0}",
    ),
    (
        "no se pudo guardar la sesión: {}",
        "could not save the session: {0}",
    ),
    (
        "es la que usa Claude ahora: cambia antes a otra",
        "it is the one Claude is using now: switch to another one first",
    ),
    ("Anthropic pide esperar", "Anthropic asks to wait"),
    // claude_store.rs
    ("Claude tiene ocupado {}", "Claude is holding {0}"),
    (
        "~/.claude.json no es un objeto",
        "~/.claude.json is not an object",
    ),
    (
        "el llavero no deja leer la cuenta de Claude: {}",
        "the keychain does not allow reading the Claude account: {0}",
    ),
    (
        "el llavero no deja guardar la cuenta de Claude: {}",
        "the keychain does not allow saving the Claude account: {0}",
    ),
    // oauth.rs
    ("falta el código", "the code is missing"),
    (
        "esa dirección no se entiende",
        "that address cannot be understood",
    ),
    (
        "en esa dirección no hay código",
        "there is no code in that address",
    ),
    (
        "el código no está completo: cópialo entero",
        "the code is incomplete: copy all of it",
    ),
    (
        "no se pudo hablar con Claude: {}",
        "could not talk to Claude: {0}",
    ),
    (
        "Claude no acepta ese código (¿caducado o ya usado?): vuelve a empezar",
        "Claude does not accept that code (expired or already used?): start again",
    ),
    (
        "Claude no ha dado la sesión ({}, {})",
        "Claude did not grant the session ({0}, {1})",
    ),
    (
        "Claude no ha dado la sesión ({})",
        "Claude did not grant the session ({0})",
    ),
    (
        "Claude ha contestado algo que no se entiende",
        "Claude replied with something that cannot be understood",
    ),
    ("respuesta que no se entiende", "unintelligible response"),
    (
        "no se pudo pedir el perfil: {}",
        "could not request the profile: {0}",
    ),
    (
        "no se pudo pedir el perfil ({})",
        "could not request the profile ({0})",
    ),
    (
        "el perfil no se entiende",
        "the profile cannot be understood",
    ),
    // group.rs
    (
        "no es un código de grupo de nido (nido://group?…)",
        "this is not a nido group code (nido://group?…)",
    ),
    (
        "al código le falta «{}» o no es válido",
        "the code is missing “{0}” or it is not valid",
    ),
    ("al código le falta el hub", "the code is missing the hub"),
    (
        "al código le falta el equipo",
        "the code is missing the machine",
    ),
    (
        "el código ha caducado: genera otro con «nido group invite»",
        "the code has expired: generate another one with “nido group invite”",
    ),
    // e2e.rs
    (
        "el otro equipo no aceptó (¿código usado o caducado?)",
        "the other machine did not accept (code used or expired?)",
    ),
    (
        "sin conexión o versión antigua",
        "no connection or old version",
    ),
    ("la respuesta es demasiado grande", "the reply is too large"),
    (
        "la petición es demasiado grande",
        "the request is too large",
    ),
    (
        "petición caducada o repetida",
        "expired or repeated request",
    ),
    (
        "respuesta no válida del otro equipo",
        "invalid reply from the other machine",
    ),
    ("petición no válida: {}", "invalid request: {0}"),
    (
        "este equipo no atiende peticiones de otros",
        "this machine does not take requests from others",
    ),
    // hub.rs
    (
        "la dirección del hub tiene que empezar por wss:// o https://",
        "the hub address has to start with wss:// or https://",
    ),
    (
        "la dirección tiene que ser ip:puerto (por ejemplo 203.0.113.5:443)",
        "the address has to be ip:port (for example 203.0.113.5:443)",
    ),
    ("el hub no contesta", "the hub does not answer"),
    ("no hay hub configurado", "no hub is configured"),
    (
        "el otro equipo cerró sin contestar",
        "the other machine closed without answering",
    ),
    (
        "el otro equipo no contesta",
        "the other machine does not answer",
    ),
    ("reto no válido", "invalid challenge"),
    (
        "el hub no mandó el reto",
        "the hub did not send the challenge",
    ),
    ("el hub no dio el id", "the hub did not give the id"),
    (
        "el hub no aceptó la firma ({})",
        "the hub did not accept the signature ({0})",
    ),
    (
        "el hub no aceptó la firma ()",
        "the hub did not accept the signature",
    ),
    // ipc.rs
    (
        "la tubería de la sesión está ocupada",
        "the session pipe is busy",
    ),
    // prompt.rs (contestar a Claude)
    (
        "no hay ningún formulario en pantalla",
        "there is no form on screen",
    ),
    ("el formulario ha desaparecido", "the form has disappeared"),
    (
        "no se despliega la lista de «{}»",
        "the list of “{0}” does not open",
    ),
    (
        "«{}»: no sé qué opción elegir ({})",
        "“{0}”: I don't know which option to choose ({1})",
    ),
    ("no se pudo elegir la opción", "could not choose the option"),
    ("falta «{}»", "“{0}” is missing"),
    (
        "el formulario sigue ahí (¿falta algún campo obligatorio?)",
        "the form is still there (is a required field missing?)",
    ),
    ("el formulario no responde", "the form does not respond"),
    ("el cursor no se mueve", "the cursor does not move"),
    ("no se llega al campo", "cannot reach the field"),
    ("la sesión se ha cerrado", "the session has closed"),
    ("la sesión no contesta", "the session does not answer"),
    (
        "no se ve dónde está el cursor",
        "cannot see where the cursor is",
    ),
    ("el menú ha desaparecido", "the menu has disappeared"),
    (
        "no se pudo llevar el cursor a la opción",
        "could not move the cursor to the option",
    ),
    ("opción sin fila", "option without a row"),
    (
        "en la caja de Claude ya hay algo escrito",
        "there is already something written in Claude's input box",
    ),
    (
        "Claude está esperando una respuesta en un menú: contéstala primero",
        "Claude is waiting for an answer in a menu: answer it first",
    ),
    (
        "el mensaje solo tiene caracteres de control",
        "the message only has control characters",
    ),
    (
        "en pantalla ya no está lo que se confirmó",
        "what was confirmed is no longer on screen",
    ),
    (
        "Claude no ha cogido el texto (¿está arrancando?)",
        "Claude did not take the text (is it starting up?)",
    ),
    (
        "Claude no ha cogido el mensaje: sigue en su caja",
        "Claude did not take the message: it is still in its input box",
    ),
    ("Claude ya no propone eso", "Claude no longer suggests that"),
    (
        "Claude no ha escrito la propuesta",
        "Claude did not write the suggestion",
    ),
    ("falta qué opción", "the option is missing"),
    (
        "no hay ningún menú en pantalla",
        "there is no menu on screen",
    ),
    (
        "en pantalla no está la opción «{}»",
        "the option “{0}” is not on screen",
    ),
    (
        "Claude no ha reaccionado a la opción",
        "Claude did not react to the option",
    ),
    (
        "eligió la opción, pero no se pudo mandar el texto",
        "the option was chosen, but the text could not be sent",
    ),
    (
        "eligió la opción, pero Claude espera en otro menú: no se mandó el texto",
        "the option was chosen, but Claude is waiting in another menu: the text was not sent",
    ),
    (
        "en pantalla no está la pregunta «{}»",
        "the question “{0}” is not on screen",
    ),
    (
        "la pregunta «{}» no tiene la opción «{}»",
        "the question “{0}” has no option “{1}”",
    ),
    (
        "falta la respuesta a «{}»",
        "the answer to “{0}” is missing",
    ),
    ("opción sin número", "option without a number"),
    (
        "esta pregunta no admite texto",
        "this question does not accept text",
    ),
    ("no se encuentra «Next»", "“Next” not found"),
    (
        "Claude no ha pasado de la pregunta «{}»",
        "Claude did not move past the question “{0}”",
    ),
    (
        "no se encuentra «Submit answers»",
        "“Submit answers” not found",
    ),
    (
        "no se pudieron mandar las respuestas",
        "the answers could not be sent",
    ),
    // prompt.rs y server.rs: el modo de permisos y el modelo
    (
        "hay un menú o una pregunta en pantalla",
        "there is a menu or a question on screen",
    ),
    (
        "no se entiende el modo en el pie de Claude",
        "Claude's footer does not show a known mode",
    ),
    (
        "Claude no ha cambiado de modo",
        "Claude did not change mode",
    ),
    (
        "no se llega al modo {} con shift+tab",
        "shift+tab does not reach mode {0}",
    ),
    (
        "Claude está trabajando: espera a que termine",
        "Claude is working: wait until it finishes",
    ),
    (
        "no se ve la caja de escribir de Claude",
        "Claude's input box is not visible",
    ),
    ("Claude no ha cogido /model", "Claude did not take /model"),
    (
        "no se abre el menú de /model",
        "the /model menu does not open",
    ),
    (
        "el menú de /model no deja usar un modelo solo en esta sesión",
        "the /model menu does not offer a model for this session only",
    ),
    (
        "en el menú de /model no está «{}»",
        "“{0}” is not in the /model menu",
    ),
    (
        "el menú de /model ha desaparecido",
        "the /model menu has disappeared",
    ),
    (
        "el cursor del menú de /model no se mueve",
        "the cursor in the /model menu does not move",
    ),
    (
        "Claude no ha dicho que cambió el modelo",
        "Claude did not confirm the model change",
    ),
    // prompt.rs: la cola de Claude
    (
        "Claude no tiene mensajes en cola",
        "Claude has no queued messages",
    ),
    (
        "la cola sigue en la pantalla de Claude",
        "the queued messages are still on Claude's screen",
    ),
    (
        "hay algo escrito en la caja de Claude",
        "there is text in Claude's input box",
    ),
    (
        "Claude no ha sacado la cola a la caja",
        "Claude did not move the queued messages to its input box",
    ),
    (
        "no se pudo vaciar la caja de Claude: el texto de la cola sigue en ella (se ve en la terminal)",
        "could not empty Claude's input box: the queued text is still in it (it can be seen in the terminal)",
    ),
    ("falta el modo", "the mode is missing"),
    ("modo desconocido: «{}»", "unknown mode: “{0}”"),
    ("falta el modelo", "the model is missing"),
    // transcript.rs (avisos dentro de la conversación)
    (
        "Claude resumió la conversación para seguir (se le acababa el contexto)",
        "Claude summarized the conversation to keep going (it was running out of context)",
    ),
    ("Tarea en segundo plano: {}", "Background task: {0}"),
    ("Tarea en segundo plano: aviso", "Background task: notice"),
    // deliver.rs y outbox.rs
    ("el texto está vacío", "the text is empty"),
    (
        "no se pudo guardar la copia: {}",
        "could not save the copy: {0}",
    ),
    ("no existe el fichero {}", "file {0} does not exist"),
    ("no se puede leer {}: {}", "cannot read {0}: {1}"),
    ("{} es una carpeta", "{0} is a folder"),
    ("{} no es un fichero normal", "{0} is not a regular file"),
    (
        "{} está en la carpeta de secretos de nido: no se manda",
        "{0} is in nido's secrets folder: not sent",
    ),
    ("{} pasa de 64 MB ({})", "{0} is over 64 MB ({1})"),
    ("{} pasa de 64 KB", "{0} is over 64 KB"),
    // prompt.rs y server.rs: el modo y el modelo
    (
        "en pantalla está el menú de /model: ciérralo desde la terminal (Esc), aquí no se contesta",
        "the /model menu is on screen: close it from the terminal (Esc), it can't be answered from here",
    ),
    (
        "Claude está trabajando en modo plan: espera a que termine para cambiar de modo",
        "Claude is working in plan mode: wait until it finishes to change the mode",
    ),
    (
        "el cuerpo no es JSON válido: {}",
        "the body is not valid JSON: {0}",
    ),
    // nido clip
    (
        "no responde el gestor en {} ({}); ¿está `nido serve` en marcha?",
        "nido is not answering at {0} ({1}); is `nido serve` running?",
    ),
    (
        "enviado al portapapeles ({} KB)",
        "sent to the clipboard ({0} KB)",
    ),
    (
        "falta o no es válida NIDO_SESSION: `nido clip` es para las órdenes de una sesión de nido",
        "NIDO_SESSION is missing or invalid: `nido clip` is for the commands of a nido session",
    ),
    (
        "la entrada no es texto (UTF-8): para mandar un binario, usa send_file",
        "the input is not text (UTF-8): to send a binary file, use send_file",
    ),
    (
        "falta qué mandar: un fichero, o la salida de una orden (`orden | nido clip`)",
        "nothing to send: give a file, or a command's output (`command | nido clip`)",
    ),
    (
        "{} no es texto (UTF-8): para mandarlo, usa send_file",
        "{0} is not text (UTF-8): to send it, use send_file",
    ),
    (
        "hace falta text o path (uno de los dos)",
        "text or path is required (exactly one of them)",
    ),
    ("sesión no válida", "invalid session"),
    ("ese envío ha caducado", "that item has expired"),
    ("no existe ese envío", "no such item"),
    (
        "no se pudo quitar el envío: {}",
        "could not dismiss the item: {0}",
    ),
    (
        "nombre de secreto no válido: {} (letras, números, «.», «_» o «-»; hasta 64)",
        "invalid secret name: {0} (letters, digits, “.”, “_” or “-”; up to 64)",
    ),
    ("falta el motivo", "the reason is missing"),
    (
        "no hay dónde preguntar: ni teléfono ni web emparejados con nido",
        "nowhere to ask: no phone or browser is paired with nido",
    ),
    (
        "esta sesión ya espera otro secreto",
        "this session is already waiting for another secret",
    ),
    (
        "el usuario no quiso dar «{}»",
        "the user declined to give “{0}”",
    ),
    (
        "nadie contestó en 15 minutos",
        "no answer within 15 minutes",
    ),
    (
        "la sesión se cerró mientras esperaba",
        "the session was closed while waiting",
    ),
    (
        "no se pudo guardar el secreto: {}",
        "could not save the secret: {0}",
    ),
    ("petición no válida", "invalid request"),
    ("el secreto está vacío", "the secret is empty"),
    ("el secreto pasa de 16 KB", "the secret is over 16 KB"),
    (
        "ese secreto ya no espera respuesta",
        "that secret is no longer waiting for an answer",
    ),
    (
        "solo un teléfono o una web emparejados pueden contestar un secreto",
        "only a paired phone or browser can answer a secret",
    ),
    (
        "solo un teléfono o una web emparejados pueden cerrar su sesión",
        "only a paired phone or browser can sign itself out",
    ),
    (
        "no hay ningún secreto recordado {}",
        "there is no remembered secret {0}",
    ),
    // plan.rs: el plan de fases y tareas
    (
        "estado desconocido \"{}\": vale pending, in_progress, done, skipped, blocked o failed",
        "unknown status \"{0}\": use pending, in_progress, done, skipped, blocked or failed",
    ),
    // Un estado vacío dentro de `phases` llega a nido (el MCP no mira lo de dentro).
    (
        "estado desconocido \"\": vale pending, in_progress, done, skipped, blocked o failed",
        "unknown status \"\": use pending, in_progress, done, skipped, blocked or failed",
    ),
    (
        "id no válido \"{}\": de 1 a 64 letras, dígitos, '.', '_' o '-'",
        "invalid id \"{0}\": 1 to 64 letters, digits, '.', '_' or '-'",
    ),
    // Un hueco nunca casa vacío: el id vacío, aparte.
    (
        "id no válido \"\": de 1 a 64 letras, dígitos, '.', '_' o '-'",
        "invalid id \"\": 1 to 64 letters, digits, '.', '_' or '-'",
    ),
    (
        "demasiadas fases ({}): como mucho {}",
        "too many phases ({0}): at most {1}",
    ),
    (
        "demasiadas tareas ({}): como mucho {}",
        "too many tasks ({0}): at most {1}",
    ),
    ("id repetido \"{}\"", "duplicate id \"{0}\""),
    (
        "la fase \"{}\" no tiene nombre",
        "phase \"{0}\" has no name",
    ),
    (
        "la tarea \"{}\" no tiene nombre",
        "task \"{0}\" has no name",
    ),
    (
        "no hay ninguna tarea \"{}\"; las que hay: {}",
        "task \"{0}\" not found; ids: {1}",
    ),
    // Un plan sin tareas: la lista, vacía.
    (
        "no hay ninguna tarea \"{}\"; las que hay: ",
        "task \"{0}\" not found; the plan has no tasks",
    ),
    (
        "no se pudo guardar el plan: {}",
        "could not save the plan: {0}",
    ),
    (
        "no se pudo borrar el plan: {}",
        "could not delete the plan: {0}",
    ),
    ("esta sesión no tiene plan", "this session has no plan"),
    (
        "no se pudo leer el plan: {}",
        "could not read the plan: {0}",
    ),
    ("la sesión ya no existe", "the session no longer exists"),
    // mcp_api.rs: las herramientas del plan
    ("falta session", "session is missing"),
    (
        "ese equipo no tiene planes (su nido es anterior)",
        "that machine has no plans (its nido is older)",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact() {
        assert_eq!(english("cuenta no válida"), "invalid account");
        assert_eq!(
            english("ese teléfono no está emparejado"),
            "that phone is not paired"
        );
    }

    #[test]
    fn holes() {
        assert_eq!(
            english("no existe /home/ana/x"),
            "/home/ana/x does not exist"
        );
        assert_eq!(
            english("no hay ninguna sesión \"web\""),
            "there is no session \"web\""
        );
        assert_eq!(
            english("no se puede ver /root: Permission denied (os error 13)"),
            "cannot view /root: Permission denied (os error 13)"
        );
        assert_eq!(
            english("Claude no ha dado la sesión (500 Internal Server Error)"),
            "Claude did not grant the session (500 Internal Server Error)"
        );
        assert_eq!(
            english("Claude no ha dado la sesión (400 Bad Request, invalid_request)"),
            "Claude did not grant the session (400 Bad Request, invalid_request)"
        );
    }

    #[test]
    fn reorders() {
        assert_eq!(english("no existe /tmp/a b"), "/tmp/a b does not exist");
        assert_eq!(
            english("«rel/x» no es una ruta completa"),
            "“rel/x” is not a full path"
        );
        assert_eq!(
            english("el otro equipo usa otro hub (https://a); este usa wss://b"),
            "the other machine uses a different hub (https://a); this one uses wss://b"
        );
    }

    #[test]
    fn most_specific_wins() {
        assert_eq!(
            english("no existe la carpeta /w"),
            "folder /w does not exist"
        );
        assert_eq!(
            english("no se pudo guardar: disco lleno"),
            "could not save: disco lleno"
        );
        assert_eq!(
            english("no se pudo guardar el código: e"),
            "could not save the code: e"
        );
        assert_eq!(
            english("no se pudo guardar la sesión: e"),
            "could not save the session: e"
        );
        assert_eq!(
            english("no se pudo guardar la sesión renovada: e"),
            "could not save the renewed session: e"
        );
        assert_eq!(
            english("Tarea en segundo plano: aviso"),
            "Background task: notice"
        );
        assert_eq!(
            english("Tarea en segundo plano: failed"),
            "Background task: failed"
        );
    }

    #[test]
    fn mode_and_model_messages() {
        assert_eq!(
            english("no se llega al modo plan con shift+tab"),
            "shift+tab does not reach mode plan"
        );
        assert_eq!(
            english("en el menú de /model no está «Gemini»"),
            "“Gemini” is not in the /model menu"
        );
        assert_eq!(english("modo desconocido: «x»"), "unknown mode: “x”");
        assert_eq!(
            english("Claude no tiene mensajes en cola"),
            "Claude has no queued messages"
        );
    }

    #[test]
    fn nested_messages_too() {
        assert_eq!(
            english("no se pudo renovar la sesión: respuesta que no se entiende"),
            "could not renew the session: unintelligible response"
        );
        assert_eq!(
            english("no se pudo cambiar la cuenta de Claude: Claude tiene ocupado /h/.claude"),
            "could not switch Claude's account: Claude is holding /h/.claude"
        );
    }

    /// Los errores del plan, también los que tienen un hueco que puede ir vacío.
    #[test]
    fn plan_messages() {
        assert_eq!(
            english("no hay ninguna tarea \"t7\"; las que hay: t1, t2, …"),
            "task \"t7\" not found; ids: t1, t2, …"
        );
        assert_eq!(
            english("no hay ninguna tarea \"t7\"; las que hay: "),
            "task \"t7\" not found; the plan has no tasks"
        );
        assert_eq!(
            english("id no válido \"\": de 1 a 64 letras, dígitos, '.', '_' o '-'"),
            "invalid id \"\": 1 to 64 letters, digits, '.', '_' or '-'"
        );
        assert_eq!(
            english("id no válido \"f 1\": de 1 a 64 letras, dígitos, '.', '_' o '-'"),
            "invalid id \"f 1\": 1 to 64 letters, digits, '.', '_' or '-'"
        );
        assert_eq!(
            english("demasiadas tareas (501): como mucho 500"),
            "too many tasks (501): at most 500"
        );
        assert_eq!(
            english(
                "estado desconocido \"hecho\": vale pending, in_progress, done, skipped, blocked o failed"
            ),
            "unknown status \"hecho\": use pending, in_progress, done, skipped, blocked or failed"
        );
        assert_eq!(
            english(
                "estado desconocido \"\": vale pending, in_progress, done, skipped, blocked o failed"
            ),
            "unknown status \"\": use pending, in_progress, done, skipped, blocked or failed"
        );
        assert_eq!(
            english("esta sesión no tiene plan"),
            "this session has no plan"
        );
        assert_eq!(
            english("no se pudo leer el plan: Is a directory (os error 21)"),
            "could not read the plan: Is a directory (os error 21)"
        );
        assert_eq!(
            english("la sesión ya no existe"),
            "the session no longer exists"
        );
    }

    /// Los de contestar en nombre del usuario (el agente de voz).
    #[test]
    fn answer_refusals() {
        assert_eq!(
            english("Claude está esperando una respuesta en un menú: contéstala primero"),
            "Claude is waiting for an answer in a menu: answer it first"
        );
        assert_eq!(
            english("en pantalla ya no está lo que se confirmó"),
            "what was confirmed is no longer on screen"
        );
        assert_eq!(
            english("el mensaje solo tiene caracteres de control"),
            "the message only has control characters"
        );
        assert_eq!(
            english("eligió la opción, pero Claude espera en otro menú: no se mandó el texto"),
            "the option was chosen, but Claude is waiting in another menu: the text was not sent"
        );
    }

    #[test]
    fn a_hole_is_never_empty() {
        assert_eq!(english("no existe "), "no existe ");
        assert_eq!(
            english("el hub no aceptó la firma ()"),
            "the hub did not accept the signature"
        );
        assert_eq!(
            english("el hub no aceptó la firma (x)"),
            "the hub did not accept the signature (x)"
        );
    }

    #[test]
    fn no_match() {
        assert_eq!(
            english("algo que nadie ha escrito"),
            "algo que nadie ha escrito"
        );
        assert_eq!(english(""), "");
        assert_eq!(
            english("No such file or directory (os error 2)"),
            "No such file or directory (os error 2)"
        );
    }

    #[tokio::test]
    async fn follows_the_request() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        assert_eq!(tr("cuenta no válida"), "invalid account");
        assert_eq!(
            scope(Lang::Es, async { tr("cuenta no válida") }).await,
            "cuenta no válida"
        );
        assert_eq!(
            scope(Lang::En, async { tr("cuenta no válida") }).await,
            "invalid account"
        );
    }

    #[test]
    fn accept_language() {
        assert_eq!(parse_accept_language("es-ES,es;q=0.9"), Lang::Es);
        assert_eq!(parse_accept_language("es"), Lang::Es);
        assert_eq!(parse_accept_language("ES-mx"), Lang::Es);
        assert_eq!(parse_accept_language("en-US"), Lang::En);
        assert_eq!(parse_accept_language("en-US,en;q=0.9,es;q=0.8"), Lang::En);
        assert_eq!(parse_accept_language("en;q=0.5, es"), Lang::Es);
        assert_eq!(parse_accept_language("fr"), Lang::En);
        assert_eq!(parse_accept_language(""), Lang::En);
        assert_eq!(parse_accept_language("*"), Lang::En);
        assert_eq!(parse_accept_language("es;q=0"), Lang::En);
        assert_eq!(parse_accept_language("estonian"), Lang::En);
    }

    /// Cada `{}` del patrón tiene su `{n}` en la plantilla (de 0 a n-1, cada uno al menos una
    /// vez), sin huecos seguidos ni patrones sin texto fijo, y sin patrones repetidos.
    #[test]
    fn the_terminal_language_comes_from_the_locale() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            lang_of_env(move |k| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            })
        };
        assert_eq!(env(&[("LANG", "es_ES.UTF-8")]), Lang::Es);
        assert_eq!(env(&[("LANG", "es")]), Lang::Es);
        assert_eq!(env(&[("LANG", "C.UTF-8")]), Lang::En);
        assert_eq!(env(&[("LANG", "en_US.UTF-8")]), Lang::En);
        assert_eq!(env(&[]), Lang::En);
        // LC_ALL manda; uno vacío no cuenta.
        assert_eq!(env(&[("LC_ALL", "en_GB"), ("LANG", "es_ES")]), Lang::En);
        assert_eq!(env(&[("LC_ALL", ""), ("LANG", "es_ES")]), Lang::Es);
        assert_eq!(
            env(&[("LC_MESSAGES", "es_MX.UTF-8"), ("LANG", "C")]),
            Lang::Es
        );
        assert_eq!(
            in_lang(Lang::En, "el texto está vacío"),
            "the text is empty"
        );
        assert_eq!(
            in_lang(Lang::Es, "el texto está vacío"),
            "el texto está vacío"
        );
    }

    #[test]
    fn table_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for (pattern, template) in TABLE {
            assert!(seen.insert(*pattern), "patrón repetido: {pattern}");
            let holes = pattern.matches("{}").count();
            assert!(!pattern.contains("{}{}"), "huecos seguidos: {pattern}");
            assert!(
                !pattern.replace("{}", "").is_empty(),
                "sin texto fijo: {pattern}"
            );
            let mut used = std::collections::BTreeSet::new();
            let mut rest = *template;
            while let Some(i) = rest.find('{') {
                let after = &rest[i + 1..];
                let end = after
                    .find('}')
                    .unwrap_or_else(|| panic!("«{{» suelto: {template}"));
                let n: usize = after[..end]
                    .parse()
                    .unwrap_or_else(|_| panic!("hueco sin número: {template}"));
                used.insert(n);
                rest = &after[end + 1..];
            }
            let want: std::collections::BTreeSet<usize> = (0..holes).collect();
            assert_eq!(used, want, "{pattern} → {template}");
            assert!(!template.contains("{}"), "plantilla con «{{}}»: {template}");
        }
    }
}
