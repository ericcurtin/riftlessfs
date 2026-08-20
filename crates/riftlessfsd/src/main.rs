/// riftlessfs daemon: a userspace virtio-fs backend.
///
/// Status: the passthrough filesystem engine (`riftlessfs-core`) and the
/// vhost-user + FUSE-over-virtio protocol implementation
/// (`riftlessfs-proto`) both exist and are tested (including a full
/// handshake + request/reply loop over a real socket with real shared
/// memory), but this has not yet been validated against a real VMM/guest
/// kernel. See the workspace README for the current validation status.
#[cfg(unix)]
#[derive(clap::Parser, Debug)]
#[command(name = "riftlessfsd", version, about)]
struct Args {
    /// Directory to share with the guest.
    #[arg(long)]
    shared_dir: std::path::PathBuf,

    /// Path to the vhost-user UNIX domain socket to listen on. riftlessfsd
    /// listens (acting as the vhost-user backend/server); point your VMM's
    /// vhost-user-fs chardev at this same path as a client.
    #[arg(long)]
    socket_path: std::path::PathBuf,
}

fn main() {
    env_logger::init();

    if !riftlessfs_core::PASSTHROUGH_SUPPORTED {
        eprintln!("riftlessfsd: this platform is not yet supported (see README)");
        std::process::exit(1);
    }

    #[cfg(unix)]
    {
        use clap::Parser;
        let args = Args::parse();

        let fs = match riftlessfs_core::PassthroughFs::new(&args.shared_dir) {
            Ok(fs) => fs,
            Err(e) => {
                eprintln!(
                    "riftlessfsd: failed to open shared dir {:?}: {}",
                    args.shared_dir, e
                );
                std::process::exit(1);
            }
        };
        log::info!("opened shared dir {:?}", args.shared_dir);

        // Remove a stale socket from a previous run, if any; ignore
        // errors (e.g. it simply not existing).
        let _ = std::fs::remove_file(&args.socket_path);

        let listener = match std::os::unix::net::UnixListener::bind(&args.socket_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "riftlessfsd: failed to bind vhost-user socket at {:?}: {}",
                    args.socket_path, e
                );
                std::process::exit(1);
            }
        };
        log::info!(
            "listening on {:?}; waiting for a vhost-user front-end (e.g. QEMU's vhost-user-fs device) to connect",
            args.socket_path
        );

        let conn = match riftlessfs_proto::vhost_user::connection::Connection::accept(&listener) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("riftlessfsd: failed to accept a connection: {e}");
                std::process::exit(1);
            }
        };
        log::info!("front-end connected");

        let session = riftlessfs_proto::fuse::dispatch::Session::new(fs);
        let server = riftlessfs_proto::vhost_user::server::Server::new(conn, session);
        if let Err(e) = server.run() {
            eprintln!("riftlessfsd: server loop exited with an error: {e}");
            std::process::exit(1);
        }
        log::info!("front-end disconnected; exiting");
    }
}
