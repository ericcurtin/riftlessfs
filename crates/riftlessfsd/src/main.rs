use clap::Parser;
use std::path::PathBuf;

/// riftlessfs daemon: a userspace virtio-fs backend.
///
/// Status: the passthrough filesystem engine (`riftlessfs-core`) is
/// functional and unit-tested; the vhost-user transport
/// (`riftlessfs-proto`) is still a work in progress, so `serve` currently
/// exits with an error. See the workspace README for the roadmap.
#[derive(Parser, Debug)]
#[command(name = "riftlessfsd", version, about)]
struct Args {
    /// Directory to share with the guest.
    #[arg(long)]
    shared_dir: PathBuf,

    /// Path to the vhost-user UNIX domain socket to listen on.
    #[arg(long)]
    socket_path: PathBuf,
}

fn main() {
    env_logger::init();
    let args = Args::parse();

    if !riftlessfs_core::PASSTHROUGH_SUPPORTED {
        eprintln!("riftlessfsd: this platform is not yet supported (see README)");
        std::process::exit(1);
    }

    #[cfg(unix)]
    {
        match riftlessfs_core::PassthroughFs::new(&args.shared_dir) {
            Ok(_fs) => {
                log::info!(
                    "opened shared dir {:?}; vhost-user transport not implemented yet, socket {:?} unused",
                    args.shared_dir,
                    args.socket_path
                );
                eprintln!(
                    "riftlessfsd: the vhost-user transport is not implemented yet (riftlessfs-proto is WIP). \
                     The passthrough engine opened {:?} successfully, but there is no wire protocol to serve \
                     it over yet.",
                    args.shared_dir
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!(
                    "riftlessfsd: failed to open shared dir {:?}: {}",
                    args.shared_dir, e
                );
                std::process::exit(1);
            }
        }
    }
}
