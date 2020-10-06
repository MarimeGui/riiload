use dirs::config_dir;
use structopt::StructOpt;

use wiiload_proto::net_send;
use wiiload_proto::WiiLoadFail;

use std::fs::read as fsread;
use std::fs::read_to_string;
use std::fs::remove_file;
use std::fs::File;
use std::io::Error as IOError;
use std::io::ErrorKind as IOErrorKind;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::process::exit;

// ---------- Command Line Opts ----------

// TODO: Disable per-subcommand version info
// TODO: Arguments
// TODO: Allow changing compression level

#[derive(StructOpt)]
enum Commands {
    /// Send an executable to a Wii running the HBC and connected to a network reachable from this computer.
    Load(LoadCommand),

    /// Configure defaults to use for omitting arguments while using "load".
    Config(ConfigCommand),
}

#[derive(StructOpt)]
struct LoadCommand {
    /// ELF/DOL executable file to send to the Wii.
    executable: String,
    /// Address of the target Wii. If not provided, the program will attempt to read the default from the configuration file.
    address: Option<String>,
    /// Sends the binary uncompressed. Compression is enabled by default as the bottleneck generally is the Wii's Rx speed.
    #[structopt(short, long)]
    no_compression: bool,
}

#[derive(StructOpt)]
enum ConfigCommand {
    /// Address to use by default for connecting to the Wii.
    DefaultAddress(ConfigDefaultAddressCommand),

    /// Config-file related functions.
    File(ConfigFileCommand),
}

#[derive(StructOpt)]
enum ConfigDefaultAddressCommand {
    /// Set the address.
    Set { address: String },
    /// Print the address.
    Get,
}

#[derive(StructOpt)]
enum ConfigFileCommand {
    /// Completely remove the configuration file.
    Delete,
    /// Print the configuration file path.
    PrintPath,
}

// ---------- Config file handling / getting address ----------

const FILE_NAME: &str = "riiload_config";

enum DefaultAddressConfigError {
    /// "dirs" crate could not find a suitable storage location
    NoSuitableFolder,
    /// No configuration found
    NoConfiguredDefault,
    /// Could not read/write to file properly
    FileAccess(IOError),
}

impl From<IOError> for DefaultAddressConfigError {
    fn from(r: IOError) -> DefaultAddressConfigError {
        DefaultAddressConfigError::FileAccess(r)
    }
}

impl DefaultAddressConfigError {
    fn print_problem_and_exit(&self) {
        eprint!("error: ");
        match self {
            DefaultAddressConfigError::NoSuitableFolder => {
                eprintln!("Could not find a folder for storing configuration, aborting.")
            }
            DefaultAddressConfigError::NoConfiguredDefault => {
                eprintln!("No configuration file found, aborting.")
            }
            DefaultAddressConfigError::FileAccess(e) => {
                eprintln!("Problem while accessing file ({:?})", e.kind())
            }
        }
        exit(1)
    }
}

fn get_config_path() -> Result<PathBuf, DefaultAddressConfigError> {
    let mut config = match config_dir() {
        Some(c) => c,
        _ => return Err(DefaultAddressConfigError::NoSuitableFolder),
    };

    config.push(FILE_NAME);

    Ok(config)
}

fn get_default_address() -> Result<String, DefaultAddressConfigError> {
    // TODO: Map error ?
    match read_to_string(get_config_path()?) {
        Ok(s) => Ok(s),
        Err(e) => match e.kind() {
            IOErrorKind::NotFound => Err(DefaultAddressConfigError::NoConfiguredDefault),
            _ => Err(DefaultAddressConfigError::FileAccess(e)),
        },
    }
}

/// Maybe gets the default address if option is not present
fn maybe_get_address(address: Option<String>) -> Result<String, DefaultAddressConfigError> {
    match address {
        Some(a) => Ok(a),
        None => get_default_address(),
    }
}

fn set_default_address(new: String) -> Result<(), DefaultAddressConfigError> {
    let mut writer = File::create(get_config_path()?)?;
    writer.write_all(&new.as_bytes())?;

    Ok(())
}

fn remove_config_files() -> Result<(), DefaultAddressConfigError> {
    if let Result::Err(e) = remove_file(get_config_path()?) {
        return match e.kind() {
            IOErrorKind::NotFound => Err(DefaultAddressConfigError::NoConfiguredDefault),
            _ => Err(DefaultAddressConfigError::FileAccess(e)),
        };
    }

    Ok(())
}

// ---------- Code for net loading ----------

enum NetLoadError {
    NoAddressPassed,
    CantResolveAddress,
    ArgsTooLong,
    BinaryTooLong,
    IOError(IOError),
    OtherConfigError(DefaultAddressConfigError),
}

impl From<WiiLoadFail> for NetLoadError {
    fn from(r: WiiLoadFail) -> NetLoadError {
        match r {
            WiiLoadFail::ArgsTooLong => NetLoadError::ArgsTooLong,
            WiiLoadFail::BinaryTooLong => NetLoadError::BinaryTooLong,
            WiiLoadFail::NetError(e) => NetLoadError::IOError(e),
        }
    }
}

impl From<DefaultAddressConfigError> for NetLoadError {
    fn from(r: DefaultAddressConfigError) -> NetLoadError {
        match r {
            DefaultAddressConfigError::NoConfiguredDefault => NetLoadError::NoAddressPassed,
            _ => NetLoadError::OtherConfigError(r),
        }
    }
}

impl From<IOError> for NetLoadError {
    fn from(r: IOError) -> NetLoadError {
        NetLoadError::IOError(r)
    }
}

impl NetLoadError {
    fn print_problem_and_exit(&self) {
        eprint!("error: ");
        match self {
            NetLoadError::NoAddressPassed => {
                eprintln!("No address argument, but not default address configured, aborting.")
            }
            NetLoadError::CantResolveAddress => {
                eprintln!("Cannot resolve passed address, aborting.")
            }
            NetLoadError::ArgsTooLong => eprintln!("Arguments too long, aborting."),
            NetLoadError::BinaryTooLong => eprintln!("Binary file too long, aborting."),
            NetLoadError::IOError(e) => eprintln!("IO error, aborting. ({:?})", e.kind()),
            NetLoadError::OtherConfigError(_) => {
                eprintln!("Configuration-related error, aborting.")
            }
        }
        exit(1)
    }
}

const DEFAULT_COMPRESSION_LEVEL: u8 = 5; // Tuning this is pretty hard, but from quick testing this might be the best value
const TCP_PORT: u16 = 4299; // Hard-coded in HBC ? Pointless to add an option to change it then.

// Perform the send operation
fn do_net_load(
    executable_path: String,
    address: Option<String>,
    compression: bool,
) -> Result<(), NetLoadError> {
    // Read file
    let executable_data = fsread(executable_path)?;

    // Connect to wii
    // TODO: Simplify this ?
    let to_connect_address = maybe_get_address(address)?;
    let sock_addr: SocketAddr =
        match format!("{}:{}", to_connect_address, TCP_PORT).to_socket_addrs() {
            Ok(mut i) => match i.next() {
                Some(v) => v,
                None => return Err(NetLoadError::CantResolveAddress),
            },
            Err(_) => return Err(NetLoadError::CantResolveAddress),
        };
    let mut stream = TcpStream::connect(sock_addr)?;

    // Actually send
    net_send(
        &mut stream,
        &executable_data,
        "".to_string(),
        if compression {
            Some(DEFAULT_COMPRESSION_LEVEL)
        } else {
            None
        },
    )?;

    Ok(())
}

// ---------- Main Code ----------

// Should just handle CLI-related stuff. Execute and print problem in case of an error.
fn main() {
    let opt = Commands::from_args();

    match opt {
        // Load
        Commands::Load(l) => {
            if let Result::Err(e) = do_net_load(l.executable, l.address, !l.no_compression) {
                e.print_problem_and_exit()
            }
        }
        // Config
        Commands::Config(c) => match c {
            // DefaultAddress
            ConfigCommand::DefaultAddress(d) => match d {
                // Set
                ConfigDefaultAddressCommand::Set { address } => {
                    if let Result::Err(e) = set_default_address(address) {
                        e.print_problem_and_exit()
                    }
                }
                // Get
                ConfigDefaultAddressCommand::Get => match get_default_address() {
                    Ok(a) => println!("{}", a),
                    Err(e) => e.print_problem_and_exit(),
                },
            },
            // File
            ConfigCommand::File(f) => match f {
                // Delete
                ConfigFileCommand::Delete => {
                    if let Result::Err(e) = remove_config_files() {
                        e.print_problem_and_exit()
                    }
                }
                // PrintPath
                ConfigFileCommand::PrintPath => match get_config_path() {
                    Ok(p) => println!("{}", p.to_string_lossy()),
                    Err(e) => e.print_problem_and_exit(),
                },
            },
        },
    }
}
