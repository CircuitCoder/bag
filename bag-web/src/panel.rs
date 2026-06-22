// A panel is a single rendered page

use leptos::prelude::*;

use bag_lib::ui::*;

use crate::Context;

#[component]
pub fn Panel(backend: ReadSignal<String>, path: ReadSignal<String>) -> impl IntoView {
    let ctx: Option<Context> = use_context();
    if ctx.is_none() {
        web_sys::console::error_1(&"Panel component must be used within a Context provider".into());
    }

    let ctx: Context = use_context().expect("WTF");
    let dispatch = Action::new(move |a: &bag_lib::action::Action| {
        let a = a.clone();
        let ctx = ctx.clone();
        async move {
            ctx.handle(&a).await
        }
    });

    let render = move |comp: &bag_lib::ui::Component| {
        match comp {
            bag_lib::ui::Component::Text(Text { content, variant }) => {
                let variant = match variant {
                    TextVariant::Title => "title",
                    TextVariant::Body => "body",
                    TextVariant::Hint => "hint",
                };
                view! {
                    <div class={format!("rendered-text rendered-text-{variant}")}>
                        {content.clone()}
                    </div>
                }.into_any()
            }
            bag_lib::ui::Component::Image(Image { resource }) => {
                let mime = mime_guess::from_path(&resource).first_or_octet_stream();
                let mime_type = mime.type_().as_str();
                if mime_type == "video" {
                    return view! {
                        <video class="rendered-video" src={format!("{}/raw/{resource}", backend.get())} controls loop />
                    }.into_any();
                }
                view! {
                    <img class="rendered-img" src={format!("{}/raw/{resource}", backend.get())} />
                }
                .into_any()
            }
            bag_lib::ui::Component::Gallery(Gallery { images }) => {
                let items: Vec<_> = images.into_iter().map(|img| {
                    let thumbnail = img.thumbnail.as_ref().and_then(|t| {
                        // It's an image, we can render it
                        Some(view! {
                            <img
                                class="rendered-gallery-thumbnail"
                                loading="lazy"
                                src={format!("{}/raw/{t}", backend.get())} />
                        }.into_any())
                    });

                    let thumbnail = thumbnail.unwrap_or_else(|| {
                        view! {
                            <div class="rendered-gallery-thumbnail-placeholder">?</div>
                        }.into_any()
                    });

                    match img.action {
                        Some(ref a) => {
                            let dispatch = dispatch.clone();
                            let a = a.clone();
                            view! {
                                <div class="rendered-gallery-img" data-name={&img.name} on:click={move |_| {
                                    dispatch.dispatch(a.clone());
                                }}>{thumbnail}</div>
                            }.into_any()
                        }
                        None => {
                            view! {
                                <div class="rendered-gallery-img" data-name={&img.name}>{thumbnail}</div>
                            }.into_any()
                        }
                    }
                }).collect();

                view! {
                    <div class="rendered-gallery">
                        {items}
                    </div>
                }
                .into_any()
            }
            bag_lib::ui::Component::Button(Button { text, icon, action }) => {
                let dispatch = dispatch.clone();
                let text = text.clone();
                let action = action.clone();
                view! { <button class="rendered-button" on:click={move |_| {
                    dispatch.dispatch(action.clone());
                }}>{text}</button> }.into_any()
            },
        }
    };

    let layout = LocalResource::new(move || async move {
        let backend = backend.get();
        let path = path.get();
        let url = format!("{backend}/render/{path}");
        let data: Result<Layout, _> = crate::util::fetch(&url).await;
        web_sys::console::log_1(&format!("Fetched layout for {path}: {data:?}").into());
        data
    });

    let inner = move || match layout.get() {
        None => view! { <div>"Loading..."</div> }.into_any(),
        Some(Err(err)) => view! { <div>"Error: " {err.to_string()}</div> }.into_any(),
        Some(Ok(layout)) => {
            let main = layout
                .main
                .iter()
                .map(|comp| render(comp))
                .collect::<Vec<_>>();
            let metadata = layout
                .metadata
                .iter()
                .map(|comp| render(comp))
                .collect::<Vec<_>>();
            let no_metadata = metadata.is_empty();
            view! {
                <main>
                    <div class="layout-main">
                        {main}
                    </div>
                    <div class="layout-metadata" class:layout-metadata-hidden={no_metadata}>
                        {metadata}
                    </div>
                </main>
            }
            .into_any()
        }
    };

    view! {
        <div>
            {inner}
        </div>
    }
}
