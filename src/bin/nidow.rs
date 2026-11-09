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
    let home = std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let dir = home.join(".local").join("state").join("nido");
    let _ = std::fs::create_dir_all(&dir);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("serve.log"));
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .envs(env)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    if let Ok(log) = log {
        if let Ok(err) = log.try_clone() {
            cmd.stderr(err);
        }
        cmd.stdout(log);
    }
    // `nido serve` va en un trabajo (job) que muere con nidow: si el Programador de tareas para
    // la tarea (termina nidow), se lleva el gestor y no queda uno huérfano ocupando el puerto.
    // El trabajo deja salir a los procesos que lo pidan: los guardianes de las sesiones salen
    // (CREATE_BREAKAWAY_FROM_JOB) y siguen vivos.
    let job = kill_on_close_job();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => std::process::exit(1),
    };
    if let Some(job) = job {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: el trabajo y el proceso son nuestros; el trabajo vive hasta que acabe nidow.
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job,
                child.as_raw_handle(),
            );
        }
    }
    let code = match child.wait() {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}

#[cfg(windows)]
fn kill_on_close_job() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: un trabajo nuevo y sin nombre, con su información de límites.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0;
        ok.then_some(job)
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("nidow: solo hace falta en Windows; usa nido");
    std::process::exit(2);
}
