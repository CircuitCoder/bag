// A panel is a single rendered page

use leptos::prelude::*;

use bag_lib::ui::*;

use crate::Context;

fn render_component(backend: &str, ctx: &Context, comp: &bag_lib::ui::Component) -> AnyView {
    match comp {
        bag_lib::ui::Component::Text(Text { content }) => {
            view! { <div class="rendered-text">{content.clone()}</div> }.into_any()
        }
        bag_lib::ui::Component::Image(Image { resource }) => {
            let mime = mime_guess::from_path(&resource).first_or_octet_stream();
            let mime_type = mime.type_().as_str();
            if mime_type == "video" {
                return view! {
                    <video class="rendered-video" src={format!("{backend}/raw/{resource}")} controls loop />
                }.into_any();
            }
            view! {
                <img class="rendered-img" src={format!("{backend}/raw/{resource}")} />
            }
            .into_any()
        }
        // bag_lib::ui::Component::Button(label) => view! { <button>{label}</button> }.into_any(),
        bag_lib::ui::Component::Gallery(Gallery { images }) => {
            let items: Vec<_> = images.into_iter().map(|img| {
                let thumbnail = img.thumbnail.as_ref().and_then(|t| {
                    let mime = mime_guess::from_path(t).first_or_octet_stream();
                    let mime_type = mime.type_().as_str();
                    if mime_type == "image" {
                        // It's an image, we can render it
                        Some(view! {
                            <img
                                class="rendered-gallery-thumbnail-img"
                                loading="lazy"
                                src={format!("{backend}/raw/{t}")} />
                        }.into_any())
                    } else if mime_type == "video" {
                        Some(view! {
                            <video
                              class="rendered-gallery-thumbnail-video"
                              src={format!("{backend}/raw/{t}#t=0.001")}
                              preload="metadata"
                              muted
                              playsinline>
                            </video>
                        }.attr("loading", "lazy").into_any())
                    } else {
                        None
                    }
                });

                let thumbnail = thumbnail.unwrap_or_else(|| {
                    view! {
                        <div class="rendered-gallery-thumbnail-placeholder">?</div>
                    }.into_any()
                });

                match img.action {
                    Some(bag_lib::action::Action::Navigate { to: ref p }) => {
                        let p = p.clone();
                        let ctx = ctx.clone();
                        view! {
                            <div class="rendered-gallery-img" data-name={&img.name} on:click={move |_| {
                                ctx.navigate(&p);
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
    }
}

#[component]
pub fn Panel(backend: ReadSignal<String>, path: ReadSignal<String>) -> impl IntoView {
    let ctx: Option<Context> = use_context();
    if ctx.is_none() {
        web_sys::console::error_1(&"Panel component must be used within a Context provider".into());
    }

    let ctx: Context = use_context().expect("WTF");

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
                .map(|comp| render_component(&backend.get(), &ctx, comp))
                .collect::<Vec<_>>();
            view! {
                <main>
                    {main}
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
