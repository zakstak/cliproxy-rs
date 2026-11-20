//! Private credential writes and browser launching for device and browser login flows.

use std::path::Path;

/// Writes a credential file atomically with mode 0600 in a 0700 directory.
///
/// Go uses `os.Create` (0666 before umask) and an in-place write; a private temp file and
/// rename is the deliberate, safer choice here (contracts review).
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("credential");
    let temporary = directory.join(format!(
        ".{name}.{}.tmp",
        suffix.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Opens `url` in the desktop browser with `xdg-open`; returns whether it started.
pub(crate) fn open_browser(url: &str) -> bool {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}
