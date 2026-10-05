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
        #[cfg(not(legacy))]
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
