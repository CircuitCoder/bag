// A panel is a single rendered page

use leptos::prelude::*;

use bag_lib::ui::*;

use crate::{Context, util::FetchError};

#[component]
pub fn Panel(data: ArcReadSignal<Option<Result<LayoutOrAction, FetchError>>>) -> impl IntoView {
    let ctx: Option<Context> = use_context();
    if ctx.is_none() {
        web_sys::console::error_1(&"Panel component must be used within a Context provider".into());
    }

    let ctx: Context =
        use_context().expect("Panel component must be used within a Context provider");
    let backend = ctx.backend.clone();
    let c = ctx.clone();
    let dispatch = Action::new(move |a: &bag_lib::action::Action| {
        let a = a.clone();
        let c = c.clone();
        async move { c.handle(&a).await }
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
                }
                .into_any()
            }
            bag_lib::ui::Component::Image(Image { resource }) => {
                let mime = mime_guess::from_path(&resource).first_or_octet_stream();
                let mime_type = mime.type_().as_str();
                if mime_type == "video" {
                    return view! {
                        <video class="rendered-video" src={format!("{}/raw/{resource}", backend)} controls loop />
                    }.into_any();
                }
                view! {
                    <img class="rendered-img" src={format!("{}/raw/{resource}", backend)} />
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
                                src={format!("{}/raw/{t}", backend)} />
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
                }}>{text}</button> }
                .into_any()
            }
        }
    };

    let render_nav = move |layout: &Layout| view! {
        <nav class="layout-nav">
            {layout.left.as_ref().map(|p| {
                let p = p.clone();
                let ctx = ctx.clone();
                view !{
                    <button
                        class="layout-nav-prev"
                        on:click={move |_| ctx.navigate(p.clone())}>prev</button>
                }
            })}
            {
                let ctx = ctx.clone();
                view! {
                    <button class="layout-nav-cfg"
                        on:click={move |_| ctx.open_cfg()}
                    >
                        Settings
                    </button>
                }
            }
            {layout.right.as_ref().map(|p| {
                let p = p.clone();
                let ctx = ctx.clone();
                view !{
                    <button
                        class="layout-nav-next"
                        on:click={move |_| ctx.navigate(p.clone())}>next</button>
                }
            })}
        </nav>
    };

    let inner = move || match &*data.read() {
        None => view! { <div>"Loading..."</div> }.into_any(),
        Some(Err(err)) => view! { <div>"Error: " {err.to_string()}</div> }.into_any(),
        Some(Ok(LayoutOrAction::Layout(layout))) => {
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
            let no_metadata =
                metadata.is_empty() && layout.left.is_none() && layout.right.is_none();
            view! {
                <main>
                    {render_nav(layout)}
                    <div class="layout-main">
                        {main}
                    </div>
                    <div class="layout-right">
                        {render_nav(layout)}
                        <div class="layout-metadata" class:layout-metadata-hidden={no_metadata}>
                            {metadata}
                        </div>
                    </div>
                </main>
            }
            .into_any()
        }
        Some(Ok(LayoutOrAction::Action(_))) => view! {}.into_any(),
    };

    view! {
        <div>
            {inner}
        </div>
    }
}
