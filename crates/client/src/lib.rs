use wasm_bindgen::prelude::wasm_bindgen;

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
    wasm_bindgen_futures::spawn_local(async {
        if let Err(err) = render::start("slash0").await {
            log::error!("render init failed: {err:?}");
        }
    });
}
