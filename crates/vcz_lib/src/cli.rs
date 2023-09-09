use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser, Debug, Default)]
#[clap(
    name = "Vincenzo, a BitTorrent client for your terminal",
    author = "Gabriel Lombardo"
)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The directory in which torrents will be downloaded
    #[clap(short, long)]
    pub download_dir: Option<String>,

    /// The magnet link of the torrent, wrapped in quotes.
    #[clap(short, long)]
    pub magnet: Option<String>,

    /// The Daemon will accept connections on this TCP address.
    #[clap(long)]
    pub daemon_addr: Option<SocketAddr>,

    /// If the program should quit after a torrent is fully downloaded
    #[clap(short, long)]
    pub quit_after_complete: bool,
}