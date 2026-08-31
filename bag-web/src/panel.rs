// A panel is a single rendered page

use std::str::FromStr;

use leptos::prelude::*;

use bag_lib::ui::*;

use crate::{Context, util::FetchError};

fn gallery_placeholder_icon(ty: &GalleryImageType) -> &'static str {
    match ty {
        GalleryImageType::Directory => "folder",
        GalleryImageType::Archive => "folder_zip",
        GalleryImageType::File => "image",
    }
}

#[component]
pub fn Panel(
    data: ArcReadSignal<Option<Result<LayoutOrAction, FetchError>>>,
    path: String,
) -> impl IntoView {
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
        path: &str,
    ) -> AnyView {
        let initial_path = bag_lib::path::Path::try_from(path).unwrap(); // TODO: handle this

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
                        let icon = gallery_placeholder_icon(&img.ty);
                        view! {
                            <div class="rendered-gallery-thumbnail-placeholder">
                                <span class="material-symbols-filled" aria-hidden="true">{icon}</span>
                            </div>
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
            bag_lib::ui::Component::Button(Button { text, icon, action }) => {
                let dispatch = *dispatch;
                let text = text.clone();
                let icon = icon.as_ref().map(|icon| {
                    view! {
                        <span class="material-symbols-filled rendered-button-icon" aria-hidden="true">
                            {icon.clone()}
                        </span>
                    }
                });
                let action = action.clone();
                view! { <button class="rendered-button" on:click={move |_| {
                    dispatch.dispatch(action.clone());
                }}>{icon}{text}</button> }
                .into_any()
            }
            bag_lib::ui::Component::Box(bag_lib::ui::Box {
                horizontal,
                children,
                action,
            }) => {
                let children: Vec<_> = children
                    .iter()
                    .map(|c| render_static(dispatch, backend, c, path))
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
            bag_lib::ui::Component::Input(bag_lib::ui::Input {
                bidir,
                segment,
                param,
                ty,
                placeholder,
                button,
            }) => {
                let init = bidir
                    .then(|| {
                        initial_path
                            .segments()
                            .get(*segment)
                            .and_then(|s| s.arg(param))
                    })
                    .flatten()
                    .unwrap_or("");
                let input_value = ArcRwSignal::new(init.to_owned());
                let initial_path = initial_path.to_static();
                let commit = {
                    let input_value = input_value.clone();
                    let dispatch = *dispatch;
                    let segment = *segment;
                    let param = param.clone();
                    move || {
                        let Some(updated_path) = initial_path.update(segment, |orig| {
                            orig.with_arg_owned(param.clone(), Some(input_value.get_untracked()))
                        }) else {
                            return;
                        };
                        dispatch.dispatch(bag_lib::action::Action::Navigate {
                            to: updated_path.to_string(),
                        });
                    }
                };
                view! {
                    <div class="rendered-input">
                        <input
                            type={ty.as_str()}
                            placeholder={placeholder.clone()}
                            value={init}
                            on:input:target={move |ev| {
                                input_value.set(ev.target().value().clone());
                            }}
                            on:keypress={
                                let commit = commit.clone();
                                move |ev| {
                                    if ev.key() == "Enter" {
                                        commit();
                                    }
                                }
                            }
                        />
                        {button.clone().map(|btn| {
                            view! {
                                <button class="rendered-input-button" on:click={move |_| {
                                    commit();
                                }}>{btn}</button>
                            }
                        })}
                    </div>
                }
                .into_any()
            }
        }
    }
    let render = {
        let backend = backend.clone();
        move |comp: &bag_lib::ui::Component| render_static(&dispatch, &backend, comp, &path)
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
        Some(Ok(LayoutOrAction::Action(_))) => ().into_any(),
    };

    view! {
        <div>
            {inner}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::gallery_placeholder_icon;
    use bag_lib::ui::GalleryImageType;

    #[test]
    fn selects_default_gallery_icons() {
        assert_eq!(
            gallery_placeholder_icon(&GalleryImageType::Directory),
            "folder"
        );
        assert_eq!(
            gallery_placeholder_icon(&GalleryImageType::Archive),
            "folder_zip"
        );
        assert_eq!(gallery_placeholder_icon(&GalleryImageType::File), "image");
    }
}
