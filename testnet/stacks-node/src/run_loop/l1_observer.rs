use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;

use stacks::burnchains::events::NewBlock;
use stacks::burnchains::BurnBlockInputChannel;
use std::thread;
use tokio::sync::oneshot;
use tokio::sync::oneshot::Receiver;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinError;
use warp;
use warp::Filter;
pub const EVENT_OBSERVER_PORT: u16 = 50303;

lazy_static! {
    static ref INDEXER_CHANNEL: Mutex<Option<Box<dyn BurnBlockInputChannel>>> = Mutex::new(None);
}

/// Route handler.
async fn handle_new_block(block: serde_json::Value) -> Result<impl warp::Reply, Infallible> {
    let parsed_block: NewBlock =
        serde_json::from_str(&block.to_string()).expect("Failed to parse events JSON");
    info!("handle_new_block receives new block {:?}", &parsed_block);
    INDEXER_CHANNEL
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .push_block(Box::new(parsed_block))
        .expect("remove this");
    Ok(warp::http::StatusCode::OK)
}

/// Define and run the `warp` server.
async fn serve(signal_receiver: Receiver<()>) -> Result<(), JoinError> {
    let first_part = warp::path!("new_block")
        .and(warp::post())
        .and(warp::body::json());
    let new_blocks = first_part.and_then(handle_new_block);

    info!("Binding warp server.");
    let (_addr, server) = warp::serve(new_blocks).bind_with_graceful_shutdown(
        ([127, 0, 0, 1], EVENT_OBSERVER_PORT),
        async {
            signal_receiver.await.ok();
        },
    );

    // Spawn the server into a runtime
    info!("Spawning warp server");
    tokio::task::spawn(server).await
}

/// Spawn a thread with a `warp` server.
pub fn spawn(channel: Box<dyn BurnBlockInputChannel>) -> Sender<()> {
    let (signal_sender, signal_receiver) = oneshot::channel();
    thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().expect("Failed to initialize tokio");
        rt.block_on(serve(signal_receiver))
            .expect("block_on failed");
    });
    signal_sender
}
