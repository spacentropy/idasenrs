use anyhow::{Context, Result, anyhow};
use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use byteorder::{ByteOrder, LittleEndian};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio::time;
use uuid::Uuid;

// --- Constants ---

const IDASEN_MIN_HEIGHT: f64 = 0.62;
const IDASEN_MAX_HEIGHT: f64 = 1.27;

// UUIDs
const UUID_HEIGHT: Uuid = Uuid::from_u128(0x99fa0021_338a_1024_8a49_009c0215f78a);
const UUID_COMMAND: Uuid = Uuid::from_u128(0x99fa0002_338a_1024_8a49_009c0215f78a);
const UUID_REFERENCE_INPUT: Uuid = Uuid::from_u128(0x99fa0031_338a_1024_8a49_009c0215f78a);
const UUID_ADV_SVC: Uuid = Uuid::from_u128(0x99fa0001_338a_1024_8a49_009c0215f78a);
const UUID_DPG: Uuid = Uuid::from_u128(0x99fa0011_338a_1024_8a49_009c0215f78a);

// Commands
const COMMAND_REFERENCE_INPUT_STOP: [u8; 2] = [0x01, 0x80];
// const COMMAND_UP: [u8; 2] = [0x47, 0x00];
// const COMMAND_DOWN: [u8; 2] = [0x46, 0x00];
const COMMAND_STOP: [u8; 2] = [0xFF, 0x00];
const COMMAND_WAKEUP: [u8; 2] = [0xFE, 0x00];

// --- Configuration ---

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// Device identifier (UUID on macOS, MAC address on Linux/Windows)
    device_id: Option<String>,
    /// Legacy MAC address field for backwards compatibility
    #[serde(skip_serializing_if = "Option::is_none")]
    mac_address: Option<String>,
    positions: HashMap<String, f64>,
}

impl Default for Config {
    fn default() -> Self {
        let mut positions = HashMap::new();
        positions.insert("sit".to_string(), 0.75);
        positions.insert("stand".to_string(), 1.10);
        Self {
            device_id: None,
            mac_address: None,
            positions,
        }
    }
}

impl Config {
    /// Get the effective device identifier, preferring device_id over legacy mac_address
    fn get_device_id(&self) -> Option<&String> {
        self.device_id.as_ref().or(self.mac_address.as_ref())
    }
}

fn get_config_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "idasen")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    let config_dir = proj_dirs.config_dir();
    if !config_dir.exists() {
        std::fs::create_dir_all(config_dir)?;
    }
    Ok(config_dir.join("idasen.yaml"))
}

fn load_config() -> Result<Config> {
    let path = get_config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let file = std::fs::File::open(path)?;
    let config: Config = serde_yaml::from_reader(file)?;
    Ok(config)
}

fn save_config(config: &Config) -> Result<()> {
    let path = get_config_path()?;
    let file = std::fs::File::create(path)?;
    serde_yaml::to_writer(file, config)?;
    Ok(())
}

// --- Desk Logic ---

struct IdasenDesk {
    peripheral: Peripheral,
}

impl IdasenDesk {
    async fn connect(device_id_str: &str) -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No Bluetooth adapter found"))?;

        // Get the event stream before starting scan
        let mut events = adapter.events().await?;

        // Start scanning
        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("Failed to start scan")?;

        // Use a channel to communicate between the event thread and main task
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Peripheral>(1);
        let target_id = device_id_str.to_string();
        let adapter_clone = adapter.clone();

        // Spawn a thread to handle BLE events (btleplug doesn't use async channels internally)
        let event_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                while let Some(event) = events.next().await {
                    match event {
                        CentralEvent::DeviceDiscovered(id) => {
                            log::debug!("DeviceDiscovered: {:?}", id);
                            if id.to_string() == target_id
                                && let Ok(peripheral) = adapter_clone.peripheral(&id).await
                            {
                                let _ = tx.send(peripheral).await;
                                break;
                            }
                        }
                        CentralEvent::StateUpdate(state) => {
                            log::debug!("AdapterStateUpdate: {:?}", state);
                        }
                        _ => {}
                    }
                }
            });
        });

        // Wait for the device to be discovered with a timeout
        let peripheral = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .context("Timeout waiting for device discovery")?
            .ok_or_else(|| anyhow!("Desk with ID {} not found. You may need to run 'init' again to rediscover the device.", device_id_str))?;

        // Stop scanning once we found the device
        let _ = adapter.stop_scan().await;

        // Wait for the event thread to finish
        let _ = event_handle.join();

        if !peripheral.is_connected().await? {
            peripheral
                .connect()
                .await
                .context("Failed to connect to desk")?;
        }

        peripheral.discover_services().await?;

        let desk = IdasenDesk { peripheral };
        desk.wakeup().await?;

        Ok(desk)
    }

    async fn discover() -> Result<Option<DiscoveredDesk>> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No Bluetooth adapter found"))?;

        adapter
            .start_scan(ScanFilter {
                services: vec![UUID_ADV_SVC],
            })
            .await?;

        time::sleep(Duration::from_secs(3)).await;

        let peripherals = adapter.peripherals().await?;

        // Return the first one found that matches the service UUID
        for p in peripherals {
            let props = p.properties().await?;
            if let Some(props) = props
                && props.services.contains(&UUID_ADV_SVC)
            {
                return Ok(Some(DiscoveredDesk {
                    device_id: p.id().to_string(),
                    name: props.local_name,
                    address: props.address.to_string(),
                }));
            }
        }
        Ok(None)
    }

    async fn get_characteristic(&self, uuid: Uuid) -> Result<Characteristic> {
        let chars = self.peripheral.characteristics();
        chars
            .into_iter()
            .find(|c| c.uuid == uuid)
            .ok_or_else(|| anyhow!("Characteristic {} not found", uuid))
    }

    async fn write_char(&self, uuid: Uuid, data: &[u8]) -> Result<()> {
        let char = self.get_characteristic(uuid).await?;
        self.peripheral
            .write(&char, data, WriteType::WithoutResponse)
            .await?;
        Ok(())
    }

    async fn wakeup(&self) -> Result<()> {
        // Try DPG wakeup sequence (for DPG1C controllers)
        if (self.get_characteristic(UUID_DPG).await).is_ok() {
            let _ = self.write_char(UUID_DPG, b"\x7f\x86\x00").await;
            let _ = self.write_char(
                UUID_DPG,
                b"\x7f\x86\x80\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11"
            ).await;
        }
        self.write_char(UUID_COMMAND, &COMMAND_WAKEUP).await
    }

    async fn get_height_and_speed(&self) -> Result<(f64, f64)> {
        let char = self.get_characteristic(UUID_HEIGHT).await?;
        let data = self.peripheral.read(&char).await?;

        if data.len() != 4 {
            return Err(anyhow!("Invalid height data length"));
        }

        let height_raw = LittleEndian::read_u16(&data[0..2]);
        let speed_raw = LittleEndian::read_i16(&data[2..4]);

        let height = (height_raw as f64 / 10000.0) + IDASEN_MIN_HEIGHT;
        let speed = speed_raw as f64 / 10000.0;

        Ok((height, speed))
    }

    async fn stop(&self) -> Result<()> {
        let _ = self.write_char(UUID_COMMAND, &COMMAND_STOP).await;
        let _ = self
            .write_char(UUID_REFERENCE_INPUT, &COMMAND_REFERENCE_INPUT_STOP)
            .await;
        Ok(())
    }

    async fn move_to_target(&self, target_meters: f64) -> Result<()> {
        if !(IDASEN_MIN_HEIGHT..=IDASEN_MAX_HEIGHT).contains(&target_meters) {
            return Err(anyhow!(
                "Target height must be between {} and {} meters",
                IDASEN_MIN_HEIGHT,
                IDASEN_MAX_HEIGHT
            ));
        }

        let (current_height, _) = self.get_height_and_speed().await?;
        if (current_height - target_meters).abs() < 0.001 {
            return Ok(());
        }

        self.wakeup().await?;
        self.write_char(UUID_COMMAND, &COMMAND_STOP).await?;

        let target_raw = ((target_meters - IDASEN_MIN_HEIGHT) * 10000.0) as u16;
        let mut target_bytes = [0u8; 2];
        LittleEndian::write_u16(&mut target_bytes, target_raw);

        loop {
            self.write_char(UUID_REFERENCE_INPUT, &target_bytes).await?;
            time::sleep(Duration::from_millis(200)).await;

            let (_, speed) = self.get_height_and_speed().await?;
            if speed == 0.0 {
                break;
            }
        }
        Ok(())
    }
}

/// Information about a discovered desk
struct DiscoveredDesk {
    /// Platform-specific device identifier (UUID on macOS, MAC on Linux/Windows)
    device_id: String,
    /// Device name if available
    name: Option<String>,
    /// Bluetooth address (may not be meaningful on macOS)
    address: String,
}

// --- CLI ---

#[derive(Parser)]
#[command(name = "idasen")]
#[command(about = "Control IKEA Idasen Desk", long_about = None)]
struct Cli {
    #[arg(
        short = 'd',
        long,
        help = "Device identifier (UUID on macOS, MAC address on Linux/Windows)"
    )]
    device_id: Option<String>,

    #[arg(short, long, action = clap::ArgAction::Count, help = "Increase logging verbosity")]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new configuration file
    Init {
        #[arg(short, long, help = "Overwrite existing configuration")]
        force: bool,
    },
    /// Pair with device (wraps discovery on systems where explicit pairing isn't manual)
    Pair,
    /// Monitor the desk position
    Monitor,
    /// Get the desk height
    Height,
    /// Get the desk speed
    Speed,
    /// Save current desk position
    Save { name: String },
    /// Remove position with given name
    Delete { name: String },
    /// Stop the desk movement
    Stop,
    /// Move to a specific position name or height
    #[command(external_subcommand)]
    MoveTo(Vec<String>),
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = load_config()?;
    let cli = Cli::parse();

    // Setup logging
    let log_level = match cli.verbose {
        0 => log::LevelFilter::Error,
        1 => log::LevelFilter::Warn,
        2 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    };
    env_logger::builder().filter_level(log_level).init();

    // Determine device ID (CLI arg > config device_id > legacy mac_address)
    let device_id = cli.device_id.or_else(|| config.get_device_id().cloned());

    match cli.command {
        Commands::Init { force } => {
            let path = get_config_path()?;
            if path.exists() && !force {
                eprintln!(
                    "Configuration file already exists at {:?}. Use --force to overwrite.",
                    path
                );
                return Ok(());
            }

            println!("Scanning for desk...");
            if let Some(discovered) = IdasenDesk::discover().await? {
                println!("Discovered desk:");
                println!("  Device ID: {}", discovered.device_id);
                if let Some(name) = &discovered.name {
                    println!("  Name: {}", name);
                }
                println!("  Address: {}", discovered.address);

                config.device_id = Some(discovered.device_id);
                // Clear legacy mac_address if present
                config.mac_address = None;
            } else {
                eprintln!("No desk found during scan.");
            }

            save_config(&config)?;
            println!("Configuration saved to {:?}", path);
        }
        Commands::Pair => {
            // On most OSs handled by btleplug, explicit pairing is handled by the OS agent,
            // but we can at least try to connect to prove it works.
            let id = device_id.ok_or_else(|| {
                anyhow!("Device ID not configured. Run 'init' or provide --device-id")
            })?;
            println!("Connecting to {}...", id);
            let _desk = IdasenDesk::connect(&id).await?;
            println!("Successfully connected!");
        }
        Commands::Height => {
            let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
            let desk = IdasenDesk::connect(&id).await?;
            let (height, _) = desk.get_height_and_speed().await?;
            println!("{:.3} meters", height);
        }
        Commands::Speed => {
            let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
            let desk = IdasenDesk::connect(&id).await?;
            let (_, speed) = desk.get_height_and_speed().await?;
            println!("{:.3} meters/second", speed);
        }
        Commands::Monitor => {
            let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
            let desk = IdasenDesk::connect(&id).await?;
            let char = desk.get_characteristic(UUID_HEIGHT).await?;

            println!("Monitoring desk... (Ctrl+C to stop)");

            desk.peripheral.subscribe(&char).await?;
            let mut stream = desk.peripheral.notifications().await?;

            while let Some(data) = stream.next().await {
                if data.uuid == UUID_HEIGHT && data.value.len() == 4 {
                    let height_raw = LittleEndian::read_u16(&data.value[0..2]);
                    let speed_raw = LittleEndian::read_i16(&data.value[2..4]);
                    let height = (height_raw as f64 / 10000.0) + IDASEN_MIN_HEIGHT;
                    let speed = speed_raw as f64 / 10000.0;
                    println!("{:.3} meters - {:.3} meters/second", height, speed);
                }
            }
        }
        Commands::Save { name } => {
            if [
                "init", "pair", "monitor", "height", "speed", "save", "delete",
            ]
            .contains(&name.as_str())
            {
                eprintln!("'{}' is a reserved name.", name);
                return Ok(());
            }
            let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
            let desk = IdasenDesk::connect(&id).await?;
            let (height, _) = desk.get_height_and_speed().await?;

            config.positions.insert(name.clone(), height);
            save_config(&config)?;
            println!("Saved position '{}' with height {:.3}m", name, height);
        }
        Commands::Delete { name } => {
            if config.positions.remove(&name).is_some() {
                save_config(&config)?;
                println!("Deleted position '{}'", name);
            } else {
                eprintln!("Position '{}' not found", name);
            }
        }
        Commands::Stop => {
            let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
            let desk = IdasenDesk::connect(&id).await?;
            desk.stop().await?;
            println!("Desk stopped.");
        }
        Commands::MoveTo(args) => {
            // Handle dynamic commands (position names)
            if let Some(arg) = args.first() {
                let target_height = if let Some(h) = config.positions.get(arg) {
                    *h
                } else if let Ok(h) = f64::from_str(arg) {
                    h
                } else {
                    eprintln!("Unknown position or invalid height: {}", arg);
                    return Ok(());
                };

                let id = device_id.ok_or_else(|| anyhow!("Device ID not configured"))?;
                let desk = IdasenDesk::connect(&id).await?;
                println!("Moving to {:.3}m...", target_height);
                desk.move_to_target(target_height).await?;
                println!("Reached target.");
            } else {
                eprintln!("Please specify a position name or height.");
            }
        }
    }

    Ok(())
}
