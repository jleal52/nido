//! Claude Code dentro de una sesión: encontrarlo, saber qué conversación tiene abierta y en
//! qué estado está, y las órdenes para lanzarlo o retomarlo.
//!
//! Claude Code escribe `<config>/sessions/<pid>.json` (`sessionId`, `cwd`, `name`, `status`…).
//! Es un formato interno: solo se leen esos campos, y si cambia se sigue con lo último conocido.
//! Los `.key` de ese directorio no se tocan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::registry::ClaudeLink;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeInfo {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    /// `busy`, `idle`… (lo que ponga Claude Code).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundClaude {
    pub pid: u32,
    pub info: ClaudeInfo,
    /// El `CLAUDE_CONFIG_DIR` con el que corre; `None` si es el de siempre (`~/.claude`).
    pub config_dir: Option<PathBuf>,
}

/// Acceso a `/proc` (en las pruebas, uno falso).
pub struct Procfs {
    root: PathBuf,
}

impl Default for Procfs {
    fn default() -> Self {
        Self {
            root: "/proc".into(),
        }
    }
}

impl Procfs {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// En macOS no hay `/proc`: se pregunta al núcleo (`sysctl`). En Windows, a la instantánea
    /// de procesos (Toolhelp).
    #[cfg(any(target_os = "macos", windows))]
    fn native(&self) -> bool {
        self.root == Path::new("/proc")
    }

    /// `pid → ppid` de todos los procesos.
    fn parents(&self) -> HashMap<u32, u32> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::parents();
        }
        #[cfg(windows)]
        if self.native() {
            return crate::sys::win::parents();
        }
        let mut map = HashMap::new();
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return map;
        };
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "pid (comm) estado ppid …": el comm puede llevar espacios y paréntesis.
            let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            if let Some(ppid) = after.split_whitespace().nth(1).and_then(|p| p.parse().ok()) {
                map.insert(pid, ppid);
            }
        }
        map
    }

    /// El proceso y todos sus descendientes, del más cercano al más lejano.
    pub fn tree(&self, root: u32) -> Vec<u32> {
        // En Windows, comprobando que cada hijo nació después que su padre (pids reusados).
        #[cfg(windows)]
        if self.native() {
            return crate::sys::win::tree(root);
        }
        let parents = self.parents();
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            let mut kids: Vec<u32> = parents
                .iter()
                .filter(|&(_, &pp)| pp == p)
                .map(|(&c, _)| c)
                .collect();
            kids.sort_unstable();
            out.extend(kids);
            i += 1;
        }
        out
    }

    pub fn comm(&self, pid: u32) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::comm(pid);
        }
        #[cfg(windows)]
        if self.native() {
            return crate::sys::win::comm(pid);
        }
        let s = std::fs::read_to_string(self.root.join(pid.to_string()).join("comm")).ok()?;
        Some(s.trim_end().to_owned())
    }

    /// Los argumentos con los que se lanzó (`argv`, con el programa). `None` si no se pueden
    /// leer; en Windows, siempre (leerlos de otro proceso no es sencillo).
    pub fn args(&self, pid: u32) -> Option<Vec<String>> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::args(pid);
        }
        #[cfg(windows)]
        if self.native() {
            let _ = pid;
            return None;
        }
        let raw = std::fs::read(self.root.join(pid.to_string()).join("cmdline")).ok()?;
        // Cada argumento acaba en un cero. Vacío: un zombi o un hilo del núcleo.
        let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
        if raw.is_empty() {
            return None;
        }
        Some(
            raw.split(|&b| b == 0)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        )
    }

    pub fn env_var(&self, pid: u32, key: &str) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::env_var(pid, key);
        }
        // En Windows leer el entorno de otro proceso no es sencillo: sin `CLAUDE_CONFIG_DIR`
        // propio por sesión, Claude se busca en `~/.claude`.
        #[cfg(windows)]
        if self.native() {
            let _ = (pid, key);
            return None;
        }
        let raw = std::fs::read(self.root.join(pid.to_string()).join("environ")).ok()?;
        raw.split(|&b| b == 0).find_map(|kv| {
            let kv = std::str::from_utf8(kv).ok()?;
            kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned)
        })
    }
}

/// Lo mismo que `/proc` en macOS, con `sysctl`: la lista de procesos (`KERN_PROC_ALL`) y los
/// argumentos y el entorno de uno (`KERN_PROCARGS2`, solo de los procesos del mismo usuario).
#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashMap;

    /// Todos los pids, y de cada uno su padre y su nombre (libproc).
    fn all() -> Vec<(u32, u32, String)> {
        // SAFETY: con buffer nulo, proc_listallpids devuelve cuántos hay.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; n as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: el buffer tiene sitio para `pids.len()` pids.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        pids.truncate(n as usize);
        pids.into_iter()
            .filter_map(|pid| info(pid as u32))
            .collect()
    }

    fn info(pid: u32) -> Option<(u32, u32, String)> {
        // SAFETY: proc_bsdinfo es POD; proc_pidinfo escribe como mucho su tamaño.
        let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if r != size {
            return None;
        }
        let name: Vec<u8> = bsd
            .pbi_comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some((pid, bsd.pbi_ppid, String::from_utf8(name).ok()?))
    }

    pub fn parents() -> HashMap<u32, u32> {
        all().into_iter().map(|(p, pp, _)| (p, pp)).collect()
    }

    /// El nombre con el que se lanzó (`argv[0]`, sin ruta) y, si no se puede leer, el del
    /// ejecutable. El instalador de Claude deja el binario con el nombre de su versión
    /// (`~/.local/share/claude/versions/2.1.319`) y un enlace `claude`: el núcleo lo llama
    /// «2.1.319», pero `argv[0]` sigue siendo `claude`.
    pub fn comm(pid: u32) -> Option<String> {
        argv0(pid).or_else(|| info(pid).map(|(_, _, n)| n))
    }

    fn argv0(pid: u32) -> Option<String> {
        let raw = procargs(pid)?;
        // Tras argc: la ruta del ejecutable, ceros de relleno y los argumentos.
        let mut parts = raw.get(4..)?.split(|&b| b == 0).filter(|p| !p.is_empty());
        parts.next()?;
        let arg = std::str::from_utf8(parts.next()?).ok()?;
        let name = arg.rsplit('/').next().unwrap_or(arg);
        (!name.is_empty()).then(|| name.to_owned())
    }

    /// `argc`, la ruta del ejecutable, los argumentos y el entorno, separados por ceros.
    fn procargs(pid: u32) -> Option<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: lee un entero.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; argmax.max(4096) as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        // SAFETY: el buffer tiene `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        Some(buf)
    }

    /// Los argumentos (`argv`): tras `argc`, la ruta del ejecutable, ceros de relleno y los
    /// `argc` argumentos.
    pub fn args(pid: u32) -> Option<Vec<String>> {
        let raw = procargs(pid)?;
        let argc = i32::from_ne_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        let rest = raw.get(4..)?;
        let mut at = rest.iter().position(|&b| b == 0)?;
        while rest.get(at) == Some(&0) {
            at += 1;
        }
        let args: Vec<String> = rest[at..]
            .split(|&b| b == 0)
            .take(argc)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        (args.len() == argc).then_some(args)
    }

    pub fn env_var(pid: u32, key: &str) -> Option<String> {
        let raw = procargs(pid)?;
        let argc = i32::from_ne_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        // Tras argc: la ruta, ceros de relleno, los argc argumentos y luego el entorno.
        let mut parts = raw[4..].split(|&b| b == 0).filter(|p| !p.is_empty());
        parts.next()?; // la ruta del ejecutable
        for _ in 0..argc {
            parts.next()?;
        }
        parts.find_map(|kv| {
            let kv = std::str::from_utf8(kv).ok()?;
            kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned)
        })
    }
}

/// El primer Claude por encima del proceso `pid` (su padre, el padre de este…). Para el `nido
/// mcp` de una sesión, el Claude que lo lanzó, aunque haya algo en medio (`cmd /c` en Windows).
pub fn claude_ancestor(procfs: &Procfs, pid: u32) -> Option<u32> {
    let parents = procfs.parents();
    let mut at = pid;
    // Con tope: un árbol raro (pids reusados) no deja dando vueltas.
    for _ in 0..64 {
        let up = *parents.get(&at)?;
        if up == 0 || up == at {
            return None;
        }
        if procfs.comm(up).is_some_and(|n| is_claude(&[n])) {
            return Some(up);
        }
        at = up;
    }
    None
}

/// El Claude de una sesión (su guardián, `holder`): el que apunta de sí mismo lo que hace
/// (`find_claude`) y, si no lo hay (otra versión de Claude que lo guarde en otro sitio, o en
/// Windows con su perfil propio), el primer Claude de su árbol. Instalado con npm su proceso es
/// `node`, y no se encuentra.
pub fn session_claude(procfs: &Procfs, holder: u32, home: &Path) -> Option<u32> {
    if let Some(f) = find_claude(procfs, holder, home) {
        return Some(f.pid);
    }
    procfs
        .tree(holder)
        .into_iter()
        .skip(1)
        .find(|&p| procfs.comm(p).is_some_and(|n| is_claude(&[n])))
}

/// ¿Hay algún Claude en marcha en este equipo, sea o no de nido?
pub fn any_running() -> bool {
    #[cfg(windows)]
    {
        crate::sys::win::processes().iter().any(|(_, _, n)| {
            let n = n.to_ascii_lowercase();
            n == "claude.exe" || n == "claude"
        })
    }
    #[cfg(not(windows))]
    {
        let p = Procfs::default();
        p.parents()
            .keys()
            .any(|&pid| p.comm(pid).as_deref() == Some("claude"))
    }
}

pub fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

pub fn live_info(config_dir: &Path, pid: u32) -> Option<ClaudeInfo> {
    let raw = std::fs::read(config_dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// El primer Claude en el árbol del proceso de la sesión (él mismo o un descendiente).
pub fn find_claude(procfs: &Procfs, root_pid: u32, home: &Path) -> Option<FoundClaude> {
    procfs.tree(root_pid).into_iter().find_map(|pid| {
        if procfs.comm(pid).as_deref() != Some("claude") {
            return None;
        }
        let custom = procfs
            .env_var(pid, "CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty());
        let dir = custom
            .as_ref()
            .map_or_else(|| default_config_dir(home), PathBuf::from);
        let info = live_info(&dir, pid)?;
        let config_dir = custom
            .map(PathBuf::from)
            .filter(|d| *d != default_config_dir(home));
        Some(FoundClaude {
            pid,
            info,
            config_dir,
        })
    })
}

/// Un Claude nuevo con la conversación ya identificada (`--session-id`) y con nombre.
pub fn new_argv(name: &str, session_id: &str) -> Vec<String> {
    ["claude", "--session-id", session_id, "-n", name]
        .map(str::to_owned)
        .to_vec()
}

/// Retomar la conversación. Devuelve la orden y las variables de entorno (su perfil de Claude,
/// si no es `~/.claude`). La cuenta es la de Claude: las sesiones con cuenta propia de cswap (ya
/// no se crean) siguen con la de todas.
pub fn resume_argv(link: &ClaudeLink) -> (Vec<String>, Vec<(String, String)>) {
    let argv = vec!["claude".into(), "--resume".into(), link.session_id.clone()];
    let env = match (&link.account, &link.config_dir) {
        (None, Some(dir)) => vec![(
            "CLAUDE_CONFIG_DIR".into(),
            dir.to_string_lossy().into_owned(),
        )],
        _ => vec![],
    };
    (argv, env)
}

/// ¿La orden es `claude`? (Con ruta o sin ella, y en Windows `claude.exe` o `claude.cmd`.)
pub fn is_claude(argv: &[String]) -> bool {
    argv.first().is_some_and(|a| {
        let name = a
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(a)
            .to_ascii_lowercase();
        let name = name
            .strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".cmd"))
            .unwrap_or(&name);
        name == "claude"
    })
}

/// El channel de nido en Claude: cargarlo (es de desarrollo: Claude pregunta al arrancar) y que
/// algunas de sus herramientas no pidan permiso.
const CHANNELS_OPT: &str = "--dangerously-load-development-channels";
const NIDO_CHANNEL: &str = "server:nido";
const ALLOWED_OPTS: [&str; 2] = ["--allowedTools", "--allowed-tools"];
/// Las herramientas de nido que no piden permiso: las que miran y hablan, y las que piden y
/// mandan cosas al usuario (`secret` lo controla su diálogo; `clipboard` y `send_file` solo le
/// mandan algo a él), y las del plan de tareas (solo tocan el de la propia sesión). Las que actúan sobre otras sesiones (`open`, `answer`, `close`, `start`,
/// `stop`) pasan por el permiso de Claude como cualquier otra (en modo auto, lo decide su
/// clasificador): una sesión engañada (un mensaje con instrucciones metidas) no puede abrir,
/// parar o arrancar sesiones ni aprobar sus preguntas sin que nadie lo vea.
pub const NIDO_TOOLS: [&str; 11] = [
    "mcp__nido__sessions",
    "mcp__nido__read",
    "mcp__nido__send",
    "mcp__nido__alert",
    "mcp__nido__secret",
    "mcp__nido__clipboard",
    "mcp__nido__send_file",
    "mcp__nido__plan_set",
    "mcp__nido__plan_task",
    "mcp__nido__plan_get",
    "mcp__nido__plan_clear",
];
const DENIED_OPTS: [&str; 2] = ["--disallowedTools", "--disallowed-tools"];
/// Lo que ponía nido antes: todas sus herramientas sin permiso. Si viene en una orden (de una
/// sesión de entonces), sale.
const OLD_NIDO_TOOLS: &str = "mcp__nido__*";

/// Los valores que lleva la opción `names` en `argv`, como los lee Claude (commander): con `=`,
/// uno; si no, todo lo que la sigue hasta la siguiente opción.
fn option_values<'a>(argv: &'a [String], names: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--" {
            break;
        }
        if let Some((name, value)) = a.split_once('=')
            && names.contains(&name)
        {
            out.push(value);
        } else if names.contains(&a) {
            while argv.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                i += 1;
                out.push(argv[i].as_str());
            }
        }
        i += 1;
    }
    out
}

/// La orden de una sesión de Claude con el MCP de nido (si `on`, el MCP está encendido en este
/// equipo): el channel y sus herramientas que no piden permiso (`NIDO_TOOLS`), si no están ya.
/// Todas las de nido sin permiso (`mcp__nido__*`, como iba antes), fuera.
///
/// Las dos opciones admiten varios valores: lo que las sigue, hasta la siguiente opción, es
/// suyo. Por eso van delante de la primera opción (justo tras el programa, en las órdenes de
/// nido), y si detrás solo hay textos sueltos (el mensaje inicial), tras un `--`.
pub fn with_nido_mcp(argv: &[String], on: bool) -> Vec<String> {
    if !on || !is_claude(argv) {
        return argv.to_vec();
    }
    let argv = &without_old_tools(argv);
    let mut add: Vec<String> = Vec::new();
    if !option_values(argv, &[CHANNELS_OPT]).contains(&NIDO_CHANNEL) {
        add.extend([CHANNELS_OPT.to_owned(), NIDO_CHANNEL.to_owned()]);
    }
    let allowed = option_values(argv, &ALLOWED_OPTS);
    let missing: Vec<String> = NIDO_TOOLS
        .iter()
        .filter(|t| !allowed.contains(t))
        .map(|t| (*t).to_owned())
        .collect();
    if !missing.is_empty() {
        add.push(ALLOWED_OPTS[0].to_owned());
        add.extend(missing);
    }
    if add.is_empty() {
        return argv.to_vec();
    }
    insert_opts(argv, add)
}

/// `argv` con `add` (opciones de nido) delante de la primera opción; si no hay ninguna y detrás
/// del programa solo hay textos sueltos (el mensaje inicial), tras un `--`.
fn insert_opts(argv: &[String], mut add: Vec<String>) -> Vec<String> {
    let at = argv
        .iter()
        .skip(1)
        .position(|a| a.starts_with('-'))
        .map(|p| p + 1);
    let mut out = argv.to_vec();
    match at {
        Some(at) => {
            out.splice(at..at, add);
        }
        None => {
            if argv.len() > 1 {
                add.push("--".to_owned());
            }
            out.splice(1..1, add);
        }
    }
    out
}

/// Una regla de `tool` (`Read` o `Edit`) para la carpeta de secretos de nido (`dir`, ver
/// `secrets.rs`). Ruta absoluta: `//` delante. En Windows, como la normaliza Claude:
/// `C:\Users\u` es `/c/Users/u`.
pub fn secrets_rule(tool: &str, dir: &str) -> String {
    // Lo que añade `canonicalize` en Windows.
    let dir = dir.strip_prefix(r"\\?\").unwrap_or(dir);
    let d = dir.replace('\\', "/");
    let d = match d.as_bytes() {
        [l, b':', rest @ ..] if l.is_ascii_alphabetic() => format!(
            "/{}{}",
            (*l as char).to_ascii_lowercase(),
            String::from_utf8_lossy(rest)
        ),
        _ => d,
    };
    format!("{tool}(/{}/**)", d.trim_end_matches('/'))
}

/// Las reglas que no dejan a Claude leer ni tocar la carpeta de secretos de nido: `Read`
/// (Claude la aplica también a `cat`, `head`, `<`… en Bash) y `Edit` (también Write). Con la
/// ruta absoluta y, si hay un enlace por medio, también con la de verdad: Claude compara la
/// ruta tal como le llega.
pub fn secrets_rules(dir: &Path) -> Vec<String> {
    let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
    let mut dirs = vec![abs.clone()];
    if let Some(real) = real_path(&abs)
        && real != abs
    {
        dirs.push(real);
    }
    dirs.iter()
        .flat_map(|d| {
            let d = d.to_string_lossy();
            ["Read", "Edit"].map(|t| secrets_rule(t, &d))
        })
        .collect()
}

/// `p` sin enlaces, aunque aún no exista: la parte que existe, resuelta, y el resto tal cual.
fn real_path(p: &Path) -> Option<PathBuf> {
    let mut base = p;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return Some(rest.iter().rev().fold(real, |acc, c| acc.join(c)));
        }
        rest.push(base.file_name()?);
        base = base.parent()?;
    }
}

/// `argv` (de `with_nido_mcp`) con `--disallowedTools` y las reglas de `rules` que no lleve ya.
pub fn with_secrets_guard(argv: &[String], rules: &[String]) -> Vec<String> {
    if !is_claude(argv) {
        return argv.to_vec();
    }
    let has = option_values(argv, &DENIED_OPTS);
    let missing: Vec<String> = rules
        .iter()
        .filter(|r| !has.contains(&r.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return argv.to_vec();
    }
    let mut add = vec![DENIED_OPTS[0].to_owned()];
    add.extend(missing);
    insert_opts(argv, add)
}

/// La primera versión de Claude Code con channels (`--channels` y
/// `--dangerously-load-development-channels`, en vista previa). No sale en `claude --help`
/// (están ocultas mientras dure la vista previa), así que se mira la versión. Un Claude más
/// antiguo no conoce esas opciones y no arranca con ellas.
pub const CHANNELS_SINCE: (u32, u32, u32) = (2, 1, 80);

/// La versión de `claude --version` («2.1.322 (Claude Code)»).
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().next()?;
    let mut n = word.split('.').map(|p| p.parse::<u32>().ok());
    let v = (n.next()??, n.next()??, n.next()??);
    n.next().is_none().then_some(v)
}

/// ¿Conoce el `claude` de `program` las opciones del channel (ver `CHANNELS_SINCE`)? Se le
/// pregunta la versión una vez por programa: por su ruta de verdad (sin enlaces) y su fecha de
/// modificación, que cambian al actualizarlo. `None`: no se sabe (no contesta, o no dice una
/// versión), y se lanza con ellas como siempre. La primera vez que no, se dice en el log.
pub fn channels_supported(program: &Path) -> Option<bool> {
    use std::sync::{Mutex, OnceLock};
    type Seen = HashMap<(PathBuf, std::time::SystemTime), Option<bool>>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let real = std::fs::canonicalize(program).ok()?;
    let modified = std::fs::metadata(&real).and_then(|m| m.modified()).ok()?;
    let key = (real, modified);
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some(known) = seen.lock().expect("versiones").get(&key) {
        return *known;
    }
    let cli = Program {
        path: program.to_owned(),
        home: None,
        cwd: Some(std::env::temp_dir()),
    };
    // Recién escrito (Claude Code se acaba de actualizar, o en las pruebas), lanzarlo puede dar
    // «Text file busy» un momento: se reintenta. Y si no se pudo lanzar, no se recuerda: la próxima
    // vez se vuelve a preguntar (si no, sus sesiones irían sin el MCP de nido hasta que cambiara).
    let mut ran = cli.run(None, &["--version"]);
    for _ in 0..20 {
        match &ran {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(25));
                ran = cli.run(None, &["--version"]);
            }
            _ => break,
        }
    }
    let Ok(ran) = ran else {
        return None;
    };
    let version = Some(ran)
        .filter(|(ok, _)| *ok)
        .and_then(|(_, text)| parse_version(&text));
    let supported = version.map(|v| v >= CHANNELS_SINCE);
    if let (Some(false), Some((a, b, c))) = (supported, version) {
        let (x, y, z) = CHANNELS_SINCE;
        eprintln!(
            "nido: {} es Claude Code {a}.{b}.{c}, sin channels (hacen falta la {x}.{y}.{z} o \
             más): sus sesiones se lanzan sin el MCP de nido",
            program.display()
        );
    }
    seen.lock().expect("versiones").insert(key, supported);
    supported
}

/// `argv` sin `mcp__nido__*` en `--allowedTools` (ni la opción, si solo llevaba ese).
fn without_old_tools(argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if i > 0 && a == "--" {
            out.extend_from_slice(&argv[i..]);
            break;
        }
        if i > 0
            && let Some((opt, value)) = a.split_once('=')
            && ALLOWED_OPTS.contains(&opt)
            && value == OLD_NIDO_TOOLS
        {
            i += 1;
            continue;
        }
        if i > 0 && ALLOWED_OPTS.contains(&a) {
            let mut values = Vec::new();
            while let Some(v) = argv.get(i + 1).filter(|v| !v.starts_with('-')) {
                values.push(v.clone());
                i += 1;
            }
            let had = !values.is_empty();
            values.retain(|v| v != OLD_NIDO_TOOLS);
            if !had || !values.is_empty() {
                out.push(a.to_owned());
                out.extend(values);
            }
            i += 1;
            continue;
        }
        out.push(a.to_owned());
        i += 1;
    }
    out
}

/// Las opciones propias con las que se lanzó `claude` (`--model haiku`, `--mcp-config x`…), para
/// conservarlas al retomar. Fuera las que eligen la conversación o el nombre (al retomar las pone
/// nido), las del MCP de nido (las pone en cada arranque) y lo que no es una opción (el mensaje
/// inicial). `argv` es la orden de la sesión.
pub fn extra_args(argv: &[String]) -> Vec<String> {
    if !is_claude(argv) {
        return Vec::new();
    }
    // Opciones que llevan valor detrás (las demás son interruptores).
    const DROP_WITH_VALUE: [&str; 5] = ["--resume", "-r", "--session-id", "-n", "--name"];
    const DROP_FLAGS: [&str; 3] = ["--continue", "-c", "--fork-session"];
    // El valor que pone nido en cada arranque: ese sale, los demás de la opción se quedan.
    let nido_value = |opt: &str, value: &str| {
        (opt == CHANNELS_OPT && value == NIDO_CHANNEL)
            || (ALLOWED_OPTS.contains(&opt)
                && (NIDO_TOOLS.contains(&value) || value == OLD_NIDO_TOOLS))
    };
    let mut out = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        // Tras `--`, solo textos sueltos (el mensaje inicial).
        if a == "--" {
            break;
        }
        if DROP_WITH_VALUE.contains(&a) {
            i += 2;
            continue;
        }
        if DROP_FLAGS.contains(&a) || a.starts_with("--resume=") || a.starts_with("--session-id=") {
            i += 1;
            continue;
        }
        if !a.starts_with('-') {
            // Un texto suelto: el mensaje inicial (o el valor de una opción, que ya se copió).
            i += 1;
            continue;
        }
        if let Some((opt, value)) = a.split_once('=') {
            if !nido_value(opt, value) {
                out.push(a.to_owned());
            }
            i += 1;
            continue;
        }
        // Sus valores, si los lleva aparte: todo lo que sigue hasta otra opción si admite varios
        // (como lo lee Claude); si no, uno.
        let many = variadic(a);
        let mut values = Vec::new();
        if many || takes_value(a) {
            while let Some(v) = argv.get(i + 1).filter(|v| !v.starts_with('-')) {
                values.push(v.clone());
                i += 1;
                if !many {
                    break;
                }
            }
        }
        let had = !values.is_empty();
        values.retain(|v| !nido_value(a, v));
        // Si solo llevaba el de nido, fuera también la opción.
        if !had || !values.is_empty() {
            out.push(a.to_owned());
            out.extend(values);
        }
        i += 1;
    }
    out
}

/// Opciones de `claude` que admiten varios valores (`<x...>` en su ayuda): se queda con todo lo
/// que las sigue hasta la siguiente opción.
fn variadic(opt: &str) -> bool {
    [
        "--add-dir",
        "--allowedTools",
        "--allowed-tools",
        "--betas",
        "--channels",
        CHANNELS_OPT,
        "--disallowedTools",
        "--disallowed-tools",
        "--file",
        "--mcp-config",
        "--tools",
    ]
    .contains(&opt)
}

/// Opciones de `claude` que llevan un valor detrás (las que suelen usarse al lanzar una sesión).
fn takes_value(opt: &str) -> bool {
    [
        "--model",
        "--mcp-config",
        "--permission-mode",
        "--settings",
        "--add-dir",
        "--agent",
        "--append-system-prompt",
        "--system-prompt",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--effort",
        "--channels",
        CHANNELS_OPT,
        "--fallback-model",
        "--plugin-dir",
        "--setting-sources",
        "--agents",
        "--betas",
    ]
    .contains(&opt)
}

/// Cómo se le piden cosas a `claude` (en las pruebas, uno falso).
pub trait ClaudeCli {
    /// `claude <args>` con ese `CLAUDE_CONFIG_DIR` (`None`: el de siempre, `~/.claude`). Si
    /// salió bien y lo que escribió (salida y errores).
    fn run(&self, config_dir: Option<&Path>, args: &[&str]) -> std::io::Result<(bool, String)>;
}

/// El programa `claude` de este equipo.
pub struct Program {
    pub path: PathBuf,
    /// Su `HOME` (y en Windows `USERPROFILE`), si no es el de nido.
    pub home: Option<PathBuf>,
    /// Dónde se ejecuta: una carpeta neutra, para que no cuente el `.mcp.json` de la carpeta en
    /// la que se arrancó nido.
    pub cwd: Option<PathBuf>,
}

/// Lo más que se espera a `claude mcp …` (`get` arranca el servidor para ver si conecta).
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Terminado `claude`, lo más que se espera a que se cierre su salida: un nieto que siguiera
/// con ella abierta dejaría esperando a nido.
const OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl ClaudeCli for Program {
    fn run(&self, config_dir: Option<&Path>, args: &[&str]) -> std::io::Result<(bool, String)> {
        use std::io::Read;
        use std::sync::{Arc, Mutex};
        let mut cmd = std::process::Command::new(&self.path);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match config_dir {
            Some(d) => cmd.env("CLAUDE_CONFIG_DIR", d),
            None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
        };
        if let Some(h) = &self.home {
            cmd.env("HOME", h);
            if cfg!(windows) {
                cmd.env("USERPROFILE", h);
            }
        }
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn()?;
        // Cada salida, a medida que llega: si no se cierra, se queda con lo que haya.
        let read = |r: Option<Box<dyn Read + Send>>| {
            let text = Arc::new(Mutex::new(Vec::new()));
            let into = text.clone();
            let done = std::thread::spawn(move || {
                let Some(mut r) = r else { return };
                let mut buf = [0u8; 4096];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    into.lock().expect("salida").extend_from_slice(&buf[..n]);
                }
            });
            (text, done)
        };
        let out = read(child.stdout.take().map(|r| Box::new(r) as _));
        let err = read(child.stderr.take().map(|r| Box::new(r) as _));
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if start.elapsed() > CLI_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::other("claude no ha contestado"));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let grace = std::time::Instant::now();
        while !(out.1.is_finished() && err.1.is_finished()) && grace.elapsed() < OUTPUT_GRACE {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut text = String::from_utf8_lossy(&out.0.lock().expect("salida")).into_owned();
        text.push_str(&String::from_utf8_lossy(&err.0.lock().expect("salida")));
        Ok((status.success(), text))
    }
}

/// Las extensiones con las que se busca `claude` en Windows: `.exe` primero y luego las de
/// `PATHEXT` (`.cmd` de npm…; `Command` las ejecuta a través de `cmd`).
pub fn windows_exts(pathext: Option<&str>) -> Vec<String> {
    let mut out = vec![".exe".to_owned()];
    for e in pathext.unwrap_or(".COM;.EXE;.BAT;.CMD").split(';') {
        let e = e.trim().to_ascii_lowercase();
        if !e.is_empty() && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Dónde está `claude`: en el `PATH` (`path`) o donde lo deja su instalador (`~/.local/bin`).
pub fn find_program(path: Option<&std::ffi::OsStr>, home: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        let exts = windows_exts(std::env::var("PATHEXT").ok().as_deref());
        let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
        find_program_in(path, home, &exts)
    } else {
        find_program_in(path, home, &[""])
    }
}

/// `claude` con cada extensión de `exts`, por orden (todas las carpetas con la primera, luego
/// con la segunda…).
pub fn find_program_in(
    path: Option<&std::ffi::OsStr>,
    home: &Path,
    exts: &[&str],
) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .chain([home.join(".local").join("bin")])
        .collect();
    exts.iter().find_map(|ext| {
        dirs.iter()
            .map(|d| d.join(format!("claude{ext}")))
            .find(|p| p.is_file())
    })
}

/// Los `CLAUDE_CONFIG_DIR` con los que corren las sesiones: el de siempre (`None`) y cada perfil
/// distinto (lo mismo que pone `resume_argv`).
pub fn config_dirs<'a>(
    links: impl Iterator<Item = &'a ClaudeLink>,
    home: &Path,
) -> Vec<Option<PathBuf>> {
    let mut out = vec![None];
    for link in links {
        for (_, dir) in resume_argv(link).1 {
            let dir = Some(PathBuf::from(dir)).filter(|d| *d != default_config_dir(home));
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out
}

/// Lo que tiene registrado Claude como `nido`, según `claude mcp get nido`.
#[derive(Debug, PartialEq, Eq)]
pub struct Registered<'a> {
    /// «User config (…)», «Local config (…)», «Project config (…)».
    pub scope: &'a str,
    pub command: &'a str,
    pub args: &'a str,
}

impl Registered<'_> {
    fn user_scope(&self) -> bool {
        // Sin la fila (otra versión de Claude), como si fuera de usuario.
        self.scope.is_empty() || self.scope.starts_with("User")
    }
}

pub fn registered_command(get: &str) -> Option<Registered<'_>> {
    let field = |name: &str| {
        get.lines()
            .find_map(|l| l.trim().strip_prefix(name))
            .map(str::trim)
    };
    Some(Registered {
        scope: field("Scope:").unwrap_or(""),
        command: field("Command:")?,
        args: field("Args:").unwrap_or(""),
    })
}

/// Qué hubo que hacer para que un perfil de Claude tenga el MCP de nido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    Already,
    Added,
    /// Apuntaba a otro `nido` (se movió, o el `.exe` en Windows): quitado y vuelto a poner.
    Replaced,
    /// Hay un `nido` en otro ámbito (de una carpeta): no se toca.
    OtherScope(String),
    Failed(String),
}

fn same_program(a: &str, b: &Path) -> bool {
    let b = b.to_string_lossy();
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// Registra `nido mcp` (con la ruta `nido`) en la configuración de usuario de Claude de cada
/// perfil: `claude mcp add -s user nido -- <nido> mcp`. Así se registra su channel (con
/// `--mcp-config` se conecta, pero el channel no). Si ya está, no se toca; si apunta a otro
/// sitio, se quita y se vuelve a poner.
pub fn ensure_mcp_registered(
    cli: &dyn ClaudeCli,
    nido: &Path,
    config_dirs: &[Option<PathBuf>],
) -> Vec<(Option<PathBuf>, Registration)> {
    let nido_s = nido.to_string_lossy();
    let add = ["mcp", "add", "-s", "user", "nido", "--", &nido_s, "mcp"];
    config_dirs
        .iter()
        .map(|dir| {
            let d = dir.as_deref();
            let done = (|| {
                let (found, text) = cli.run(d, &["mcp", "get", "nido"])?;
                let mut res = Registration::Added;
                if found {
                    let reg = registered_command(&text);
                    if let Some(r) = reg.as_ref().filter(|r| !r.user_scope()) {
                        return Ok(Registration::OtherScope(r.scope.to_owned()));
                    }
                    if reg.is_some_and(|r| same_program(r.command, nido) && r.args == "mcp") {
                        return Ok(Registration::Already);
                    }
                    // Si no se puede quitar (de otro ámbito), el `add` dirá por qué.
                    cli.run(d, &["mcp", "remove", "nido", "-s", "user"])?;
                    res = Registration::Replaced;
                }
                let (ok, text) = cli.run(d, &add)?;
                Ok::<_, std::io::Error>(if ok {
                    res
                } else {
                    Registration::Failed(text.trim().to_owned())
                })
            })();
            let done = done.unwrap_or_else(|e| Registration::Failed(e.to_string()));
            (dir.clone(), done)
        })
        .collect()
}

/// Comillas simples de shell (para escribir la orden dentro de una shell al restaurar).
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@".contains(&b))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// La línea que se escribe en la shell para retomar el Claude que corría dentro, sin leer los
/// secretos (`rules`, de `secrets_rules`).
pub fn resume_line(link: &ClaudeLink, rules: &[String]) -> String {
    let (argv, env) = resume_argv(link);
    let argv = with_secrets_guard(&argv, rules);
    let mut parts: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", sh_quote(v)))
        .collect();
    parts.extend(argv.iter().map(|a| sh_quote(a)));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(root: &Path, pid: u32, ppid: u32, comm: &str, env: &[&str]) {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!("{pid} ({comm}) S {ppid} {pid} {pid} 0"),
        )
        .unwrap();
        std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
        let mut environ = env.join("\0");
        environ.push('\0');
        std::fs::write(d.join("environ"), environ).unwrap();
    }

    #[test]
    fn finds_claude_inside_a_shell_with_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_root = tmp.path().join("proc");
        let home = tmp.path().join("home");
        let profile = tmp.path().join("perfil-2");
        fake_proc(&proc_root, 100, 1, "bash", &["HOME=/h"]);
        fake_proc(&proc_root, 150, 100, "sleep", &[]);
        fake_proc(
            &proc_root,
            200,
            100,
            "claude",
            &[&format!("CLAUDE_CONFIG_DIR={}", profile.display())],
        );
        // Un comm raro, con espacios y paréntesis, no rompe la lectura de los demás.
        fake_proc(&proc_root, 300, 1, "a) (b", &[]);
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::write(
            profile.join("sessions/200.json"),
            r#"{"pid":200,"sessionId":"abc-123","cwd":"/w","name":"proyecto","status":"busy","extra":1}"#,
        )
        .unwrap();

        let procfs = Procfs::at(&proc_root);
        assert_eq!(procfs.tree(100), [100, 150, 200]);
        let found = find_claude(&procfs, 100, &home).expect("debería encontrar el claude");
        assert_eq!(found.pid, 200);
        assert_eq!(found.info.session_id, "abc-123");
        assert_eq!(found.info.status.as_deref(), Some("busy"));
        assert_eq!(found.config_dir, Some(profile));
        assert!(find_claude(&procfs, 150, &home).is_none());
    }

    #[test]
    fn the_claude_above_a_process() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proc");
        fake_proc(&root, 1, 0, "init", &[]);
        fake_proc(&root, 100, 1, "claude", &[]);
        // `nido mcp` directamente, o con algo en medio (`cmd /c` en Windows).
        fake_proc(&root, 110, 100, "nido", &[]);
        fake_proc(&root, 120, 100, "cmd.exe", &[]);
        fake_proc(&root, 121, 120, "nido", &[]);
        // Otro Claude dentro de la sesión (`claude -p` desde su Bash), con su `nido mcp`.
        fake_proc(&root, 130, 100, "bash", &[]);
        fake_proc(&root, 140, 130, "claude.exe", &[]);
        fake_proc(&root, 150, 140, "nido", &[]);
        // Sin Claude por encima.
        fake_proc(&root, 200, 1, "nido", &[]);
        let p = Procfs::at(&root);
        assert_eq!(claude_ancestor(&p, 110), Some(100));
        assert_eq!(claude_ancestor(&p, 121), Some(100));
        assert_eq!(claude_ancestor(&p, 150), Some(140));
        assert_eq!(claude_ancestor(&p, 200), None);
        assert_eq!(claude_ancestor(&p, 999), None);
        // Uno mismo no cuenta: se busca por encima.
        assert_eq!(claude_ancestor(&p, 100), None);
    }

    #[test]
    fn the_claude_of_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proc");
        let home = tmp.path().join("home");
        // Con lo que apunta de sí mismo (`sessions/<pid>.json`), el de `find_claude`, aunque
        // haya otro antes en el árbol.
        fake_proc(&root, 10, 1, "nido", &[]);
        fake_proc(&root, 11, 10, "bash", &[]);
        fake_proc(&root, 12, 11, "claude", &[]);
        fake_proc(&root, 13, 10, "claude", &[]);
        std::fs::create_dir_all(home.join(".claude/sessions")).unwrap();
        std::fs::write(
            home.join(".claude/sessions/13.json"),
            r#"{"sessionId":"c"}"#,
        )
        .unwrap();
        // Sin él (otra versión de Claude, o en Windows con su perfil propio): el primer Claude
        // del árbol.
        fake_proc(&root, 20, 1, "nido", &[]);
        fake_proc(&root, 21, 20, "bash", &[]);
        fake_proc(&root, 22, 21, "claude", &[]);
        fake_proc(&root, 23, 22, "claude", &[]);
        // Instalado con npm: el proceso es `node`. No hay Claude que encontrar.
        fake_proc(&root, 30, 1, "nido", &[]);
        fake_proc(&root, 31, 30, "node", &[]);
        let p = Procfs::at(&root);
        assert_eq!(session_claude(&p, 10, &home), Some(13));
        assert_eq!(session_claude(&p, 20, &home), Some(22));
        assert_eq!(session_claude(&p, 30, &home), None);
        assert_eq!(session_claude(&p, 99, &home), None);
    }

    #[test]
    fn keeps_own_claude_options_when_resuming() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            extra_args(&v(&[
                "claude",
                "--model",
                "haiku",
                "--mcp-config",
                "/x/cfg.json",
                "--strict-mcp-config"
            ])),
            v(&[
                "--model",
                "haiku",
                "--mcp-config",
                "/x/cfg.json",
                "--strict-mcp-config"
            ])
        );
        // Lo que elige la conversación lo pone nido; el mensaje inicial no se repite.
        assert_eq!(
            extra_args(&v(&["claude", "--resume", "abc", "--model=opus", "hola"])),
            v(&["--model=opus"])
        );
        assert_eq!(
            extra_args(&v(&["claude", "--session-id", "u", "-n", "api"])),
            Vec::<String>::new()
        );
        assert_eq!(extra_args(&v(&["npm", "run", "dev"])), Vec::<String>::new());
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Las de nido, como van en la orden: el channel y las herramientas que no piden permiso
    /// (las que miran y hablan, y las que mandan cosas al usuario; `open`, `answer`, `close`,
    /// `start` y `stop` no).
    const FLAGS: [&str; 14] = [
        "--dangerously-load-development-channels",
        "server:nido",
        "--allowedTools",
        "mcp__nido__sessions",
        "mcp__nido__read",
        "mcp__nido__send",
        "mcp__nido__alert",
        "mcp__nido__secret",
        "mcp__nido__clipboard",
        "mcp__nido__send_file",
        "mcp__nido__plan_set",
        "mcp__nido__plan_task",
        "mcp__nido__plan_get",
        "mcp__nido__plan_clear",
    ];

    fn flagged(program: &str, rest: &[&str]) -> Vec<String> {
        let mut out = vec![program.to_owned()];
        out.extend(FLAGS.iter().map(|s| s.to_string()));
        out.extend(rest.iter().map(|s| s.to_string()));
        out
    }

    #[test]
    fn claude_gets_the_nido_mcp_flags_right_after_the_program() {
        // Nueva: antes de `--session-id`, para que tras las herramientas venga una opción.
        assert_eq!(
            with_nido_mcp(&new_argv("p", "u-1"), true),
            flagged("claude", &["--session-id", "u-1", "-n", "p"])
        );
        // Retomada.
        let link = ClaudeLink {
            session_id: "u-1".into(),
            config_dir: None,
            account: None,
            inside_shell: false,
        };
        assert_eq!(
            with_nido_mcp(&resume_argv(&link).0, true),
            flagged("claude", &["--resume", "u-1"])
        );
        // Solo el programa (con ruta o `.exe`).
        assert_eq!(with_nido_mcp(&v(&["claude"]), true), flagged("claude", &[]));
        assert_eq!(
            with_nido_mcp(&v(&["/usr/bin/claude", "-c"]), true),
            flagged("/usr/bin/claude", &["-c"])
        );
        assert_eq!(
            with_nido_mcp(&v(&[r"C:\bin\Claude.exe"]), true),
            flagged(r"C:\bin\Claude.exe", &[])
        );
        // Sin opciones, el mensaje no puede ir detrás de `--allowedTools`: tras `--`.
        assert_eq!(
            with_nido_mcp(&v(&["claude", "hola", "y adiós"]), true),
            flagged("claude", &["--", "hola", "y adiós"])
        );
        // Un mensaje y luego opciones: delante de la primera opción.
        let mut want = v(&["claude", "hola"]);
        want.extend(FLAGS.iter().map(|s| s.to_string()));
        want.extend(v(&["--model", "haiku"]));
        assert_eq!(
            with_nido_mcp(&v(&["claude", "hola", "--model", "haiku"]), true),
            want
        );
        // Sin repetirlas: ni el channel ni las herramientas que ya estén (también con `=`).
        let once = with_nido_mcp(&new_argv("p", "u-1"), true);
        assert_eq!(with_nido_mcp(&once, true), once);
        let one = v(&["claude", "--allowedTools=mcp__nido__read", "-c"]);
        assert_eq!(
            with_nido_mcp(&one, true),
            v(&[
                "claude",
                FLAGS[0],
                FLAGS[1],
                FLAGS[2],
                "mcp__nido__sessions",
                "mcp__nido__send",
                "mcp__nido__alert",
                "mcp__nido__secret",
                "mcp__nido__clipboard",
                "mcp__nido__send_file",
                "mcp__nido__plan_set",
                "mcp__nido__plan_task",
                "mcp__nido__plan_get",
                "mcp__nido__plan_clear",
                "--allowedTools=mcp__nido__read",
                "-c"
            ])
        );
        let theirs = v(&[
            "claude",
            "--dangerously-load-development-channels",
            "server:otro",
            "server:nido",
            "--allowed-tools",
            "Bash",
            "mcp__nido__sessions",
            "mcp__nido__read",
            "mcp__nido__send",
            "mcp__nido__alert",
            "mcp__nido__secret",
            "mcp__nido__clipboard",
            "mcp__nido__send_file",
            "mcp__nido__plan_set",
            "mcp__nido__plan_task",
            "mcp__nido__plan_get",
            "mcp__nido__plan_clear",
        ]);
        assert_eq!(with_nido_mcp(&theirs, true), theirs);
        // Las de una orden de antes (todas sin permiso), fuera: con `=`, sola o con otras.
        for old in [
            v(&["claude", "--allowedTools=mcp__nido__*", "-c"]),
            v(&["claude", "--allowedTools", "mcp__nido__*", "-c"]),
        ] {
            assert_eq!(
                with_nido_mcp(&old, true),
                flagged("claude", &["-c"]),
                "{old:?}"
            );
        }
        let mixed = v(&["claude", "--allowed-tools", "Bash", "mcp__nido__*", "-c"]);
        assert_eq!(
            with_nido_mcp(&mixed, true),
            flagged("claude", &["--allowed-tools", "Bash", "-c"])
        );
        // Tras `--` es el mensaje: no se toca.
        let said = v(&["claude", "-c", "--", "--allowedTools", "mcp__nido__*"]);
        assert_eq!(
            with_nido_mcp(&said, true),
            flagged("claude", &["-c", "--", "--allowedTools", "mcp__nido__*"])
        );
        // Apagado, u otro programa: tal cual.
        assert_eq!(
            with_nido_mcp(&new_argv("p", "u-1"), false),
            new_argv("p", "u-1")
        );
        assert_eq!(with_nido_mcp(&v(&["npm", "run"]), true), v(&["npm", "run"]));
        assert_eq!(with_nido_mcp(&v(&["claudette"]), true), v(&["claudette"]));
        assert_eq!(with_nido_mcp(&[], true), Vec::<String>::new());
    }

    #[test]
    fn the_secrets_rules_are_absolute_as_claude_writes_them() {
        assert_eq!(
            secrets_rule("Read", "/home/u/.local/state/nido/secrets"),
            "Read(//home/u/.local/state/nido/secrets/**)"
        );
        assert_eq!(
            secrets_rule("Edit", "/r/state/secrets/"),
            "Edit(//r/state/secrets/**)"
        );
        assert_eq!(
            secrets_rule("Read", r"C:\Users\u\.local\state\nido\secrets"),
            "Read(//c/Users/u/.local/state/nido/secrets/**)"
        );
        // Lo que devuelve `canonicalize` en Windows.
        assert_eq!(
            secrets_rule("Read", r"\\?\C:\Users\u\secrets"),
            "Read(//c/Users/u/secrets/**)"
        );
        // Una ruta relativa sale absoluta.
        for r in secrets_rules(Path::new("estado/secrets")) {
            assert!(
                (r.starts_with("Read(//") || r.starts_with("Edit(//"))
                    && r.ends_with("/estado/secrets/**)"),
                "{r}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn secrets_rules_cover_the_real_path_and_editing() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("state")).unwrap();
        let link = tmp.path().join("enlace");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let rule = |t: &str, p: &Path| secrets_rule(t, &p.to_string_lossy());
        // La carpeta de secretos aún no existe: la ruta de verdad, por la parte que sí.
        let by_link = link.join("state/secrets");
        let by_real = std::fs::canonicalize(&real).unwrap().join("state/secrets");
        assert_eq!(
            secrets_rules(&by_link),
            [
                rule("Read", &by_link),
                rule("Edit", &by_link),
                rule("Read", &by_real),
                rule("Edit", &by_real),
            ]
        );
        // Sin enlaces por medio, dos.
        assert_eq!(
            secrets_rules(&by_real),
            [rule("Read", &by_real), rule("Edit", &by_real)]
        );
    }

    #[test]
    fn claude_gets_the_secrets_rules_once_and_in_front() {
        let rules = vec![
            "Read(//r/state/secrets/**)".to_owned(),
            "Edit(//r/state/secrets/**)".to_owned(),
        ];
        let (r0, r1) = (rules[0].as_str(), rules[1].as_str());
        let launched = with_secrets_guard(&with_nido_mcp(&new_argv("p", "u-1"), true), &rules);
        assert_eq!(&launched[..4], ["claude", "--disallowedTools", r0, r1]);
        assert_eq!(launched[4], "--dangerously-load-development-channels");
        assert_eq!(with_secrets_guard(&launched, &rules), launched);
        // Si ya lleva una (también con `=`), solo la otra.
        let one = format!("--disallowedTools={r0}");
        assert_eq!(
            with_secrets_guard(&v(&["claude", &one, "-c"]), &rules),
            v(&["claude", "--disallowedTools", r1, &one, "-c"])
        );
        // Un mensaje sin opciones: delante y con `--`.
        assert_eq!(
            with_secrets_guard(&v(&["claude", "hola"]), &rules),
            v(&["claude", "--disallowedTools", r0, r1, "--", "hola"])
        );
        assert_eq!(with_secrets_guard(&v(&["npm"]), &rules), v(&["npm"]));
        // Las reglas que lleve la orden se quedan al retomar, aunque se parezcan a las de nido:
        // pueden ser del usuario. Nido no las repite.
        let own = v(&["claude", "--disallowedTools", "Read(//home/u/secrets/**)"]);
        assert_eq!(extra_args(&own), own[1..]);
        let mut saved = launched.clone();
        saved.extend(v(&["--model", "haiku"]));
        let kept = extra_args(&saved);
        assert_eq!(kept, v(&["--disallowedTools", r0, r1, "--model", "haiku"]));
        let mut again = v(&["claude", "--resume", "u-1"]);
        again.extend(kept);
        let again = with_secrets_guard(&again, &rules);
        assert_eq!(again.iter().filter(|a| *a == r0).count(), 1, "{again:?}");
    }

    #[test]
    fn claude_versions() {
        assert_eq!(parse_version("2.1.322 (Claude Code)\n"), Some((2, 1, 322)));
        assert_eq!(parse_version("2.1.80"), Some((2, 1, 80)));
        for bad in ["", "Claude Code", "2.1", "2.1.x (Claude Code)", "2.1.3.4"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!((2, 1, 80) >= CHANNELS_SINCE && (2, 1, 79) < CHANNELS_SINCE);
        assert!((3, 0, 0) >= CHANNELS_SINCE && (2, 0, 99) < CHANNELS_SINCE);
    }

    #[cfg(unix)]
    #[test]
    fn channels_only_with_a_claude_that_knows_them() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let fake = |name: &str, out: &str| {
            let p = tmp.path().join(name);
            let runs = tmp.path().join(format!("{name}.runs"));
            let script = format!(
                "#!/bin/sh\necho \"{out}\"\necho x >> '{}'\n",
                runs.display()
            );
            std::fs::write(&p, script).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let runs = |name: &str| {
            std::fs::read_to_string(tmp.path().join(format!("{name}.runs")))
                .map_or(0, |s| s.lines().count())
        };
        let new = fake("nuevo", "2.1.322 (Claude Code)");
        let old = fake("viejo", "2.0.14 (Claude Code)");
        let odd = fake("raro", "no sé");
        assert_eq!(channels_supported(&new), Some(true));
        assert_eq!(channels_supported(&old), Some(false));
        assert_eq!(channels_supported(&odd), None, "sin versión: no se sabe");
        assert_eq!(channels_supported(&tmp.path().join("no-existe")), None);
        // Una vez por programa (también por un enlace a él).
        let link = tmp.path().join("enlace");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert_eq!(channels_supported(&old), Some(false));
        assert_eq!(channels_supported(&link), Some(false));
        assert_eq!(runs("viejo"), 1);
        // Actualizado (otra fecha): se pregunta otra vez.
        let new_old = fake("viejo", "2.1.90 (Claude Code)");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&new_old)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(channels_supported(&old), Some(true));
        assert_eq!(runs("viejo"), 2);
        // Si no se pudo lanzar, no se recuerda: cuando ya se puede, se pregunta (con la misma
        // fecha: chmod no la cambia).
        let locked = fake("cerrado", "2.1.322 (Claude Code)");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(channels_supported(&locked), None);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(channels_supported(&locked), Some(true));
    }

    #[test]
    fn nido_mcp_flags_are_not_own_options() {
        // Se ponen en cada arranque: al retomar no se guardan como opciones propias.
        assert_eq!(
            extra_args(&with_nido_mcp(&new_argv("p", "u-1"), true)),
            Vec::<String>::new()
        );
        assert_eq!(
            extra_args(&with_nido_mcp(
                &v(&["claude", "--model", "haiku", "--allowedTools", "Bash"]),
                true
            )),
            v(&["--model", "haiku", "--allowedTools", "Bash"])
        );
        assert_eq!(
            extra_args(&v(&[
                "claude",
                "--dangerously-load-development-channels=server:nido",
                "--allowedTools=mcp__nido__*",
                "--model=opus"
            ])),
            v(&["--model=opus"])
        );
        // Lo que va tras `--` es el mensaje inicial: ni `--` ni lo de detrás.
        assert_eq!(
            extra_args(&with_nido_mcp(&v(&["claude", "hola"]), true)),
            Vec::<String>::new()
        );
        assert_eq!(
            extra_args(&v(&["claude", "--model", "x", "--", "-no-es-opción"])),
            v(&["--model", "x"])
        );
        // Las opciones de varios valores se quedan con todos; de las de nido, solo sale su valor
        // (y la opción, si se queda sin ninguno).
        assert_eq!(
            extra_args(&v(&[
                "claude",
                "--allowedTools",
                "mcp__nido__*",
                "Bash",
                "--add-dir",
                "/a",
                "/b",
                "--dangerously-load-development-channels",
                "server:otro",
                "server:nido",
                "--channels",
                "plugin:x",
                "--resume",
                "u"
            ])),
            v(&[
                "--allowedTools",
                "Bash",
                "--add-dir",
                "/a",
                "/b",
                "--dangerously-load-development-channels",
                "server:otro",
                "--channels",
                "plugin:x"
            ])
        );
        assert_eq!(
            extra_args(&v(&["claude", "--mcp-config", "a.json", "b.json", "-c"])),
            v(&["--mcp-config", "a.json", "b.json"])
        );
        // La de una sola: su valor, y lo siguiente es el mensaje.
        assert_eq!(
            extra_args(&v(&["claude", "--model", "haiku", "hola"])),
            v(&["--model", "haiku"])
        );
        // También en Windows (`claude.exe`, `claude.cmd`).
        for program in [r"C:\bin\claude.exe", "claude.cmd"] {
            assert!(is_claude(&v(&[program])), "{program}");
            assert_eq!(
                extra_args(&v(&[program, "--model", "haiku"])),
                v(&["--model", "haiku"])
            );
        }
    }

    /// Un `claude` de pruebas: contesta a `mcp get` lo que se le diga y apunta lo que le piden.
    struct FakeCli {
        get: std::collections::HashMap<Option<PathBuf>, (bool, String)>,
        fail_add: bool,
        calls: std::sync::Mutex<Vec<(Option<PathBuf>, Vec<String>)>>,
    }

    impl ClaudeCli for FakeCli {
        fn run(&self, config_dir: Option<&Path>, args: &[&str]) -> std::io::Result<(bool, String)> {
            let dir = config_dir.map(Path::to_owned);
            self.calls
                .lock()
                .unwrap()
                .push((dir.clone(), args.iter().map(|s| s.to_string()).collect()));
            Ok(match args.get(1) {
                Some(&"get") => self
                    .get
                    .get(&dir)
                    .cloned()
                    .unwrap_or((false, String::new())),
                Some(&"add") => (!self.fail_add, "no se pudo".into()),
                _ => (true, String::new()),
            })
        }
    }

    fn get_output(command: &str) -> String {
        get_output_in("User config (available in all your projects)", command)
    }

    fn get_output_in(scope: &str, command: &str) -> String {
        format!(
            "nido:\n  Scope: {scope}\n  Status: ✓ Connected\n  \
             Type: stdio\n  Command: {command}\n  Args: mcp\n  Environment:\n\n\
             To remove this server, run: claude mcp remove nido -s user\n"
        )
    }

    #[test]
    fn registers_the_nido_mcp_where_it_is_missing_or_moved() {
        let nido = Path::new("/opt/con espacio/nido");
        let moved = PathBuf::from("/perfil-movido");
        let fine = PathBuf::from("/perfil-bien");
        let local = PathBuf::from("/perfil-local");
        let local_scope = "Local config (private to you in this project)";
        let cli = FakeCli {
            get: [
                (None, (false, "No MCP server named \"nido\".".into())),
                (Some(moved.clone()), (true, get_output("/viejo/nido"))),
                (
                    Some(fine.clone()),
                    (true, get_output("/opt/con espacio/nido")),
                ),
                (
                    Some(local.clone()),
                    (true, get_output_in(local_scope, "/otro/nido")),
                ),
            ]
            .into_iter()
            .collect(),
            fail_add: false,
            calls: Default::default(),
        };
        let dirs = [
            None,
            Some(moved.clone()),
            Some(fine.clone()),
            Some(local.clone()),
        ];
        let got = ensure_mcp_registered(&cli, nido, &dirs);
        assert_eq!(
            got,
            [
                (None, Registration::Added),
                (Some(moved.clone()), Registration::Replaced),
                (Some(fine.clone()), Registration::Already),
                // De otro ámbito: no se toca (cada arranque lo quitaría y pondría otra vez).
                (
                    Some(local.clone()),
                    Registration::OtherScope(local_scope.into())
                ),
            ]
        );
        let add = v(&[
            "mcp",
            "add",
            "-s",
            "user",
            "nido",
            "--",
            "/opt/con espacio/nido",
            "mcp",
        ]);
        let get = v(&["mcp", "get", "nido"]);
        assert_eq!(
            *cli.calls.lock().unwrap(),
            [
                (None, get.clone()),
                (None, add.clone()),
                (Some(moved.clone()), get.clone()),
                (
                    Some(moved.clone()),
                    v(&["mcp", "remove", "nido", "-s", "user"])
                ),
                (Some(moved), add),
                (Some(fine), get.clone()),
                (Some(local), get),
            ]
        );
        // Si no se puede añadir, se dice (con lo que contestó claude).
        let cli = FakeCli {
            get: Default::default(),
            fail_add: true,
            calls: Default::default(),
        };
        let got = ensure_mcp_registered(&cli, nido, &[None]);
        assert!(
            matches!(&got[..], [(None, Registration::Failed(e))] if e.contains("no se pudo")),
            "{got:?}"
        );
    }

    #[test]
    fn mcp_get_output_gives_the_command() {
        assert_eq!(
            registered_command(&get_output("/a b/nido")),
            Some(Registered {
                scope: "User config (available in all your projects)",
                command: "/a b/nido",
                args: "mcp"
            })
        );
        assert_eq!(registered_command("No MCP server named \"nido\"."), None);
    }

    #[test]
    fn config_dirs_to_register() {
        let home = Path::new("/h");
        let link = |dir: Option<&str>, account: Option<&str>| ClaudeLink {
            session_id: "s".into(),
            config_dir: dir.map(PathBuf::from),
            account: account.map(str::to_owned),
            inside_shell: false,
        };
        let links = [
            link(None, None),
            link(Some("/perfil-2"), None),
            link(Some("/perfil-2"), None),
            // Con cuenta propia corre con el de siempre.
            link(Some("/perfil-3"), Some("a@b.c")),
            link(Some("/h/.claude"), None),
        ];
        assert_eq!(
            config_dirs(links.iter(), home),
            [None, Some(PathBuf::from("/perfil-2"))]
        );
    }

    #[test]
    fn finds_the_claude_program_with_its_windows_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let home = tmp.path().join("home");
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path = std::env::join_paths([&a, &b]).unwrap();
        let exts = [".exe", ".cmd"];
        std::fs::write(a.join("claude.cmd"), "").unwrap();
        assert_eq!(
            find_program_in(Some(&path), &home, &exts),
            Some(a.join("claude.cmd"))
        );
        // `.exe` antes, aunque esté más adelante en el PATH.
        std::fs::write(b.join("claude.exe"), "").unwrap();
        assert_eq!(
            find_program_in(Some(&path), &home, &exts),
            Some(b.join("claude.exe"))
        );
        assert_eq!(
            windows_exts(Some(".COM;.EXE;.BAT;.CMD")),
            [".exe", ".com", ".bat", ".cmd"]
        );
        assert_eq!(windows_exts(None), [".exe", ".com", ".bat", ".cmd"]);
        assert_eq!(windows_exts(Some(".CMD")), [".exe", ".cmd"]);
    }

    #[test]
    fn finds_the_claude_program() {
        let tmp = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let home = tmp.path().join("home");
        for d in [&a, &b, &home.join(".local/bin")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(find_program(Some(&path), &home), None);
        std::fs::write(home.join(".local/bin").join(name), "").unwrap();
        assert_eq!(
            find_program(Some(&path), &home),
            Some(home.join(".local/bin").join(name))
        );
        std::fs::write(b.join(name), "").unwrap();
        assert_eq!(find_program(Some(&path), &home), Some(b.join(name)));
        assert_eq!(
            find_program(None, &home),
            Some(home.join(".local/bin").join(name))
        );
    }

    /// Con el `claude` de verdad no: un guion que apunta sus argumentos y su `CLAUDE_CONFIG_DIR`,
    /// con `HOME` en un temporal.
    #[cfg(unix)]
    #[test]
    fn runs_the_claude_program_with_its_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("log");
        let script = tmp.path().join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"HOME=$HOME CCD=${{CLAUDE_CONFIG_DIR:-no}} PWD=$(pwd) $*\" >> '{}'\n\
                 [ \"$2\" = get ] && {{ echo 'No MCP server named \"nido\".'; exit 1; }}\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let home = tmp.path().join("home");
        // En una carpeta neutra: un `.mcp.json` en la de `nido serve` no cuenta.
        let neutral = std::fs::canonicalize(tmp.path()).unwrap().join("estado");
        std::fs::create_dir_all(&neutral).unwrap();
        let cli = Program {
            path: script,
            home: Some(home.clone()),
            cwd: Some(neutral.clone()),
        };
        let profile = tmp.path().join("perfil");
        let got = ensure_mcp_registered(&cli, Path::new("/x/nido"), &[None, Some(profile.clone())]);
        assert_eq!(
            got,
            [
                (None, Registration::Added),
                (Some(profile.clone()), Registration::Added)
            ]
        );
        let h = home.display();
        let p = profile.display();
        let w = neutral.display();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!(
                "HOME={h} CCD=no PWD={w} mcp get nido\nHOME={h} CCD=no PWD={w} mcp add -s user nido -- /x/nido mcp\n\
                 HOME={h} CCD={p} PWD={w} mcp get nido\nHOME={h} CCD={p} PWD={w} mcp add -s user nido -- /x/nido mcp\n"
            )
        );
        // Si no se puede ejecutar, un fallo (no un pánico).
        let gone = Program {
            path: tmp.path().join("no-existe"),
            home: None,
            cwd: None,
        };
        assert!(matches!(
            &ensure_mcp_registered(&gone, Path::new("/x/nido"), &[None])[..],
            [(None, Registration::Failed(_))]
        ));
    }

    /// Un nieto que se queda con la salida abierta no deja esperando a nido.
    #[cfg(unix)]
    #[test]
    fn a_grandchild_holding_the_output_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        let pidfile = tmp.path().join("nieto");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\necho hecho\nexit 0\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let cli = Program {
            path: script,
            home: None,
            cwd: None,
        };
        let start = std::time::Instant::now();
        let got = cli.run(None, &["mcp", "get", "nido"]);
        let took = start.elapsed();
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|p| p.trim().parse::<i32>().ok())
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
        let (ok, text) = got.unwrap();
        assert!(ok);
        assert!(text.contains("hecho"), "{text:?}");
    }

    #[test]
    fn launch_and_resume_commands() {
        assert_eq!(
            new_argv("proyecto", "u-1"),
            ["claude", "--session-id", "u-1", "-n", "proyecto"]
        );
        let link = ClaudeLink {
            session_id: "u-1".into(),
            config_dir: Some("/perfil con espacio".into()),
            account: None,
            inside_shell: true,
        };
        let (argv, env) = resume_argv(&link);
        assert_eq!(argv, ["claude", "--resume", "u-1"]);
        assert_eq!(
            env,
            [(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/perfil con espacio".to_string()
            )]
        );
        assert_eq!(
            resume_line(&link, &[]),
            "CLAUDE_CONFIG_DIR='/perfil con espacio' claude --resume u-1"
        );
        let with_acct = ClaudeLink {
            account: Some("a@b.c".into()),
            ..link
        };
        assert_eq!(resume_argv(&with_acct).1, []);
        assert_eq!(resume_line(&with_acct, &[]), "claude --resume u-1");
        // Sin leer los secretos.
        assert_eq!(
            resume_line(&with_acct, &["Read(//r/secrets/**)".to_owned()]),
            "claude --disallowedTools 'Read(//r/secrets/**)' --resume u-1"
        );
    }
}
