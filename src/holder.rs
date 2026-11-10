//! Guardián de una sesión: tiene el PTY y el proceso, el emulador con su historial, y atiende a
//! los clientes por un socket unix (en Windows, una tubería con nombre). Pequeño a propósito: si
//! falla, se lleva su sesión.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::emu::Emu;
use crate::history::History;
use crate::ipc;
use crate::proto::{self, ExitInfo, Frame, Hello, Role, Size};

pub struct HolderConfig {
    pub id: String,
    pub cwd: PathBuf,
    /// Orden y argumentos; no pasa por una shell.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub socket: PathBuf,
    pub history_file: Option<PathBuf>,
    pub history_max: usize,
    pub cols: u16,
    pub rows: u16,
    /// Lo que se escribe en el terminal nada más arrancar (p. ej. `claude --resume …` en una
    /// shell al restaurar).
    pub initial_input: Option<Vec<u8>>,
    /// Es un Claude lanzado con el channel de nido: `NIDO_CHANNEL=1`, y su `nido mcp` recoge la
    /// bandeja. Sin él, aunque el gestor lo tuviera, la sesión no lo hereda.
    pub channel: bool,
}

/// Lo que se difunde a los clientes conectados.
#[derive(Clone)]
enum Event {
    Output(Arc<Vec<u8>>),
    Exit(Option<i32>),
}

enum Cmd {
    Attach {
        client: u64,
        hello: Hello,
        reply: oneshot::Sender<(Vec<u8>, broadcast::Receiver<Event>)>,
    },
    Input {
        client: u64,
        data: Vec<u8>,
    },
    Resize {
        client: u64,
        size: Size,
    },
    History {
        all: bool,
        screen: bool,
        reply: oneshot::Sender<String>,
    },
    /// Un cliente que se quedó atrás: pantalla entera otra vez y suscripción nueva.
    Resync {
        reply: oneshot::Sender<(Vec<u8>, broadcast::Receiver<Event>)>,
    },
    Detach {
        client: u64,
    },
    /// Que pare (la trama `Stop`: en Windows es como llega lo que en Unix es SIGTERM).
    Stop,
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(2, 1000),
        rows: rows.clamp(2, 500),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn apply_size(master: &dyn MasterPty, emu: &mut Emu, size: Size) {
    if emu.size() != (size.cols, size.rows) {
        let _ = master.resize(pty_size(size.cols, size.rows));
        emu.resize(size.cols, size.rows);
    }
}

/// Las marcas que deja Claude Code a sus hijos. Si `nido serve` arranca desde una sesión de
/// Claude Code, el Claude de cada sesión las heredaría, se creería hijo de otra sesión y no
/// guardaría ni la transcripción ni su estado (se pierden el chat y el `--resume`). Las de
/// configuración (`CLAUDE_CONFIG_DIR`, `CLAUDE_EFFORT`…) se conservan.
const CLAUDE_CODE_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
];

/// Cuelga al proceso y a lo que haya lanzado. En Unix, SIGHUP a su grupo (como al cerrar un
/// terminal) y, con `force`, SIGKILL. En Windows no hay señales: se termina el árbol entero.
fn hang_up(pid: Option<u32>, force: bool) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        let signal = if force { libc::SIGKILL } else { libc::SIGHUP };
        // El hijo es líder de su sesión (portable-pty hace setsid): -pid es su grupo.
        unsafe {
            libc::kill(-pid, signal);
            libc::kill(pid, signal);
        }
    }
    #[cfg(windows)]
    {
        let _ = force;
        if let Some(pid) = pid {
            crate::sys::win::kill_tree(pid);
        }
    }
}

/// Lo que en Unix pide parar desde fuera: SIGTERM o SIGHUP. En Windows, nada (llega por la trama
/// `Stop`).
#[cfg(unix)]
async fn stop_signal() -> io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    })
}

#[cfg(windows)]
async fn stop_signal() -> io::Result<impl std::future::Future<Output = ()>> {
    Ok(std::future::pending::<()>())
}

/// Lo que se escribe al PTY. En Unix, un dup del maestro, nunca take_writer() (al soltarlo manda
/// "\n" + ^D). En Windows (ConPTY) no hay descriptor: su escritor, que no manda nada al soltarlo.
fn pty_writer(master: &dyn MasterPty) -> io::Result<Box<dyn Write + Send>> {
    #[cfg(unix)]
    {
        use std::os::fd::BorrowedFd;
        let raw = master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("el PTY no tiene descriptor"))?;
        // SAFETY: `raw` es el maestro de `master`, abierto mientras dura el guardián.
        let fd = unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned()?;
        Ok(Box::new(std::fs::File::from(fd)))
    }
    #[cfg(windows)]
    {
        master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Arranca la sesión y la atiende hasta que el proceso termina (o llega SIGTERM). Devuelve el
/// código de salida del proceso.
pub async fn run(cfg: HolderConfig) -> io::Result<Option<i32>> {
    let mut listener = ipc::Listener::bind(&cfg.socket).await?;
    let history = match &cfg.history_file {
        Some(path) => History::open(path, cfg.history_max)?,
        None => History::in_memory(cfg.history_max),
    };

    let pair = native_pty_system()
        .openpty(pty_size(cfg.cols, cfg.rows))
        .map_err(|e| io::Error::other(e.to_string()))?;
    let (program, args) = cfg
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "orden vacía"))?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.cwd(&cfg.cwd);
    for var in ["TMUX", "TMUX_PANE"].iter().chain(CLAUDE_CODE_SESSION_VARS) {
        cmd.env_remove(var);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("NIDO_SESSION", &cfg.id);
    if cfg.channel {
        cmd.env("NIDO_CHANNEL", "1");
    } else {
        cmd.env_remove("NIDO_CHANNEL");
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io::Error::other(e.to_string()))?;
    drop(pair.slave);
    let pid = child.process_id();
    let master: Box<dyn MasterPty + Send> = pair.master;

    // Leer: un hilo, porque el PTY es bloqueante.
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Escribir, en otro hilo.
    let mut writer = pty_writer(&*master)?;
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        while let Some(data) = in_rx.blocking_recv() {
            // Si falla, se sigue vaciando el canal: soltar el writer no manda nada raro.
            let _ = writer.write_all(&data).and_then(|()| writer.flush());
        }
    });
    if let Some(input) = cfg.initial_input.clone() {
        let _ = in_tx.send(input).await;
    }

    // Esperar al hijo, también en un hilo.
    let (exit_tx, mut exit_rx) = oneshot::channel::<Option<i32>>();
    std::thread::spawn(move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let _ = exit_tx.send(code);
    });

    let (events, _) = broadcast::channel::<Event>(1024);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(256);

    // Clientes.
    let accept_cmds = cmd_tx.clone();
    let acceptor = tokio::spawn(async move {
        let mut next = 0u64;
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    next += 1;
                    tokio::spawn(serve_client(next, stream, accept_cmds.clone()));
                }
                Err(e) => {
                    eprintln!("nido holder: no se pudo atender a un cliente: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    let mut emu = Emu::new(cfg.cols, cfg.rows, history);
    let mut sizes: HashMap<u64, (Role, Size)> = HashMap::new();
    let mut flush = tokio::time::interval(Duration::from_secs(2));
    let stop = stop_signal().await?;
    tokio::pin!(stop);
    let mut stopping: Option<tokio::time::Instant> = None;

    // ConPTY (Windows) empieza preguntando dónde está el cursor y no pinta nada hasta saberlo; sin
    // terminal de verdad detrás, contesta el guardián. Solo a esa primera pregunta: las demás las
    // contesta el terminal del cliente (la web), como en Unix.
    let mut answer_cursor = cfg!(windows);

    let code = loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(data) => {
                    emu.process(&data);
                    if answer_cursor && data.windows(4).any(|w| w == b"\x1b[6n") {
                        answer_cursor = false;
                        let (row, col) = emu.cursor();
                        let _ = in_tx.send(format!("\x1b[{row};{col}R").into_bytes()).await;
                    }
                    let _ = events.send(Event::Output(Arc::new(data)));
                }
                // EOF del PTY: el proceso terminó; el código llega por exit_rx.
                None => {
                    let code = (&mut exit_rx).await.ok().flatten();
                    break code;
                }
            },
            code = &mut exit_rx => break code.ok().flatten(),
            Some(cmd) = cmd_rx.recv() => match cmd {
                Cmd::Attach { client, hello, reply } => {
                    let size = Size { cols: hello.cols, rows: hello.rows };
                    sizes.insert(client, (hello.role, size));
                    // Un driver que llega se queda el tamaño (como abrir la terminal).
                    if hello.role == Role::Driver {
                        apply_size(&*master, &mut emu, size);
                    }
                    let _ = reply.send((emu.snapshot(), events.subscribe()));
                }
                Cmd::Input { client, data } => {
                    // El último driver que escribe manda en el tamaño.
                    if let Some((Role::Driver, size)) = sizes.get(&client).copied() {
                        apply_size(&*master, &mut emu, size);
                    }
                    let _ = in_tx.send(data).await;
                }
                Cmd::Resize { client, size } => {
                    if let Some(entry) = sizes.get_mut(&client) {
                        entry.1 = size;
                        if entry.0 == Role::Driver {
                            apply_size(&*master, &mut emu, size);
                        }
                    }
                }
                Cmd::History { all, screen, reply } => {
                    let text = if screen { emu.screen_text() } else { emu.history_text(all) };
                    let _ = reply.send(text);
                }
                Cmd::Resync { reply } => {
                    let _ = reply.send((emu.snapshot(), events.subscribe()));
                }
                Cmd::Detach { client } => {
                    sizes.remove(&client);
                }
                Cmd::Stop => {
                    hang_up(pid, false);
                    stopping.get_or_insert_with(tokio::time::Instant::now);
                }
            },
            _ = flush.tick() => {
                let _ = emu.history_mut().flush();
                if stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
                    hang_up(pid, true);
                }
            }
            () = &mut stop, if stopping.is_none() => {
                hang_up(pid, false);
                stopping.get_or_insert_with(tokio::time::Instant::now);
            }
        }
    };

    // Lo que quedara por leer del PTY también es historial (hasta que se cierre, con tope).
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(data) = out_rx.recv().await {
            emu.process(&data);
            let _ = events.send(Event::Output(Arc::new(data)));
        }
    })
    .await;
    let _ = events.send(Event::Exit(code));
    emu.finish();
    acceptor.abort();
    // El socket lo quita quien lo creó; en Unix se borra aquí (el listener ya se fue con la tarea).
    #[cfg(unix)]
    let _ = std::fs::remove_file(&cfg.socket);
    // Un momento para que los clientes reciban el Exit.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(code)
}

async fn serve_client(client: u64, stream: ipc::Stream, cmds: mpsc::Sender<Cmd>) {
    let (mut rd, mut wr) = ipc::split(stream);
    let hello = match proto::read_frame(&mut rd).await {
        Ok(Some(Frame::Hello(h))) => h,
        _ => return,
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if cmds
        .send(Cmd::Attach {
            client,
            hello,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    let Ok((snapshot, mut rx)) = reply_rx.await else {
        return;
    };
    if proto::write_frame(&mut wr, &Frame::Snapshot(snapshot))
        .await
        .is_err()
    {
        let _ = cmds.send(Cmd::Detach { client }).await;
        return;
    }

    // Lo que manda el cliente.
    let reader_cmds = cmds.clone();
    let (hist_tx, mut hist_rx) = mpsc::channel::<String>(4);
    let mut reading = tokio::spawn(async move {
        while let Ok(Some(frame)) = proto::read_frame(&mut rd).await {
            let cmd = match frame {
                Frame::Input(data) => Cmd::Input { client, data },
                Frame::Resize(size) => Cmd::Resize { client, size },
                Frame::Stop => Cmd::Stop,
                Frame::HistoryRequest(req) => {
                    let (reply, wait) = oneshot::channel();
                    if reader_cmds
                        .send(Cmd::History {
                            all: req.all,
                            screen: req.screen,
                            reply,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if let Ok(text) = wait.await {
                        let _ = hist_tx.send(text).await;
                    }
                    continue;
                }
                _ => continue,
            };
            if reader_cmds.send(cmd).await.is_err() {
                break;
            }
        }
    });

    // Lo que le llega: la salida en vivo, el historial que pidió y el final.
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Event::Output(data)) => {
                    if proto::write_frame(&mut wr, &Frame::Output(data.to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok(Event::Exit(code)) => {
                    let _ = proto::write_frame(&mut wr, &Frame::Exit(ExitInfo { code })).await;
                    break;
                }
                // Se ha quedado atrás: se le repinta la pantalla entera.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (reply, wait) = oneshot::channel();
                    if cmds.send(Cmd::Resync { reply }).await.is_err() {
                        break;
                    }
                    let Ok((snapshot, new_rx)) = wait.await else { break };
                    rx = new_rx;
                    if proto::write_frame(&mut wr, &Frame::Snapshot(snapshot)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(text) = hist_rx.recv() => {
                if proto::write_frame(&mut wr, &Frame::History(text.into_bytes())).await.is_err() {
                    break;
                }
            }
            // El cliente se fue.
            _ = &mut reading => break,
        }
    }
    reading.abort();
    let _ = cmds.send(Cmd::Detach { client }).await;
}
