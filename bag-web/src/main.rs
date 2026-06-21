use leptos::prelude::*;

mod panel;
mod util;

// TODO: configurable backend
fn main() {
    let local_storage = web_sys::window()
        .unwrap()
        .local_storage()
        .unwrap()
        .unwrap();
    let backend = local_storage.get_item("backend").unwrap().unwrap();
    web_sys::console::log_1(&format!("Using backend: {}", backend).into());
    let (backend, _) = signal(backend);

    let initial_path = web_sys::window()
        .unwrap()
        .location()
        .pathname()
        .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
        .unwrap_or_else(|_| String::new());

    let (path, _) = signal(initial_path);

    leptos::mount::mount_to_body(move || view! {
        <panel::Panel backend=backend path=path />
    });
}
