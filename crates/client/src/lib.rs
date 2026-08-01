use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use log::info;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::js_sys::Promise;

mod render;

fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&info.to_string().into());
    }));
}

#[wasm_bindgen(start)]
pub fn main() {
    set_panic_hook();
    console_log::init_with_level(log::Level::Info).ok();
    info!("Hello world");
    // wasm_bindgen_futures::spawn_local(async {
    //     if let Err(err) = render::start("slash0").await {
    //         log::error!("render init failed: {err:?}");
    //     }
    // });
    garbo_main();
}

// TODO: This is all garbage, rewrite properly once I know how websockets/serialization will work

pub fn garbo_main() {
    let ws = WebSocket::open("/ws").unwrap();

    let (mut write, _read) = ws.split();
    spawn_local(async move {
        loop {
            write
                .send(Message::Text("Hello world".to_string()))
                .await
                .unwrap();
            let p = Promise::new(&mut |resolve, _| {
                web_sys::window()
                    .unwrap()
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1000)
                    .unwrap();
            });
            let _ = JsFuture::from(p).await;
        }
    })
}
