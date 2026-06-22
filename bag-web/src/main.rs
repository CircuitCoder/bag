use bag_lib::action::Action;
use leptos::prelude::*;
use web_sys::wasm_bindgen::prelude::*;

mod panel;
mod util;

#[derive(Clone)]
struct Context {
    set_path: WriteSignal<String>,
}

impl Context {
    pub async fn handle(&self, action: &Action) {
        match action {
            Action::Navigate { to: p } => {
                self.navigate(p);
            }
        }
    }

    // TODO: relative navigation
    pub fn navigate(&self, p: &str) {
        web_sys::window()
            .unwrap()
            .history()
            .unwrap()
            .push_state_with_url(&JsValue::NULL, "", Some(&format!("/{}", p)))
            .unwrap();
        self.set_path.set(p.to_string());
    }
}

#[component]
fn App() -> impl IntoView {
    web_sys::console::log_1(&"App mounted".into());
    let local_storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    let backend = local_storage.get_item("backend").unwrap().unwrap();
    web_sys::console::log_1(&format!("Using backend: {}", backend).into());
    let (backend, _) = signal(backend);

    let initial_path = web_sys::window()
        .unwrap()
        .location()
        .pathname()
        .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
        .unwrap_or_else(|_| String::new());

    let (path, set_path) = signal(initial_path);

    let ctx = Context { set_path };
    provide_context(ctx);

    // Listen for popstate events to handle browser navigation (back/forward)
    {
        let closure = Closure::wrap(Box::new(move |_event: web_sys::PopStateEvent| {
            let new_path = web_sys::window()
                .unwrap()
                .location()
                .pathname()
                .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
                .unwrap_or_else(|_| String::new());
            set_path.set(new_path);
        }) as Box<dyn FnMut(_)>);
        web_sys::window()
            .unwrap()
            .set_onpopstate(Some(closure.as_ref().unchecked_ref()));
        closure.forget(); // Prevent the closure from being dropped
    }

    view! {
        <panel::Panel backend=backend path=path />
    }
}

// TODO: configurable backend
fn main() {
    leptos::mount::mount_to_body(App);
}
