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
const FLASH_CHUNK_SIZE: usize = 10_240;
const FLASH_FIRST_DATA_SIZE: usize = CTRL_SIZE - 16;
const FLASH_DATA_SIZE: usize = CTRL_SIZE - 4;
const BOOT_HEADER_SIZE: usize = 32;
const BOOT_FRAME_ENTRY_SIZE: usize = 16;
const BOOT_RECORD_NAME_SIZE: usize = 8;
const BOOT_TRAILER_SIZE: usize = 16;
const BOOT_FRAME_FLAGS: u32 = 0x4000_0008;
const MIN_BOOT_FRAME_DELAY_MS: u32 = 80;

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

    /// Query device for liquid coolant temperature.
    pub fn read_liquid_temp(&mut self) -> io::Result<f32> {
        self.ctrl_write(&[0x80, 0x01, 0x00, 0x80])?;
        let resp = self.ctrl_read()?;
        parse_liquid_temp(&resp)
    }

    /// Stream JPEG data without writing a control-interface brightness value.
    /// Use this for a continuous stream after brightness has been set once.
    pub fn send_frame_data(&mut self, jpeg: &[u8]) -> io::Result<()> {
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

    /// Set the persistent LCD brightness, from 0 (dark) through 100.
    pub fn set_brightness(&mut self, brightness: u8) -> io::Result<()> {
        if brightness > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "brightness must be between 0 and 100",
            ));
        }
        self.ctrl_write(&[0x12, 0x01, 0x00, 0x80, brightness])
    }

    /// Set the persistent RGB color of the standby pump-temperature overlay.
    pub fn set_pump_temperature_color(&mut self, rgb: [u8; 3]) -> io::Result<()> {
        let command = [
            0x16, 0x01, 0x00, 0x80, rgb[0], rgb[1], rgb[2], 0xff,
        ];
        self.ctrl_write(&command)?;
        // The vendor app repeats this write about 280 ms later. A single write
        // did not visibly update the color in a native Linux test.
        thread::sleep(Duration::from_millis(300));
        self.ctrl_write(&command)
    }

    /// Persist a JPEG as the device's standby image.
    ///
    /// This is the flash-upload protocol used by the vendor application.  It is
    /// intentionally separate from `send_frame`, which only streams a transient
    /// image over the display endpoint.
    pub fn upload_standby(&mut self, jpeg: &[u8]) -> io::Result<()> {
        if jpeg.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "standby JPEG is empty"));
        }
        let total = u32::try_from(jpeg.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "standby JPEG is too large")
        })?;

        // Clear any incomplete upload, then start a kind=0 (standby) transaction.
        self.ctrl_write(&[0x82, 0x01, 0x00, 0x80])?;
        self.ctrl_read()?;
        let mut init = [0u8; 12];
        init[..5].copy_from_slice(&[0x0a, 0x01, 0x00, 0x80, 0x00]);
        init[8..12].copy_from_slice(&total.to_le_bytes());
        self.ctrl_write(&init)?;
        self.ctrl_read()?;

        for (n, chunk) in jpeg.chunks(FLASH_CHUNK_SIZE).enumerate() {
            let offset = n * FLASH_CHUNK_SIZE;
            self.write_flash_chunk(offset, jpeg.len(), chunk)?;
            self.ctrl_read()?; // one ACK for the complete logical chunk
        }

        self.ctrl_write(&[0x82, 0x01, 0x00, 0x80])?;
        self.ctrl_read()?;
        Ok(())
    }

    /// Persist a boot-animation container built by `build_boot_container`.
    pub fn upload_boot(&mut self, container: &[u8], frame_delay_ms: u32) -> io::Result<()> {
        if container.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "boot container is empty"));
        }
        if frame_delay_ms < MIN_BOOT_FRAME_DELAY_MS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "boot frame delay must be at least 80 ms",
            ));
        }
        let total = u32::try_from(container.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
        })?;

        self.ctrl_write(&[0x82, 0x01, 0x00, 0x80])?;
        self.ctrl_read()?;
        let mut init = [0u8; 12];
        init[..5].copy_from_slice(&[0x0a, 0x01, 0x00, 0x80, 0x01]);
        init[8..12].copy_from_slice(&total.to_le_bytes());
        self.ctrl_write(&init)?;
        self.ctrl_read()?;

        for (n, chunk) in container.chunks(FLASH_CHUNK_SIZE).enumerate() {
            let offset = n * FLASH_CHUNK_SIZE;
            self.write_flash_chunk(offset, container.len(), chunk)?;
            self.ctrl_read()?;
        }

        // The vendor application leaves a short gap after the final chunk ACK
        // before its fire-and-forget flash-commit command.
        thread::sleep(Duration::from_millis(80));
        let mut commit = [0u8; 8];
        commit[..4].copy_from_slice(&[0x14, 0x01, 0x00, 0x80]);
        commit[4..8].copy_from_slice(&(frame_delay_ms - 1).to_le_bytes());
        self.ctrl_write(&commit)?;
        thread::sleep(Duration::from_millis(20));

        self.ctrl_write(&[0x82, 0x01, 0x00, 0x80])?;
        self.ctrl_read()?;
        Ok(())
    }

    fn write_flash_chunk(&mut self, offset: usize, total: usize, chunk: &[u8]) -> io::Result<()> {
        let packet_count = (chunk.len() + 16).div_ceil(FLASH_DATA_SIZE);
        let progress = ((offset + chunk.len()) * 100 / total) as u32;
        let mut first = [0u8; CTRL_SIZE];
        first[..4].copy_from_slice(&[0x0b, packet_count as u8, 0x00, 0x80]);
        first[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        first[8..12].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
        first[12..16].copy_from_slice(&progress.to_le_bytes());
        let first_len = chunk.len().min(FLASH_FIRST_DATA_SIZE);
        first[16..16 + first_len].copy_from_slice(&chunk[..first_len]);
        self.ctrl.write_all(&first)?;

        for index in 1..packet_count {
            let start = FLASH_FIRST_DATA_SIZE + (index - 1) * FLASH_DATA_SIZE;
            let end = (start + FLASH_DATA_SIZE).min(chunk.len());
            let mut pkt = [0u8; CTRL_SIZE];
            pkt[..4].copy_from_slice(&[0x0b, index as u8, 0x00, 0x00]);
            if start < end {
                pkt[4..4 + end - start].copy_from_slice(&chunk[start..end]);
            }
            self.ctrl.write_all(&pkt)?;
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

fn parse_liquid_temp(resp: &[u8; CTRL_SIZE]) -> io::Result<f32> {
    // The status response starts `80 01 00 80 temp+0x24 temp+0x25 ...`.
    // The adjacent encoding acts as a small integrity check. Bytes 6-7 are
    // instead pump RPM (e.g. 09 10 = 2320 RPM).
    let encoded = resp[4];
    if encoded < 0x24 || resp[5] != encoded.saturating_add(1) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid liquid-temperature response",
        ));
    }
    Ok((encoded - 0x24) as f32)
}

/// Build the verified `Update_Boot_GIF` container from encoded JPEG frames.
pub fn build_boot_container(frames: &[Vec<u8>]) -> io::Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "boot GIF has no frames"));
    }
    let frame_count = u8::try_from(frames.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot GIF has more than 255 frames")
    })?;
    if frames.iter().any(|frame| frame.is_empty()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "boot GIF has an empty frame"));
    }

    let table_len = frames.len().checked_mul(BOOT_FRAME_ENTRY_SIZE).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot frame table is too large")
    })?;
    let mut record_offset = BOOT_HEADER_SIZE.checked_add(table_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
    })?;
    let mut entries = Vec::with_capacity(frames.len());

    for frame in frames {
        let jpeg_size = u32::try_from(frame.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "boot JPEG is too large")
        })?;
        let offset = u32::try_from(record_offset).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
        })?;
        let record_end = record_offset
            .checked_add(BOOT_RECORD_NAME_SIZE)
            .and_then(|value| value.checked_add(frame.len()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large"))?;
        let record_end_i32 = i32::try_from(record_end).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
        })?;
        entries.push((-record_end_i32, offset, jpeg_size));
        record_offset = record_end;
    }

    let data_end = u32::try_from(record_offset).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
    })?;
    let container_len = record_offset.checked_add(BOOT_TRAILER_SIZE).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
    })?;
    let checksum_input = u64::from(data_end)
        + u64::try_from(table_len).unwrap()
        + BOOT_TRAILER_SIZE as u64
        + u64::from(frame_count);
    let checksum = -i32::try_from(checksum_input).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "boot container is too large")
    })?;

    let mut container = Vec::with_capacity(container_len);
    container.extend_from_slice(&checksum.to_le_bytes());
    container.extend_from_slice(&data_end.to_le_bytes());
    container.extend_from_slice(&(table_len as u32).to_le_bytes());
    container.extend_from_slice(&(BOOT_TRAILER_SIZE as u16).to_le_bytes());
    container.push(frame_count);
    container.push(b'P');
    container.extend_from_slice(b"Update_Boot_GIF\0");

    for (end, offset, jpeg_size) in &entries {
        container.extend_from_slice(&end.to_le_bytes());
        container.extend_from_slice(&offset.to_le_bytes());
        container.extend_from_slice(&jpeg_size.to_le_bytes());
        container.extend_from_slice(&BOOT_FRAME_FLAGS.to_le_bytes());
    }
    for (index, frame) in frames.iter().enumerate() {
        let name = format!("{index:03}.jpg\0");
        debug_assert_eq!(name.len(), BOOT_RECORD_NAME_SIZE);
        container.extend_from_slice(name.as_bytes());
        container.extend_from_slice(frame);
    }
    container.extend_from_slice(&[0; BOOT_TRAILER_SIZE - 1]);
    container.push(0x10);
    debug_assert_eq!(container.len(), container_len);
    Ok(container)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    fn temp_file(name: &str) -> (std::path::PathBuf, File) {
        let path = std::env::temp_dir().join(format!(
            "th420-device-{name}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        (path, file)
    }

    #[test]
    fn coolant_temperature_uses_encoded_status_field() {
        let mut response = [0u8; CTRL_SIZE];
        response[..8].copy_from_slice(&[0x80, 0x01, 0x00, 0x80, 0x3e, 0x3f, 0x09, 0x10]);

        assert_eq!(parse_liquid_temp(&response).unwrap(), 26.0);
        response[5] = 0;
        assert!(parse_liquid_temp(&response).is_err());
    }

    #[test]
    fn boot_container_has_verified_offsets_and_checksum() {
        let container = build_boot_container(&[vec![1, 2, 3, 4], vec![5, 6, 7]]).unwrap();

        assert_eq!(container.len(), 103);
        assert_eq!(i32::from_le_bytes(container[0..4].try_into().unwrap()), -137);
        assert_eq!(u32::from_le_bytes(container[4..8].try_into().unwrap()), 87);
        assert_eq!(u32::from_le_bytes(container[8..12].try_into().unwrap()), 32);
        assert_eq!(&container[16..32], b"Update_Boot_GIF\0");
        assert_eq!(i32::from_le_bytes(container[32..36].try_into().unwrap()), -76);
        assert_eq!(u32::from_le_bytes(container[36..40].try_into().unwrap()), 64);
        assert_eq!(u32::from_le_bytes(container[40..44].try_into().unwrap()), 4);
        assert_eq!(i32::from_le_bytes(container[48..52].try_into().unwrap()), -87);
        assert_eq!(u32::from_le_bytes(container[52..56].try_into().unwrap()), 76);
        assert_eq!(&container[64..76], b"000.jpg\0\x01\x02\x03\x04");
        assert_eq!(&container[76..87], b"001.jpg\0\x05\x06\x07");
        assert_eq!(&container[87..102], &[0; 15]);
        assert_eq!(container[102], 0x10);
    }

    #[test]
    fn boot_container_rejects_empty_frames_and_too_many_frames() {
        assert!(build_boot_container(&[]).is_err());
        assert!(build_boot_container(&[vec![]]).is_err());
        assert!(build_boot_container(&vec![vec![1]; 256]).is_err());
    }

    #[test]
    fn frame_data_is_split_into_protocol_packets() {
        let (ctrl_path, ctrl) = temp_file("ctrl");
        let (image_path, image) = temp_file("image");
        let mut device = Device { ctrl, image };
        let jpeg = vec![0x5a; IMG_DATA_SIZE + 3];
        device.send_frame_data(&jpeg).unwrap();

        let mut image = File::open(&image_path).unwrap();
        let mut bytes = Vec::new();
        image.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), IMG_PKT_SIZE * 2);
        assert_eq!(&bytes[..4], &[0x08, 2, 0, 0x80]);
        assert_eq!(&bytes[4..4 + IMG_DATA_SIZE], &jpeg[..IMG_DATA_SIZE]);
        assert_eq!(&bytes[IMG_PKT_SIZE..IMG_PKT_SIZE + 4], &[0x08, 1, 0, 0]);
        assert_eq!(&bytes[IMG_PKT_SIZE + 4..IMG_PKT_SIZE + 7], &jpeg[IMG_DATA_SIZE..]);

        drop(device);
        fs::remove_file(ctrl_path).unwrap();
        fs::remove_file(image_path).unwrap();
    }

    #[test]
    fn brightness_validates_range_and_writes_padded_command() {
        let (ctrl_path, ctrl) = temp_file("brightness-ctrl");
        let (image_path, image) = temp_file("brightness-image");
        let mut device = Device { ctrl, image };
        assert!(device.set_brightness(101).is_err());
        device.set_brightness(42).unwrap();

        device.ctrl.seek(SeekFrom::Start(0)).unwrap();
        let mut command = [0; CTRL_SIZE];
        device.ctrl.read_exact(&mut command).unwrap();
        assert_eq!(&command[..5], &[0x12, 0x01, 0, 0x80, 42]);
        assert!(command[5..].iter().all(|&b| b == 0));

        drop(device);
        fs::remove_file(ctrl_path).unwrap();
        fs::remove_file(image_path).unwrap();
    }
}
