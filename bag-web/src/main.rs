use bag_lib::{action::Action, ui::LayoutOrAction};
use futures::{channel::oneshot::{Receiver, Sender}, select, StreamExt};
use leptos::{prelude::*, task::spawn_local};
use web_sys::wasm_bindgen::prelude::*;

use crate::util::FetchError;

mod panel;
mod util;

#[derive(Clone)]
struct Context {
    targets: RwSignal<RenderTargetSet>,
    backend: String,
}

impl Context {
    pub fn new(backend: String, init: String) -> Self {
        let (retire_tx, retire_rx) = futures::channel::oneshot::channel();
        let cur = RenderTarget::new(&backend, init, Some(retire_tx));
        let ret = Self {
            targets: RwSignal::new(RenderTargetSet {
                current: cur,
                next: None,
                prev: None,
            }),
            backend,
        };
        ret.subscribe(&ret.targets.read_untracked().current, retire_rx);
        ret
    }

    pub async fn handle(&self, action: &Action) {
        match action {
            Action::Navigate { to: p } => {
                self.navigate(p);
            }
        }
    }

    // TODO: relative navigation
    pub fn navigate(&self, p: &str) {
        let targets = self.targets.read_untracked();
        if p == targets.current.path { return; }

        web_sys::window()
            .unwrap()
            .history()
            .unwrap()
            .push_state_with_url(&JsValue::NULL, "", Some(&format!("/{}", p)))
            .unwrap();

        #[derive(Debug)]
        enum Operation {
            RotR,
            RotL,
            Reset,
        }

        let op = if let Some(ref prev) = targets.prev && prev.path == p {
            Operation::RotR
        } else if let Some(ref next) = targets.next && next.path == p {
            Operation::RotL
        } else {
            Operation::Reset
        };

        drop(targets);

        web_sys::console::log_1(&format!("Navigating to {p} with operation {:?}", op).into());
        let Some(mut targets) = self.targets.try_write() else { return };
        web_sys::console::log_1(&format!("Navigating to {p} with operation {:?}", op).into());

        match op {
            Operation::RotR => {
                let replacing = if let Some(prev) = targets.prev.take() {
                    prev
                } else {
                    self.initiate(p.to_string())
                };
                let replaced = std::mem::replace(
                    &mut targets.current,
                    replacing
                );
                targets.next = Some(replaced);
            }
            Operation::RotL => {
                let replacing =  if let Some(next) = targets.next.take() {
                    next
                } else {
                    self.initiate(p.to_string())
                };
                let replaced = std::mem::replace(
                    &mut targets.current,
                    replacing
                );
                targets.prev = Some(replaced);
            }
            Operation::Reset => {
                targets.current = self.initiate(p.to_string());
                targets.prev = None;
                targets.next = None;
            }
        }

        web_sys::console::log_1(&format!("Navigating to {p} with operation {:?}", op).into());
        if let Some(Some(Ok(data))) = targets.current.data.try_get_untracked() {
            drop(targets);
            self.activate(data);
        }
    }

    fn initiate(&self, path: String) -> RenderTarget {
        let (retire_tx, retire_rx) = futures::channel::oneshot::channel();
        let target = RenderTarget::new(&self.backend, path, Some(retire_tx));
        self.subscribe(&target, retire_rx);
        target
    }

    fn activate(&self, data: LayoutOrAction) {
        let Some(mut wr) = self.targets.try_write() else { return };
        match data {
            LayoutOrAction::Layout(layout) => {
                let prev_update = layout.left.as_ref() != wr.prev.as_ref().map(|t| &t.path);
                let next_update = layout.right.as_ref() != wr.next.as_ref().map(|t| &t.path);
                if prev_update || next_update {
                    if prev_update {
                        wr.prev = layout.left.map(|p| self.initiate(p));
                    }
                    if next_update {
                        wr.next = layout.right.map(|p| self.initiate(p));
                    }
                } else {
                    wr.untrack();
                }
            }
            LayoutOrAction::Action(action) => {
                let ctx = self.clone();
                spawn_local(async move { ctx.handle(&action).await });
            }
        }
    }

    fn subscribe(&self, target: &RenderTarget, mut retire: Receiver<()>) {
        let data = target.data.clone();
        let path = target.path.clone();
        let targets = self.targets.clone();
        let ctx = self.clone();
        spawn_local(async move {
            let mut stream = data.to_stream().fuse();
            loop {
                select! {
                    _ = retire => {
                        web_sys::console::log_1(&"Retiring render target: {path}".into());
                        break;
                    },
                    update = stream.next() => {
                        if let Some(Some(Ok(update))) = update {
                            web_sys::console::log_1(&format!("Render target updated {path}: {:?}", update).into());

                            if let Some(rd) = targets.try_read_untracked() {
                                // Ptr Eq here
                                if rd.current.data == data {
                                    drop(rd);
                                    ctx.activate(update);
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}

struct RenderTarget {
    path: String,
    // None = fetching
    data: ArcRwSignal<Option<Result<LayoutOrAction, FetchError>>>,
    // Whether this render target is retired on drop.
    // Avoids memory leaks
    retire: Option<Sender<()>>,
}

impl RenderTarget {
    pub fn new(backend: &str, path: String, retire: Option<Sender<()>>) -> Self {
        let url = format!("{backend}/render/{path}");
        let data = ArcRwSignal::new(None);
        {
            let data = data.clone();
            spawn_local(async move {
                let fetched: Result<LayoutOrAction, _> = crate::util::fetch(&url).await;
                web_sys::console::log_1(&format!("Fetched layout: {data:?}").into());
                data.set(Some(fetched));
            })
        }

        Self {
            path,
            data,
            retire,
        }
    }

    pub fn weak(&self) -> RenderTarget {
        RenderTarget {
            path: self.path.clone(),
            data: self.data.clone(),
            retire: None,
        }
    }
}

impl Drop for RenderTarget {
    fn drop(&mut self) {
        web_sys::console::log_2(&"Dropping render target for path: {}".to_owned().into(), &self.path.clone().into());
        if let Some(retire) = self.retire.take() {
            retire.send(()).unwrap();
        }
    }
}

struct RenderTargetSet {
    current: RenderTarget,
    next: Option<RenderTarget>,
    prev: Option<RenderTarget>,
}

#[derive(Eq, PartialEq, Hash, Clone, Copy)]
enum RenderTargetPersona {
    Current,
    Next,
    Prev,
}

impl RenderTargetPersona {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenderTargetPersona::Current => "current",
            RenderTargetPersona::Next => "next",
            RenderTargetPersona::Prev => "prev",
        }
    }
}

impl RenderTargetSet {
    pub fn as_targets(&self) -> Vec<(RenderTarget, RenderTargetPersona)> {
        let mut targets = vec![];
        if let Some(ref prev) = self.prev {
            targets.push((prev.weak(), RenderTargetPersona::Prev));
        }
        targets.push((self.current.weak(), RenderTargetPersona::Current));
        if let Some(ref next) = self.next {
            targets.push((next.weak(), RenderTargetPersona::Next));
        }
        targets
    }
}

#[component]
fn App() -> impl IntoView {
    console_error_panic_hook::set_once();
    web_sys::console::log_1(&"App mounted".into());
    let local_storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    let backend = local_storage.get_item("backend").unwrap().unwrap();
    web_sys::console::log_1(&format!("Using backend: {}", backend).into());
    // FIXME: dynamic backend

    let initial_path = web_sys::window()
        .unwrap()
        .location()
        .pathname()
        .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
        .unwrap_or_else(|_| String::new());

    let ctx = Context::new(backend, initial_path);
    provide_context(ctx.clone());

    // Listen for popstate events to handle browser navigation (back/forward)
    {
        let ctx = ctx.clone();
        let closure = Closure::wrap(Box::new(move |_event: web_sys::PopStateEvent| {
            let new_path = web_sys::window()
                .unwrap()
                .location()
                .pathname()
                .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
                .unwrap_or_else(|_| String::new());
            ctx.navigate(&new_path);
        }) as Box<dyn FnMut(_)>);
        web_sys::window()
            .unwrap()
            .set_onpopstate(Some(closure.as_ref().unchecked_ref()));
        closure.forget(); // Prevent the closure from being dropped
    }

    view! {
        <For
            each=move || ctx.targets.read().as_targets()
            key=|(target, persona)| (target.path.clone(), *persona)
            let ((target, persona))
        >
            <div class={format!("panel panel-{}", persona.as_str())}>
                <panel::Panel data={target.data.read_only()} />
            </div>
        </For>
    }
}

// TODO: configurable backend
fn main() {
    leptos::mount::mount_to_body(App);
}
