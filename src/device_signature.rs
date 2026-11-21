use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_rp::Peri;
use static_cell::StaticCell;

const FLASH_SIZE: usize = 2 * 1024 * 1024;
const DEVICE_ID_LEN: usize = 22;
static DEVICE_ID_STR: StaticCell<[u8; DEVICE_ID_LEN]> = StaticCell::new();

/// Returns the flash unique ID followed by the flash JEDEC ID as a lower case hex string.
pub fn device_id_hex(flash: Peri<'static, FLASH>) -> &'static str {
    let mut flash = Flash::<_, Blocking, FLASH_SIZE>::new_blocking(flash);

    let mut uid = [0; 8];
    if let Err(e) = flash.blocking_unique_id(&mut uid) {
        defmt::warn!("Reading the flash unique ID failed: {}", e);
    }

    let jedec = match flash.blocking_jedec_id() {
        Ok(jedec) => jedec.to_be_bytes(),
        Err(e) => {
            defmt::warn!("Reading the flash JEDEC ID failed: {}", e);
            [0; 4]
        }
    };

    let mut device_id_str = [0; DEVICE_ID_LEN];
    let hex = b"0123456789abcdef";
    for (i, b) in uid.iter().chain(jedec[1..].iter()).enumerate() {
        device_id_str[i * 2] = hex[(b >> 4) as usize];
        device_id_str[i * 2 + 1] = hex[(b & 0xf) as usize];
    }

    let id = DEVICE_ID_STR.init(device_id_str);
    core::str::from_utf8(id).unwrap_or_default()
}
