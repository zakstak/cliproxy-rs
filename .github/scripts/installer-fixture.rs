// Local release fixture: no provider clients, auth files or network access. It reports
// a version, then hangs and ignores SIGTERM, so the installer must fall back to SIGKILL
// and roll back to the previous binary.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("cliproxy 999.2.0");
        return;
    }
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
