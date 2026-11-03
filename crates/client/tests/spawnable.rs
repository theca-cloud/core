//! The engines' futures are `Send` natively, so clients can spawn them on a
//! multi-threaded runtime (docs/decisions.md, Architecture). Checked at
//! compile time: rustc can lose this through `join!` and closures ("not
//! general enough") without any code saying so.

use theca_client::{
    Cancel, Client, DownloadOptions, DownloadProgress, Downloader, UploadOptions, UploadProgress,
    Uploader,
};

fn assert_send<T: Send>(_: T) {}

fn upload_progress(_: &UploadProgress) {}

fn download_progress(_: &DownloadProgress) {}

#[test]
fn the_engines_can_be_spawned() {
    let client = Client::new("http://localhost").unwrap();
    let cancel = Cancel::default();
    let options = UploadOptions::default();
    let (_uploader, run) = Uploader::new(&client, &options, &cancel, upload_progress);
    assert_send(run);
    let options = DownloadOptions::default();
    let (_downloader, run) = Downloader::new(&client, &options, &cancel, download_progress);
    assert_send(run);
}
