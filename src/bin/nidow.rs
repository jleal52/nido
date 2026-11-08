//! `nidow`: en Windows, lanza `nido` sin ventana. La tarea programada que arranca nido al iniciar
//! sesión lo usa: `nido.exe` es un programa de consola y, lanzado tal cual, abriría la suya.
//!
//! `nidow [--env CLAVE=valor]… <argumentos de nido>`: busca `nido.exe` junto a sí mismo, lo lanza
//! sin consola con la salida en `~/.local/state/nido/serve.log` y termina con su mismo código
//! (así el Programador de tareas sabe si tiene que volver a lanzarlo).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut env = Vec::new();
    while args.first().map(String::as_str) == Some("--env") && args.len() >= 2 {
        if let Some((k, v)) = args[1].split_once('=') {
            env.push((k.to_owned(), v.to_owned()));
        }
        args.drain(..2);
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nido.exe")))
        .unwrap_or_else(|| "nido.exe".into());
    let home = std::env::var_os("USERPROFILE").map(std::path::PathBuf::from).unwrap_or_default();
    let dir = home.join(".local").join("state").join("nido");
    let _ = std::fs::create_dir_all(&dir);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("serve.log"));
    let mut cmd = Command::new(&exe);
    cmd.args(&args).envs(env).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    if let Ok(log) = log {
        if let Ok(err) = log.try_clone() {
            cmd.stderr(err);
        }
        cmd.stdout(log);
    }
    let code = match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("nidow: solo hace falta en Windows; usa nido");
    std::process::exit(2);
}
