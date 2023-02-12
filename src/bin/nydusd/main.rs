// Copyright 2022 Alibaba Cloud. All rights reserved.
// Copyright 2020 Ant Group. All rights reserved.
// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)
#![deny(warnings)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate nydus_error;

use std::convert::TryInto;
use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::{Arg, ArgAction, ArgMatches, Command};
use mio::{Events, Poll, Token, Waker};
use nix::sys::signal;
use rlimit::Resource;

use nydus::blob_cache::BlobCacheMgr;
use nydus::daemon::NydusDaemon;
use nydus::{
    create_daemon, get_build_time_info, validate_threads_configuration, Error as NydusError,
    FsBackendMountCmd, FsService, ServiceArgs, SubCmdArgs,
};
use nydus_api::BuildTimeInfo;
use nydus_app::{dump_program_info, setup_logging};

use crate::api_server_glue::ApiServerController;

#[cfg(feature = "virtiofs")]
mod virtiofs;

mod api_server_glue;

/// Minimal number of file descriptors reserved for system.
const RLIMIT_NOFILE_RESERVED: u64 = 16384;
/// Default number of file descriptors.
const RLIMIT_NOFILE_MAX: u64 = 1_000_000;

lazy_static! {
    static ref DAEMON_CONTROLLER: DaemonController = DaemonController::new();
    static ref BTI_STRING: String = get_build_time_info().0;
    static ref BTI: BuildTimeInfo = get_build_time_info().1;
}

/// Controller to manage registered filesystem/blobcache/fscache services.
pub struct DaemonController {
    active: AtomicBool,
    singleton_mode: AtomicBool,
    daemon: Mutex<Option<Arc<dyn NydusDaemon>>>,
    blob_cache_mgr: Mutex<Option<Arc<BlobCacheMgr>>>,
    // For backward compatibility to support singleton fusedev/virtiofs server.
    fs_service: Mutex<Option<Arc<dyn FsService>>>,
    waker: Arc<Waker>,
    poller: Mutex<Poll>,
}

impl DaemonController {
    fn new() -> Self {
        let poller = Poll::new().expect("Failed to create poller for DaemonController");
        let waker = Waker::new(poller.registry(), Token(1))
            .expect("Failed to create waker for DaemonController");

        Self {
            active: AtomicBool::new(true),
            singleton_mode: AtomicBool::new(true),
            daemon: Mutex::new(None),
            blob_cache_mgr: Mutex::new(None),
            fs_service: Mutex::new(None),
            waker: Arc::new(waker),
            poller: Mutex::new(poller),
        }
    }

    /// Check whether the service controller is still in active/working state.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Allocate a waker to notify stop events.
    pub fn alloc_waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }

    /// Enable/disable singleton mode.
    pub fn set_singleton_mode(&self, enabled: bool) {
        self.singleton_mode.store(enabled, Ordering::Release);
    }

    /// Set the daemon service object.
    pub fn set_daemon(&self, daemon: Arc<dyn NydusDaemon>) -> Option<Arc<dyn NydusDaemon>> {
        self.daemon.lock().unwrap().replace(daemon)
    }

    /// Get the daemon service object.
    ///
    /// Panic if called before `set_daemon()` has been called.
    pub fn get_daemon(&self) -> Arc<dyn NydusDaemon> {
        self.daemon.lock().unwrap().clone().unwrap()
    }

    /// Get the optional blob cache manager.
    pub fn get_blob_cache_mgr(&self) -> Option<Arc<BlobCacheMgr>> {
        self.blob_cache_mgr.lock().unwrap().clone()
    }

    /// Set the optional blob cache manager.
    pub fn set_blob_cache_mgr(&self, mgr: Arc<BlobCacheMgr>) -> Option<Arc<BlobCacheMgr>> {
        self.blob_cache_mgr.lock().unwrap().replace(mgr)
    }

    /// Set the default fs service object.
    pub fn set_fs_service(&self, service: Arc<dyn FsService>) -> Option<Arc<dyn FsService>> {
        self.fs_service.lock().unwrap().replace(service)
    }

    /// Get the default fs service object.
    pub fn get_fs_service(&self) -> Option<Arc<dyn FsService>> {
        self.fs_service.lock().unwrap().clone()
    }

    fn shutdown(&self) {
        // Marking exiting state.
        self.active.store(false, Ordering::Release);
        DAEMON_CONTROLLER.set_singleton_mode(false);
        // Signal the `run_loop()` working thread to exit.
        let _ = self.waker.wake();

        let daemon = self.daemon.lock().unwrap().take();
        if let Some(d) = daemon {
            if let Err(e) = d.trigger_stop() {
                error!("failed to stop daemon: {}", e);
            }
            if let Err(e) = d.wait() {
                error!("failed to wait daemon: {}", e)
            }
        }
    }

    fn run_loop(&self) {
        let mut events = Events::with_capacity(8);

        loop {
            match self.poller.lock().unwrap().poll(&mut events, None) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => error!("failed to receive notification from waker: {}", e),
                Ok(_) => {}
            }

            for event in events.iter() {
                if event.is_error() {
                    error!("Got error on the monitored event.");
                    continue;
                }

                if event.is_readable() && event.token() == Token(1) {
                    if self.active.load(Ordering::Acquire) {
                        return;
                    } else if !self.singleton_mode.load(Ordering::Acquire) {
                        self.active.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }
}

fn thread_validator(v: &str) -> std::result::Result<String, String> {
    validate_threads_configuration(v).map(|s| s.to_string())
}

fn append_fs_options(app: Command) -> Command {
    app.arg(
        Arg::new("bootstrap")
            .long("bootstrap")
            .short('B')
            .help("Path to the RAFS filesystem metadata file")
            .conflicts_with("shared-dir"),
    )
    .arg(
        Arg::new("localfs-dir")
            .long("localfs-dir")
            .short('D')
            .help(
                "Path to the `localfs` working directory, which also enables the `localfs` storage backend"
            )
            .conflicts_with("config"),
    )
    .arg(
        Arg::new("shared-dir")
            .long("shared-dir")
            .short('s')
            .help("Path to the directory to be shared via the `passthroughfs` FUSE driver")
    )
    .arg(
        Arg::new("prefetch-files")
            .long("prefetch-files")
            .help("Path to the prefetch configuration file containing a list of directories/files separated by newlines")
            .required(false)
            .requires("bootstrap")
            .num_args(1),
    )
    .arg(
        Arg::new("virtual-mountpoint")
            .long("virtual-mountpoint")
            .short('m')
            .help("Mountpoint within the FUSE/virtiofs device to mount the RAFS/passthroughfs filesystem")
            .default_value("/")
            .required(false),
    )
}

fn append_fuse_options(app: Command) -> Command {
    app.arg(
        Arg::new("mountpoint")
            .long("mountpoint")
            .short('M')
            .help("Mountpoint for the FUSE filesystem, target for `mount.fuse`")
            .required(true),
    )
    .arg(
        Arg::new("failover-policy")
            .long("failover-policy")
            .default_value("resend")
            .help("FUSE server failover policy")
            .value_parser(["resend", "flush"])
            .required(false),
    )
    .arg(
        Arg::new("fuse-threads")
            .long("fuse-threads")
            .alias("thread-num")
            .default_value("4")
            .help("Number of worker threads to serve FUSE I/O requests")
            .value_parser(thread_validator)
            .required(false),
    )
    .arg(
        Arg::new("writable")
            .long("writable")
            .action(ArgAction::SetTrue)
            .help("Mounts FUSE filesystem in rw mode"),
    )
}

fn append_fuse_subcmd_options(cmd: Command) -> Command {
    let subcmd = Command::new("fuse").about("Run the Nydus daemon as a dedicated FUSE server");
    let subcmd = append_fuse_options(subcmd);
    let subcmd = append_fs_options(subcmd);
    cmd.subcommand(subcmd)
}

#[cfg(feature = "virtiofs")]
fn append_virtiofs_options(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("hybrid-mode")
            .long("hybrid-mode")
            .help("Enables both `RAFS` and `passthroughfs` filesystem drivers")
            .action(ArgAction::SetFalse)
            .required(false),
    )
    .arg(
        Arg::new("sock")
            .long("sock")
            .help("Path to the vhost-user API socket")
            .required(true),
    )
}

#[cfg(feature = "virtiofs")]
fn append_virtiofs_subcmd_options(cmd: Command) -> Command {
    let subcmd =
        Command::new("virtiofs").about("Run the Nydus daemon as a dedicated virtio-fs server");
    let subcmd = append_virtiofs_options(subcmd);
    let subcmd = append_fs_options(subcmd);
    cmd.subcommand(subcmd)
}

fn append_fscache_options(app: Command) -> Command {
    app.arg(
        Arg::new("fscache")
            .long("fscache")
            .short('F')
            .help("Working directory for Linux fscache driver to store cache files"),
    )
    .arg(
        Arg::new("fscache-tag")
            .long("fscache-tag")
            .help("Tag to identify the fscache daemon instance")
            .requires("fscache"),
    )
    .arg(
        Arg::new("fscache-threads")
            .long("fscache-threads")
            .default_value("4")
            .help("Number of working threads to serve fscache requests")
            .required(false)
            .value_parser(thread_validator),
    )
}

fn append_singleton_subcmd_options(cmd: Command) -> Command {
    let subcmd = Command::new("singleton")
        .about("Run the Nydus daemon to host multiple blobcache/fscache/fuse/virtio-fs services");
    let subcmd = append_fscache_options(subcmd);

    // TODO: enable support of fuse service
    /*
    let subcmd = subcmd.arg(
        Arg::new("fuse")
            .long("fuse")
            .short("f")
            .help("Run as a shared FUSE server"),
    );
    let subcmd = append_fuse_options(subcmd);
    let subcmd = append_fs_options(subcmd);
    */

    cmd.subcommand(subcmd)
}

fn prepare_commandline_options() -> Command {
    let cmdline = Command::new("nydusd")
        .about("Nydus daemon to provide BlobCache, FsCache, FUSE, Virtio-fs and container image services")
        .arg(
            Arg::new("apisock")
                .long("apisock")
                .short('A')
                .help("Path to the Nydus daemon administration API socket")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .short('C')
                .help("Path to the Nydus daemon configuration file")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("id")
                .long("id")
                .help("Service instance identifier")
                .required(false)
                .requires("supervisor")
                .global(true),
        )
        .arg(
            Arg::new("log-level")
                .long("log-level")
                .short('l')
                .help("Log level:")
                .default_value("info")
                .value_parser(["trace", "debug", "info", "warn", "error"])
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("log-file")
                .long("log-file")
                .short('L')
                .help("Log messages to the file. Default extension \".log\" will be used if no extension specified.")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("log-rotation-size")
                .long("log-rotation-size")
                .help("Specify log rotation size(MB), 0 to disable")
                .default_value("0")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("rlimit-nofile")
                .long("rlimit-nofile")
                .default_value("1000000")
                .help("Set rlimit for maximum file descriptor number (0 leaves it unchanged)")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("supervisor")
                .long("supervisor")
                .help("Path to the Nydus daemon supervisor API socket")
                .required(false)
                .requires("id")
                .global(true),
        )
        .arg(
            Arg::new("upgrade")
                .long("upgrade")
                .help("Start Nydus daemon in upgrade mode")
                .action(ArgAction::SetTrue)
                .required(false)
                .global(true),
        )
        .args_conflicts_with_subcommands(true);

    let cmdline = append_fuse_options(cmdline);
    let cmdline = append_fs_options(cmdline);
    let cmdline = append_fuse_subcmd_options(cmdline);
    #[cfg(feature = "virtiofs")]
    let cmdline = append_virtiofs_subcmd_options(cmdline);
    append_singleton_subcmd_options(cmdline)
}

#[cfg(target_os = "macos")]
fn get_max_rlimit_nofile() -> Result<u64> {
    let mut mib = [nix::libc::CTL_KERN, nix::libc::KERN_MAXFILES];
    let mut file_max: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // Safe because the arguments are valid and we have checked the result.
    let res = unsafe {
        nix::libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut file_max) as *mut u64 as *mut nix::libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    nix::errno::Errno::result(res)?;
    Ok(file_max)
}

#[cfg(target_os = "linux")]
fn get_max_rlimit_nofile() -> Result<u64> {
    let file_max = std::fs::read_to_string("/proc/sys/fs/file-max")?;
    file_max
        .trim()
        .parse::<u64>()
        .map_err(|_| eother!("invalid content from fs.file-max"))
}

/// Handle command line option to tune rlimit for maximum file descriptor number.
fn handle_rlimit_nofile_option(args: &ArgMatches, option_name: &str) -> Result<()> {
    // `rlimit-nofile` has a default value, so safe to unwrap().
    let rlimit_nofile: u64 = args
        .get_one::<String>(option_name)
        .unwrap()
        .parse()
        .map_err(|_e| {
            Error::new(
                ErrorKind::InvalidInput,
                "invalid value for option `rlimit-nofile`",
            )
        })?;

    if rlimit_nofile != 0 {
        // Ensures there are fds available for other processes so we don't cause resource exhaustion.
        let rlimit_nofile_max = get_max_rlimit_nofile()?;
        if rlimit_nofile_max < 2 * RLIMIT_NOFILE_RESERVED {
            return Err(eother!(
                "The fs.file-max sysctl is too low to allow a reasonable number of open files."
            ));
        }

        // Reduce max_fds below the system-wide maximum, if necessary.
        let rlimit_nofile_max = std::cmp::min(
            rlimit_nofile_max - RLIMIT_NOFILE_RESERVED,
            RLIMIT_NOFILE_MAX,
        );
        let rlimit_nofile_max = Resource::NOFILE.get().map(|(curr, _)| {
            if curr >= rlimit_nofile_max {
                curr
            } else {
                rlimit_nofile_max
            }
        })?;
        let rlimit_nofile = std::cmp::min(rlimit_nofile, rlimit_nofile_max);
        info!(
            "Set rlimit-nofile to {}, maximum {}",
            rlimit_nofile, rlimit_nofile_max
        );
        Resource::NOFILE.set(rlimit_nofile, rlimit_nofile)?;
    }

    Ok(())
}

fn process_fs_service(
    args: SubCmdArgs,
    bti: BuildTimeInfo,
    apisock: Option<&str>,
    is_fuse: bool,
) -> Result<()> {
    // shared-dir means fs passthrough
    let shared_dir = args.value_of("shared-dir");
    // bootstrap means rafs only
    let bootstrap = args.value_of("bootstrap");
    // safe as virtual_mountpoint default to "/"
    let virtual_mnt = args.value_of("virtual-mountpoint").unwrap();

    let mut opts = fuse_backend_rs::api::VfsOptions::default();
    let mount_cmd = if let Some(shared_dir) = shared_dir {
        let cmd = FsBackendMountCmd {
            fs_type: nydus::FsBackendType::PassthroughFs,
            source: shared_dir.to_string(),
            config: "".to_string(),
            mountpoint: virtual_mnt.to_string(),
            prefetch_files: None,
        };

        // passthroughfs requires !no_open
        opts.no_open = false;
        opts.no_opendir = false;
        opts.killpriv_v2 = true;

        Some(cmd)
    } else if let Some(b) = bootstrap {
        let config = match args.value_of("localfs-dir") {
            Some(v) => {
                format!(
                    r###"
        {{
            "device": {{
                "backend": {{
                    "type": "localfs",
                    "config": {{
                        "dir": {:?},
                        "readahead": true
                    }}
                }},
                "cache": {{
                    "type": "blobcache",
                    "config": {{
                        "compressed": false,
                        "work_dir": {:?}
                    }}
                }}
            }},
            "mode": "direct",
            "digest_validate": false,
            "iostats_files": false
        }}
        "###,
                    v, v
                )
            }
            None => match args.value_of("config") {
                Some(v) => std::fs::read_to_string(v)?,
                None => {
                    let e = NydusError::InvalidArguments(
                        "both --config and --localfs-dir are missing".to_string(),
                    );
                    return Err(e.into());
                }
            },
        };

        // read the prefetch list of files from prefetch-files
        let prefetch_files: Option<Vec<String>> = match args.value_of("prefetch-files") {
            Some(v) => {
                let content = match std::fs::read_to_string(v) {
                    Ok(v) => v,
                    Err(_) => {
                        let e = NydusError::InvalidArguments(
                            "the prefetch-files arg is not a file path".to_string(),
                        );
                        return Err(e.into());
                    }
                };
                let mut prefetch_files: Vec<String> = Vec::new();
                for line in content.lines() {
                    if line.is_empty() || line.trim().is_empty() {
                        continue;
                    }
                    prefetch_files.push(line.trim().to_string());
                }
                Some(prefetch_files)
            }
            None => None,
        };

        let cmd = FsBackendMountCmd {
            fs_type: nydus::FsBackendType::Rafs,
            source: b.to_string(),
            config,
            mountpoint: virtual_mnt.to_string(),
            prefetch_files,
        };

        // rafs can be readonly and skip open
        opts.no_open = true;

        Some(cmd)
    } else {
        None
    };

    // Enable all options required by passthroughfs
    if !is_fuse && args.is_present("hybrid-mode") {
        opts.no_open = false;
        opts.no_opendir = false;
        opts.killpriv_v2 = true;
    }

    let vfs = fuse_backend_rs::api::Vfs::new(opts);
    let vfs = Arc::new(vfs);
    // Basically, below two arguments are essential for live-upgrade/failover/ and external management.
    let daemon_id = args.value_of("id").map(|id| id.to_string());
    let supervisor = args.value_of("supervisor").map(|s| s.to_string());

    if is_fuse {
        // threads means number of fuse service threads
        let threads: u32 = args
            .value_of("fuse-threads")
            .map(|n| n.parse().unwrap_or(1))
            .unwrap_or(1);

        let p = args
            .value_of("failover-policy")
            .unwrap_or(&"flush".to_string())
            .try_into()
            .map_err(|e| {
                error!("Invalid failover policy");
                e
            })?;

        // mountpoint means fuse device only
        let mountpoint = args.value_of("mountpoint").ok_or_else(|| {
            NydusError::InvalidArguments("Mountpoint must be provided for FUSE server!".to_string())
        })?;

        let daemon = {
            nydus::create_fuse_daemon(
                mountpoint,
                vfs,
                supervisor,
                daemon_id,
                threads,
                DAEMON_CONTROLLER.alloc_waker(),
                apisock,
                args.is_present("upgrade"),
                !args.is_present("writable"),
                p,
                mount_cmd,
                bti,
            )
            .map(|d| {
                info!("Fuse daemon started!");
                d
            })
            .map_err(|e| {
                error!("Failed in starting daemon: {}", e);
                e
            })?
        };
        DAEMON_CONTROLLER.set_daemon(daemon);
    } else {
        #[cfg(feature = "virtiofs")]
        {
            let vu_sock = args.value_of("sock").ok_or_else(|| {
                NydusError::InvalidArguments("vhost socket must be provided!".to_string())
            })?;
            let _ = apisock.as_ref();
            DAEMON_CONTROLLER.set_daemon(virtiofs::create_virtiofs_daemon(
                daemon_id, supervisor, vu_sock, vfs, mount_cmd, bti,
            )?);
        }
    }

    Ok(())
}

fn process_singleton_arguments(
    subargs: &SubCmdArgs,
    _apisock: Option<&str>,
    bti: BuildTimeInfo,
) -> Result<()> {
    info!("Start Nydus daemon in singleton mode!");
    let daemon = create_daemon(subargs, bti, DAEMON_CONTROLLER.alloc_waker()).map_err(|e| {
        error!("Failed to start singleton daemon: {}", e);
        e
    })?;
    DAEMON_CONTROLLER.set_singleton_mode(true);
    if let Some(blob_mgr) = daemon.get_blob_cache_mgr() {
        DAEMON_CONTROLLER.set_blob_cache_mgr(blob_mgr);
    }
    DAEMON_CONTROLLER.set_daemon(daemon);
    Ok(())
}

extern "C" fn sig_exit(_sig: std::os::raw::c_int) {
    DAEMON_CONTROLLER.shutdown();
}

fn main() -> Result<()> {
    let bti = BTI.to_owned();
    let cmd_options = prepare_commandline_options().version(BTI_STRING.as_str());
    let args = cmd_options.get_matches();
    let logging_file = args.get_one::<String>("log-file").map(|l| l.into());
    // Safe to unwrap because it has default value and possible values are defined
    let level = args
        .get_one::<String>("log-level")
        .unwrap()
        .parse()
        .unwrap();
    let apisock = args.get_one::<String>("apisock").map(|s| s.as_str());
    let rotation_size = args
        .get_one::<String>("log-rotation-size")
        .unwrap()
        .parse::<u64>()
        .map_err(|e| einval!(format!("Invalid log rotation size: {}", e)))?;

    setup_logging(logging_file, level, rotation_size)?;

    // Initialize and run the daemon controller event loop.
    nydus_app::signal::register_signal_handler(signal::SIGINT, sig_exit);
    nydus_app::signal::register_signal_handler(signal::SIGTERM, sig_exit);

    dump_program_info();
    handle_rlimit_nofile_option(&args, "rlimit-nofile")?;

    match args.subcommand_name() {
        Some("singleton") => {
            // Safe to unwrap because the subcommand is `singleton`.
            let subargs = args.subcommand_matches("singleton").unwrap();
            let subargs = SubCmdArgs::new(&args, subargs);
            process_singleton_arguments(&subargs, apisock, bti)?;
        }
        Some("fuse") => {
            // Safe to unwrap because the subcommand is `fuse`.
            let subargs = args.subcommand_matches("fuse").unwrap();
            let subargs = SubCmdArgs::new(&args, subargs);
            process_fs_service(subargs, bti, apisock, true)?;
        }
        Some("virtiofs") => {
            // Safe to unwrap because the subcommand is `virtiofs`.
            let subargs = args.subcommand_matches("virtiofs").unwrap();
            let subargs = SubCmdArgs::new(&args, subargs);
            process_fs_service(subargs, bti, apisock, false)?;
        }
        _ => {
            let subargs = SubCmdArgs::new(&args, &args);
            process_fs_service(subargs, bti, apisock, true)?;
        }
    }

    let daemon = DAEMON_CONTROLLER.get_daemon();
    if let Some(fs) = daemon.get_default_fs_service() {
        DAEMON_CONTROLLER.set_fs_service(fs);
    }

    // Start the HTTP Administration API server
    let mut api_controller = ApiServerController::new(apisock);
    api_controller.start()?;

    // Run the main event loop
    if DAEMON_CONTROLLER.is_active() {
        DAEMON_CONTROLLER.run_loop();
    }

    // Gracefully shutdown system.
    info!("nydusd quits");
    api_controller.stop();
    DAEMON_CONTROLLER.shutdown();

    Ok(())
}
