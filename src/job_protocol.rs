use std::io::{self, Read, Write};
pub const READY: &str = "TH420_DEVICE_JOB_READY_V1";

pub fn authorize(mut input: impl Read) -> io::Result<()> {
    let mut command = [0u8; 6];
    input.read_exact(&mut command)?;
    if &command != b"begin\n" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "device write was not authorized",
        ));
    }
    Ok(())
}

pub fn wait_for_gui(enabled: bool) -> io::Result<()> {
    if enabled {
        println!("{READY}");
        io::stdout().flush()?;
        authorize(io::stdin().lock())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_begin_authorizes_writes() {
        assert!(authorize(&b"begin\n"[..]).is_ok());
        for input in [b"".as_slice(), b"begin", b"cancel", b"BEGIN\n"] {
            assert!(authorize(input).is_err());
        }
    }
}
