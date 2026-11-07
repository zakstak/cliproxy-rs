//! Private files and directories: Go's `0o700` directories and `0o600` files.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::path::Path;

/// Go `os.MkdirAll(path, 0o700)`.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Go `os.OpenFile(path, O_WRONLY|O_CREATE|O_TRUNC, 0o600)`.
pub(crate) fn create_truncate(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Go `os.Chmod(path, 0o600)`.
pub(crate) fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
