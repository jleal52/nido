//! `nido secrets run`: el secreto de la sesión (o el recordado del equipo) en una variable de la
//! orden, sin enseñarlo, y la orden sale con su código. Con `NIDO_HOME` en un temporal.

// Con sh: solo en Unix (Windows lo compila el CI).
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use nido::secrets::{Remember, Store};

fn nido(home: &Path, session: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nido"));
    c.args(args)
        .env("NIDO_HOME", home)
        .env_remove("NIDO_SESSION")
        .stdin(Stdio::null());
    if let Some(s) = session {
        c.env("NIDO_SESSION", s);
    }
    c.output().unwrap()
}

fn shown(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn runs_the_command_with_the_secret_in_a_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("nido");
    let store = Store::new(home.join("state/secrets"));
    let now = nido::registry::now_ms();
    store
        .save_session("aaaa1111", "tok", b"de-la-sesion")
        .unwrap();
    store
        .save_team("tok", b"del-equipo", Remember::Always, "api", now)
        .unwrap();
    store
        .save_team("viejo", b"x", Remember::TwoHours, "api", 0)
        .unwrap();
    store
        .save_team("binario", &[0xff, 0xfe, 0x00], Remember::Always, "api", now)
        .unwrap();
    let got = tmp.path().join("got");
    // La orden apunta lo que recibe (sin enseñarlo) y sale con 7.
    let script = format!("printf %s \"$T\" > '{}'; exit 7", got.display());
    let run = |session: Option<&str>, name: &str| {
        nido(
            &home,
            session,
            &[
                "secrets", "run", name, "--env", "T", "--", "sh", "-c", &script,
            ],
        )
    };

    let o = run(Some("aaaa1111"), "tok");
    assert_eq!(o.status.code(), Some(7), "{}", shown(&o));
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "de-la-sesion");
    assert!(!shown(&o).contains("de-la-sesion"), "{}", shown(&o));
    // Otra sesión: el del equipo.
    let o = run(Some("bbbb2222"), "tok");
    assert_eq!(o.status.code(), Some(7), "{}", shown(&o));
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "del-equipo");
    assert!(!shown(&o).contains("del-equipo"), "{}", shown(&o));

    // Los errores, claros y sin el valor.
    let o = run(None, "tok");
    assert_eq!(o.status.code(), Some(1));
    assert!(shown(&o).contains("NIDO_SESSION"), "{}", shown(&o));
    let o = run(Some("aaaa1111"), "nada");
    assert_eq!(o.status.code(), Some(1));
    assert!(
        shown(&o).contains("no hay ningún secreto «nada»"),
        "{}",
        shown(&o)
    );
    let o = run(Some("aaaa1111"), "viejo");
    assert!(
        shown(&o).contains("no hay ningún secreto «viejo»"),
        "{}",
        shown(&o)
    );
    let o = run(Some("aaaa1111"), "a/b");
    assert_eq!(o.status.code(), Some(1));
    assert!(shown(&o).contains("no válido"), "{}", shown(&o));
    let o = run(Some("aaaa1111"), "binario");
    assert_eq!(o.status.code(), Some(1));
    assert!(shown(&o).contains("no es texto"), "{}", shown(&o));
    let o = nido(
        &home,
        Some("aaaa1111"),
        &["secrets", "run", "tok", "--env", "1X", "--", "true"],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(shown(&o).contains("variable"), "{}", shown(&o));
    let o = nido(
        &home,
        Some("aaaa1111"),
        &[
            "secrets",
            "run",
            "tok",
            "--env",
            "T",
            "--",
            "no-existe-esta-orden",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(shown(&o).contains("no-existe-esta-orden"), "{}", shown(&o));
    assert!(!shown(&o).contains("de-la-sesion"));
    // Sin orden, clap dice que falta.
    let o = nido(
        &home,
        Some("aaaa1111"),
        &["secrets", "run", "tok", "--env", "T"],
    );
    assert_ne!(o.status.code(), Some(0));
    // Y no ha creado nada que no hubiera (ni token ni carpetas de nido).
    assert!(!home.join("config").exists(), "{}", shown(&o));
}
