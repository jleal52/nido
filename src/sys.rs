//! Lo que cambia de un sistema a otro (Linux, macOS, Windows): permisos de ficheros, procesos,
//! el nombre del equipo y en qué arranque estamos.

use std::fs::OpenOptions;
use std::path::Path;

/// Ficheros con secretos o historial: solo para el usuario (0600). En Windows, los permisos que
/// hereda del perfil del usuario, que ya son solo suyos.
pub fn private(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(opts, 0o600);
    opts
}

/// Un directorio solo para el usuario (0700).
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Crea `dir` y las carpetas que le falten, ya solo para el usuario (0700) desde el primer
/// momento. Las que ya existían quedan como estaban (para eso, `private_dir`).
pub fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Un fichero que ya existe, solo para el usuario (0600).
pub fn private_file(file: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::set_permissions(file, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
            return None;
        }
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8(buf[..n].to_vec()).ok()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
}

/// Una hora local, hasta el minuto.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1–12.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
}

/// La hora local de un instante (ms desde 1970), con la zona del sistema.
pub fn local_time(ms: u64) -> Option<LocalTime> {
    let secs = libc::time_t::try_from(ms / 1000).ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return None;
    }
    #[cfg(windows)]
    if unsafe { libc::localtime_s(&mut tm, &secs) } != 0 {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: u32::try_from(tm.tm_mon + 1).ok()?,
        day: u32::try_from(tm.tm_mday).ok()?,
        hour: u32::try_from(tm.tm_hour).ok()?,
        min: u32::try_from(tm.tm_min).ok()?,
    })
}

/// El usuario, para separar lo de cada uno en los directorios compartidos (el temporal).
pub fn user_tag() -> String {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }.to_string()
    }
    #[cfg(windows)]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "usuario".into())
    }
}

/// El directorio del usuario: `HOME` y, en Windows, `USERPROFILE`.
pub fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("USERPROFILE")
            } else {
                None
            }
        })
        .map(std::path::PathBuf::from)
}

/// La ruta absoluta y sin enlaces. En Windows sin el prefijo `\\?\` que pone `canonicalize`
/// (`\\?\C:\x` → `C:\x`): `cmd` no lo acepta como carpeta y Claude no lo usa al nombrar sus
/// conversaciones.
pub fn canonical(path: &Path) -> std::path::PathBuf {
    let Ok(c) = std::fs::canonicalize(path) else {
        return path.to_owned();
    };
    #[cfg(windows)]
    if let Some(rest) = c.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return rest.into();
    }
    c
}

/// En Windows, que los hijos no hereden la entrada y salida de este proceso. Al lanzar un
/// guardián, Windows le pasa todos los identificadores heredables, y la salida del gestor (su
/// registro) se quedaría abierta en cada sesión: un gestor nuevo no podría abrirla para escribir.
pub fn keep_stdio_to_self() {
    #[cfg(windows)]
    win::keep_stdio_to_self();
}

/// ¿Sigue existiendo el proceso?
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(pid).is_ok_and(|p| unsafe { libc::kill(p, 0) } == 0)
    }
    #[cfg(windows)]
    {
        win::alive(pid)
    }
}

/// Pide al proceso que termine (SIGTERM). En Windows no hay señales: lo termina sin más.
pub fn terminate(pid: u32) {
    #[cfg(unix)]
    if let Ok(p) = i32::try_from(pid) {
        unsafe {
            libc::kill(p, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    win::kill_tree(pid);
}

/// Lo que distingue este arranque del equipo de los anteriores.
pub fn boot_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::timeval>();
        // SAFETY: kern.boottime es un timeval y el buffer tiene su tamaño.
        let r = unsafe {
            libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                (&mut tv as *mut libc::timeval).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0).then(|| format!("{}.{}", tv.tv_sec, tv.tv_usec))
    }
    #[cfg(windows)]
    {
        // La hora del arranque: ahora menos lo que lleva encendido. Sale con unos milisegundos
        // de baile entre llamadas: por eso `same_boot` compara con margen. Y la sesión de Windows
        // (su LUID): al cerrar sesión mueren las sesiones de nido, y al volver a entrar hay que
        // restaurarlas aunque el equipo no se haya reiniciado.
        let up = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        let boot = now.saturating_sub(up) / 1000;
        Some(match win::logon_id() {
            Some(l) => format!("{boot}:{l}"),
            None => boot.to_string(),
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}

/// ¿Es el mismo arranque? Iguales, o (si son horas, como en Windows) a menos de un minuto; lo que
/// vaya tras `:` (la sesión de Windows), igual.
pub fn same_boot(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ta, ra) = a.split_once(':').unwrap_or((a, ""));
    let (tb, rb) = b.split_once(':').unwrap_or((b, ""));
    if ra != rb {
        return false;
    }
    match (ta.parse::<f64>(), tb.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() < 60.0,
        _ => false,
    }
}

/// Una orden para lanzar desde nido. En Windows, sin ventana de consola: el gestor y los
/// guardianes no tienen consola, y cada programa de consola que lanzaran abriría la suya.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, win::CREATE_NO_WINDOW);
    cmd
}

/// Lo mismo para tokio.
pub fn async_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(win::CREATE_NO_WINDOW);
    cmd
}

#[cfg(windows)]
pub mod win {
    use std::collections::HashMap;

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, TerminateProcess,
    };

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub const DETACHED_PROCESS: u32 = 0x0000_0008;
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// Todos los procesos: pid, el de su padre y el nombre del ejecutable (`claude.exe`).
    pub fn processes() -> Vec<(u32, u32, String)> {
        let mut out = Vec::new();
        // SAFETY: instantánea de los procesos; se cierra al acabar.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                out.push((e.th32ProcessID, e.th32ParentProcessID, name));
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn parents() -> HashMap<u32, u32> {
        processes().into_iter().map(|(p, pp, _)| (p, pp)).collect()
    }

    /// El nombre del ejecutable sin `.exe` (`claude`), como `comm` en Linux.
    pub fn comm(pid: u32) -> Option<String> {
        let (_, _, name) = processes().into_iter().find(|(p, _, _)| *p == pid)?;
        let lower = name.to_ascii_lowercase();
        Some(match lower.strip_suffix(".exe") {
            Some(_) => name[..name.len() - 4].to_owned(),
            None => name,
        })
    }

    /// El identificador de esta sesión de Windows (el LUID de su inicio de sesión).
    pub fn logon_id() -> Option<String> {
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: se lee la información del token propio y se cierra.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut st: TOKEN_STATISTICS = std::mem::zeroed();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenStatistics,
                (&mut st as *mut TOKEN_STATISTICS).cast(),
                std::mem::size_of::<TOKEN_STATISTICS>() as u32,
                &mut len,
            ) != 0;
            CloseHandle(token);
            ok.then(|| {
                format!(
                    "{}-{}",
                    st.AuthenticationId.HighPart, st.AuthenticationId.LowPart
                )
            })
        }
    }

    pub fn keep_stdio_to_self() {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: identificadores propios; quitarles la herencia no los cierra.
            unsafe {
                let h = GetStdHandle(which);
                if !h.is_null() && h != INVALID_HANDLE_VALUE {
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    pub fn alive(pid: u32) -> bool {
        // SAFETY: se abre para preguntar y se cierra.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }

    fn kill(pid: u32) {
        // SAFETY: se abre para terminarlo y se cierra.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    /// Cuándo se creó el proceso (en unidades de 100 ns).
    fn created(pid: u32) -> Option<u64> {
        // SAFETY: se abre para preguntar y se cierra.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let z = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut c, mut e, mut k, mut u) = (z, z, z, z);
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
            CloseHandle(h);
            ok.then(|| (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
        }
    }

    /// Los descendientes de `root` (él incluido, el primero). En Windows el padre de un proceso es
    /// solo un número: si el padre murió y otro proceso heredó su pid, el huérfano parecería hijo
    /// del nuevo. Por eso un hijo cuenta solo si se creó después que su padre.
    pub fn tree(root: u32) -> Vec<u32> {
        let parents = parents();
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            let born = created(p);
            let mut kids: Vec<u32> = parents
                .iter()
                .filter(|&(c, pp)| *pp == p && *c != p && !out.contains(c))
                .map(|(c, _)| *c)
                .filter(|c| match (born, created(*c)) {
                    (Some(b), Some(k)) => k >= b,
                    _ => false,
                })
                .collect();
            kids.sort_unstable();
            out.extend(kids);
            i += 1;
        }
        out
    }

    /// El proceso y todos sus descendientes (lo que en Unix sería su grupo): primero los hijos,
    /// para que no los herede nadie.
    pub fn kill_tree(root: u32) {
        for pid in tree(root).into_iter().rev() {
            kill(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_boot_allows_jitter_only_for_times() {
        assert!(same_boot("abc", "abc"));
        assert!(!same_boot("abc", "abd"));
        assert!(same_boot("1793948104", "1793948105"));
        assert!(!same_boot("1793948104", "1793958104"));
        assert!(same_boot("1793948104.463287", "1793948104.463287"));
        assert!(same_boot("1793948104:0-1234", "1793948120:0-1234"));
        assert!(!same_boot("1793948104:0-1234", "1793948104:0-9999"));
    }

    #[test]
    fn local_time_moves_by_the_minute() {
        // 2023-11-14 22:00:00 UTC: las zonas van de cuarto en cuarto de hora.
        let a = local_time(1_699_999_200_000).unwrap();
        let b = local_time(1_699_999_260_000).unwrap();
        assert!([0, 15, 30, 45].contains(&a.min), "{a:?}");
        assert_eq!(b.min, a.min + 1);
        assert_eq!(
            (b.year, b.month, b.day, b.hour),
            (a.year, a.month, a.day, a.hour)
        );
        assert!((1..=12).contains(&a.month) && (1..=31).contains(&a.day) && a.hour < 24);
    }
}
