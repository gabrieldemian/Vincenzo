//! A daemon that runs on the background and handles everything
//! that is not the UI.
use futures::{SinkExt, StreamExt};
use hashbrown::HashMap;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr}, sync::Arc, time::Duration
};
use tokio_util::codec::Framed;
use tracing::{error, info, trace, warn};

use tokio::{
    net::{TcpListener, TcpStream}, select, spawn, sync::{mpsc, oneshot, RwLock}, time::interval
};

use crate::{
    daemon_wire::{DaemonCodec, Message}, disk::{Disk, DiskMsg}, error::Error, magnet::Magnet, torrent::{Torrent, TorrentCtx, TorrentMsg, TorrentState, TorrentStatus}, utils::to_human_readable
};
use clap::Parser;

/// CLI flags used by the Daemon binary. These values
/// will take preference over values of the config file.
#[derive(Parser, Debug, Default)]
#[clap(name = "Vincenzo Daemon", author = "Gabriel Lombardo")]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The Daemon will accept TCP connections on this address.
    #[clap(long)]
    pub daemon_addr: Option<SocketAddr>,

    /// The directory in which torrents will be downloaded
    #[clap(short, long)]
    pub download_dir: Option<String>,

    /// Download a torrent using it's magnet link, wrapped in quotes.
    #[clap(short, long)]
    pub magnet: Option<String>,

    /// If the program should quit after all torrents are fully downloaded
    #[clap(short, long)]
    pub quit_after_complete: bool,

    /// Print all torrent status on stdout
    #[clap(short, long)]
    pub stats: bool,
}

/// The daemon is the highest-level entity in the library.
/// It owns [`Disk`] and [`Torrent`]s, which owns Peers.
///
/// The communication with the daemon happens via TCP with messages
/// documented at [`DaemonCodec`].
///
/// The daemon is decoupled from the UI and can even run on different machines,
/// and so, they need a way to communicate. We use TCP, so we can benefit
/// from the Framed utilities that tokio provides, making it easy
/// to create a protocol for the Daemon. HTTP wastes more bandwith
/// and would reduce consistency since the BitTorrent protocol nowadays rarely
/// uses HTTP.
pub struct Daemon {
    pub config: DaemonConfig,
    pub disk_tx: Option<mpsc::Sender<DiskMsg>>,
    pub ctx: Arc<DaemonCtx>,
    rx: mpsc::Receiver<DaemonMsg>,
}

/// Context of the [`Daemon`] that may be shared between other types.
pub struct DaemonCtx {
    pub tx: mpsc::Sender<DaemonMsg>,
    /// key: info_hash
    /// States of all Torrents, updated each second by the Torrent struct.
    pub torrent_states: RwLock<HashMap<[u8; 20], TorrentState>>,
    /// key: info_hash
    pub torrent_ctxs: RwLock<HashMap<[u8; 20], Arc<TorrentCtx>>>,
}

/// Configuration of the [`Daemon`], the values here are
/// evaluated between the config file and CLI flags.
/// The CLI flag will have preference over the same value in the config file.
pub struct DaemonConfig {
    /// The Daemon will accept TCP connections on this address.
    pub listen: SocketAddr,
    /// The directory in which torrents will be downloaded
    pub download_dir: String,
    /// If the program should quit after all torrents are fully downloaded
    pub quit_after_complete: bool,
}

/// Messages used by the [`Daemon`] for internal communication.
/// All of these local messages have an equivalent remote message
/// on [`DaemonMsg`].
#[derive(Debug)]
pub enum DaemonMsg {
    /// Tell Daemon to add a new torrent and it will immediately
    /// announce to a tracker, connect to the peers, and start the download.
    AddTorrent(Magnet),
    AddTorrentWithPeers(Magnet, Vec<SocketAddr>),
    /// Message that the Daemon will send to all connectors when the state
    /// of a torrent updates (every 1 second).
    TorrentState(TorrentState),
    /// Ask the Daemon to send a [`TorrentState`] of the torrent with the given
    /// hash_info.
    RequestTorrentState([u8; 20], oneshot::Sender<Option<TorrentState>>),
    /// Pause/Resume a torrent.
    TogglePause([u8; 20]),
    /// Gracefully shutdown the Daemon
    Quit,
    /// Print the status of all Torrents to stdout
    PrintTorrentStatus,
    MutateTorrent([u8; 20], Arc<TorrentCtx>),
}

impl Daemon {
    pub const DEFAULT_LISTENER: SocketAddr =
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 3030);

    /// Initialize the Daemon struct with the default [`DaemonConfig`].
    pub fn new(download_dir: String) -> Self {
        let (tx, rx) = mpsc::channel::<DaemonMsg>(300);

        let daemon_config = DaemonConfig {
            download_dir,
            listen: Self::DEFAULT_LISTENER,
            quit_after_complete: false,
        };

        Self {
            rx,
            disk_tx: None,
            config: daemon_config,
            ctx: Arc::new(DaemonCtx {
                tx,
                torrent_ctxs: RwLock::new(HashMap::new()),
                torrent_states: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// This function will listen to 3 different event loops:
    /// - The daemon internal messages via MPSC [`DaemonMsg`] (external)
    /// - The daemon TCP framed messages [`DaemonCodec`] (internal)
    /// - The Disk event loop [`Disk`]
    ///
    /// # Important
    ///
    /// Both internal and external messages share the same API.
    /// When the daemon receives a TCP message, it forwards to the
    /// mpsc event loop.
    ///
    /// This is useful to keep consistency, because the same command
    /// that can be fired remotely (via TCP),
    /// can also be fired internaly (via CLI flags).
    pub async fn run(&mut self) -> Result<(), Error> {
        let socket = TcpListener::bind(self.config.listen).await.unwrap();

        let (disk_tx, disk_rx) = mpsc::channel::<DiskMsg>(300);
        self.disk_tx = Some(disk_tx);

        let mut disk = Disk::new(disk_rx, self.config.download_dir.to_string());

        spawn(async move {
            disk.run().await.unwrap();
        });

        let ctx = self.ctx.clone();

        info!("Daemon listening on: {}", self.config.listen);

        // Listen to remote TCP messages
        let handle = spawn(async move {
            loop {
                match socket.accept().await {
                    Ok((socket, addr)) => {
                        info!("connected with remote: {addr}");

                        let ctx = ctx.clone();

                        spawn(async move {
                            let socket = Framed::new(socket, DaemonCodec);
                            let _ = Self::listen_remote_msgs(socket, ctx).await;
                        });
                    }
                    Err(e) => {
                        error!("could not connect with remote: {e:#?}");
                    }
                }
            }
        });

        let ctx = self.ctx.clone();

        // Listen to internal mpsc messages
        loop {
            select! {
                Some(msg) = self.rx.recv() => {
                    match msg {
                        DaemonMsg::TorrentState(torrent_state) => {
                            let mut torrent_states = self.ctx.torrent_states.write().await;

                            torrent_states.insert(torrent_state.info_hash, torrent_state.clone());

                            if self.config.quit_after_complete && torrent_states.values().all(|v| v.status == TorrentStatus::Seeding) {
                                let _ = ctx.tx.send(DaemonMsg::Quit).await;
                            }

                            drop(torrent_states);
                        }
                        DaemonMsg::AddTorrent(magnet) => {
                            let _ = self.add_torrent(magnet, None).await;
                        }
                        DaemonMsg::AddTorrentWithPeers(magnet, peers) => {
                            let _ = self.add_torrent(magnet, Some(peers)).await;
                        }
                        DaemonMsg::MutateTorrent(info_hash, ctx) => {
                            let mut ctxs = self.ctx.torrent_ctxs.write().await;
                            let local_ctx = ctxs.get_mut(&info_hash).unwrap();
                            *local_ctx = ctx;
                        }
                        DaemonMsg::TogglePause(info_hash) => {
                            let _ = self.toggle_pause(info_hash).await;
                        }
                        DaemonMsg::RequestTorrentState(info_hash, recipient) => {
                            let torrent_states = self.ctx.torrent_states.read().await;
                            let torrent_state = torrent_states.get(&info_hash);
                            let _ = recipient.send(torrent_state.cloned());
                        }
                        DaemonMsg::PrintTorrentStatus => {
                            let torrent_states = self.ctx.torrent_states.read().await;

                            println!("Showing stats of {} torrents.", torrent_states.len());

                            for state in torrent_states.values() {
                                let status_line: String = match state.status {
                                    TorrentStatus::Downloading => {
                                        format!(
                                            "{} - {}",
                                            to_human_readable(state.downloaded as f64),
                                            to_human_readable(state.download_rate as f64),
                                        )
                                    }
                                    _ => state.status.clone().into()
                                };

                                println!(
                                    "\n{}\n{}\nSeeders {} Leechers {}\n{status_line}",
                                    state.name,
                                    to_human_readable(state.size as f64),
                                    state.stats.seeders,
                                    state.stats.leechers,
                                );
                            }
                        }
                        DaemonMsg::Quit => {
                            let _ = self.quit().await;
                            handle.abort();
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Listen to messages sent remotely via TCP,
    /// A UI can be a standalone binary that is executing on another machine,
    /// and wants to control the daemon using the [`DaemonCodec`] protocol.
    async fn listen_remote_msgs(
        socket: Framed<TcpStream, DaemonCodec>,
        ctx: Arc<DaemonCtx>,
    ) -> Result<(), Error> {
        trace!("daemon listen_msgs");

        let mut draw_interval = interval(Duration::from_secs(1));
        let (mut sink, mut stream) = socket.split();

        loop {
            select! {
                // listen to messages sent remotely via TCP, and pass them
                // to our rx. We do this so we can use the exact same messages
                // when sent remotely via TCP (i.e UI on remote server),
                // or locally on the same binary (i.e CLI).
                Some(Ok(msg)) = stream.next() => {
                    match msg {
                        Message::NewTorrent(magnet_link) => {
                            trace!("daemon received NewTorrent {magnet_link}");
                            let magnet = Magnet::new(&magnet_link);
                            if let Ok(magnet) = magnet {
                                let _ = ctx.tx.send(DaemonMsg::AddTorrent(magnet)).await;
                            }
                        }
                        Message::RequestTorrentState(info_hash) => {
                            trace!("daemon RequestTorrentState {info_hash:?}");
                            let (tx, rx) = oneshot::channel();
                            let _ = ctx.tx.send(DaemonMsg::RequestTorrentState(info_hash, tx)).await;
                            let r = rx.await?;

                            let _ = sink.send(Message::TorrentState(r)).await;
                        }
                        Message::TogglePause(id) => {
                            trace!("daemon received TogglePause {id:?}");
                            let _ = ctx.tx.send(DaemonMsg::TogglePause(id)).await;
                        }
                        Message::Quit => {
                            trace!("daemon received Quit");
                            let _ = ctx.tx.send(DaemonMsg::Quit).await;
                        }
                        Message::PrintTorrentStatus => {
                            trace!("daemon received PrintTorrentStatus");
                            let _ = ctx.tx.send(DaemonMsg::PrintTorrentStatus).await;
                        }
                        _ => {}
                    }
                }
                // listen to messages sent locally, from the daemon binary.
                // a Torrent that is owned by the Daemon, may send messages to this channel
                _ = draw_interval.tick() => {
                    let _ = Self::draw(&mut sink, ctx.clone()).await;
                }
            }
        }
    }

    /// Pause/resume the torrent, making the download an upload stale.
    pub async fn toggle_pause(&self, info_hash: [u8; 20]) -> Result<(), Error> {
        let ctxs = self.ctx.torrent_ctxs.read().await;
        let ctx = ctxs
            .get(&info_hash)
            .ok_or(Error::TorrentDoesNotExist)?;

        ctx.tx.send(TorrentMsg::TogglePause).await?;

        Ok(())
    }

    /// Sends a Draw message to the [`UI`] with the updated state of a torrent.
    async fn draw<T>(sink: &mut T, ctx: Arc<DaemonCtx>) -> Result<(), Error>
    where
        T: SinkExt<Message> + Sized + std::marker::Unpin + Send,
    {
        let torrent_states = ctx.torrent_states.read().await;

        for state in torrent_states.values().cloned() {
            // debug!("{state:#?}");
            sink.send(Message::TorrentState(Some(state)))
                .await
                .map_err(|_| Error::SendErrorTcp)?;
        }

        drop(torrent_states);
        Ok(())
    }

    /// Create a new [`Torrent`] given a magnet link URL
    /// and run the torrent's event loop.
    ///
    /// # Errors
    ///
    /// This fn may return an [`Err`] if the magnet link is invalid
    ///
    /// # Panic
    ///
    /// This fn will panic if it is being called BEFORE run
    pub async fn add_torrent(
        &mut self,
        magnet: Magnet,
        peers: Option<Vec<SocketAddr>>,
    ) -> Result<(), Error> {
        trace!("magnet: {}", *magnet);
        let info_hash = magnet.parse_xt();

        let mut torrent_states = self.ctx.torrent_states.write().await;

        if torrent_states.get(&info_hash).is_some() {
            warn!("This torrent is already present on the Daemon");
            return Err(Error::NoDuplicateTorrent);
        }

        let torrent_state = TorrentState {
            name: magnet.parse_dn(),
            info_hash,
            ..Default::default()
        };

        torrent_states.insert(info_hash, torrent_state);
        drop(torrent_states);

        // disk_tx is not None at this point, this is safe
        // (if calling after run)
        let disk_tx = self.disk_tx.clone().unwrap();
        let mut torrent = Torrent::new(disk_tx, self.ctx.tx.clone(), magnet);

        let mut ctxs = self.ctx.torrent_ctxs.write().await;
        ctxs.insert(info_hash, torrent.ctx.clone());
        info!("Downloading torrent: {}", torrent.name);

        spawn(async move {
            if let Some(peers) = peers {
                torrent.start_and_run_with_peers(peers).await?;
            } else {
                torrent.start_and_run(None).await?;
            }
            Ok::<(), Error>(())
        });

        Ok(())
    }

    async fn quit(&mut self) -> Result<(), Error> {
        // tell all torrents that we are gracefully shutting down,
        // each torrent will kill their peers tasks, and their tracker task
        let ctxs = self.ctx.torrent_ctxs.read().await.clone();

        for ctx in ctxs {
            spawn(async move {
                let _ = ctx.1.tx.send(TorrentMsg::Quit).await;
            });
        }

        let _ = self.disk_tx.as_ref().unwrap().send(DiskMsg::Quit).await;

        Ok(())
    }
}
