use crate::model::Channel;
use anyhow::{Result, bail, ensure};
use i2cdev::{core::I2CDevice, linux::LinuxI2CDevice};

pub fn decode(payload: &[u8]) -> Result<[Channel; 6]> {
    ensure!(
        payload.len() == 24,
        "expected exactly 24 sensor bytes, got {}",
        payload.len()
    );
    let channels = std::array::from_fn(|i| {
        let p = &payload[i * 4..i * 4 + 4];
        let voltage_v = f64::from(u16::from_be_bytes([p[0], p[1]])) / 1000.0;
        let current_a = f64::from(u16::from_be_bytes([p[2], p[3]])) / 1000.0;
        Channel {
            channel_id: i as u8 + 1,
            voltage_v,
            current_a,
            power_w: voltage_v * current_a,
        }
    });
    // A zero current can be real; a zero supply voltage is not usable for protection.
    ensure!(
        channels.iter().all(|c| c.voltage_v > 0.0),
        "zero supply voltage; sample invalid"
    );
    Ok(channels)
}
pub fn read(adapter: u32) -> Result<[Channel; 6]> {
    let mut device = LinuxI2CDevice::new(format!("/dev/i2c-{adapter}"), 0x2b)?;
    // A fixed-length I2C block read, NOT SMBus block-data (device-supplied count).
    decode(&device.smbus_read_i2c_block_data(0x80, 24)?)
}
pub fn model(device: u32, subsystem: u32) -> Option<&'static str> {
    if device != 0x2b8510de {
        return None;
    }
    match subsystem {
        0x89e31043 => Some("ROG-ASTRAL-RTX5090-O32G"),
        0x8a2e1043 => Some("ROG-ASTRAL-RTX5090-O32G-WHITE"),
        0x8a5a1043 => Some("ROG- STRIX-RTX5090-032G-BTF"),
        _ => None,
    }
}
pub fn adapters(pci_root: &std::path::Path) -> Result<Vec<u32>> {
    let mut adapters = vec![];
    for entry in std::fs::read_dir(pci_root)? {
        let name = entry?.file_name();
        if let Some(number) = name.to_str().and_then(|n| n.strip_prefix("i2c-"))
            && let Ok(number) = number.parse()
        {
            adapters.push(number);
        }
    }
    adapters.sort_unstable();
    adapters.dedup();
    Ok(adapters)
}
pub fn select_adapter(adapters: &[u32], override_id: Option<u32>) -> Result<u32> {
    if let Some(id) = override_id {
        ensure!(
            adapters.contains(&id),
            "adapter {id} does not belong to this GPU PCI directory"
        );
        return Ok(id);
    }
    match adapters {
        [id] => Ok(*id),
        [] => bail!("no PCI-local I2C adapter; load i2c-dev and check driver"),
        _ => bail!(
            "ambiguous PCI-local I2C adapters {adapters:?}; specify an adapter for this GPU UUID"
        ),
    }
}
