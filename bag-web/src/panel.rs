// A panel is a single rendered page

use std::str::FromStr;

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

    fn render_static(
        dispatch: &Action<bag_lib::action::Action, ()>,
        backend: &str,
        comp: &bag_lib::ui::Component,
    ) -> AnyView {
        match comp {
            bag_lib::ui::Component::Text(Text {
                content,
                variant,
                action,
            }) => {
                let variant = match variant {
                    TextVariant::Title => "title",
                    TextVariant::Body => "body",
                    TextVariant::Hint => "hint",
                };
                let mut class = format!("rendered-text rendered-text-{variant}");
                if action.is_some() {
                    class.push_str(" rendered-text-link");
                }
                let action = action.clone();
                let dispatch = *dispatch;
                view! {
                    <div class={class} on:click={move |_| {
                        if let Some(ref a) = action {
                            dispatch.dispatch(a.clone());
                        }
                    }}>
                        {content.clone()}
                    </div>
                }
                .into_any()
            }
            bag_lib::ui::Component::Image(Image { resource, mime }) => {
                let mime = mime
                    .as_ref()
                    .and_then(|m| mime_guess::mime::Mime::from_str(m).ok())
                    .unwrap_or_else(|| mime_guess::from_path(resource).first_or_octet_stream());
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
                let items: Vec<_> = images.iter().map(|img| {
                    let thumbnail = img.thumbnail.as_ref().map(|t| view! {
                            <img
                                class="rendered-gallery-thumbnail"
                                loading="lazy"
                                src={format!("{}/raw/{t}", backend)} />
                        }.into_any());

                    let thumbnail = thumbnail.unwrap_or_else(|| {
                        view! {
                            <div class="rendered-gallery-thumbnail-placeholder">?</div>
                        }.into_any()
                    });

                    match img.action {
                        Some(ref a) => {
                            let dispatch = *dispatch;
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
            bag_lib::ui::Component::Button(Button {
                text,
                icon: _,
                action,
            }) => {
                let dispatch = *dispatch;
                let text = text.clone();
                let action = action.clone();
                view! { <button class="rendered-button" on:click={move |_| {
                    dispatch.dispatch(action.clone());
                }}>{text}</button> }
                .into_any()
            }
            bag_lib::ui::Component::Box(bag_lib::ui::Box {
                horizontal,
                children,
                action,
            }) => {
                let children: Vec<_> = children
                    .iter()
                    .map(|c| render_static(dispatch, backend, c))
                    .collect();
                let mut class = "rendered-box".to_owned();
                if *horizontal {
                    class.push_str(" rendered-box-horizontal");
                };
                if action.is_some() {
                    class.push_str(" rendered-box-link");
                }
                let dispatch = *dispatch;
                let action = action.clone();
                view! {
                    <div class={class} on:click={move |_| {
                        if let Some(ref a) = action {
                            dispatch.dispatch(a.clone());
                        }
                    }}>
                        {children}
                    </div>
                }
                .into_any()
            }
        }
    }
    let render = {
        let dispatch = dispatch;
        let backend = backend.clone();
        move |comp: &bag_lib::ui::Component| render_static(&dispatch, &backend, comp)
    };

    let nav_ctx = ctx.clone();
    let render_nav = move |layout: &Layout| {
        view! {
            <nav class="layout-nav">
                {layout.left.as_ref().map(|p| {
                    let p = p.clone();
                    let ctx = nav_ctx.clone();
                    view !{
                        <button
                            class="layout-nav-prev"
                            on:click={move |_| ctx.navigate(p.clone(), crate::NavigateType::Replace)}>prev</button>
                    }
                })}
                {
                    let ctx = nav_ctx.clone();
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
                    let ctx = nav_ctx.clone();
                    view !{
                        <button
                            class="layout-nav-next"
                            on:click={move |_| ctx.navigate(p.clone(), crate::NavigateType::Replace)}>next</button>
                    }
                })}
            </nav>
        }
    };

    let inner = move || match &*data.read() {
        None => view! { <div class="panel-loading">"Loading..."</div> }.into_any(),
        Some(Err(err)) => {
            let ctx = ctx.clone();
            view! {
                <div class="panel-error">
                    <h1 class="panel-error-title">Error</h1>
                    <div class="panel-error-hint">{err.to_string()}</div>

                    <button class="panel-error-settings" on:click={move |_| ctx.open_cfg()}>Settings</button>
                </div>
            }.into_any()
        }
        Some(Ok(LayoutOrAction::Layout(layout))) => {
            let main = layout.main.iter().map(&render).collect::<Vec<_>>();
            let metadata = layout.metadata.iter().map(&render).collect::<Vec<_>>();
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
        Some(Ok(LayoutOrAction::Action(_))) => {
            let _: () = view! {};
            ().into_any()
        }
    };

    view! {
        <div>
            {inner}
        </div>
    }
}
