use std::io;
use std::path::PathBuf;

pub fn home() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("DAS_HOME") {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".das"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is not set; set DAS_HOME explicitly",
            )
        })
}
