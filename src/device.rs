use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::Duration;
use std::thread;

const VID: &str = "264A";
const PID: &str = "233C";
const CTRL_SIZE: usize = 440;
const IMG_PKT_SIZE: usize = 1024;
const IMG_DATA_SIZE: usize = 1020;

pub struct Device {
    ctrl: File,
    image: File,
}

impl Device {
    pub fn open() -> io::Result<Self> {
        let mut ctrl_path: Option<String> = None;
        let mut image_path: Option<String> = None;

        for entry in fs::read_dir("/sys/class/hidraw")? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            let uevent = fs::read_to_string(
                format!("/sys/class/hidraw/{}/device/uevent", name)
            ).unwrap_or_default().to_uppercase();

            if !uevent.contains(VID) || !uevent.contains(PID) {
                continue;
            }

            // Read sysfs symlink to determine interface (1.0 = ctrl, 1.1 = image)
            let link = fs::read_link(format!("/sys/class/hidraw/{}", name))
                .unwrap_or_default();
            let link_str = link.to_string_lossy();

            let dev = format!("/dev/{}", name);
            if link_str.contains(":1.0") {
                ctrl_path = Some(dev);
            } else if link_str.contains(":1.1") {
                image_path = Some(dev);
            }
        }

        let ctrl = OpenOptions::new()
            .read(true)
            .write(true)
            .open(ctrl_path.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound,
                    "TH420 control interface not found (VID 264A PID 233C)")
            })?)?;

        let image = OpenOptions::new()
            .read(true)
            .write(true)
            .open(image_path.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound,
                    "TH420 image interface not found")
            })?)?;

        Ok(Self { ctrl, image })
    }

    pub fn init(&mut self) -> io::Result<()> {
        let sequence: &[&[u8]] = &[
            &[0x85, 0x01, 0x00, 0x80],
            &[0x87, 0x01, 0x00, 0x80],
            &[0x85, 0x01, 0x00, 0x80],
            &[0x87, 0x01, 0x00, 0x80],
            &[0x84, 0x01, 0x00, 0x80],
            &[0x81, 0x01, 0x00, 0x80],
        ];

        for cmd in sequence {
            self.ctrl_write(cmd)?;
            self.ctrl_read()?;
            thread::sleep(Duration::from_millis(50));
        }

        Ok(())
    }

    /// Query device for liquid coolant temperature (bytes 6-7 big-endian, millidegrees)
    pub fn read_liquid_temp(&mut self) -> io::Result<f32> {
        self.ctrl_write(&[0x80, 0x01, 0x00, 0x80])?;
        let resp = self.ctrl_read()?;
        let millideg = u16::from_be_bytes([resp[6], resp[7]]) as f32;
        Ok(millideg / 1000.0)
    }

    pub fn send_frame(&mut self, jpeg: &[u8]) -> io::Result<()> {
        self.ctrl_write(&[0x12, 0x01, 0x00, 0x80, 0x64])?;
        // device may or may not ACK the frame-start command
        let _ = self.ctrl_read();

        let chunks: Vec<&[u8]> = jpeg.chunks(IMG_DATA_SIZE).collect();
        let total = chunks.len();

        for (i, chunk) in chunks.iter().enumerate() {
            let mut pkt = [0u8; IMG_PKT_SIZE];
            pkt[0] = 0x08;
            pkt[1] = if i == 0 { total as u8 } else { i as u8 };
            pkt[2] = 0x00;
            pkt[3] = if i == 0 { 0x80 } else { 0x00 };
            pkt[4..4 + chunk.len()].copy_from_slice(chunk);

            self.image.write_all(&pkt)?;
        }

        Ok(())
    }

    fn ctrl_write(&mut self, cmd: &[u8]) -> io::Result<()> {
        let mut pkt = [0u8; CTRL_SIZE];
        pkt[..cmd.len()].copy_from_slice(cmd);
        self.ctrl.write_all(&pkt)
    }

    fn ctrl_read(&mut self) -> io::Result<[u8; CTRL_SIZE]> {
        poll_read(&self.ctrl, 1000)?;
        let mut buf = [0u8; CTRL_SIZE];
        self.ctrl.read_exact(&mut buf)?;
        Ok(buf)
    }
}

fn poll_read(file: &File, timeout_ms: i32) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let ret = unsafe {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pfd as *mut libc::pollfd, 1, timeout_ms)
    };
    match ret {
        0 => Err(io::Error::new(io::ErrorKind::TimedOut, "device did not respond")),
        n if n < 0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}
