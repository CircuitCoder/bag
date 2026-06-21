// A panel is a single rendered page

use leptos::prelude::*;

use bag_lib::ui::Layout;

#[component]
pub fn Panel(
    backend: ReadSignal<String>,
    path: ReadSignal<String>,
) -> impl IntoView {
    let layout = LocalResource::new(move || async move {
        let backend = backend.get();
        let path = path.get();
        let url = format!("{backend}/render/{path}");
        let data: Result<Layout, _> = crate::util::fetch(&url).await;
        web_sys::console::log_1(&format!("Fetched layout for {path}: {data:?}").into());
        data
    });

    let inner = move || match layout.get() {
        None => { view! { <div>"Loading..."</div> }.into_any() }
        Some(Err(err)) => { view! { <div>"Error: " {err.to_string()}</div> }.into_any() }
        Some(Ok(layout)) => { view! {
            <div>
                Layout: {serde_json::to_string(&layout)}
            </div>
        }.into_any() }
    };

    view! {
        <div>
            Rendered for {path.get()}
            {inner}
        </div>
    }
}
