use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
#[cfg(unix)]
use nido::attach;
use nido::client::Api;
use nido::launch::{self, Launcher, Paths};
use nido::registry::{self, AlertRec, Registry};
use nido::server::{self, AppState};
use nido::sys::LocalTime;
use nido::{auth, holder, import};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::watch;
use unicode_width::UnicodeWidthStr;

/// El puerto de siempre. En Windows, otro: el WSL de ese mismo equipo ya puede tener el 7690 (y
/// lo reenvía a Windows).
#[cfg(not(windows))]
const DEFAULT_ADDR: &str = "127.0.0.1:7690";
#[cfg(windows)]
const DEFAULT_ADDR: &str = "127.0.0.1:7691";

/// Gestor de sesiones de terminal (sobre todo Claude Code): consola, y app y navegador por un canal cifrado.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Dirección del gestor para las órdenes de la CLI.
    #[arg(
        long,
        env = "NIDO_SERVER",
        default_value = DEFAULT_ADDR,
        global = true
    )]
    server: SocketAddr,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum HubCmd {
    /// Usar este hub (`wss://…`).
    Set {
        url: String,
        /// Conectar a esta `ip:puerto` (si el nombre del hub aún no resuelve aquí).
        #[arg(long)]
        address: Option<String>,
    },
    /// Dejar de usar el hub (la app solo llegará por la LAN o la VPN).
    Off,
    /// Mandar un aviso de prueba a los teléfonos.
    Test,
}

#[derive(Subcommand)]
enum GroupCmd {
    /// Un código para que otro equipo se una (vale 10 minutos y una vez).
    Invite,
    /// Unir este equipo al grupo de otro, con su código de `nido group invite`.
    Join { code: String },
    /// Sacar un equipo del grupo.
    Rm { id: String },
}

#[derive(Subcommand)]
enum PhonesCmd {
    /// Quita un teléfono: deja de poder entrar en el momento.
    Rm { id: String },
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// Pasa Claude (todas las sesiones) a esta cuenta (número, correo o alias).
    Use { account: String },
    /// Entrar en una cuenta de Claude nueva, o volver a entrar en una (con su número o correo).
    Login { account: Option<String> },
    /// Olvidar una cuenta guardada (no la activa).
    Rm { number: u32 },
    /// Cambio automático: al llegar la cuenta activa al 99 % (5 h o semana), pasar a otra.
    /// Sin más, cómo está.
    Auto {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// El gestor: API, web, restauración al arrancar.
    Serve {
        /// Dónde escuchar; se repite o va separado por comas (localhost + VPN + LAN…).
        #[arg(
            long,
            env = "NIDO_BIND",
            value_delimiter = ',',
            default_value = DEFAULT_ADDR
        )]
        bind: Vec<SocketAddr>,
        /// Sin token de acceso (el control de Origin y Host se mantiene).
        #[arg(long)]
        no_auth: bool,
        /// No reabrir las sesiones con autoarranque al empezar.
        #[arg(long)]
        no_restore: bool,
    },
    /// (Interno) el guardián de una sesión.
    #[command(hide = true)]
    Holder { id: String },
    /// El MCP de nido en las sesiones de Claude: `on` u `off` (sin más, el servidor por stdio,
    /// que lanza Claude Code).
    Mcp {
        #[command(subcommand)]
        action: Option<McpCmd>,
    },
    /// Lista las sesiones.
    Ls,
    /// Las alertas de las sesiones que aún no has visto.
    Alerts,
    /// Manda un texto al portapapeles de esta sesión de nido, para copiarlo desde la web o la app,
    /// sin enseñarlo: el fichero o, sin él, la entrada estándar (`orden | nido clip`). Solo texto
    /// UTF-8, hasta 64 KB.
    Clip {
        /// El fichero de texto; sin él, lo que llegue por la entrada estándar.
        file: Option<PathBuf>,
        /// El título con el que se enseña.
        #[arg(long)]
        title: Option<String>,
    },
    /// Los secretos que recuerda este equipo (sin su valor); `forget` olvida uno.
    Secrets {
        #[command(subcommand)]
        action: Option<SecretsCmd>,
    },
    /// Emparejar un teléfono con la app de nido: enseña un QR para escanear (vale 10 minutos).
    Pair {
        /// Un código reutilizable durante estos días (máx. 90), para una demostración: quien lo
        /// tenga puede manejar este equipo. Úsalo solo en un equipo de demo.
        #[arg(long, value_name = "DÍAS", conflicts_with = "stop")]
        days: Option<u64>,
        /// Retira el código reutilizable (los dispositivos emparejados con él se quitan con
        /// `nido phones rm`).
        #[arg(long)]
        stop: bool,
    },
    /// El hub: para usar la app sin VPN y recibir avisos. Sin más, cómo va.
    Hub {
        #[command(subcommand)]
        action: Option<HubCmd>,
    },
    /// El grupo de equipos: juntos en la web y en la app. Sin más, los que hay.
    Group {
        #[command(subcommand)]
        action: Option<GroupCmd>,
    },
    /// Teléfonos emparejados; `nido phones rm <id>` quita uno.
    Phones {
        #[command(subcommand)]
        action: Option<PhonesCmd>,
    },
    /// Cuentas de Claude y lo que lleva gastado cada una.
    Accounts {
        #[command(subcommand)]
        action: Option<AccountsCmd>,
    },
    /// Nueva sesión: sin orden, Claude; si no, la orden que venga tras `--`.
    New {
        name: String,
        /// Carpeta (por defecto, la actual).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// No reabrirla al arrancar el equipo.
        #[arg(long)]
        no_autostart: bool,
        /// Carpeta lógica donde agruparla.
        #[arg(long)]
        folder: Option<String>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Arranca una sesión parada.
    Start { name: String },
    /// Para una sesión (queda registrada).
    Stop { name: String },
    /// Para y vuelve a arrancar (Claude retoma su conversación).
    Restart { name: String },
    /// Para y borra una sesión.
    Rm { name: String },
    /// Mueve una sesión a una carpeta lógica (sin carpeta: la saca).
    Mv {
        name: String,
        folder: Option<String>,
    },
    /// Registra los Claude que corren en tmux (no toca tmux ni los arranca).
    ImportTmux {
        /// Solo enseñar lo que se importaría.
        #[arg(long)]
        dry_run: bool,
    },
    /// Engancharse a una sesión desde esta terminal (Ctrl+\ para desengancharse).
    Attach {
        /// Nombre de la sesión; sin él, se elige de una lista.
        name: Option<String>,
    },
    /// En Windows: que nido arranque solo al iniciar sesión (una tarea programada).
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Olvida un secreto recordado: la próxima vez que una sesión lo pida, se pregunta.
    Forget { name: String },
    /// Ejecuta una orden con un secreto en una variable de entorno, sin enseñarlo: el de esta
    /// sesión (`NIDO_SESSION`) o el recordado de este equipo. Sale con el código de la orden.
    Run {
        name: String,
        /// La variable de entorno en la que va el secreto.
        #[arg(long = "env", value_name = "VAR")]
        var: String,
        /// La orden y sus argumentos, tras `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Las próximas sesiones de Claude, con el MCP de nido (lo normal).
    On,
    /// Las próximas sesiones de Claude, sin él.
    Off,
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceCmd {
    /// Crea (o rehace) la tarea y la arranca ya.
    Install {
        /// Nombre del equipo en la web y la app.
        #[arg(long, default_value = "Windows")]
        name: String,
        /// Dónde escucha; por defecto solo en este equipo (el resto entra por el hub).
        #[arg(long, default_value = DEFAULT_ADDR)]
        bind: String,
    },
    /// Para nido y quita la tarea (las sesiones en marcha siguen hasta que se cierren).
    Uninstall,
}

/// Escucha con IP_FREEBIND: la IP de la VPN puede no existir aún al arrancar.
fn listen(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    // Solo en Linux: escuchar en una IP que aún no existe (la de la VPN al arrancar).
    #[cfg(target_os = "linux")]
    if addr.is_ipv4() {
        socket.set_freebind_v4(true)?;
    } else {
        socket.set_freebind_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(socket.into())
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("nido: {msg}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = Paths::from_env();
    match cli.cmd {
        Cmd::Serve {
            bind,
            no_auth,
            no_restore,
        } => serve(paths, bind, no_auth, no_restore),
        Cmd::Holder { id } => run_holder(paths, &id),
        Cmd::Mcp { action: Some(a) } => match mcp_switch(&paths, a) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        },
        #[cfg(windows)]
        Cmd::Service { action } => match windows_service(action) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        },
        // Sin token ni gestor: lee el secreto del disco.
        Cmd::Secrets {
            action: Some(SecretsCmd::Run { name, var, command }),
        } => secrets_run(&paths, &name, &var, &command),
        cmd => {
            // Órdenes de consola: con la salida cortada (`nido ls | head`), terminar sin más,
            // como cualquier herramienta de terminal. El gestor y el guardián siguen ignorándolo.
            #[cfg(unix)]
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            // Con `NIDO_HOME`, el de ese nido (ver `Paths::token`).
            let token = auth::load_or_create_token(&paths.token()).ok();
            let api = Api {
                addr: cli.server,
                token,
            };
            match client_cmd(&api, &paths, cmd) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
    }
}

/// Cuánto se espera a registrar el MCP en Claude antes de restaurar las sesiones: a partir de
/// `REGISTER_WARN` se avisa, y con `REGISTER_CAP` se restaura sin él.
const REGISTER_WARN: std::time::Duration = std::time::Duration::from_secs(20);
const REGISTER_CAP: std::time::Duration = std::time::Duration::from_secs(60);

#[tokio::main]
async fn serve(
    mut paths: Paths,
    bind: Vec<SocketAddr>,
    no_auth: bool,
    no_restore: bool,
) -> ExitCode {
    nido::sys::keep_stdio_to_self();
    let token = if no_auth {
        None
    } else {
        match auth::load_or_create_token(&paths.token()) {
            Ok(t) => Some(t),
            Err(e) => return fail(format!("no se pudo leer ni crear el token: {e}")),
        }
    };
    let mut listeners = Vec::new();
    for &addr in &bind {
        match listen(addr) {
            Ok(l) => listeners.push((addr, l)),
            Err(e) => return fail(format!("no se pudo escuchar en {addr}: {e}")),
        }
    }
    // Sus sesiones hablan con este nido, esté donde esté (sin depender de `NIDO_SERVER`).
    let bound: Vec<SocketAddr> = listeners
        .iter()
        .map(|(a, l)| l.local_addr().unwrap_or(*a))
        .collect();
    paths.server = nido::launch::sessions_server(&bound);
    let launcher = Launcher::detect();
    let loopback_only = bind.iter().all(|a| a.ip().is_loopback());
    let state = match AppState::new(paths, launcher, token.clone(), loopback_only) {
        Ok(s) => s,
        Err(e) => return fail(format!("registro: {e}")),
    };
    // Los flujos del túnel cifrado entran por una escucha propia en loopback (aunque nido solo
    // escuche en la LAN), y el QR lleva las direcciones de fuera.
    let tunnel = match listen(SocketAddr::from(([127, 0, 0, 1], 0))) {
        Ok(l) => l,
        Err(e) => return fail(format!("no se pudo abrir la escucha interna: {e}")),
    };
    if let Ok(a) = tunnel.local_addr() {
        let _ = state.tunnel_addr.set(a);
    }
    let _ = state.direct.set(
        bind.iter()
            .filter(|a| !a.ip().is_loopback())
            .map(|a| format!("http://{a}"))
            .collect(),
    );
    listeners.push((tunnel.local_addr().unwrap_or(bind[0]), tunnel));
    server::spawn_scanner(state.clone());
    // Primero que Claude tenga `nido mcp` y luego las sesiones: así las restauradas arrancan ya
    // con el channel, y `claude mcp add` no reescribe su configuración mientras ellas la usan. En
    // segundo plano (la API atiende ya) y con tope: `claude mcp …` tarda unos segundos. El
    // registro va en su tarea: aunque se deje de esperar, acaba y dice cómo fue.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let register = tokio::spawn(server::register_mcp(st.clone()));
            let done = async {
                let _ = register.await;
            };
            if server::registration_done(done, REGISTER_WARN, REGISTER_CAP).await
                == server::Registered::GaveUp
            {
                eprintln!(
                    "nido: registrar el MCP en Claude no acaba en {} s; se restauran las sesiones sin esperar más",
                    REGISTER_CAP.as_secs()
                );
            }
            if !no_restore {
                st.restore().await;
            }
        });
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let mut servers = Vec::new();
    for (addr, listener) in listeners {
        if Some(&addr) != state.tunnel_addr.get() {
            let what = if addr.ip().is_loopback() {
                "la API para la consola"
            } else {
                "solo el canal cifrado"
            };
            eprintln!("nido escuchando en {addr}: {what} (guardianes: {launcher:?})");
        }
        let mut stop = stop_rx.clone();
        // Con la dirección de quien conecta: el resto de la API solo se atiende en loopback.
        let app = server::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        servers.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|s| *s).await;
                })
                .await
        }));
    }
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        let mut close = tokio::signal::windows::ctrl_close().expect("ctrl_close");
        let mut shutdown = tokio::signal::windows::ctrl_shutdown().expect("ctrl_shutdown");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
        }
    }
    // Parar el gestor no para las sesiones: cada una tiene su guardián.
    let _ = stop_tx.send(true);
    for s in servers {
        let _ = s.await;
    }
    ExitCode::SUCCESS
}

#[tokio::main]
async fn run_holder(paths: Paths, id: &str) -> ExitCode {
    nido::sys::keep_stdio_to_self();
    let registry = match Registry::load(&paths.registry()) {
        Ok(r) => r,
        Err(e) => return fail(format!("registro: {e}")),
    };
    let Some(session) = registry.get(id).cloned() else {
        return fail(format!("no hay ninguna sesión {id}"));
    };
    let cfg = launch::holder_config(&paths, &session, 120, 40);
    let pidfile = paths.pidfile(id);
    if let Some(dir) = pidfile.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&pidfile, std::process::id().to_string());
    eprintln!(
        "nido holder {} ({}): {:?} en {}",
        session.name,
        id,
        cfg.argv,
        cfg.cwd.display()
    );
    let res = holder::run(cfg).await;
    // Solo si sigue siendo el nuestro: al reiniciar, el guardián nuevo ya puede haber escrito el suyo.
    let mine = std::process::id().to_string();
    if std::fs::read_to_string(&pidfile).is_ok_and(|p| p.trim() == mine) {
        let _ = std::fs::remove_file(&pidfile);
    }
    match res {
        Ok(code) => {
            eprintln!(
                "nido holder {}: el proceso terminó con {code:?}",
                session.name
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("guardián de {}: {e}", session.name)),
    }
}

fn client_cmd(api: &Api, paths: &Paths, cmd: Cmd) -> io::Result<()> {
    let id_of = |name: &str| -> io::Result<String> {
        let list = api.call("GET", "/api/v1/sessions", None)?;
        list.as_array()
            .and_then(|a| a.iter().find(|s| s["name"] == name || s["id"] == name))
            .and_then(|s| s["id"].as_str().map(str::to_owned))
            .ok_or_else(|| io::Error::other(format!("no hay ninguna sesión {name:?}")))
    };
    match cmd {
        Cmd::Pair { stop: true, .. } => {
            let r = api.call("DELETE", "/api/v1/pair", None)?;
            println!(
                "{}",
                if r["stopped"] == true {
                    "Código reutilizable retirado. Los dispositivos ya emparejados siguen: `nido phones` y `nido phones rm <id>`."
                } else {
                    "No había ningún código reutilizable."
                }
            );
        }
        Cmd::Pair { days, .. } => {
            let body = days.map(|d| serde_json::json!({ "days": d }));
            let r = api.call("POST", "/api/v1/pair", body.as_ref())?;
            let uri = r["uri"].as_str().unwrap_or_default();
            let qr = qrcode::QrCode::new(pair_qr_text(&r).as_bytes()).map_err(io::Error::other)?;
            println!(
                "{}",
                qr.render::<qrcode::render::unicode::Dense1x2>()
                    .dark_color(qrcode::render::unicode::Dense1x2::Light)
                    .light_color(qrcode::render::unicode::Dense1x2::Dark)
                    .build()
            );
            let days = (r["reusable"] == true).then(|| {
                (r["expires"].as_u64().unwrap_or(0).saturating_sub(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                ) + 3_599)
                    / 86_400
            });
            println!("{}", pair_hint(pair_qr_text(&r) != uri, days));
            let direct: Vec<&str> = r["direct"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str())
                .collect();
            if !direct.is_empty() {
                println!("El móvil lo buscará en: {}", direct.join(", "));
            }
            match r["hub"].as_str() {
                Some(h) => println!("Y, si no llega, por el hub {h}."),
                None if direct.is_empty() => println!(
                    "Ojo: nido solo escucha en este equipo y no hay hub (`nido hub set`): el móvil no llegará."
                ),
                None => {}
            }
            println!(
                "La clave de este nido está en ~/.config/nido/hub.key: si se pierde, hay que volver a emparejar los teléfonos."
            );
            println!("Sin escáner, pega este código en la app o en la web:\n{uri}");
        }
        Cmd::Hub { action } => {
            let r = match action {
                None => api.call("GET", "/api/v1/hub", None)?,
                Some(HubCmd::Set { url, address }) => {
                    api.call(
                        "POST",
                        "/api/v1/hub",
                        Some(&serde_json::json!({ "url": url, "address": address })),
                    )?;
                    // Que le dé tiempo a conectar antes de contar cómo va.
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    api.call("GET", "/api/v1/hub", None)?
                }
                Some(HubCmd::Off) => api.call(
                    "POST",
                    "/api/v1/hub",
                    Some(&serde_json::json!({ "url": null })),
                )?,
                Some(HubCmd::Test) => {
                    let t = api.call("POST", "/api/v1/push/test", None)?;
                    println!("Aviso de prueba a {} teléfono(s).", t["phones"]);
                    for f in t["failed"].as_array().into_iter().flatten() {
                        println!("  fallo: {}", f.as_str().unwrap_or_default());
                    }
                    return Ok(());
                }
            };
            match r["url"].as_str() {
                None => println!("Sin hub: la app solo llega por la LAN o la VPN."),
                Some(u) if r["connected"] == true => {
                    println!("Conectado a {u} como {}.", r["id"].as_str().unwrap_or("?"))
                }
                Some(u) => println!(
                    "Hub {u}: sin conexión{}.",
                    r["error"]
                        .as_str()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                ),
            }
        }
        Cmd::Group { action } => {
            match action {
                Some(GroupCmd::Invite) => {
                    let r = api.call("POST", "/api/v1/group/invite", None)?;
                    println!("En el otro equipo (ya conectado al mismo hub):\n");
                    println!("  nido group join '{}'\n", r["uri"].as_str().unwrap_or(""));
                    println!("Vale 10 minutos y una sola vez.");
                    return Ok(());
                }
                Some(GroupCmd::Join { code }) => {
                    api.call(
                        "POST",
                        "/api/v1/group/join",
                        Some(&serde_json::json!({ "code": code })),
                    )?;
                    println!("Este equipo ya está en el grupo.");
                }
                Some(GroupCmd::Rm { id }) => {
                    api.call("DELETE", &format!("/api/v1/group/members/{id}"), None)?;
                    println!("Equipo sacado del grupo.");
                }
                None => {}
            }
            let r = api.call("GET", "/api/v1/group", None)?;
            let members = r["members"].as_array().cloned().unwrap_or_default();
            if members.len() <= 1 {
                println!(
                    "Este equipo está solo. Para juntar otro: `nido group invite` aquí y `nido group join` allí."
                );
            }
            for m in members {
                println!(
                    "{}  {}{}",
                    m["id"].as_str().unwrap_or(""),
                    m["name"].as_str().unwrap_or(""),
                    if m["self"] == true { "  (este)" } else { "" }
                );
            }
        }
        Cmd::Phones { action: None } => {
            let list = api.call("GET", "/api/v1/phones", None)?;
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No hay teléfonos emparejados. Empareja uno con `nido pair`.");
            }
            for p in list {
                println!(
                    "{}  {:<24} {}",
                    p["id"].as_str().unwrap_or(""),
                    p["name"].as_str().unwrap_or(""),
                    if p["push"] == true { "avisos" } else { "" }
                );
            }
        }
        Cmd::Phones {
            action: Some(PhonesCmd::Rm { id }),
        } => {
            api.call("DELETE", &format!("/api/v1/phones/{id}"), None)?;
            println!("Teléfono quitado.");
        }
        Cmd::Accounts {
            action: Some(AccountsCmd::Use { account }),
        } => {
            let r = api.call(
                "POST",
                "/api/v1/accounts/switch",
                Some(&serde_json::json!({ "account": account })),
            )?;
            for w in r["warnings"].as_array().into_iter().flatten() {
                eprintln!("aviso: {}", w.as_str().unwrap_or_default());
            }
            let to = &r["to"];
            if r["switched"] == true {
                println!(
                    "Ahora la cuenta activa es la {} ({}).",
                    to["number"],
                    to["email"].as_str().unwrap_or("")
                );
            } else {
                println!(
                    "{}",
                    r["message"].as_str().unwrap_or("No ha cambiado la cuenta.")
                );
            }
        }
        Cmd::Accounts {
            action: Some(AccountsCmd::Login { account }),
        } => {
            // Volver a entrar: el correo de esa cuenta, para que la página ya lo traiga.
            let email = match &account {
                Some(a) => {
                    let v = api.call("GET", "/api/v1/accounts", None)?;
                    let found = v["accounts"].as_array().into_iter().flatten().find(|x| {
                        a.parse::<u64>().ok() == x["number"].as_u64()
                            || x["email"].as_str() == Some(a.as_str())
                    });
                    match found {
                        Some(x) => x["email"].as_str().map(str::to_owned),
                        None => {
                            return Err(std::io::Error::other(format!(
                                "no hay ninguna cuenta «{a}»"
                            )));
                        }
                    }
                }
                None => None,
            };
            let r = api.call(
                "POST",
                "/api/v1/accounts/login",
                Some(&serde_json::json!({ "email": email })),
            )?;
            println!("Abre esta dirección en un navegador y entra en la cuenta de Claude:\n");
            println!("  {}\n", r["url"].as_str().unwrap_or_default());
            println!("Al acabar, la página enseña un código. Pégalo aquí:");
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            let done = api.call(
                "POST",
                "/api/v1/accounts/login/finish",
                Some(&serde_json::json!({ "state": r["state"], "code": code.trim() })),
            )?;
            println!(
                "{} {} ({}){}.",
                if done["new"] == true {
                    "Añadida la cuenta"
                } else {
                    "Sesión renovada en la cuenta"
                },
                done["number"],
                done["email"].as_str().unwrap_or_default(),
                if done["active"] == true {
                    ", que es la que usa Claude"
                } else {
                    ""
                }
            );
        }
        Cmd::Accounts {
            action: Some(AccountsCmd::Rm { number }),
        } => {
            api.call("DELETE", &format!("/api/v1/accounts/{number}"), None)?;
            println!("Cuenta {number} olvidada.");
        }
        Cmd::Accounts {
            action: Some(AccountsCmd::Auto { state }),
        } => {
            let v = match state.as_deref() {
                Some(s) => api.call(
                    "POST",
                    "/api/v1/accounts/auto",
                    Some(&serde_json::json!({ "enabled": s == "on" })),
                )?,
                None => api.call("GET", "/api/v1/accounts", None)?,
            };
            println!("{}", auto_line(&v));
        }
        Cmd::Accounts { action: None } => {
            let v = api.call("GET", "/api/v1/accounts", None)?;
            if let Some(e) = v["error"].as_str() {
                eprintln!("aviso: {e}");
            }
            let pct = |w: &serde_json::Value| {
                w["pct"]
                    .as_f64()
                    .map_or("  —".to_owned(), |p| format!("{p:>3.0}%"))
            };
            for a in v["accounts"].as_array().into_iter().flatten() {
                let usage = if a["usage"].is_object() {
                    &a["usage"]
                } else {
                    &a["last_good"]
                };
                let models: Vec<String> = usage["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| format!("{} {}", m["name"].as_str().unwrap_or("?"), pct(m).trim()))
                    .collect();
                let mut notes = Vec::new();
                if a["active"] == true {
                    notes.push("activa".to_owned());
                }
                if a["disabled"] == true {
                    notes.push("desactivada".to_owned());
                }
                match a["status"].as_str() {
                    Some("ok") | None => {}
                    Some("token_expired" | "relogin_required") => {
                        notes.push("hay que volver a entrar (nido accounts login N)".to_owned())
                    }
                    Some("unavailable") => notes.push("sin datos recientes".to_owned()),
                    Some(s) => notes.push(s.to_owned()),
                }
                if let Some(p) = a["problem"].as_str() {
                    notes.push(p.to_owned());
                }
                if usage["seven_day"]["ahead_of_pace"] == true {
                    notes.push("7d por encima del ritmo".to_owned());
                }
                println!(
                    "{} {:<2} {:<32} 5h {}  7d {}  {}{}",
                    if a["active"] == true { '*' } else { ' ' },
                    a["number"],
                    a["alias"].as_str().or(a["email"].as_str()).unwrap_or(""),
                    pct(&usage["five_hour"]),
                    pct(&usage["seven_day"]),
                    models.join("  "),
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!("  ({})", notes.join(", "))
                    }
                );
            }
            println!("{}", auto_line(&v));
        }
        Cmd::Ls => {
            let list = api.call("GET", "/api/v1/sessions", None)?;
            let mut sessions = list.as_array().cloned().unwrap_or_default();
            if sessions.is_empty() {
                println!("No hay sesiones. Crea una con `nido new <nombre>`.");
            }
            // Por carpetas (alfabético) y al final las que no tienen; dentro, en su orden.
            sessions.sort_by_key(|s| {
                let f = s["folder"].as_str().map(str::to_lowercase);
                (f.is_none(), f)
            });
            let any_folder = sessions.iter().any(|s| s["folder"].is_string());
            let mut last: Option<Option<String>> = None;
            for s in sessions {
                let folder = s["folder"].as_str().map(str::to_owned);
                if any_folder && last.as_ref() != Some(&folder) {
                    println!(
                        "{}",
                        folder
                            .as_deref()
                            .map_or("— sin carpeta —".to_owned(), |f| format!("[{f}]"))
                    );
                    last = Some(folder);
                }
                let state = if s["alive"] == true {
                    match s["claude"]["status"].as_str() {
                        Some("busy") => "ocupado",
                        Some("waiting") => "te necesita",
                        Some(_) if s["unread"] == true => "sin leer",
                        Some(_) => "esperando",
                        None => "en marcha",
                    }
                } else {
                    "parada"
                };
                let what = if s["claude"].is_object() {
                    "claude".to_owned()
                } else {
                    s["argv"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default()
                };
                let cwd = s["cwd"].as_str().unwrap_or("");
                let cwd = cwd.replacen(&paths.home.to_string_lossy().into_owned(), "~", 1);
                let alerts = s["alerts"].as_array().map_or(0, Vec::len);
                println!(
                    "{}",
                    ls_line(s["name"].as_str().unwrap_or(""), state, alerts, &what, &cwd)
                );
            }
        }
        Cmd::Alerts => {
            // Las de todos los equipos del grupo.
            let v = api.call("GET", "/api/v1/mcp/alerts", None)?;
            let now = nido::sys::local_time(registry::now_ms()).unwrap_or_default();
            let (lines, warnings) =
                alert_lines(&v, now, |at| nido::sys::local_time(at).unwrap_or_default());
            if lines.is_empty() {
                println!("Sin alertas.");
            }
            for l in lines {
                println!("{l}");
            }
            for w in warnings {
                eprintln!("{w}");
            }
        }
        Cmd::Secrets { action: None } => {
            let v = api.call("GET", "/api/v1/secrets", None)?;
            let now = nido::sys::local_time(registry::now_ms()).unwrap_or_default();
            let lines = secret_lines(&v, now, |at| nido::sys::local_time(at).unwrap_or_default());
            if lines.is_empty() {
                println!("No hay secretos recordados.");
            }
            for l in lines {
                println!("{l}");
            }
        }
        Cmd::Secrets {
            action: Some(SecretsCmd::Forget { name }),
        } => {
            // Los nombres válidos son `[A-Za-z0-9._-]`: van tal cual en la URL; con otra cosa, el
            // gestor dice que no es válido.
            if !nido::secrets::valid_name(&name) {
                return Err(io::Error::other(format!(
                    "nombre de secreto no válido: {name:?} (letras, números, «.», «_» o «-»; hasta 64)"
                )));
            }
            api.call("DELETE", &format!("/api/v1/secrets/{name}"), None)?;
            println!("Olvidado: {name}.");
        }
        Cmd::New {
            name,
            cwd,
            no_autostart,
            folder,
            command,
        } => {
            let cwd = match cwd {
                Some(c) => c,
                None => std::env::current_dir()?,
            };
            let cwd = nido::sys::canonical(&cwd);
            let body = serde_json::json!({
                "name": name, "cwd": cwd,
                "autostart": !no_autostart, "command": command, "folder": folder,
            });
            let s = api.call("POST", "/api/v1/sessions", Some(&body))?;
            println!(
                "Sesión {} creada y en marcha.",
                s["name"].as_str().unwrap_or(&name)
            );
        }
        Cmd::Start { name } => {
            api.call(
                "POST",
                &format!("/api/v1/sessions/{}/start", id_of(&name)?),
                None,
            )?;
        }
        Cmd::Stop { name } => {
            api.call(
                "POST",
                &format!("/api/v1/sessions/{}/stop", id_of(&name)?),
                None,
            )?;
        }
        Cmd::Restart { name } => {
            api.call(
                "POST",
                &format!("/api/v1/sessions/{}/restart", id_of(&name)?),
                None,
            )?;
        }
        Cmd::Mv { name, folder } => {
            let body = serde_json::json!({ "folder": folder.unwrap_or_default() });
            api.call(
                "PATCH",
                &format!("/api/v1/sessions/{}", id_of(&name)?),
                Some(&body),
            )?;
        }
        Cmd::Rm { name } => {
            api.call(
                "DELETE",
                &format!("/api/v1/sessions/{}", id_of(&name)?),
                None,
            )?;
        }
        Cmd::ImportTmux { dry_run } => {
            let found = import::scan(std::path::Path::new("/proc"), &paths.home);
            let list = api.call("GET", "/api/v1/sessions", None)?;
            let known: Vec<String> = list
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["claude"]["session_id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let mut names: Vec<String> = list
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if found.is_empty() {
                println!("No hay ningún Claude corriendo en tmux.");
            }
            for f in found {
                if known.contains(&f.session_id) {
                    println!("= {:<24} ya registrada", f.name);
                    continue;
                }
                let mut name = f.name.clone();
                let mut n = 2;
                while names.contains(&name) {
                    name = format!("{}-{n}", f.name);
                    n += 1;
                }
                names.push(name.clone());
                let profile = f
                    .config_dir
                    .as_ref()
                    .map(|d| format!(" (perfil {})", d.display()))
                    .unwrap_or_default();
                println!(
                    "+ {:<24} {} en {}{profile} [tmux {}]",
                    name,
                    f.session_id,
                    f.cwd.display(),
                    f.tmux
                );
                if !dry_run {
                    let body = serde_json::json!({
                        "name": name, "cwd": f.cwd, "claude_session_id": f.session_id,
                        "config_dir": f.config_dir, "start": false, "autostart": true,
                        // La sesión de tmux en la que estaba (0-trabajo, personal…) como carpeta.
                        "folder": f.tmux.split(':').next(),
                    });
                    api.call("POST", "/api/v1/sessions", Some(&body))?;
                }
            }
            if dry_run {
                println!("(prueba: no se ha registrado nada)");
            }
        }
        Cmd::Attach { name } => return attach_cmd(api, paths, name),
        Cmd::Clip { file, title } => return clip_cmd(api, file, title),
        Cmd::Mcp { action: None } => nido::mcp::run(api.clone()),
        Cmd::Serve { .. }
        | Cmd::Holder { .. }
        | Cmd::Mcp { .. }
        | Cmd::Secrets {
            action: Some(SecretsCmd::Run { .. }),
        } => unreachable!(),
        #[cfg(windows)]
        Cmd::Service { .. } => unreachable!(),
    }
    Ok(())
}

/// `nido clip`: lo que hace la herramienta `clipboard` del MCP, desde una orden de la sesión (la
/// de `NIDO_SESSION`). Con un fichero, lo lee el gestor con las comprobaciones de `send_file`
/// (nada de la carpeta de secretos); sin él, la entrada estándar. El contenido no sale por la
/// salida: solo cuánto se ha mandado. Lo lee sobre todo Claude: en el idioma de la terminal
/// (`LANG`), no siempre en español.
fn clip_cmd(api: &Api, file: Option<PathBuf>, title: Option<String>) -> io::Result<()> {
    let lang = nido::i18n::env_lang();
    let tr = |msg: &str| nido::i18n::in_lang(lang, msg);
    clip_send(api, lang, file, title)
        .map(|size| println!("{}", tr(&format!("enviado al portapapeles ({size} KB)"))))
        .map_err(|e| io::Error::new(e.kind(), tr(&e.to_string())))
}

/// Lo de `nido clip`, con los errores en español; devuelve los KB mandados.
fn clip_send(
    api: &Api,
    lang: nido::i18n::Lang,
    file: Option<PathBuf>,
    title: Option<String>,
) -> io::Result<u64> {
    use nido::outbox::{self, MAX_TEXT};
    use std::io::{IsTerminal, Read};
    let Some(session) = nido::mcp::session_from(std::env::var("NIDO_SESSION").ok()) else {
        return Err(io::Error::other(
            "falta o no es válida NIDO_SESSION: `nido clip` es para las órdenes de una sesión de nido",
        ));
    };
    let mut body = match file {
        // Absoluto: el gestor no está en esta carpeta.
        Some(f) => serde_json::json!({ "path": std::path::absolute(&f)? }),
        None => {
            let stdin = io::stdin();
            // Sin nada que leer, no se queda esperando a que se escriba.
            if stdin.is_terminal() {
                use clap::CommandFactory;
                let mut cli = Cli::command();
                cli.build();
                let clip = cli.find_subcommand_mut("clip").expect("la orden clip");
                clip.error(
                    clap::error::ErrorKind::MissingRequiredArgument,
                    nido::i18n::in_lang(
                        lang,
                        "falta qué mandar: un fichero, o la salida de una orden (`orden | nido clip`)",
                    ),
                )
                .exit();
            }
            let mut buf = Vec::new();
            stdin
                .lock()
                .take(MAX_TEXT as u64 + 1)
                .read_to_end(&mut buf)?;
            if buf.len() > MAX_TEXT {
                return Err(io::Error::other(format!(
                    "el texto pasa de {} KB",
                    MAX_TEXT / 1024
                )));
            }
            let Some(text) = outbox::text_of(buf) else {
                return Err(io::Error::other(
                    "la entrada no es texto (UTF-8): para mandar un binario, usa send_file",
                ));
            };
            if text.trim().is_empty() {
                return Err(io::Error::other("el texto está vacío"));
            }
            serde_json::json!({ "text": text })
        }
    };
    if let Some(t) = title {
        body["title"] = t.into();
    }
    // Los errores del gestor, en el mismo idioma.
    let accept = match lang {
        nido::i18n::Lang::Es => "es",
        nido::i18n::Lang::En => "en",
    };
    let r = api.call_with(
        "POST",
        "/api/v1/mcp/clipboard",
        Some(&body),
        &[("X-Nido-Session", &session), ("Accept-Language", accept)],
    )?;
    Ok(r["size"].as_u64().unwrap_or(0).div_ceil(1024).max(1))
}

/// `nido mcp on|off`: crea o borra `<config>/mcp-off`. Vale para las sesiones que arranquen luego.
fn mcp_switch(paths: &Paths, action: McpCmd) -> io::Result<()> {
    let on = matches!(action, McpCmd::On);
    launch::set_mcp(paths, on)?;
    if on {
        println!(
            "MCP de nido encendido: las próximas sesiones de Claude lo tendrán (nido lo registra \
             en Claude al arrancar `nido serve`)."
        );
    } else {
        println!(
            "MCP de nido apagado: las próximas sesiones de Claude arrancarán sin él (las que ya \
             están en marcha siguen igual).\n\
             Sigue registrado en Claude; para quitarlo también: claude mcp remove -s user nido \
             (y en cada perfil: CLAUDE_CONFIG_DIR=<perfil> claude mcp remove -s user nido)."
        );
    }
    Ok(())
}

/// `nido attach [nombre]`: elige la sesión, la arranca si está parada y engancha la terminal.
fn attach_cmd(api: &Api, paths: &Paths, name: Option<String>) -> io::Result<()> {
    let registry =
        Registry::load(&paths.registry()).map_err(|e| io::Error::other(e.to_string()))?;
    let sessions = registry.sessions();
    if sessions.is_empty() {
        return Err(io::Error::other(
            "no hay sesiones; crea una con `nido new <nombre>`",
        ));
    }
    let session = match name {
        Some(n) => registry
            .find(&n)
            .ok_or_else(|| io::Error::other(format!("no hay ninguna sesión {n:?}")))?,
        None => {
            for (i, s) in sessions.iter().enumerate() {
                let state = if launch::is_alive(paths, &s.id) {
                    "en marcha"
                } else {
                    "parada"
                };
                println!("{:>3}  {:<24} {}", i + 1, s.name, state);
            }
            print!("¿Cuál? ");
            io::Write::flush(&mut io::stdout())?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            let line = line.trim();
            line.parse::<usize>()
                .ok()
                .and_then(|n| sessions.get(n.wrapping_sub(1)))
                .or_else(|| registry.find(line))
                .ok_or_else(|| io::Error::other(format!("no es ninguna sesión: {line:?}")))?
        }
    };
    if !launch::is_alive(paths, &session.id) {
        eprintln!("Arrancando {}…", session.name);
        api.call(
            "POST",
            &format!("/api/v1/sessions/{}/start", session.id),
            None,
        )?;
    }
    // Engancharse es verla: deja de estar «sin leer» (y al soltarla, igual).
    let seen = format!("/api/v1/sessions/{}/seen", session.id);
    let _ = api.call("POST", &seen, None);
    attach_run(api, paths, session, &seen)
}

/// En Windows, de momento, las sesiones se ven en la web y la app (la consola de Windows pide su
/// propio modo crudo).
#[cfg(windows)]
fn attach_run(
    _api: &Api,
    _paths: &Paths,
    _session: &registry::Session,
    _seen: &str,
) -> io::Result<()> {
    Err(io::Error::other(
        "en Windows, nido attach aún no está: abre la sesión en la web o en la app",
    ))
}

#[cfg(unix)]
fn attach_run(api: &Api, paths: &Paths, session: &registry::Session, seen: &str) -> io::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt.block_on(attach::run(&paths.socket(&session.id)));
    let _ = api.call("POST", seen, None);
    let outcome = outcome?;
    match outcome {
        attach::Outcome::Detached => {
            eprintln!("[desenganchado de {}; sigue en marcha]", session.name)
        }
        attach::Outcome::Exited(code) => {
            let code = code.map(|c| format!(" con {c}")).unwrap_or_default();
            eprintln!("[{} terminó{code}]", session.name);
        }
        attach::Outcome::Lost => eprintln!("[se perdió la conexión con {}]", session.name),
    }
    // La lectura de la terminal se queda bloqueada en su hilo: salir sin esperarla.
    std::process::exit(0);
}

/// La tarea que arranca nido al iniciar sesión: con tu usuario y tu escritorio (sin contraseña),
/// sin ventana (`nidow.exe`), sin límite de tiempo y relanzándolo si se cae.
#[cfg(windows)]
fn windows_service(action: ServiceCmd) -> io::Result<()> {
    const TASK: &str = "nido";
    let schtasks = |args: &[&str]| -> io::Result<std::process::Output> {
        nido::sys::command("schtasks.exe").args(args).output()
    };
    match action {
        ServiceCmd::Uninstall => {
            let _ = schtasks(&["/End", "/TN", TASK]);
            let out = schtasks(&["/Delete", "/TN", TASK, "/F"])?;
            if !out.status.success() {
                return Err(io::Error::other(
                    String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                ));
            }
            println!("Tarea quitada: nido ya no arrancará al iniciar sesión.");
            Ok(())
        }
        ServiceCmd::Install { name, bind } => {
            let dir = std::env::current_exe()?
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| io::Error::other("no sé dónde está nido.exe"))?;
            let nidow = dir.join("nidow.exe");
            if !nidow.is_file() {
                return Err(io::Error::other(format!(
                    "falta {} (va junto a nido.exe)",
                    nidow.display()
                )));
            }
            let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
                (Ok(d), Ok(u)) => format!("{d}\\{u}"),
                (_, Ok(u)) => u,
                _ => return Err(io::Error::other("no sé qué usuario eres (USERNAME)")),
            };
            let esc = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
            };
            let args = format!("--env NIDO_NAME={} serve --bind {}", name, bind);
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>nido: gestor de sesiones de terminal (sobre todo Claude Code)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
      <Arguments>{args}</Arguments>
      <WorkingDirectory>{wd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
                user = esc(&user),
                cmd = esc(&nidow.to_string_lossy()),
                args = esc(&args),
                wd = esc(&nido::sys::home()
                    .unwrap_or_else(|| dir.clone())
                    .to_string_lossy()),
            );
            // schtasks quiere el XML en UTF-16 con BOM.
            let mut bytes = vec![0xFF, 0xFE];
            for u in xml.encode_utf16() {
                bytes.extend_from_slice(&u.to_le_bytes());
            }
            let file = std::env::temp_dir().join("nido-tarea.xml");
            std::fs::write(&file, bytes)?;
            let out = schtasks(&[
                "/Create",
                "/TN",
                TASK,
                "/XML",
                &file.to_string_lossy(),
                "/F",
            ]);
            let _ = std::fs::remove_file(&file);
            let out = out?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "schtasks: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let run = schtasks(&["/Run", "/TN", TASK])?;
            if !run.status.success() {
                return Err(io::Error::other(format!(
                    "la tarea está creada pero no arrancó: {}",
                    String::from_utf8_lossy(&run.stderr).trim()
                )));
            }
            println!(
                "nido arrancará solo al iniciar sesión (tarea «{TASK}»), y ya está en marcha: {bind}.\nRegistro: ~\\.local\\state\\nido\\serve.log"
            );
            Ok(())
        }
    }
}

/// Ancho en pantalla de la columna del estado en `nido ls`: cabe «te necesita 🔔10» (como mucho
/// se guardan 10 alertas y el 🔔 ocupa dos columnas).
const STATE_WIDTH: usize = 16;

/// Una línea de `nido ls`: nombre, estado (con 🔔 si hay alertas sin ver), orden y carpeta.
fn ls_line(name: &str, state: &str, alerts: usize, what: &str, cwd: &str) -> String {
    let state = match alerts {
        0 => state.to_owned(),
        1 => format!("{state} 🔔"),
        n => format!("{state} 🔔{n}"),
    };
    // A mano: `{:<N}` cuenta caracteres, no columnas.
    let pad = " ".repeat(STATE_WIDTH.saturating_sub(state.width()));
    format!("{name:<24} {state}{pad} {what:<10} {cwd}")
}

/// La hora de una alerta: «HH:MM» si es de hoy; si no, «dd/mm HH:MM».
fn clock(at: LocalTime, now: LocalTime) -> String {
    if (at.year, at.month, at.day) == (now.year, now.month, now.day) {
        format!("{:02}:{:02}", at.hour, at.min)
    } else {
        format!("{:02}/{:02} {:02}:{:02}", at.day, at.month, at.hour, at.min)
    }
}

/// Un nombre de variable de entorno que vale en todos los sistemas: `[A-Za-z_][A-Za-z0-9_]*`.
fn valid_env_name(v: &str) -> bool {
    let mut b = v.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// `nido secrets run`: la orden, con el secreto en `var`. En Unix nido se sustituye por ella
/// (`exec`); en Windows la lanza, espera y sale con su código. El valor no se escribe en ningún
/// sitio: solo va en el entorno de la orden.
fn secrets_run(paths: &Paths, name: &str, var: &str, command: &[String]) -> ExitCode {
    if !valid_env_name(var) {
        return fail(format!(
            "nombre de variable no válido: {var:?} (letras, números y «_»; no empieza por número)"
        ));
    }
    let Some((program, args)) = command.split_first() else {
        return fail("falta la orden, tras `--`");
    };
    let store = nido::secrets::Store::new(paths.state.join("secrets"));
    let session = std::env::var("NIDO_SESSION").ok();
    let path = match store.usable(session.as_deref(), name, registry::now_ms()) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    // Del error de `from_utf8`, nada: lleva dentro los bytes del secreto.
    let value = match std::fs::read(&path).map(String::from_utf8) {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            return fail(format!(
                "el secreto «{name}» no es texto: no va en una variable"
            ));
        }
        Err(e) => return fail(format!("no se pudo leer el secreto «{name}»: {e}")),
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).env(var, value);
    // La salida cortada (`… | head`) termina la orden como a cualquier otra.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    #[cfg(unix)]
    let e = {
        use std::os::unix::process::CommandExt;
        // Solo vuelve si no se pudo ejecutar.
        cmd.exec()
    };
    // Sin `exec`: nido espera a la orden. Ctrl+C llega a las dos (comparten consola); que lo
    // atienda ella y nido salga con su código. Con un manejador propio y no con
    // `SetConsoleCtrlHandler(None, TRUE)`, que la orden heredaría y tampoco atendería Ctrl+C.
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
        };
        unsafe extern "system" fn ignore(event: u32) -> windows_sys::Win32::Foundation::BOOL {
            (event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT).into()
        }
        // SAFETY: `ignore` vale para todo el proceso y no toca nada.
        unsafe {
            SetConsoleCtrlHandler(Some(ignore), 1);
        }
    }
    #[cfg(not(unix))]
    let e = match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => e,
    };
    fail(format!("no se pudo ejecutar «{program}»: {e}"))
}

/// Lo que enseña `nido secrets` de `GET /api/v1/secrets`: «nombre · hasta cuándo · quién lo
/// pidió y cuándo».
fn secret_lines(
    v: &serde_json::Value,
    now: LocalTime,
    local_time: impl Fn(u64) -> LocalTime,
) -> Vec<String> {
    v["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let until = match s["expires"].as_u64() {
                None => "siempre".to_owned(),
                Some(t) => format!("hasta las {}", clock(local_time(t), now)),
            };
            let at = clock(local_time(s["at"].as_u64().unwrap_or(0)), now);
            format!(
                "{} · {until} · lo pidió {} a las {at}",
                s["name"].as_str().unwrap_or("?"),
                s["by"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

/// Una línea de `nido alerts`: «equipo · sesión · hora · texto» (sin equipo, si no se sabe).
fn alert_line(
    machine: Option<&str>,
    session: &str,
    a: &AlertRec,
    at: LocalTime,
    now: LocalTime,
) -> String {
    let machine = machine.map_or(String::new(), |m| format!("{m} · "));
    format!("{machine}{session} · {} · {}", clock(at, now), a.text)
}

/// Lo que enseña `nido alerts` de `GET /api/v1/mcp/alerts`: una línea por alerta, de la más
/// antigua a la más nueva, con su equipo (como sale en las direcciones: el nombre o, si dos se
/// llaman igual, el id), y los equipos que no contestaron.
fn alert_lines(
    v: &serde_json::Value,
    now: LocalTime,
    local_time: impl Fn(u64) -> LocalTime,
) -> (Vec<String>, Vec<String>) {
    let mut alerts: Vec<&serde_json::Value> =
        v["alerts"].as_array().into_iter().flatten().collect();
    alerts.sort_by_key(|a| a["at"].as_u64().unwrap_or(0));
    let lines = alerts
        .into_iter()
        .map(|a| {
            let session = a["session"].as_str().unwrap_or("");
            let machine = session
                .rsplit_once('/')
                .map(|(m, _)| m)
                .or_else(|| a["machine"].as_str());
            let rec = AlertRec {
                text: a["text"].as_str().unwrap_or("").to_owned(),
                at: a["at"].as_u64().unwrap_or(0),
                kind: None,
                item: None,
            };
            let name = a["name"].as_str().unwrap_or("");
            alert_line(machine, name, &rec, local_time(rec.at), now)
        })
        .collect();
    let warnings = v["unreachable"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|u| {
            format!(
                "{}: no contesta ({})",
                u["machine"].as_str().unwrap_or("?"),
                u["error"].as_str().unwrap_or("")
            )
        })
        .collect();
    (lines, warnings)
}

/// «Cambio automático de cuenta: sí, al 99 %» (o «no»).
fn auto_line(v: &serde_json::Value) -> String {
    match v["auto_switch"].as_bool() {
        Some(true) => format!(
            "Cambio automático de cuenta: sí, al {} % (5 h o semana). `nido accounts auto off` lo apaga.",
            v["auto_pct"].as_f64().unwrap_or(99.0)
        ),
        Some(false) => {
            "Cambio automático de cuenta: no. `nido accounts auto on` lo enciende.".to_owned()
        }
        None => "Este nido no tiene cambio automático de cuenta.".to_owned(),
    }
}

/// Lo que lleva el QR de `nido pair`: el enlace a la web del hub si lo hay (la cámara del móvil
/// lo abre); si no (sin hub, o un nido de antes), el código `nido://pair?…`.
fn pair_qr_text(r: &serde_json::Value) -> &str {
    r["link"]
        .as_str()
        .filter(|l| !l.is_empty())
        .or_else(|| r["uri"].as_str())
        .unwrap_or_default()
}

/// Cómo escanear el QR de `nido pair` (`link`: es un enlace a la web) y cuánto vale (`days`: el
/// código reutilizable).
fn pair_hint(link: bool, days: Option<u64>) -> String {
    let scan = if link {
        "Escanéalo con la cámara del móvil (abre la web de nido) o con la app de nido (Emparejar)."
    } else {
        "Escanéalo con la app de nido (Emparejar)."
    };
    match days {
        None => format!("{scan} Vale 10 minutos y una sola vez."),
        Some(d) => {
            let reusable = format!(
                "Código REUTILIZABLE: vale para cualquier número de dispositivos durante {d} días.\nQuien lo tenga puede manejar este equipo. Retíralo con `nido pair --stop`."
            );
            if link {
                format!("{scan}\n{reusable}")
            } else {
                reusable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(year: i32, month: u32, day: u32, hour: u32, min: u32) -> LocalTime {
        LocalTime {
            year,
            month,
            day,
            hour,
            min,
        }
    }

    #[test]
    fn the_pair_qr_is_the_link_if_there_is_one() {
        let uri = "nido://pair?k=1";
        let link = "https://hub.example.org/app#nido=nido%3A%2F%2Fpair%3Fk%3D1";
        let r = serde_json::json!({ "uri": uri, "link": link });
        assert_eq!(pair_qr_text(&r), link);
        // Un nido de antes no manda `link`.
        assert_eq!(pair_qr_text(&serde_json::json!({ "uri": uri })), uri);
        assert_eq!(
            pair_qr_text(&serde_json::json!({ "uri": uri, "link": "" })),
            uri
        );
    }

    #[test]
    fn the_pair_hint_says_how_to_scan_and_how_long() {
        assert_eq!(
            pair_hint(false, None),
            "Escanéalo con la app de nido (Emparejar). Vale 10 minutos y una sola vez."
        );
        assert_eq!(
            pair_hint(true, None),
            "Escanéalo con la cámara del móvil (abre la web de nido) o con la app de nido (Emparejar). Vale 10 minutos y una sola vez."
        );
        let reusable = "Código REUTILIZABLE: vale para cualquier número de dispositivos durante 30 días.\nQuien lo tenga puede manejar este equipo. Retíralo con `nido pair --stop`.";
        assert_eq!(pair_hint(false, Some(30)), reusable);
        assert_eq!(
            pair_hint(true, Some(30)),
            format!(
                "Escanéalo con la cámara del móvil (abre la web de nido) o con la app de nido (Emparejar).\n{reusable}"
            )
        );
    }

    #[test]
    fn clock_is_short_today_and_with_date_otherwise() {
        let now = t(2026, 11, 10, 18, 30);
        assert_eq!(clock(t(2026, 11, 10, 9, 5), now), "09:05");
        assert_eq!(clock(t(2026, 11, 9, 23, 59), now), "09/11 23:59");
        // Mismo día y mes, otro año: con fecha.
        assert_eq!(clock(t(2025, 11, 10, 9, 5), now), "10/11 09:05");
    }

    #[test]
    fn alert_line_with_and_without_machine() {
        let a = AlertRec {
            text: "acabé los tests".into(),
            at: 0,
            kind: None,
            item: None,
        };
        let at = t(2026, 11, 10, 9, 5);
        let now = t(2026, 11, 10, 18, 30);
        assert_eq!(
            alert_line(None, "web", &a, at, now),
            "web · 09:05 · acabé los tests"
        );
        assert_eq!(
            alert_line(Some("portátil"), "web", &a, at, now),
            "portátil · web · 09:05 · acabé los tests"
        );
    }

    #[test]
    fn alerts_of_every_machine_with_their_column() {
        let v = serde_json::json!({
            "alerts": [
                {"machine": "Mac", "machine_id": "mac-id", "session": "Mac/ab", "name": "web",
                 "text": "listo", "at": 2},
                {"machine": "Portatil", "machine_id": "k1", "session": "k1/cd", "name": "api",
                 "text": "acabé los tests", "at": 1},
            ],
            "unreachable": [{"machine": "Sobremesa", "machine_id": "s-id",
                "error": "sin conexión o versión antigua"}],
        });
        let now = t(2026, 11, 10, 18, 30);
        let (lines, warnings) = alert_lines(&v, now, |at| t(2026, 11, 10, 9, at as u32));
        assert_eq!(
            lines,
            [
                "k1 · api · 09:01 · acabé los tests",
                "Mac · web · 09:02 · listo",
            ]
        );
        assert_eq!(
            warnings,
            ["Sobremesa: no contesta (sin conexión o versión antigua)"]
        );
        let (lines, warnings) = alert_lines(&serde_json::json!({"alerts": []}), now, |_| now);
        assert!(lines.is_empty() && warnings.is_empty());
    }

    #[test]
    fn env_var_names() {
        for ok in ["T", "GH_TOKEN", "_x1"] {
            assert!(valid_env_name(ok), "{ok}");
        }
        for bad in ["", "1X", "A-B", "A=B", "Ñ", "A B"] {
            assert!(!valid_env_name(bad), "{bad}");
        }
    }

    #[test]
    fn remembered_secrets_list() {
        let now = t(2026, 11, 11, 18, 0);
        let v = serde_json::json!({"secrets": [
            {"name": "gh", "expires": null, "by": "api", "at": 2},
            {"name": "npm", "expires": 5, "by": "web, api", "at": 3},
        ]});
        let lines = secret_lines(&v, now, |at| LocalTime {
            hour: at as u32,
            ..now
        });
        assert_eq!(
            lines,
            [
                "gh · siempre · lo pidió api a las 02:00",
                "npm · hasta las 05:00 · lo pidió web, api a las 03:00",
            ]
        );
        assert!(secret_lines(&serde_json::json!({"secrets": []}), now, |_| now).is_empty());
    }

    #[test]
    fn ls_line_bell_keeps_columns_aligned() {
        let plain = ls_line("web", "esperando", 0, "claude", "~/src/web");
        let one = ls_line("web", "esperando", 1, "claude", "~/src/web");
        let three = ls_line("web", "te necesita", 3, "claude", "~/src/web");
        let ten = ls_line("web", "te necesita", 10, "claude", "~/src/web");
        assert!(!plain.contains('🔔'), "{plain}");
        assert!(one.contains("esperando 🔔 "), "{one}");
        assert!(!one.contains("🔔1"), "{one}");
        assert!(three.contains("te necesita 🔔3 "), "{three}");
        assert!(ten.contains("te necesita 🔔10 "), "{ten}");
        // La orden y la carpeta empiezan en la misma columna de pantalla (el 🔔 ocupa dos).
        let col = |l: &str, s: &str| l[..l.find(s).unwrap()].width();
        for l in [&one, &three, &ten] {
            assert_eq!(col(l, "claude"), col(&plain, "claude"), "{l}");
            assert_eq!(col(l, "~/src"), col(&plain, "~/src"), "{l}");
        }
    }
}
