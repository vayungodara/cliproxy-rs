// Local release fixtures: no provider clients, auth files or network access.
fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg == "--version" {
        println!("cliproxy 999.2.0");
    } else if arg == "--help" {
        #[cfg(legacy)]
        println!("--config PATH");
        #[cfg(not(legacy))]
        println!("--config PATH --log-file PATH --working-dir DIR");
    } else {
        #[cfg(legacy)]
        std::process::exit(2);
        #[cfg(plain_health)]
        {
            use std::io::{Read, Write};
            let args: Vec<_> = std::env::args().collect();
            let config =
                std::fs::read_to_string(&args[args.iter().position(|arg| arg == "--config").unwrap() + 1]).unwrap();
            let value = |key| {
                config
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(key))
                    .unwrap()
                    .trim()
                    .trim_matches('"')
            };
            let listener =
                std::net::TcpListener::bind((value("host:"), value("port:").parse::<u16>().unwrap())).unwrap();
            // Deliberately speaks HTTP despite a TLS config: a TCP-only probe
            // accepts this broken upgrade; a real HTTPS probe must roll it back.
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let _ = stream.read(&mut [0; 1024]);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
            }
        }
        #[cfg(not(any(legacy, plain_health)))]
        {
            #[cfg(unix)]
            unsafe {
                unsafe extern "C" {
                    fn signal(sig: i32, handler: usize) -> usize;
                }
                // SIG_IGN for SIGTERM: exercise the installer's bounded SIGKILL fallback.
                signal(15, 1);
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }
}
