//! Para las pruebas: un tope de tiempo para una prueba entera. Si se cuelga (también con un
//! cerrojo que nunca se suelta, que ningún `timeout` de tokio corta), se corta el binario de
//! pruebas entero con un mensaje en vez de quedarse esperando para siempre.

use std::sync::mpsc;
use std::time::Duration;

/// Mientras viva, vigila; al acabar la prueba (bien o con un pánico) deja de vigilar.
pub struct Limit {
    _watch: mpsc::Sender<()>,
}

/// Si la prueba de este hilo no ha acabado en `d`, sale del proceso con 124.
pub fn limit(d: Duration) -> Limit {
    let name = std::thread::current()
        .name()
        .unwrap_or("una prueba")
        .to_owned();
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(d) {
            eprintln!("{name}: se ha colgado (más de {d:?}); se corta aquí");
            std::process::exit(124);
        }
    });
    Limit { _watch: tx }
}
