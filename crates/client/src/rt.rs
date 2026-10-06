//! What differs between the platforms besides reading files: sleeping.

use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

/// A future that is `Send` natively (so clients can spawn the engines) and
/// doesn't need to be in the browser (where nothing is `Send`).
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + 'a>>;

/// `Send + Sync` natively (the engines share sources across their stages and
/// clients can spawn them); nothing in the browser.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> MaybeSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

/// Runs `request` again on network errors and 5xx, with growing delays.
pub async fn retry<T, F, Fut>(retries: u32, mut request: F) -> Result<T, crate::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, crate::Error>>,
{
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0;
    loop {
        match request().await {
            Err(e) if e.transient() && attempt < retries => {
                attempt += 1;
                sleep(delay).await;
                delay *= 2;
            }
            result => return result,
        }
    }
}
